//! `POST /v1/completions` (legacy text completions, openai-protocol upstreams
//! only). The universal counter, `POST /v1/count_tokens`, lives in `count.rs`.

use std::time::Instant;

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use crate::egress::{apply_bearer_auth, for_protocol, with_timeout};
use crate::error::GatewayError;
use crate::ingress::ClientProto;
use crate::ir::Usage;
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::*;

/// `POST /v1/completions` — legacy text completions. The IR is chat-shaped,
/// so this is a thin pass-through supported only for `openai`-protocol
/// upstreams (covers llama.cpp / llama-server FIM and completion use-cases).
pub async fn handle_legacy_completions(
    state: SharedState,
    ctx: RequestCtx,
    body: Value,
) -> Response {
    let started = Instant::now();
    let proto = ClientProto::OpenaiChat;
    let streamed = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    // Opens the in-flight gauge, and writes the `499` row should the client
    // go away before any row is written (`unanswered.rs`).
    let mut unanswered = Unanswered::open(
        &state,
        proto,
        &ctx,
        body.get("model").and_then(Value::as_str).unwrap_or("?"),
        started,
        RequestClass::Chat,
    );
    unanswered.streamed(streamed);
    let Some(alias) = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        // The 400 every other ingress gives (`ingress/openai.rs`), logged as
        // `/v1/chat/completions` logs its parse errors. It used to route the
        // placeholder name `?` and answer 404 "unknown model alias: ?".
        let e = GatewayError::BadRequest("missing 'model'".into());
        unanswered.logging();
        record(
            LogParams {
                state: &state,
                proto,
                ctx: &ctx,
                alias: "?".into(),
                route: None,
                started,
                streamed,
                class: RequestClass::Chat,
                timings: None,
                max_tokens_clamped: None,
                fallback: None,
                rung: None,
                degraded: None,
                quantities: Default::default(),
            },
            e.http_status().as_u16(),
            None,
            Usage::default(),
            Some((e.kind(), e.to_string())),
        )
        .await;
        return error_response(proto, &e);
    };

    unanswered.stage("checking the key's policy");
    if let Some(e) = policy_refusal(&state, &ctx, &alias).await {
        unanswered.logging();
        record_refusal(&state, proto, &ctx, &alias, started, RequestClass::Chat, &e).await;
        return error_response(proto, &e);
    }

    // The gate's per-request half. The hold swap runs *before* the protocol
    // check, not after: the check has to be made against the upstream the
    // bytes actually go to (gpu-hold design §4). A fallback on an
    // anthropic-protocol upstream therefore fails here with this handler's own
    // error, exactly as it would if the client had named that alias — never a
    // silent misroute.
    unanswered.stage("waiting for admission (GPU room, or the model's container starting)");
    let opened = match async {
        crate::gate::resolve(&state, &alias, crate::gate::RouteCheck::LegacyCompletions)
            .await?
            .using(crate::gate::legacy_facets(&body))?
            .admit(&state)
            .await
    }
    .await
    {
        Ok(o) => o,
        Err(f) => {
            let out = error_response(proto, &f.error);
            unanswered.logging();
            record(
                LogParams {
                    state: &state,
                    proto,
                    ctx: &ctx,
                    alias,
                    route: f.route.as_deref(),
                    started,
                    streamed,
                    class: RequestClass::Chat,
                    timings: None,
                    max_tokens_clamped: None,
                    fallback: f.headers.fallback_reason(),
                    rung: None,
                    degraded: None,
                    quantities: Default::default(),
                },
                f.error.http_status().as_u16(),
                None,
                Usage::default(),
                Some((f.error.kind(), f.error.to_string())),
            )
            .await;
            return f.headers.stamp(out);
        }
    };
    serve_legacy(
        state,
        ctx,
        alias,
        body,
        streamed,
        opened,
        started,
        &mut unanswered,
    )
    .await
}

/// The post-admission half of [`handle_legacy_completions`] — run once, and
/// once more on the fallback's route when a ladder climb hands the request to
/// it before anything was sent (ladder design §12 entry 8). That fallback
/// already passed this endpoint's route check: the climb judges it with the
/// check the admission recorded on the hold.
///
/// `unanswered` holds the request's row until a row is written: it is handed
/// over right before each write here, and to the relay task with a stream.
#[allow(clippy::too_many_arguments)]
async fn serve_legacy(
    state: SharedState,
    ctx: RequestCtx,
    alias: String,
    mut body: Value,
    streamed: bool,
    opened: crate::gate::Opened,
    started: Instant,
    unanswered: &mut Unanswered,
) -> Response {
    let proto = ClientProto::OpenaiChat;
    let crate::gate::Opened {
        route,
        hold: admission,
        mut headers,
    } = opened;
    unanswered.routed_to(&route, headers.fallback_reason());
    unanswered.stage("waiting for the model's answer");
    // A guarded row's or a ladder's fit rewrites the body's max-output
    // fields; wherever the gate reroutes the request instead gets the body
    // as the client sent it. Only a ladder row or a candidate alias's model
    // can be rerouted here, so no other row pays for the copy.
    let pristine = (admission
        .as_ref()
        .and_then(crate::vram::LocalHold::rung)
        .is_some()
        || crate::gate::candidate::reroutable(admission.as_ref()))
    .then(|| body.clone());

    // The gate's per-send half on the raw body: a guarded row's clamp, count
    // and pool reservation — none of it, and no change to the body, on any
    // other route.
    let mut lease =
        match crate::gate::fit_text(&state, admission.as_ref(), &route, &alias, &mut body).await {
            Ok(l) => l,
            Err(f) => {
                let e = f.error;
                let out = error_response(proto, &e);
                unanswered.logging();
                record(
                    LogParams {
                        state: &state,
                        proto,
                        ctx: &ctx,
                        alias,
                        route: Some(&route),
                        started,
                        streamed,
                        class: RequestClass::Chat,
                        timings: None,
                        // The clamp (step 1) already ran even though the
                        // reservation refused — this row must still carry it
                        // (review finding 7).
                        max_tokens_clamped: f.max_tokens_clamped,
                        fallback: headers.fallback_reason(),
                        rung: f.rung.as_ref().map(crate::gate::RungTag::log),
                        degraded: None,
                        quantities: Default::default(),
                    },
                    e.http_status().as_u16(),
                    None,
                    Usage::default(),
                    Some((e.kind(), e.to_string())),
                )
                .await;
                // Same reason: the response itself must still carry the
                // header a client reads this off of, not only the log row.
                headers.set_rung(f.rung);
                let annotations = Annotations {
                    max_tokens_clamped: f.max_tokens_clamped,
                    ..Default::default()
                };
                return with_annotations(headers.stamp(out), &annotations);
            }
        };
    let max_tokens_clamped = lease.max_tokens_clamped();
    let annotations = Annotations {
        max_tokens_clamped,
        ..Default::default()
    };

    body["model"] = json!(route.upstream_model);
    let timeout = route.upstream.request_timeout();
    let send_result = crate::gate::send_gated(
        &state,
        admission.as_ref(),
        &route,
        &mut lease,
        crate::gate::CountInput::Text { body: &body },
        None,
        timeout,
        |r| {
            let url = format!("{}/completions", r.upstream.base());
            let rb = apply_bearer_auth(state.http.post(url).json(&body), &r.upstream);
            Ok(if streamed {
                rb
            } else {
                with_timeout(rb, timeout)
            })
        },
    )
    .await;
    headers.set_rung(lease.rung());
    let rung = lease.rung_log();

    let resp = match send_result {
        Ok(crate::gate::Sent::Upstream(r)) => r,
        // Served again on what the gate picked (ladder design §12 entry 8,
        // candidate-aliases §12 entry 45), with the body as the client sent
        // it; nothing was sent on this one.
        Ok(crate::gate::Sent::Rerouted(Ok(opened))) => {
            drop(lease);
            drop(admission);
            let body = pristine.unwrap_or(body);
            return Box::pin(serve_legacy(
                state, ctx, alias, body, streamed, opened, started, unanswered,
            ))
            .await;
        }
        // A refusal instead (a candidate alias's deferral), under its own
        // headers.
        Ok(crate::gate::Sent::Rerouted(Err(f))) => {
            headers = f.headers;
            let out = error_response(proto, &f.error);
            unanswered.logging();
            record(
                LogParams {
                    state: &state,
                    proto,
                    ctx: &ctx,
                    alias,
                    route: Some(&route),
                    started,
                    streamed,
                    class: RequestClass::Chat,
                    timings: None,
                    max_tokens_clamped: None,
                    fallback: headers.fallback_reason(),
                    rung: None,
                    degraded: None,
                    quantities: Default::default(),
                },
                f.error.http_status().as_u16(),
                None,
                Usage::default(),
                Some((f.error.kind(), f.error.to_string())),
            )
            .await;
            return headers.stamp(out);
        }
        Err(e) => {
            let out = error_response(proto, &e);
            unanswered.logging();
            record(
                LogParams {
                    state: &state,
                    proto,
                    ctx: &ctx,
                    alias,
                    route: Some(&route),
                    started,
                    streamed,
                    class: RequestClass::Chat,
                    timings: None,
                    max_tokens_clamped,
                    fallback: headers.fallback_reason(),
                    rung,
                    degraded: None,
                    quantities: Default::default(),
                },
                e.http_status().as_u16(),
                None,
                Usage::default(),
                Some((e.kind(), e.to_string())),
            )
            .await;
            return with_annotations(headers.stamp(out), &annotations);
        }
    };

    let status = resp.status();
    if !status.is_success() || !streamed {
        let bytes = resp.bytes().await;
        // The answer is read: this send no longer occupies the pool — at
        // once when it was read to its end, once llama-server lets go of the
        // slot when it was not.
        lease.end(bytes.is_ok());
        let bytes = bytes.unwrap_or_default();
        let (status_out, usage, error) = if status.is_success() {
            // The chat route's own reader, so a cache write is read here too.
            let usage = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .map(|v| crate::egress::openai_wire::parse_usage(v.get("usage")))
                .unwrap_or_default();
            (200u16, usage, None)
        } else {
            // The route's own egress: llama.cpp's context refusal is its own.
            let e = crate::gate::attribute(
                for_protocol(route.upstream.protocol).map_error(status.as_u16(), &bytes),
                &route,
            );
            (
                e.http_status().as_u16(),
                Usage::default(),
                Some((e.kind().to_string(), e.to_string())),
            )
        };
        unanswered.logging();
        record(
            LogParams {
                state: &state,
                proto,
                ctx: &ctx,
                alias,
                route: Some(&route),
                started,
                streamed,
                class: RequestClass::Chat,
                timings: None,
                max_tokens_clamped,
                fallback: headers.fallback_reason(),
                rung,
                degraded: None,
                quantities: Default::default(),
            },
            status_out,
            Some(started.elapsed().as_millis() as i64),
            usage,
            error.as_ref().map(|(k, m)| (k.as_str(), m.clone())),
        )
        .await;
        let mut builder = Response::builder().status(status_out);
        if status.is_success() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            return with_annotations(
                headers.stamp(builder.body(Body::from(bytes)).unwrap()),
                &annotations,
            );
        }
        // Normalize the error into OpenAI shape.
        let e = crate::gate::attribute(
            for_protocol(route.upstream.protocol).map_error(status.as_u16(), &bytes),
            &route,
        );
        return with_annotations(headers.stamp(error_response(proto, &e)), &annotations);
    }

    // Streaming pass-through: pipe upstream SSE bytes straight to the client.
    // Deliberately *not* counted into the live tok/s readout: this path never
    // parses the stream (that is the whole point of a passthrough), and turning
    // it into a decoder just to feed a titlebar number would be the wrong
    // trade. Legacy completions therefore show as active without a rate.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let state2 = state.clone();
    let ctx2 = ctx.clone();
    let route2 = route.clone();
    let fallback = headers.fallback_reason();
    // The relay task writes the stream's row from here on, a client that goes
    // away mid-stream included (`canceled`).
    unanswered.logging();
    tokio::spawn(async move {
        let mut upstream = resp.bytes_stream();
        let mut error: Option<(String, String)> = None;
        let mut completed = false;
        loop {
            match upstream.next().await {
                None => {
                    completed = true;
                    break;
                }
                Some(Ok(b)) => {
                    if tx.send(Ok(b)).await.is_err() {
                        error = Some(("canceled".into(), "client disconnected".into()));
                        break;
                    }
                }
                Some(Err(e)) => {
                    error = Some(("transport".into(), e.to_string()));
                    break;
                }
            }
        }
        // The relay is over, however it ended. The upstream connection is
        // closed *first* (second review, finding 6): while it is alive llama-server is
        // still generating into the slot, so the lease must not end before it
        // — and certainly not with it still open across the log write below.
        drop(upstream);
        // Then the reservation: at once after the stream's own end, once
        // llama-server lets go of the slot after a disconnect or an error.
        lease.end(completed);
        record(
            LogParams {
                state: &state2,
                proto,
                ctx: &ctx2,
                alias,
                route: Some(&route2),
                started,
                streamed: true,
                class: RequestClass::Chat,
                timings: None,
                max_tokens_clamped,
                fallback,
                rung,
                degraded: None,
                // Past the upstream's 2xx: answered, a client gone
                // mid-stream (`canceled`) included (billable-units §4.5).
                quantities: crate::pricing::Quantities::answered(),
            },
            200,
            None,
            Usage::default(),
            error.as_ref().map(|(k, m)| (k.as_str(), m.clone())),
        )
        .await;
        drop(admission);
    });
    with_annotations(
        headers.stamp(
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .body(Body::from_stream(ReceiverStream::new(rx)))
                .unwrap(),
        ),
        &annotations,
    )
}
