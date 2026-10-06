//! `POST /v1/chat/completions` and `POST /v1/messages`: the unary chat
//! handler, its post-admission serve path, and the non-streaming send.

use std::time::Instant;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::config::Route;
use crate::egress::{for_protocol, with_timeout, Egress};
use crate::error::GatewayError;
use crate::gate::GateHeaders;
use crate::ingress::{anthropic, openai, ClientProto};
use crate::ir::{ChatRequest, Usage};
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::*;

/// Entry point for `POST /v1/chat/completions` and `POST /v1/messages`.
pub async fn handle_chat(
    state: SharedState,
    proto: ClientProto,
    ctx: RequestCtx,
    body: Value,
) -> Response {
    let started = Instant::now();
    let alias_hint = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    // Opens the in-flight gauge, and writes the `499` row should the client
    // go away before any row is written (`unanswered.rs`).
    let mut unanswered = Unanswered::open(
        &state,
        proto,
        &ctx,
        &alias_hint,
        started,
        RequestClass::Chat,
    );

    let parsed = match proto {
        // `Realtime`, `Chat` and `AdminChat` never reach this handler (their
        // labels are for rows).
        ClientProto::OpenaiChat
        | ClientProto::Realtime
        | ClientProto::Chat
        | ClientProto::AdminChat => openai::parse_chat_request(&body),
        ClientProto::AnthropicMessages => anthropic::parse_messages_request(&body),
    };
    let mut ir = match parsed {
        Ok(ir) => ir,
        Err(e) => {
            let resp = error_response(proto, &e);
            unanswered.logging();
            record(
                LogParams {
                    state: &state,
                    proto,
                    ctx: &ctx,
                    alias: alias_hint,
                    route: None,
                    started,
                    streamed: false,
                    class: RequestClass::Chat,
                    timings: None,
                    max_tokens_clamped: None,
                    fallback: None,
                    rung: None,
                    degraded: None,
                },
                e.http_status().as_u16(),
                None,
                Usage::default(),
                Some((e.kind(), e.to_string())),
            )
            .await;
            return resp;
        }
    };
    ir.anthropic_beta = ctx.anthropic_beta.clone();
    unanswered.alias(&ir.model_alias);
    unanswered.streamed(ir.stream);

    unanswered.stage("checking the key's policy");
    if let Some(e) = policy_refusal(&state, &ctx, &ir.model_alias).await {
        unanswered.logging();
        record_refusal(
            &state,
            proto,
            &ctx,
            &ir.model_alias,
            started,
            RequestClass::Chat,
            &e,
        )
        .await;
        return error_response(proto, &e);
    }

    // The gate's per-request half: the hold swap (not `resolve` — a local
    // model under a GPU hold is re-routed to its fallback, or refused with
    // `gpu_hold`, before anything downstream reads the route; gpu-hold design
    // §4), the media-route check, then GPU admission (§9b) — room is made for
    // a local model that is not resident before a single byte goes upstream,
    // or it is refused by name. A cloud route or a model already loaded
    // passes straight through, and a local route comes back on the port its
    // container answers on (§5). A fallback that cannot see gets the images
    // as placeholders at the send (`gate::fallback_images`). A candidate
    // alias refuses a facet it does not enable here, before anything starts
    // (`Routed::using`).
    let uses = crate::gate::request_facets(&ir, ctx.reasoning_control().as_ref());
    unanswered.stage("waiting for admission (GPU room, or the model's container starting)");
    let opened = match async {
        crate::gate::resolve(
            &state,
            &ir.model_alias,
            crate::gate::RouteCheck::Text("/v1/chat/completions"),
        )
        .await?
        .using(uses)?
        .admit(&state)
        .await
    }
    .await
    {
        Ok(o) => o,
        Err(f) => {
            let resp = error_response(proto, &f.error);
            unanswered.logging();
            record(
                LogParams {
                    state: &state,
                    proto,
                    ctx: &ctx,
                    alias: ir.model_alias.clone(),
                    route: f.route.as_deref(),
                    started,
                    streamed: ir.stream,
                    class: RequestClass::Chat,
                    timings: None,
                    max_tokens_clamped: None,
                    fallback: f.headers.fallback_reason(),
                    rung: None,
                    degraded: None,
                },
                f.error.http_status().as_u16(),
                None,
                Usage::default(),
                Some((f.error.kind(), f.error.to_string())),
            )
            .await;
            return f.headers.stamp(resp);
        }
    };
    serve_opened(&state, proto, &ctx, &ir, opened, started, &mut unanswered).await
}

/// The post-admission half of [`handle_chat`]: the route's own defaults, the
/// gate's per-send half, and the send. Run once — and once more when a ladder
/// climb hands the request to its fallback before anything was sent (ladder
/// design §12 entry 8): the fallback's route is then served exactly as if the
/// admission had swapped to it, with no hold, so it cannot climb again.
///
/// `unanswered` holds the request's row until a row is written: it is handed
/// over right before each write here, and to the relay task with a stream.
async fn serve_opened(
    state: &SharedState,
    proto: ClientProto,
    ctx: &RequestCtx,
    ir: &ChatRequest,
    opened: crate::gate::Opened,
    started: Instant,
    unanswered: &mut Unanswered,
) -> Response {
    let crate::gate::Opened {
        mut route,
        hold: admission,
        mut headers,
    } = opened;
    unanswered.routed_to(&route, headers.fallback_reason());
    unanswered.stage("waiting for the model's answer");

    // §5.4: the cap lmgw picks when nobody else did, before the egress' own
    // hidden default can apply.
    let max_tokens_defaulted =
        anthropic_max_tokens_default(state, &mut route, &ir.params, ir.stream).await;

    let egress = for_protocol(route.upstream.protocol);
    let mut params = resolve_params(ir, ctx, &route);
    let mut annotations = Annotations {
        ignored: reasoning_ignored(
            route.upstream.protocol,
            &params.reasoning_control(),
            crate::egress::openai::has_reasoning_object(ir),
        ),
        max_tokens_defaulted,
        max_tokens_raised: anthropic_max_tokens_raised(&route, &params),
        max_tokens_clamped: None,
        images_omitted: None,
    };
    // An off this cloud model cannot take as asked goes out in the form it
    // can (§5.6); reported below with the other ignored controls.
    let mut fitted = super::reasoning_fit::fit(state, &route, &mut params).await;
    // What the send's content lost to a model that lacks a capability, for
    // an error row written below (`request_logs.degraded`).
    let mut degraded = None;

    // The gate's per-send half, on the final params: a guarded row's clamp,
    // count and pool reservation, a ladder's clamp. Its lease goes wherever
    // the response goes.
    let result = match crate::gate::fit_chat(
        state,
        admission.as_ref(),
        &route,
        ir,
        &mut params,
        ir.stream,
        None,
    )
    .await
    {
        // The clamp (step 1) already ran even though the reservation (step
        // 4) refused — the response still has to carry it (review finding
        // 7), so it is read off the refusal itself rather than a lease that
        // was never returned. So does the rung it was refused on.
        Err(f) => {
            annotations.max_tokens_clamped = f.max_tokens_clamped;
            headers.set_rung(f.rung);
            Err(f.error)
        }
        Ok((lease, gated)) => {
            annotations.max_tokens_clamped = lease.max_tokens_clamped();
            // A fallback that cannot see got the images as placeholders: how
            // many, next to `x-lmgw-fallback` (`gate::fallback_images`).
            annotations.images_omitted = lease.images_omitted();
            degraded = lease.degraded();
            if ir.stream {
                // The guard and the lease travel into the stream: a streaming
                // response outlives this function, and the model is in use
                // until the last token.
                let r = stream_chat(
                    state,
                    proto,
                    ctx,
                    &gated,
                    &route,
                    &mut headers,
                    egress,
                    &params,
                    &mut fitted,
                    started,
                    admission,
                    lease,
                )
                .await;
                // The relay task writes the stream's row from here on, a
                // client that goes away mid-stream included (`canceled`).
                if matches!(r, Ok(Turn::Answered(_))) {
                    unanswered.logging();
                }
                r
            } else {
                let r = unary_chat(
                    state,
                    proto,
                    ctx,
                    &gated,
                    &route,
                    &mut headers,
                    admission.as_ref(),
                    lease,
                    egress,
                    &params,
                    &mut fitted,
                    started,
                    unanswered,
                )
                .await;
                drop(admission);
                r
            }
        }
    };

    let result = match result {
        Ok(Turn::Answered(resp)) => Ok(resp),
        // Served again on what the gate picked (ladder design §12 entry 8,
        // candidate-aliases §12 entry 45) — nothing was sent on this one.
        Ok(Turn::Rerouted(Ok(opened))) => {
            return Box::pin(serve_opened(
                state, proto, ctx, ir, opened, started, unanswered,
            ))
            .await;
        }
        // A refusal instead (a candidate alias's deferral) answers under its
        // own headers: nothing the first route said applies to it.
        Ok(Turn::Rerouted(Err(f))) => {
            headers = f.headers;
            annotations.max_tokens_clamped = None;
            annotations.images_omitted = None;
            degraded = None;
            Err(f.error)
        }
        Err(e) => Err(e),
    };
    fitted.report(&mut annotations.ignored);
    let resp = match result {
        Ok(resp) => resp,
        Err(e) => {
            let resp = error_response(proto, &e);
            unanswered.logging();
            record(
                LogParams {
                    state,
                    proto,
                    ctx,
                    alias: ir.model_alias.clone(),
                    route: Some(&route),
                    started,
                    streamed: ir.stream,
                    class: RequestClass::Chat,
                    timings: None,
                    max_tokens_clamped: annotations.max_tokens_clamped,
                    fallback: headers.fallback_reason(),
                    rung: headers.rung().map(crate::gate::RungTag::log),
                    degraded,
                },
                e.http_status().as_u16(),
                None,
                Usage::default(),
                Some((e.kind(), e.to_string())),
            )
            .await;
            resp
        }
    };
    with_annotations(headers.stamp(resp), &annotations)
}

/// `headers` learn the rung the send was judged on or served from, whatever
/// the outcome (ladder design §6).
#[allow(clippy::too_many_arguments)]
async fn unary_chat(
    state: &SharedState,
    proto: ClientProto,
    ctx: &RequestCtx,
    ir: &ChatRequest,
    route: &Route,
    headers: &mut GateHeaders,
    hold: Option<&crate::vram::LocalHold>,
    mut lease: crate::gate::TurnLease,
    egress: &'static dyn Egress,
    params: &crate::ir::Params,
    fitted: &mut super::reasoning_fit::Fitted,
    started: Instant,
    unanswered: &mut Unanswered,
) -> Result<Turn<Response>, GatewayError> {
    let max_tokens_clamped = lease.max_tokens_clamped();
    let degraded = lease.degraded();
    let timeout = route.upstream.request_timeout();
    let sent = super::reasoning_fit::send_chat(
        state,
        hold,
        route,
        &mut lease,
        ir,
        params,
        false,
        None,
        None,
        None,
        fitted,
        super::reasoning_fit::RowAs::Public {
            proto,
            ctx,
            alias: &ir.model_alias,
            fallback: headers.fallback_reason(),
        },
        |r, p| {
            Ok(with_timeout(
                egress.build_chat(&state.http, &r.upstream, &r.upstream_model, ir, p, false)?,
                timeout,
            ))
        },
    )
    .await;
    headers.set_rung(lease.rung());
    let rung = lease.rung_log();
    let resp = match sent? {
        crate::gate::Sent::Upstream(resp) => resp,
        crate::gate::Sent::Rerouted(r) => return Ok(Turn::Rerouted(r)),
    };
    let status = resp.status();
    let ttfb = started.elapsed().as_millis() as i64;
    // A body that could not be read to its end (a timeout, a reset) drops the
    // lease here unfinished: released once llama-server lets go of the slot.
    let bytes = resp.bytes().await.map_err(GatewayError::from)?;
    // The whole answer is read: the generation is over, and so is its claim
    // on the pool.
    lease.complete();
    if !status.is_success() {
        return Err(crate::gate::attribute(
            egress.map_error(status.as_u16(), &bytes),
            route,
        ));
    }
    let completion = egress.parse_completion(&bytes)?;
    let body = proto.serialize_completion(&ir.model_alias, &completion);
    unanswered.logging();
    record(
        LogParams {
            state,
            proto,
            ctx,
            alias: ir.model_alias.clone(),
            route: Some(route),
            started,
            streamed: false,
            class: RequestClass::Chat,
            timings: completion.timings,
            max_tokens_clamped,
            fallback: headers.fallback_reason(),
            rung,
            degraded,
        },
        StatusCode::OK.as_u16(),
        Some(ttfb),
        completion.usage,
        None,
    )
    .await;
    Ok(Turn::Answered(axum::Json(body).into_response()))
}
