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
/// response's own headers, before the first event. `include_usage` is the
/// client's `stream_options.include_usage`: the upstream is asked for usage
/// regardless, and the row gets it either way.
#[allow(clippy::too_many_arguments)]
pub(super) async fn stream_chat(
    state: &SharedState,
    proto: ClientProto,
    ctx: &RequestCtx,
    ir: std::sync::Arc<ChatRequest>,
    include_usage: bool,
    route: &Route,
    headers: &mut GateHeaders,
    egress: &'static dyn Egress,
    params: &crate::ir::Params,
    fitted: &mut super::reasoning_fit::Fitted,
    started: Instant,
    admission: Option<crate::vram::LocalHold>,
    mut lease: crate::gate::TurnLease,
    prompt_sent: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<Turn<Response>, GatewayError> {
    let max_tokens_clamped = lease.max_tokens_clamped();
    let degraded = lease.degraded();
    let timeout = route.upstream.request_timeout();
    // What a cloud stream cut before its usage chunk is counted by (openai
    // egress design §13.6; an estimate off the platform, `floor_count`):
    // the request, shared, read only if the usage never comes; the relayed
    // text is kept only there.
    let local = state.snapshot().is_local_upstream(route.upstream.id);
    let counted = (!local).then(|| ir.clone());
    let ir: &ChatRequest = &ir;
    // The connect is retried through the shared dead-container path (§3.2);
    // once the relay below has started, the response is the client's — and
    // on a ladder row it starts only after the count's verdict. A refused
    // off is retried before that, too (§5.6).
    let sent = super::reasoning_fit::send_chat(
        state,
        admission.as_ref(),
        route,
        &mut lease,
        ir,
        params,
        true,
        None,
        timeout,
        prompt_sent.as_deref(),
        fitted,
        super::reasoning_fit::RowAs::Public {
            proto,
            ctx,
            alias: &ir.model_alias,
            fallback: headers.fallback_reason(),
        },
        Some(&super::reasoning_fit::Handover {
            check: crate::gate::RouteCheck::Text(super::chat::endpoint(proto)),
            origin: ctx.origin(),
            chain: headers.chain().clone(),
            // The stream's send is the handler's own: a hang-up cancels it.
            deferred: None,
        }),
        |r, p| {
            // As each attempt leaves: what a client gone before the 2xx is
            // counted by (`Unanswered::count_if_sent`).
            if let Some(sent) = &prompt_sent {
                sent.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            egress.build_chat(&state.http, &r.upstream, &r.upstream_model, ir, p, true)
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
        // An error answer: the upstream took no prompt to work on.
        if let Some(sent) = &prompt_sent {
            sent.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        let answer_headers = resp.headers().clone();
        let bytes = resp.bytes().await;
        // An error answer read to its end is a task the server is done with.
        lease.end(bytes.is_ok());
        let bytes = bytes.unwrap_or_default();
        // Before the stream: the classified refusal answers as the unary
        // send's would (structured-output design §9).
        return Err(crate::gate::attribute(
            crate::egress::chat_error(
                egress,
                status.as_u16(),
                &answer_headers,
                &bytes,
                ir.format_ask(),
            ),
            route,
        ));
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let mut encoder = proto.new_stream_encoder(&ir.model_alias, include_usage);
    let decoder = egress.decoder_for(&route.upstream);
    let state2 = state.clone();
    let ctx2 = ctx.clone();
    let alias = ir.model_alias.clone();
    let route2 = route.clone();
    let requested_tier = ir.requested_service_tier();

    tokio::spawn(async move {
        // A device's connection gets `TCP_USER_TIMEOUT` for as long as the
        // stream runs, and its accept-time timers back after it (device
        // link design §4.3).
        let _timers = crate::server::device_listener::chat_stream_timers(
            ctx2.conn.as_ref(),
            &state2.snapshot().settings,
        );
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
        //
        // The relay also ends the moment the client's end of the channel is
        // gone — hyper drops the body once it reads the connection's EOF —
        // not only at the next frame that fails to send: a model that
        // prefills, or streams only what the encoder writes nothing for, would
        // otherwise go on generating into a slot nobody reads (a runaway that
        // kept a one-slot server generating for minutes). Past the
        // answer's last delta it is no longer heard: the usage still to come
        // is the row's (`drive_upstream_until`).
        let client = tx.clone();
        let (st, rt, al) = (&state2, &route2, alias.as_str());
        let key_id = ctx2.principal.key_id();
        let started_ok = send(encoder.start()).await;
        let outcome = if started_ok {
            drive_upstream_until(
                resp,
                decoder,
                timeout,
                started,
                &state2.telemetry,
                counted.is_some(),
                |delta| {
                    // The answer's items are recorded before the stop that
                    // follows them reaches the client (`answer_items`).
                    let items = match &delta {
                        StreamDelta::UpstreamItems(i) => Some(i.clone()),
                        _ => None,
                    };
                    let frame = encoder.delta(&delta);
                    let send = &send;
                    async move {
                        if let Some(i) = items {
                            super::answer_items::remember(st, rt, key_id, al, &i).await;
                        }
                        send(frame).await
                    }
                },
                client.closed(),
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
        let local_count = match &counted {
            Some(ir) if !outcome.usage.reported() => {
                super::floor_count(&route2, local, ir, outcome.relayed).await
            }
            _ => None,
        };

        record_counted(
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
                degraded,
                // Past the upstream's 2xx: answered, a client gone
                // mid-stream (`canceled`) included (billable-units §4.5).
                quantities: crate::pricing::Quantities::answered(),
                service_tier: crate::pricing::ServedTier::of(
                    route2.upstream.protocol,
                    outcome.service_tier.clone(),
                    requested_tier,
                ),
            },
            StatusCode::OK.as_u16(),
            outcome.ttfb_ms,
            outcome.usage,
            error.as_ref().map(|(k, m)| (k.as_str(), m.clone())),
            local_count,
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
    /// The last `service_tier` the stream reported
    /// ([`StreamDelta::ServiceTier`]); only the OpenAI platform's sets it.
    pub service_tier: Option<String>,
    /// The upstream's record of the answer ([`StreamDelta::UpstreamItems`]):
    /// only an OpenAI platform answer on the Responses wire sends one.
    pub upstream_items: Option<crate::ir::UpstreamItems>,
    /// The output text the stream relayed, kept only when asked
    /// (`keep_text`, a platform stream's): its text and tool-argument
    /// deltas (no reasoning), what a platform stream cut before its usage
    /// chunk is counted by (openai egress design §13.6). Counted once at the
    /// end, never per delta: counting fragments one by one overcounts.
    pub relayed: String,
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
                StreamDelta::TextDelta(t)
                | StreamDelta::RefusalDelta(t)
                | StreamDelta::ReasoningDelta(t)
                    if !t.is_empty() =>
                {
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
/// `keep_text` keeps the output text in [`StreamOutcome::relayed`], for a
/// stream whose usage may have to be counted locally (an `openai` row's).
/// `timeout` bounds each **silence between events**, not the stream's total
/// length — a stream that keeps producing may run all day — and keep-alives
/// break a silence only while the model may be thinking out of sight
/// ([`super::silence`]). One that runs out ends the
/// stream as a `timeout` that names it, handed to `sink` as a
/// [`StreamDelta::Error`] like any upstream failure. `None` (the upstream's
/// ceiling is 0, i.e. the maximum possible) removes even that: the stream then
/// ends when the upstream ends it or the consumer goes away.
pub(crate) async fn drive_upstream<S, Fut>(
    resp: reqwest::Response,
    decoder: Box<dyn EgressStreamDecoder>,
    timeout: Option<Duration>,
    started: Instant,
    telemetry: &crate::telemetry::TelemetryBus,
    keep_text: bool,
    sink: S,
) -> StreamOutcome
where
    S: FnMut(StreamDelta) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let never = std::future::pending();
    drive_upstream_until(
        resp, decoder, timeout, started, telemetry, keep_text, sink, never,
    )
    .await
}

/// [`drive_upstream`], which also ends the moment `stop` resolves — recorded
/// as `aborted`, like a sink that said stop — even with no chunk in sight: a
/// long prefill, a slow provider. The consumer's cooperative stop (realtime
/// design §4.3) has to reach a stream that is not producing, and a sink only
/// hears from one that is; so does a public client's going away
/// ([`stream_chat`] stops on its channel closing), whose sink hears nothing
/// while the model streams what the client's encoder writes nothing for.
///
/// Not once a terminal delta has passed: the model is done then, and what
/// follows — OpenAI's usage in a chunk of its own, the body's end — is read
/// to the end, bounded by `timeout` as any gap is. A sink that says stop
/// from there on (its client went away with the finish) is handed nothing
/// more, but the upstream is still read: draining after the finish costs no
/// generation, and it is the only way to the usage of an OpenAI-shaped
/// stream, which comes after `finish_reason` (openai egress design §13.4).
/// Such a stream is `aborted`, with its usage. A client that hangs up on the
/// finish it was sent leaves a whole row with its usage, and the lease is
/// released at once.
///
/// Past the terminal delta the answer is whole (*changed 2026-11-05*, as
/// the legacy relay had it): an upstream that then falls silent for
/// `timeout` while the usage tail or the body's end is still to come, or
/// whose body breaks there, ends the drain with no error and nothing more
/// for the sink, so its caller ends its client's stream as the body's end
/// would have (the encoder's `[DONE]`, `message_stop`, `response.completed`)
/// and the row is whole. Usage that came is kept; a stream whose usage never
/// came is counted as a cloud stream cut before its usage chunk is. Not
/// `completed`: the body did not end, and the lease waits for the server to
/// let go of the slot.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drive_upstream_until<S, Fut>(
    resp: reqwest::Response,
    decoder: Box<dyn EgressStreamDecoder>,
    timeout: Option<Duration>,
    started: Instant,
    telemetry: &crate::telemetry::TelemetryBus,
    keep_text: bool,
    mut sink: S,
    stop: impl std::future::Future<Output = ()>,
) -> StreamOutcome
where
    S: FnMut(StreamDelta) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::pin!(stop);
    // A call that ends with no argument text is `{}` on every wire
    // (`egress::no_args`).
    let mut decoder = crate::egress::no_args::NoArgsCalls::wrap(decoder);
    let mut sse = crate::sse::SseDecoder::new();
    let mut out = StreamOutcome::default();
    let mut progress = StreamProgress::default();
    let mut upstream = resp.bytes_stream();
    let mut silence = super::silence::Silence::new(timeout);
    // The sink said stop after the terminal delta: nothing more goes to it,
    // and the rest is read for the usage.
    let mut sink_gone = false;
    // The terminal delta (a `Stop`) passed: the answer is whole, and a
    // silence or a broken body after it is no failure.
    let mut finished = false;
    'drain: loop {
        let next = silence.bound(upstream.next());
        let next = tokio::select! {
            biased;
            () = &mut stop, if !progress.done => {
                out.aborted = true;
                break;
            }
            n = next => n,
        };
        // The body's end flushes the decoder: a last event whose final line
        // ended in a lone CR is only complete then (`SseDecoder::finish`),
        // and goes through the same handling as every other.
        let (events, ended) = match next {
            // The answer was whole: what the upstream holds back after it
            // (its usage, `[DONE]`, the body's end) undoes nothing.
            None | Some(Some(Err(_))) if finished => break,
            None => {
                let msg = silence.message();
                out.error = Some((super::silence::KIND.into(), msg.clone()));
                if !sink_gone {
                    sink(StreamDelta::Error(msg)).await;
                }
                break;
            }
            Some(None) => (sse.finish(), true),
            Some(Some(Err(e))) => {
                out.error = Some(("transport".into(), e.to_string()));
                if !sink_gone {
                    sink(StreamDelta::Error(e.to_string())).await;
                }
                break;
            }
            Some(Some(Ok(bytes))) => {
                let events = sse.feed(&bytes);
                silence.hears(!bytes.is_empty(), &events);
                (events, false)
            }
        };
        let mut batches: Vec<Vec<StreamDelta>> =
            events.iter().map(|ev| decoder.on_event(ev)).collect();
        if ended {
            batches.push(decoder.on_end());
        }
        for batch in batches {
            progress.observe(&batch, telemetry);
            for delta in batch {
                if out.ttfb_ms.is_none()
                    && matches!(
                        delta,
                        StreamDelta::TextDelta(_)
                            | StreamDelta::RefusalDelta(_)
                            | StreamDelta::ToolCallStart { .. }
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
                if let StreamDelta::ServiceTier(t) = &delta {
                    out.service_tier = Some(t.clone());
                }
                if let StreamDelta::UpstreamItems(i) = &delta {
                    out.upstream_items = Some(i.clone());
                }
                match &delta {
                    _ if !keep_text => {}
                    // Not reasoning: a summary is not the tokens it sums
                    // up (§13.4), as the prompt's count leaves it out.
                    StreamDelta::TextDelta(t) | StreamDelta::RefusalDelta(t) => {
                        out.relayed.push_str(t)
                    }
                    StreamDelta::ToolCallArgsDelta { fragment, .. } => {
                        out.relayed.push_str(fragment)
                    }
                    _ => {}
                }
                if let StreamDelta::Error(msg) = &delta {
                    out.error = Some(("upstream".into(), msg.clone()));
                }
                if matches!(delta, StreamDelta::Stop(_)) {
                    finished = true;
                }
                if sink_gone {
                    continue;
                }
                if !sink(delta).await {
                    out.aborted = true;
                    if !progress.done {
                        break 'drain;
                    }
                    sink_gone = true;
                }
            }
        }
        if ended {
            out.completed = out.error.is_none();
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An upstream whose last event's lines end in lone CRs, the last one
    /// at the very end of the body: the decoder holds that CR back to see
    /// whether a `\n` follows, so the event is complete only at the body's
    /// end, which flushes it (`SseDecoder::finish`) through the same handling
    /// as every other event.
    #[tokio::test]
    async fn a_last_event_ended_by_lone_crs_is_read_at_the_body_s_end() {
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"one\"}}]}\n\n\
                    data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"two\"},\
                    \"finish_reason\":\"stop\"}]}\r\r";
        let resp = reqwest::Response::from(
            axum::http::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(body)
                .unwrap(),
        );
        let decoder =
            crate::egress::for_protocol(crate::config::Protocol::OpenaiCompatible).new_decoder();
        let telemetry = crate::telemetry::TelemetryBus::new();
        let seen = std::sync::Mutex::new(Vec::new());
        let out = drive_upstream(
            resp,
            decoder,
            None,
            Instant::now(),
            &telemetry,
            false,
            |d| {
                seen.lock().unwrap().push(d);
                async { true }
            },
        )
        .await;
        let text: String = seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|d| match d {
                StreamDelta::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "onetwo");
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .any(|d| matches!(d, StreamDelta::Stop(_))),
            "the last event's finish was read"
        );
        assert!(out.completed && !out.aborted, "{:?}", out.error);
    }

    /// A platform stream: content, the finish, then the usage chunk.
    fn platform_stream(
        protocol: crate::config::Protocol,
    ) -> (reqwest::Response, Box<dyn EgressStreamDecoder>) {
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n\
                    data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                    data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2}}\n\n\
                    data: [DONE]\n\n";
        let resp = reqwest::Response::from(
            axum::http::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(body)
                .unwrap(),
        );
        (resp, crate::egress::for_protocol(protocol).new_decoder())
    }

    /// A sink that goes away with the finish (its client left after the
    /// content) still gets the stream's usage: the chunk after
    /// `finish_reason` is read, though nothing more is handed to the sink,
    /// on both OpenAI-shaped protocols.
    #[tokio::test]
    async fn a_sink_gone_at_the_finish_still_gets_the_usage_chunk() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        for protocol in [
            crate::config::Protocol::Openai,
            crate::config::Protocol::OpenaiCompatible,
        ] {
            let (resp, decoder) = platform_stream(protocol);
            let telemetry = crate::telemetry::TelemetryBus::new();
            let after_stop = AtomicUsize::new(0);
            let stopped = AtomicBool::new(false);
            let out = drive_upstream(
                resp,
                decoder,
                None,
                Instant::now(),
                &telemetry,
                false,
                |d| {
                    if stopped.load(Ordering::Relaxed) {
                        after_stop.fetch_add(1, Ordering::Relaxed);
                    }
                    let go = !matches!(d, StreamDelta::Stop(_));
                    if !go {
                        stopped.store(true, Ordering::Relaxed);
                    }
                    async move { go }
                },
            )
            .await;
            assert!(out.aborted, "{protocol:?}");
            assert_eq!(
                (out.usage.prompt_tokens, out.usage.completion_tokens),
                (Some(9), Some(2)),
                "{protocol:?}"
            );
            assert_eq!(
                after_stop.load(Ordering::Relaxed),
                0,
                "nothing more reached the sink"
            );
            assert!(out.completed, "read to the end: {:?}", out.error);
        }
    }

    /// A sink that goes away before the finish cuts the stream there: the
    /// model may still be generating.
    #[tokio::test]
    async fn a_sink_gone_before_the_finish_cuts_the_stream() {
        let (resp, decoder) = platform_stream(crate::config::Protocol::Openai);
        let telemetry = crate::telemetry::TelemetryBus::new();
        let out = drive_upstream(
            resp,
            decoder,
            None,
            Instant::now(),
            &telemetry,
            false,
            |d| {
                let go = !matches!(d, StreamDelta::TextDelta(_));
                async move { go }
            },
        )
        .await;
        assert!(out.aborted && !out.completed);
        assert!(!out.usage.reported(), "{:?}", out.usage);
    }
}
