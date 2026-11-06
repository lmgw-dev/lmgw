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

mod unary;
use unary::unary_chat;

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
        // `Realtime`, `Chat`, `AdminChat` and `SystemOne` never reach this
        // handler (their labels are for rows).
        ClientProto::OpenaiChat
        | ClientProto::Realtime
        | ClientProto::Chat
        | ClientProto::AdminChat
        | ClientProto::SystemOne => openai::parse_chat_request(&body),
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
                    quantities: Default::default(),
                    service_tier: Default::default(),
                },
                e.http_status().as_u16(),
                None,
                Usage::default(),
                Some((e.kind(), e.log_text())),
            )
            .await;
            return resp;
        }
    };
    ir.anthropic_beta = ctx.anthropic_beta.clone();
    // Shared, not copied, with whatever outlives this handler: a platform
    // stream's relay reads the request's text from it only if its usage
    // never comes (openai egress design §13.4).
    let ir = std::sync::Arc::new(ir);
    // What a stream shows the client, never what goes upstream (the egress
    // asks for usage itself), so it rides beside the IR, not in it.
    let include_usage = openai::stream_include_usage(&body);
    unanswered.alias(&ir.model_alias);
    unanswered.streamed(ir.stream);

    unanswered.stage("checking the key's policy");
    // After the policy, a reasoning seal the client sends back must be one
    // its key was given (openai egress design §7.3.2): refused as an
    // unknown item otherwise.
    let refused = match policy_refusal(&state, &ctx, &ir.model_alias).await {
        Some(e) => Some(e),
        None => super::seals::check(&state.db, crate::responses::reader(&ctx), &ir.messages)
            .await
            .err(),
    };
    if let Some(e) = refused {
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
    // (`Routed::using`), and a structured output the model is known not to
    // produce is refused right after (`Routed::serves_format`,
    // structured-output design §7).
    let uses = crate::gate::request_facets(&ir, ctx.reasoning_control().as_ref());
    // The route the client called, which a refusal names: this handler
    // serves both chat dialects.
    let endpoint = endpoint(proto);
    unanswered.stage("waiting for admission (GPU room, or the model's container starting)");
    let opened = match async {
        crate::gate::resolve(
            &state,
            &ir.model_alias,
            crate::gate::RouteCheck::Text(endpoint),
            ctx.origin(),
        )
        .await?
        .using(uses)?
        .serves_format(&state, ir.response_format.as_ref().and_then(|f| f.kind()))
        .await?
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
                    quantities: Default::default(),
                    service_tier: Default::default(),
                },
                f.error.http_status().as_u16(),
                None,
                Usage::default(),
                Some((f.error.kind(), f.error.log_text())),
            )
            .await;
            return f.headers.stamp(resp);
        }
    };
    serve_opened(
        &state,
        proto,
        &ctx,
        &ir,
        include_usage,
        opened,
        started,
        &mut unanswered,
    )
    .await
}

/// The route a chat request called, which a refusal names: this handler
/// serves both chat dialects.
pub(super) fn endpoint(proto: ClientProto) -> &'static str {
    match proto {
        ClientProto::AnthropicMessages => "/v1/messages",
        _ => "/v1/chat/completions",
    }
}

/// The post-admission half of [`handle_chat`]: the route's own defaults, the
/// gate's per-send half, and the send. Run once — and once more when a ladder
/// climb hands the request to its fallback before anything was sent (ladder
/// design §12 entry 8): the fallback's route is then served exactly as if the
/// admission had swapped to it, with no hold, so it cannot climb again.
///
/// `unanswered` holds the request's row until a row is written: it is handed
/// over right before each write here, and to the relay task with a stream.
/// `include_usage` is the client's `stream_options.include_usage`, for the
/// stream's encoder.
#[allow(clippy::too_many_arguments)]
async fn serve_opened(
    state: &SharedState,
    proto: ClientProto,
    ctx: &RequestCtx,
    ir: &std::sync::Arc<ChatRequest>,
    include_usage: bool,
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
    // Flex asked for by a budgeted caller where this route's scope has no
    // flex price (openai egress design §13.3): refused on the route that
    // would have answered, before the send. Run again whenever the request
    // is served again on another route (a ladder's fallback, a candidate's
    // re-pick).
    let tier_refused = priced_refusal(
        state,
        &ctx.key_ref(),
        proto.as_str(),
        &ir.model_alias,
        &route,
        crate::telemetry::RequestClass::Chat,
        ir.requested_service_tier().as_deref(),
    )
    .err();

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
            crate::egress::openai_wire::has_reasoning_object(ir),
        ),
        // The samplers the route leaves out; the request's own
        // protocol-only fields follow the fit, below.
        params_ignored: params_ignored(route.upstream.protocol, &params),
        // The send says (`reasoning_fit::send_chat`), below.
        params_refused: Vec::new(),
        max_tokens_defaulted,
        max_tokens_raised: anthropic_max_tokens_raised(&route, &params),
        max_tokens_clamped: None,
        images_omitted: None,
        // A Messages client's verbatim Anthropic blocks, which only an
        // Anthropic route is sent (`ir::AnthropicBlock`).
        blocks_ignored: ir.blocks_ignored(&route.upstream),
    };
    // An off this cloud model cannot take as asked goes out in the form it
    // can (§5.6); reported below with the other ignored controls.
    let mut fitted = super::reasoning_fit::fit(state, &route, &mut params).await;
    // The request's own protocol-only fields the route does not send
    // (`context_management` off an Anthropic route, a `thinking.display`
    // beside the off that went out), on the control the fit left.
    annotations
        .params_ignored
        .extend(ir.fields_ignored(route.upstream.protocol, &params.reasoning_control()));
    // What the send's content lost to a model that lacks a capability, for
    // an error row written below (`request_logs.degraded`).
    let mut degraded = None;

    // The gate's per-send half, on the final params: a guarded row's clamp,
    // count and pool reservation, a ladder's clamp. Its lease goes wherever
    // the response goes.
    let fit = match tier_refused {
        Some(error) => Err(crate::gate::FitRefusal {
            error,
            max_tokens_clamped: None,
            rung: None,
        }),
        None => {
            crate::gate::fit_chat(
                state,
                admission.as_ref(),
                &route,
                ir,
                &mut params,
                ir.stream,
                None,
            )
            .await
        }
    };
    let result = match fit {
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
            // The request the gate fitted, shared: the request itself where
            // the gate left it as it was.
            let gated = match gated {
                std::borrow::Cow::Borrowed(_) => ir.clone(),
                std::borrow::Cow::Owned(o) => std::sync::Arc::new(o),
            };
            if ir.stream {
                // A platform stream's prompt counts once it went out, should
                // the client go away before the upstream's 2xx (§13.4).
                let prompt_sent =
                    (!state.snapshot().is_local_upstream(route.upstream.id)).then(|| {
                        let sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        unanswered.count_if_sent(gated.clone(), sent.clone());
                        sent
                    });
                // The guard and the lease travel into the stream: a streaming
                // response outlives this function, and the model is in use
                // until the last token.
                let r = stream_chat(
                    state,
                    proto,
                    ctx,
                    gated,
                    include_usage,
                    &route,
                    &mut headers,
                    egress,
                    &params,
                    &mut fitted,
                    started,
                    admission,
                    lease,
                    prompt_sent,
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
                    admission,
                    lease,
                    egress,
                    &params,
                    &mut fitted,
                    started,
                    unanswered,
                )
                .await;
                r
            }
        }
    };

    // A refusal from where the request was handed (review B6): its row is on
    // the hop that gave it, not on the route it left.
    let mut failed_route: Option<Box<Route>> = None;
    let mut refused_elsewhere = false;
    let result = match result {
        Ok(Turn::Answered(resp)) => Ok(resp),
        // Served again on what the gate picked (ladder design §12 entry 8,
        // candidate-aliases §12 entry 45) — nothing was sent on this one.
        Ok(Turn::Rerouted(Ok(opened))) => {
            return Box::pin(serve_opened(
                state,
                proto,
                ctx,
                ir,
                include_usage,
                opened,
                started,
                unanswered,
            ))
            .await;
        }
        // A refusal instead (a candidate alias's deferral) answers under its
        // own headers: nothing the first route said applies to it.
        Ok(Turn::Rerouted(Err(f))) => {
            failed_route = f.route;
            headers = f.headers;
            annotations.max_tokens_clamped = None;
            annotations.images_omitted = None;
            degraded = None;
            refused_elsewhere = true;
            Err(f.error)
        }
        Err(e) => Err(e),
    };
    fitted.report(&mut annotations.ignored);
    // What the first route left out says nothing of the refusal that
    // answers instead.
    if !refused_elsewhere {
        annotations.params_refused = fitted.params_refused();
    }
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
                    route: Some(failed_route.as_deref().unwrap_or(&route)),
                    started,
                    streamed: ir.stream,
                    class: RequestClass::Chat,
                    timings: None,
                    max_tokens_clamped: annotations.max_tokens_clamped,
                    fallback: headers.fallback_reason(),
                    rung: headers.rung().map(crate::gate::RungTag::log),
                    degraded,
                    quantities: Default::default(),
                    service_tier: Default::default(),
                },
                // The status the client got: Anthropic's wire has its own for
                // an upstream quota.
                resp.status().as_u16(),
                None,
                Usage::default(),
                Some((e.kind(), e.log_text())),
            )
            .await;
            resp
        }
    };
    with_annotations(headers.stamp(resp), &annotations)
}
