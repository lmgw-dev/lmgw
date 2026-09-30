//! The streaming half of the chat path: `stream_chat`, its live token-rate
//! tracking, and the shared upstream-SSE-draining helper `drive_upstream`.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::config::Route;
use crate::egress::{Egress, EgressStreamDecoder};
use crate::error::GatewayError;
use crate::gate::GateHeaders;
use crate::ingress::ClientProto;
use crate::ir::{ChatRequest, StreamDelta, Timings, Usage};
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::*;

/// `headers` learn the rung, as for [`unary_chat`](crate::proxy::chat); they leave with the SSE
/// response's own headers, before the first event.
#[allow(clippy::too_many_arguments)]
pub(super) async fn stream_chat(
    state: &SharedState,
    proto: ClientProto,
    ctx: &RequestCtx,
    ir: &ChatRequest,
    route: &Route,
    headers: &mut GateHeaders,
    egress: &'static dyn Egress,
    params: &crate::ir::Params,
    started: Instant,
    admission: Option<crate::vram::LocalHold>,
    mut lease: crate::gate::TurnLease,
) -> Result<Turn<Response>, GatewayError> {
    let max_tokens_clamped = lease.max_tokens_clamped();
    let timeout = route.upstream.request_timeout();
    // The connect is retried through the shared dead-container path (§3.2);
    // once the relay below has started, the response is the client's — and
    // on a ladder row it starts only after the count's verdict.
    let sent = crate::gate::send_gated(
        state,
        admission.as_ref(),
        route,
        &mut lease,
        crate::gate::CountInput::Chat {
            ir,
            params,
            stream: true,
        },
        None,
        timeout,
        |r| {
            egress.build_chat(
                &state.http,
                &r.upstream,
                &r.upstream_model,
                ir,
                params,
                true,
            )
        },
    )
    .await;
    headers.set_rung(lease.rung());
    let rung = lease.rung_log();
    let fallback = headers.fallback_reason();
    let resp = match sent? {
        crate::gate::Sent::Upstream(resp) => resp,
        crate::gate::Sent::Rerouted(r) => return Ok(Turn::Rerouted(r)),
    };

    let status = resp.status();
    if !status.is_success() {
        let bytes = resp.bytes().await;
        // An error answer read to its end is a task the server is done with.
        lease.end(bytes.is_ok());
        let bytes = bytes.unwrap_or_default();
        return Err(crate::gate::attribute(
            egress.map_error(status.as_u16(), &bytes),
            route,
        ));
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let mut encoder = proto.new_stream_encoder(&ir.model_alias);
    let decoder = egress.new_decoder();
    let state2 = state.clone();
    let ctx2 = ctx.clone();
    let alias = ir.model_alias.clone();
    let route2 = route.clone();

    tokio::spawn(async move {
        let send = |s: String| {
            let tx = tx.clone();
            async move {
                if s.is_empty() {
                    return true;
                }
                tx.send(Ok(Bytes::from(s))).await.is_ok()
            }
        };

        // Open the client stream, then relay each normalized delta through the
        // client's protocol encoder. Upstream draining + usage/ttfb/error
        // bookkeeping is shared with the in-process chat UI via `drive_upstream`.
        let started_ok = send(encoder.start()).await;
        let outcome = if started_ok {
            drive_upstream(
                resp,
                decoder,
                timeout,
                started,
                &state2.telemetry,
                |delta| send(encoder.delta(&delta)),
            )
            .await
        } else {
            // The client was gone before the first frame: close the upstream
            // connection here, *before* the lease ends — a response still
            // alive is a slot llama-server is still generating into (second
            // review, finding 6). `drive_upstream` does the same on the other branch
            // by consuming it.
            drop(resp);
            StreamOutcome {
                aborted: true,
                ..StreamOutcome::default()
            }
        };
        // The upstream has ended — finished, failed, or cut off because the
        // client went away — and its connection is closed. A normal end
        // releases at once; anything else once llama-server lets go of the
        // slot. Before the log write, not after it: nothing below uses KV.
        lease.end(outcome.completed);

        let client_gone = !started_ok || outcome.aborted;
        if !client_gone {
            let _ = send(encoder.finish()).await;
        }
        let mut error = outcome.error;
        if client_gone && error.is_none() {
            error = Some(("canceled".into(), "client disconnected".into()));
        }

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
                timings: outcome.timings,
                max_tokens_clamped,
                fallback,
                rung,
            },
            StatusCode::OK.as_u16(),
            outcome.ttfb_ms,
            outcome.usage,
            error.as_ref().map(|(k, m)| (k.as_str(), m.clone())),
        )
        .await;
        // The model stops counting as in use here, not when `stream_chat`
        // returned — which was before the first token.
        drop(admission);
    });

    let body = Body::from_stream(ReceiverStream::new(rx));
    Ok(Turn::Answered(
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("x-accel-buffering", "no")
            .body(body)
            .unwrap(),
    ))
}

/// The result of draining an upstream streaming chat response.
#[derive(Default)]
pub(crate) struct StreamOutcome {
    /// Time-to-first-byte (the first text/tool delta), if any was produced.
    pub ttfb_ms: Option<i64>,
    /// Token usage as reported by the upstream.
    pub usage: Usage,
    /// Latest llama.cpp timing snapshot seen (prefill/decode speeds), if the
    /// upstream reported one. Cloud upstreams leave this `None`.
    pub timings: Option<Timings>,
    /// First terminal error (kind, message), if the stream ended badly.
    pub error: Option<(String, String)>,
    /// True if `sink` asked to stop early (the consumer/client went away).
    pub aborted: bool,
    /// The upstream body was read to its normal end — EOF, with no error and
    /// no early stop. What a send's pool lease is released on at once
    /// ([`crate::gate::TurnLease::end`]): llama-server finished the task
    /// before it closed the stream. Anything else leaves the slot running
    /// until the server notices, and the lease waits for it.
    pub completed: bool,
}

/// Live output-token bookkeeping for **one** stream, feeding the gateway-wide
/// tok/s readout ([`TelemetryBus::note_stream_progress`]).
///
/// Two sources, in order of preference:
///
/// 1. **Exact cumulative counts** the upstream reports *while still
///    generating*: Anthropic's `message_delta` `usage.output_tokens`, Gemini's
///    per-chunk `usageMetadata`, llama.cpp's `timings.predicted_n` (per token
///    when `timings_per_token` is on).
/// 2. **One token per non-empty content chunk** otherwise — text, reasoning, or
///    tool-argument fragments. llama-server emits a chunk per token, so this is
///    near-exact for local models; cloud providers batch, so it under-reads.
///    That is why the UI writes the number as "~42 tok/s" rather than "42".
///
/// `counted` is monotonic and only ever moves forward by the difference, so an
/// exact count arriving after some chunk counting corrects the total without
/// double counting, and a lower exact count never rewinds it.
///
/// The terminal batch is deliberately excluded from source 1: an upstream that
/// reports its usage only at the end (the common case) would otherwise dump the
/// whole correction — hundreds of tokens — into a single instant and spike the
/// rate right as the stream finishes. Per-request token totals are unaffected:
/// they come from [`StreamOutcome::usage`], which still sees every frame.
#[derive(Default)]
pub(super) struct StreamProgress {
    /// Output tokens already published to telemetry for this stream.
    pub(super) counted: u64,
    /// Set once the upstream signalled the turn is over (stop or error);
    /// nothing after that is live generation.
    done: bool,
}

impl StreamProgress {
    pub(super) fn observe(
        &mut self,
        batch: &[StreamDelta],
        telemetry: &crate::telemetry::TelemetryBus,
    ) {
        if self.done {
            return;
        }
        let terminal = batch
            .iter()
            .any(|d| matches!(d, StreamDelta::Stop(_) | StreamDelta::Error(_)));
        let mut target = self.counted;
        for delta in batch {
            match delta {
                StreamDelta::TextDelta(t) | StreamDelta::ReasoningDelta(t) if !t.is_empty() => {
                    target += 1
                }
                StreamDelta::ToolCallArgsDelta { fragment, .. } if !fragment.is_empty() => {
                    target += 1
                }
                StreamDelta::Usage(u) if !terminal => {
                    target = target.max(u.completion_tokens.unwrap_or(0))
                }
                StreamDelta::Timings(t) if !terminal => target = target.max(t.predicted_n),
                _ => {}
            }
        }
        if target > self.counted {
            telemetry.note_stream_progress(target - self.counted);
            self.counted = target;
        }
        self.done = terminal;
    }
}

/// Drain `resp` — an upstream SSE chat stream — decoding it into normalized IR
/// [`StreamDelta`]s and handing each to `sink`. Tracks TTFB, accumulates usage,
/// publishes live token progress (see [`StreamProgress`]), and captures the
/// first timeout / transport / upstream error (also forwarded to `sink` as a
/// [`StreamDelta::Error`]). Returns when the upstream ends, errors, or `sink`
/// returns `false` (consumer gone, recorded as `aborted`).
///
/// Shared by the public protocol relay ([`stream_chat`]), the in-process
/// streaming caller ([`stream_once`]) and the chat UI's relay, so all three
/// observe identical events, timing, usage — and feed the same live rate.
/// `timeout` bounds the **gap between chunks**, not the stream's total length
/// — a stream that keeps producing may run all day. `None` (the upstream's
/// ceiling is 0, i.e. the maximum possible) removes even that: the stream then
/// ends when the upstream ends it or the consumer goes away.
pub(crate) async fn drive_upstream<S, Fut>(
    resp: reqwest::Response,
    mut decoder: Box<dyn EgressStreamDecoder>,
    timeout: Option<Duration>,
    started: Instant,
    telemetry: &crate::telemetry::TelemetryBus,
    mut sink: S,
) -> StreamOutcome
where
    S: FnMut(StreamDelta) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut sse = crate::sse::SseDecoder::new();
    let mut out = StreamOutcome::default();
    let mut progress = StreamProgress::default();
    let mut upstream = resp.bytes_stream();
    'drain: loop {
        let next = async {
            match timeout {
                Some(d) => tokio::time::timeout(d, upstream.next()).await,
                None => Ok(upstream.next().await),
            }
        };
        let chunk = match next.await {
            Err(_) => {
                let msg = "upstream stalled mid-stream".to_string();
                out.error = Some(("timeout".into(), msg.clone()));
                sink(StreamDelta::Error(msg)).await;
                break;
            }
            Ok(None) => {
                out.completed = out.error.is_none();
                break;
            }
            Ok(Some(Err(e))) => {
                out.error = Some(("transport".into(), e.to_string()));
                sink(StreamDelta::Error(e.to_string())).await;
                break;
            }
            Ok(Some(Ok(bytes))) => bytes,
        };
        for ev in sse.feed(&chunk) {
            let batch = decoder.on_event(&ev);
            progress.observe(&batch, telemetry);
            for delta in batch {
                if out.ttfb_ms.is_none()
                    && matches!(
                        delta,
                        StreamDelta::TextDelta(_) | StreamDelta::ToolCallStart { .. }
                    )
                {
                    out.ttfb_ms = Some(started.elapsed().as_millis() as i64);
                }
                if let StreamDelta::Usage(u) = &delta {
                    out.usage.merge(u);
                }
                if let StreamDelta::Timings(t) = &delta {
                    out.timings = Some(*t);
                }
                if let StreamDelta::Error(msg) = &delta {
                    out.error = Some(("upstream".into(), msg.clone()));
                }
                if !sink(delta).await {
                    out.aborted = true;
                    break 'drain;
                }
            }
        }
    }
    out
}
