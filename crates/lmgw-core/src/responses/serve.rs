//! The serving half of `POST /v1/responses` (openai egress design §6,
//! §14.4): native or the synthesized loop on the route the gate settled on,
//! the priced check on that route (decision 24, §13.3), and — when a native
//! hop could not answer (an exhausted quota, an unreachable upstream,
//! [`native::Native::HandOn`]) — its fallback, opened as the hop it is and
//! served here once more as if the client had named it: `path::choose`
//! decides native or loop for the fallback's upstream, and its own priced
//! check runs on its route.

use std::time::Instant;

use axum::response::{IntoResponse, Response};
use serde_json::Value;

use super::*;
use crate::gate::chain::{self, Next, Walk};
use crate::gate::{GateHeaders, Opened, Routed};

/// The route a request is served on: as the gate resolved it (admission
/// still ahead, for the loop), or a fallback a native hop handed it to
/// (admitted already).
pub(super) enum Ready {
    Routed(Routed),
    Opened(Opened),
}

impl Ready {
    fn resolved(&self) -> &Route {
        match self {
            Ready::Routed(r) => r.resolved(),
            Ready::Opened(o) => &o.route,
        }
    }

    fn headers(&self) -> &GateHeaders {
        match self {
            Ready::Routed(r) => r.headers(),
            Ready::Opened(o) => &o.headers,
        }
    }
}

/// The hop a fallback took the request over from: the error it gives if
/// the fallback cannot take the request after all, and the fallback's name.
pub(super) struct Handed {
    error: GatewayError,
    name: String,
}

/// Native or the loop on `ready`'s route (module doc). `handed` is set when
/// a native hop handed the request to this route.
#[allow(clippy::too_many_arguments)]
pub(super) async fn serve_routed(
    state: SharedState,
    ctx: RequestCtx,
    mut req: ResponsesRequest,
    body: Value,
    ready: Ready,
    started: Instant,
    alias_hint: &str,
    handed: Option<Handed>,
) -> Response {
    // Native or the loop (openai egress design §6): an upstream that
    // implements the API gets the request unless only the loop can serve it.
    let path = path::choose(&state, &ctx, &ready.resolved().upstream, &req, &body).await;
    let path = match path {
        Ok(p) => p,
        Err(e) => {
            // A fallback that cannot take the request after all (a part only
            // a native upstream serves, a `previous_response_id` only the
            // provider holds): the hop's own error stands, saying why
            // (§14.4).
            let e = match handed {
                Some(h) => h.error.with_clause(&format!(
                    " (fallback '{}' cannot serve this endpoint: {})",
                    h.name,
                    e.to_string().trim_end_matches('.')
                )),
                None => e,
            };
            let fallback = ready.headers().fallback_reason();
            let resp = reject(&state, &ctx, alias_hint, fallback, started, e).await;
            return ready.headers().stamp(resp);
        }
    };
    // Flex asked for by a budgeted caller where the route's scope has no
    // flex price (openai egress design §13.3), on either path: refused on
    // the route that would have answered, before anything is sent. The
    // loop's turns check again at their own open (`proxy::in_process`).
    if let Err(e) = proxy::priced_refusal(
        &state,
        &ctx.key_ref(),
        RESPONSES_PROTO,
        &req.ir.model_alias,
        ready.resolved(),
        crate::telemetry::RequestClass::Chat,
        req.ir.requested_service_tier().as_deref(),
    ) {
        let fallback = ready.headers().fallback_reason();
        let status = e.http_status();
        proxy::record_request_failure(
            &state,
            &ctx,
            RESPONSES_PROTO,
            alias_hint,
            Some(ready.resolved()),
            fallback,
            started,
            &e,
        )
        .await;
        let resp = (status, axum::Json(wire::serialize_error(&e))).into_response();
        return ready.headers().stamp(resp);
    }
    if path == path::Path::Native {
        // The provider implements the API — including its own store — so a
        // `previous_response_id` lmgw does not hold, and approvals, are its
        // business, not ours.
        //
        // The control is resolved through all three tiers here too (§5.2): the
        // alias' `param_defaults.reasoning` is as much a part of what this
        // alias means on a native upstream as on a synthesized one, and
        // reading only the headers would have made the alias default apply
        // everywhere except the one route that forwards the body verbatim.
        let route = ready.resolved();
        let control = proxy::resolve_params(&req.ir, &ctx, route).reasoning_control();
        let fallback = ready.headers().fallback_reason();
        // A fallback that cannot see gets the body's images as placeholders,
        // as every other send gets the IR's (`gate::fallback_images`).
        let mut sent = body.clone();
        let omitted = unseen::fit(&state, route, &req.ir.model_alias, &mut sent).await;
        let (resp, fitted) = match native::native_passthrough(
            &state,
            &ctx,
            (route, fallback),
            &sent,
            started,
            &control,
            omitted.as_ref().map(|o| o.marker.clone()),
            ready.headers().chain().tried(),
        )
        .await
        {
            native::Native::Answered { resp, fitted } => (resp, fitted),
            // The fallback answers under its own headers: nothing this hop
            // left out, or sent its off as, is named on its answer.
            native::Native::HandOn { error, reason, row } => {
                return hand_on(
                    state,
                    ctx,
                    req,
                    body,
                    ready,
                    started,
                    alias_hint,
                    (error, reason, row),
                )
                .await
            }
        };
        // What this forward's own sends left out because the model refused
        // it (`native::refused_params`) and the form their off went out in
        // (`native::reasoning_off`), whole and streamed alike: a stream's
        // refusal comes before its first byte.
        let annotations = proxy::Annotations {
            images_omitted: omitted.map(|o| o.count),
            ..native_annotations(&control, &fitted)
        };
        return proxy::with_annotations(ready.headers().stamp(resp), &annotations);
    }

    // A `text.format` type lmgw does not model went to the native upstream
    // as it came; the synthesized loop has no chat spelling to send it as
    // (structured-output design §11.3).
    if let Some(f @ crate::ir::ResponseFormat::Other { .. }) = &req.ir.response_format {
        let e = GatewayError::ResponseFormatUnsupported {
            message: format!(
                "text.format type '{}' has no chat-completions equivalent to send to '{}' — use \
                 json_schema, json_object or text",
                f.type_name(),
                req.ir.model_alias
            ),
        };
        let fallback = ready.headers().fallback_reason();
        let resp = reject(&state, &ctx, alias_hint, fallback, started, e).await;
        return ready.headers().stamp(resp);
    }

    // Continue a stored conversation, if asked. This both prepends its history
    // and settles whatever calls it stopped at.
    let mut prior: Option<StoredResponse> = None;
    let mut resume: Vec<DecidedCall> = Vec::new();
    if let Some(prev_id) = req.previous_response_id.clone() {
        // Whoever continues the response decides its pending calls: the
        // request's own principal.
        let by = crate::mcp::host::CallFrom::of(&ctx).caller;
        match load_chain(&state, &prev_id, &mut req, Some(&by), reader(&ctx)).await {
            Ok((stored, decided)) => {
                prior = Some(stored);
                resume = decided;
            }
            // Refused *after* the hold already picked a fallback, so the
            // rejection is attributed to it like every other error on this
            // route (gpu-hold design §4, `proxy::Failed`).
            Err(e) => {
                let fallback = ready.headers().fallback_reason();
                let resp = reject(&state, &ctx, alias_hint, fallback, started, e).await;
                return ready.headers().stamp(resp);
            }
        }
    }

    // GPU admission (§9b) for the whole synthesized run — the gate's last
    // per-request stage: make room for a local model that is not resident, or
    // refuse by name, before the first turn. A local route comes back on the
    // port its container answers on (§5); the turn loop below carries this
    // route in its runner, so that reaches every turn, not only the first.
    // The stored chain is prepended by now, so this sees every image the
    // run will send (`Routed::using`).
    let admitted = match ready {
        Ready::Routed(routed) => match routed.using(uses(&req, ctx.reasoning_control().as_ref())) {
            Ok(routed) => routed.admit(&state).await,
            Err(f) => Err(f),
        },
        // A fallback a native hop handed the request to: admitted already.
        Ready::Opened(opened) => Ok(opened),
    };
    let crate::gate::Opened {
        mut route,
        hold: admission,
        mut headers,
    } = match admitted {
        Ok(o) => o,
        Err(f) => {
            let fallback = f.headers.fallback_reason();
            let resp = reject(&state, &ctx, alias_hint, fallback, started, f.error).await;
            return f.headers.stamp(resp);
        }
    };

    // §5.2/§5.4, once for the whole run: every turn the loop makes reads the
    // same `req.ir.params` and the same `route.param_defaults`, so resolving
    // here reaches turn 7 as surely as turn 1.
    let max_tokens_defaulted =
        proxy::anthropic_max_tokens_default(&state, &mut route, &req.ir.params, req.stream).await;
    let resolved = proxy::resolve_params(&req.ir, &ctx, &route);
    let mut annotations = proxy::Annotations {
        ignored: proxy::reasoning_ignored(
            route.upstream.protocol,
            &resolved.reasoning_control(),
            crate::egress::openai_wire::has_reasoning_object(&req.ir),
        ),
        params_ignored: proxy::params_ignored(route.upstream.protocol, &resolved),
        // What a refusal on record leaves out of every turn's send; a refusal
        // the run itself meets is read again once a unary run is done.
        params_refused: proxy::reasoning_fit::params_on_record(&state, &route, &resolved),
        max_tokens_defaulted,
        max_tokens_raised: proxy::anthropic_max_tokens_raised(&route, &resolved),
        // The headers leave before the first turn runs, so this is what every
        // turn's own per-send clamp will be — they all send these `params`.
        // Each turn logs its own value as it runs.
        max_tokens_clamped: crate::gate::planned_clamp(admission.as_ref(), &req.ir, &resolved),
        // Likewise the images the run opened with that go to a fallback
        // that cannot see as placeholders (`gate::fallback_images`).
        images_omitted: crate::gate::fallback_images::decide(&state, &route, &req.ir)
            .await
            .map(|_| crate::gate::fallback_images::count_images(&req.ir, &route.upstream)),
        // What the stored chain the run continues carries that this row is
        // not sent (an Anthropic block another scope signed, a verbatim
        // block off an Anthropic route); a unary run's answers' blocks
        // follow once it is done (below).
        blocks_ignored: req.ir.blocks_ignored(&route.upstream),
    };
    // Likewise `x-lmgw-rung`: the rung the model runs as the run opens
    // (ladder design §6). A turn that climbs logs the rung it climbed to; a
    // unary run one of whose turns the fallback answered says so instead
    // (`run_loop`).
    headers.set_rung(crate::gate::planned_rung(admission.as_ref()));
    req.ir.params = resolved;

    let fallback = headers.fallback_reason();
    let streamed = req.stream;
    let mut run_refused = Vec::new();
    let mut answer_blocks = Vec::new();
    let resp = run_loop(
        state.clone(),
        ctx,
        req,
        route,
        fallback,
        body,
        started,
        prior,
        resume,
        admission,
        &mut headers,
        &mut run_refused,
        &mut answer_blocks,
    )
    .await;
    // A unary run a ladder climb handed to the fallback mid-loop (`run_loop`
    // set the fallback, which was not there at open): that turn was sent the
    // client's own body, never clamped, so the clamp planned at open does not
    // describe the answer — like the rung, it goes (ladder §12 entry 72).
    if fallback.is_none() && headers.fallback_reason().is_some() {
        annotations.max_tokens_clamped = None;
    }
    // A unary run's headers leave after its turns: what its own sends left
    // out, a refusal one of them met included. A stream's left before (its
    // refused attempt's row says it).
    // So do the Anthropic blocks its answers carried, which the Responses
    // wire has no item for; a stream's are a log line
    // (`GatewayRunner::note_answer_blocks`).
    if !streamed {
        annotations.params_refused = run_refused;
        for name in answer_blocks {
            if !annotations.blocks_ignored.contains(&name) {
                annotations.blocks_ignored.push(name);
            }
        }
    }
    proxy::with_annotations(headers.stamp(resp), &annotations)
}

/// A native hop could not answer for `reason` (§14.4): its fallback, opened
/// as the hop it is, serves the request; a fallback that cannot take it
/// leaves `error` standing with the clause that says why.
#[allow(clippy::too_many_arguments)]
async fn hand_on(
    state: SharedState,
    ctx: RequestCtx,
    req: ResponsesRequest,
    body: Value,
    ready: Ready,
    started: Instant,
    alias_hint: &str,
    (error, reason, row): (GatewayError, FallbackReason, Option<Box<native::HandedRow>>),
) -> Response {
    let route = ready.resolved();
    let hop = route
        .fallback
        .as_deref()
        .unwrap_or(&req.ir.model_alias)
        .to_string();
    let snap = state.snapshot();
    let Some(key) = snap.hop_key(&hop) else {
        let fallback = ready.headers().fallback_reason();
        let resp = reject(&state, &ctx, alias_hint, fallback, started, error).await;
        return ready.headers().stamp(resp);
    };
    let walk = Walk {
        check: crate::gate::RouteCheck::Text("/v1/responses"),
        origin: ctx.origin(),
    };
    let next = snap.next_hop(&key);
    let chain_now = ready.headers().chain().clone();
    let before = chain_now.hops().len();
    let mut tried = chain_now.clone();
    tried.leave(&hop, key.clone());
    // The sent attempt's own row, once the walk handed the request on: to
    // the first hop it reached (§14.7). A walk that ends writes none — its
    // one row is the hop's error (review B5).
    let attempt = |c: &crate::gate::Chain| c.hops().get(before).map(|(n, _)| n.clone());
    // Held while the walk admits a local fallback: a client that leaves
    // then still gets the attempt's row (`native::HeldRow`).
    let configured = snap.fallback_of(&key).unwrap_or_default().to_string();
    let mut row = native::HeldRow::arm(row, configured, reason);
    let handed = |to: Option<String>, row: Option<Box<native::HandedRow>>| async move {
        if let (Some(row), Some(to)) = (row, to) {
            row.write(&to, reason).await;
        }
    };
    match chain::fall_back(&state, walk, chain_now, (&hop, key), next, reason).await {
        Next::Opened(opened) => {
            handed(attempt(opened.headers.chain()), row.take()).await;
            let name = opened.headers.fallback().unwrap_or_default().to_string();
            Box::pin(serve_routed(
                state.clone(),
                ctx,
                req,
                body,
                Ready::Opened(opened),
                started,
                alias_hint,
                Some(Handed { error, name }),
            ))
            .await
        }
        Next::Failed(f) => {
            handed(attempt(f.headers.chain()), row.take()).await;
            let fallback = f.headers.fallback_reason();
            let resp = reject(&state, &ctx, alias_hint, fallback, started, f.error).await;
            f.headers.stamp(resp)
        }
        Next::Ended(clause) => {
            drop(row.take());
            let e = error.with_clause(&format!("{clause}{}", tried.tried()));
            let fallback = ready.headers().fallback_reason();
            let resp = reject(&state, &ctx, alias_hint, fallback, started, e).await;
            ready.headers().stamp(resp)
        }
    }
}
