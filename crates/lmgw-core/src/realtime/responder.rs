//! The responder (realtime design §3.2, §4.1, §4.3, §9.1, §10.3, §11): the
//! task that makes one response's model call.
//!
//! In order:
//! 1. the per-call policy check with the session's key — scope, budget and
//!    the key's per-minute counts (§10.3);
//! 2. the chat route for **this response**: the gate's routing (the GPU
//!    hold's swap, a candidate alias's pick) and admission, exactly as the
//!    dashboard Chat opens its route. The resulting
//!    [`LocalHold`](crate::vram::LocalHold) keeps the
//!    model from being evicted **until the call returns** — and no longer:
//!    it is dropped here, not at `response.done`, which with paced audio
//!    comes seconds later, and would never come for a client that stopped
//!    reading (§9.1). The hold dropped is the one this call took; a re-route
//!    inside the call takes and drops its own (WP3's TTS hold follows the
//!    same rule: dropped when the last clause is synthesized);
//! 3. one streamed call through `proxy::stream_once_on`, whose per-send fit
//!    measures the request against the route's real context (§7.5), with a
//!    sink that forwards each delta to the core **as it arrives** (§3.2).
//!
//! **A cancel is cooperative** (§4.3): the core raises the response's stop —
//! or drops it, when the session ends — and the call ends at its next await
//! (the admission queue, the wait for the first byte, the next chunk). The
//! task is never aborted, so the call always writes its `request_logs` row:
//! `canceled`, with what it cost so far (`proxy::stopped_usage`), counted
//! against the key's tokens and spend like any call. Whatever it still says
//! afterwards reaches a core that has moved on, and is dropped there by its
//! generation.
//!
//! Reasoning deltas are never forwarded: they are not part of what the
//! listener gets (§7.6). Usage is, so a cancelled or failed response's
//! `response.done` carries what the upstream reported. The rows carry the
//! session's client key, under the `realtime` label (§11).
//!
//! **A speaking response** (`speech`) runs a second task beside the stream:
//! the text is cut into clauses as it arrives and synthesized in order over
//! one TTS route, whose hold is dropped after the last clause — the chat
//! hold still at the end of the stream (§9.1). Once the last clause is with
//! the core, the core hears so ([`Msg::SpeakerDone`], realtime-server-tools
//! §2.5).
//!
//! **The model's calls of offered MCP tools** run once the stream has ended,
//! inside the chat call and before its end is reported (`tools`,
//! realtime-server-tools §2.4): the response's `response.done` follows every
//! call.
//!
//! **Every call reports back.** One that panics answers `Err(Internal)`, as
//! the ASR path's does, or the response would stay Generating for good.

use std::time::{Duration, Instant};

use futures::FutureExt;
use tokio::sync::mpsc;

use super::heard::Written;
use super::policy;
use crate::agent::{DeltaSink, ToolOutcome};
use crate::config::Route;
use crate::error::GatewayError;
use crate::gate::GateHeaders;
use crate::ingress::ClientProto;
use crate::ir::{ChatRequest, Completion, StreamDelta};
use crate::proxy::{RequestCtx, StopSignal};
use crate::state::SharedState;
use crate::telemetry::RequestClass;
use crate::vram::LocalHold;

mod speech;
mod tools;

pub(crate) use speech::{speak, Speaker, Speech, Splitter};
pub(crate) use tools::{SentCall, ServerTools};

/// What the responder tells the core, tagged with the response's
/// generation so a message from a cancelled response is recognised.
pub(crate) enum Msg {
    /// A delta the listener gets — text, a tool call's start or arguments,
    /// the end of generation — or the usage reported so far. A speaking
    /// response's text comes as [`Msg::Clause`]s instead.
    Delta(StreamDelta),
    /// One synthesized clause of a speaking response: its spoken text, what
    /// the model wrote for it (the model's history keeps that, §7.2) and
    /// its audio, PCM16-LE at 24 kHz — in stream order with the deltas.
    Clause {
        text: String,
        written: Written,
        pcm: bytes::Bytes,
    },
    /// What the model wrote after a speaking response's last clause and the
    /// voice left out (a closing code block), before the end of generation
    /// or a call: the model's history keeps it (§7.2).
    Unspoken(String),
    /// A moment of a spoken answer the core cannot see itself, for the §11
    /// timing line: its text goes to the clause splitter, not the core.
    Mark(Mark),
    /// The voice list of TTS alias `alias`, read at a spoken response's
    /// first clause (§5.3): the session's to keep, whichever response read
    /// it.
    Voices { alias: String, names: Vec<String> },
    /// The spoken response's TTS route is being opened, or is open
    /// ([`TtsEvent`]). The Chat's speech reports it as `state` and `voice`
    /// frames (chat-voice design §6.3); a realtime session has no use for
    /// it.
    Tts(TtsEvent),
    /// A chat-turn frame of a session bound to a chat thread (chat-voice
    /// design §8.2), relayed as `lmgw.chat.frame` whatever became of the
    /// response — the page renders the turn's bubble from them.
    ChatFrame {
        event: &'static str,
        data: serde_json::Value,
    },
    /// What a bound response answers and speaks with, once its turn is
    /// planned: the thread's chat model and TTS as re-read — and the thread
    /// as the session says it (`session.lmgw.resolved.chat_thread`), and the
    /// session's own voice as the thread resolves it now
    /// (personality-profiles design D21, `thread::reshape`).
    Planned {
        chat: String,
        tts: Option<String>,
        thread: super::protocol::ChatThreadRef,
        voice: Box<super::thread::reshape::Reshape>,
    },
    /// A bound response goes to its model as its transcript after all
    /// (voice-audio-input design §3.5): its audio attempt was refused, or a
    /// thread-level row of the verdict failed since the commit.
    Input {
        input: crate::store::InputPath,
        why: String,
    },
    /// A model's server refused a heard turn's audio, or failed on it
    /// (§3.5): the session keeps it (`Bound.refused`, by model), so its
    /// later turns to that model go as their transcript until voice mode is
    /// entered again. `note`: said to the client now (`lmgw.chat.input`),
    /// when no transcript retry said it already.
    Refused {
        refused: super::thread::turn::audio::Refused,
        note: Option<String>,
    },
    /// Whether a heard response's audio attempt carried the turn's audio to
    /// the model (voice-audio-input design §3.2, WP3 review #3): `true` once
    /// the model answered from it, `false` when the attempt was skipped,
    /// refused, or ended before the model said anything. Said once; only a
    /// turn a model heard keeps a failed transcription to itself.
    Carried(bool),
    /// A spoken response's speaker handed its last clause to the core: what
    /// it says is all with the writer (realtime-server-tools §2.5). Queued
    /// behind the clauses, so it never overtakes one.
    SpeakerDone,
    /// A server-side call went to its server — said straight to the core,
    /// never queued behind clauses, so a cancel knows it may have run, with
    /// what the core needs to announce its item itself should the call's
    /// own deltas never reach it (realtime-server-tools §2.5). Speech mode
    /// only: in text mode [`Msg::ToolRunning`] is never queued.
    ToolSent(SentCall),
    /// A server-side call of the response started at `at`
    /// (realtime-server-tools §2.4); `index` is its upstream ordinal, as its
    /// deltas name it.
    ToolRunning { index: usize, at: Instant },
    /// A server-side call ended at `at` with `outcome` — or failed unrun,
    /// its arguments no JSON object (§2.3).
    ToolDone {
        index: usize,
        outcome: ToolOutcome,
        at: Instant,
    },
    /// The call is over, and its claims on the models are already released.
    /// Boxed: one per response, and the deltas need not be its size.
    Finished(Box<Result<Completion, GatewayError>>),
}

/// The opening of a spoken response's one TTS route ([`Msg::Tts`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TtsEvent {
    /// Its admission starts now (at the first clause): it may start or
    /// load the model.
    Opening,
    /// Open, and the voice settled on it: `answered_by` is the alias that
    /// answers in the TTS alias's place (a fallback), `voice` what every
    /// clause is sent with.
    Opened {
        answered_by: Option<String>,
        voice: Option<String>,
    },
}

/// The moments [`Msg::Mark`] reports (§11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark {
    /// The chat stream's first text or tool call.
    FirstToken,
    /// The first clause cut for TTS.
    FirstClause,
    /// The chat stream's first reasoning (a bound turn's first `reasoning`
    /// frame): how long the model reasoned before its first token.
    Reasoning,
}

pub(crate) type Tx = mpsc::UnboundedSender<(u64, Msg)>;

/// One response's model call.
pub(crate) struct Job {
    pub state: SharedState,
    pub ctx: RequestCtx,
    pub gen: u64,
    /// The session's id, for the log lines (`realtime {session}: …`).
    pub session: String,
    /// The rendered request; its `model_alias` is the chat alias.
    pub ir: ChatRequest,
    pub tx: Tx,
    /// The response's cooperative stop (module doc).
    pub stop: StopSignal,
    /// How the response speaks; `None` for text output.
    pub speech: Option<Speech>,
    /// The MCP tools it offered, which it runs itself (`tools`).
    pub tools: ServerTools,
}

/// Run `job` to its end; the result goes to the core as [`Msg::Finished`].
pub(crate) async fn run(job: Job) {
    let result = std::panic::AssertUnwindSafe(call(&job))
        .catch_unwind()
        .await
        .unwrap_or_else(|panic| {
            let why = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "no message".into());
            tracing::error!(
                "realtime {}: the response's model call panicked: {why}",
                job.session
            );
            Err(GatewayError::Internal(format!(
                "the response failed inside lmgw: {why}"
            )))
        });
    let _ = job.tx.send((job.gen, Msg::Finished(Box::new(result))));
}

async fn call(job: &Job) -> Result<Completion, GatewayError> {
    let Job {
        state,
        ctx,
        gen,
        session,
        ir,
        tx,
        stop,
        speech,
        tools: served,
    } = job;
    let alias = ir.model_alias.as_str();
    policy::check_call(state, ctx, alias, RequestClass::Chat).await?;

    // Judged as the text endpoint it is: a fallback the hold or admission
    // swaps in has to be a chat model too. Raced against the stop: a
    // cancelled response does not wait in the admission queue — nor start
    // a model — for an answer nobody wants.
    let started = std::time::Instant::now();
    let opened = tokio::select! {
        biased;
        () = stop.raised() => {
            return Err(crate::proxy::canceled("stopped by the caller during admission"))
        }
        o = open(state, alias, ir) => o,
    };
    let opened = match opened {
        Ok(o) => o,
        Err(f) => {
            // A refusal is traffic (§11): the hold with no fallback, a
            // candidate that cannot take the request, admission's queue.
            crate::proxy::record_request_failure(
                state,
                ctx,
                ClientProto::Realtime.as_str(),
                alias,
                f.route.as_deref(),
                f.headers.fallback_reason(),
                started,
                &f.error,
            )
            .await;
            return Err(f.error);
        }
    };
    let crate::gate::Opened {
        route,
        hold,
        headers,
    } = opened;
    if let Some(fallback) = headers.fallback() {
        tracing::info!("realtime {session}: '{alias}' is answered by its fallback '{fallback}'");
    }

    let Some(speech) = speech else {
        let mut sink = Forward {
            gen: *gen,
            tx,
            stop,
        };
        let mut collect = tools::Collect::new(&mut sink, served);
        let result = stream(state, ctx, hold, &route, &headers, ir, &mut collect).await;
        let calls = collect.into_calls();
        if let Ok(c) = &result {
            let report = tools::Report::direct(*gen, tx);
            let stop = Some(stop.clone());
            tools::run((state, ctx), served, calls, &c.finish_reason, stop, &report).await;
        }
        return result;
    };
    // Speaking: the stream feeds the speaker's queue, and the speaker stops
    // the stream — through the sink's own stop — on a cancel or a failed
    // voice.
    let (work, queue) = mpsc::unbounded_channel();
    let (stop_chat, chat_stop) = crate::proxy::stop_pair();
    // A stock session skips code and tables silently (no announcement).
    let mut sink = speech::Splitter::new(*gen, tx, work, chat_stop, &speech.label, None);
    let chat = async {
        let mut collect = tools::Collect::new(&mut sink, served);
        let result = stream(state, ctx, hold, &route, &headers, ir, &mut collect).await;
        let calls = collect.into_calls();
        // The last clause, and the end of the speaker's queue — once the
        // calls are reported behind it.
        sink.finish();
        sink.said_all();
        if let Ok(c) = &result {
            let report = sink.report();
            let stop = sink.stop();
            tools::run((state, ctx), served, calls, &c.finish_reason, stop, &report).await;
        }
        drop(sink);
        result
    };
    let speaker = speech::speak(speech::Speaker {
        state,
        ctx,
        gen: *gen,
        speech,
        queue,
        tx,
        stop,
        stop_chat,
    });
    let (result, spoken) = tokio::join!(chat, speaker);
    // A failed voice is why the stream was stopped: that is the response's
    // error, not the stop.
    spoken.and(result)
}

/// The streamed chat call over the opened route. Takes the hold, and drops
/// it as soon as the stream ends: the model is free for others now, not
/// when the client has heard the answer (§9.1, WP1c review H3).
async fn stream(
    state: &SharedState,
    ctx: &RequestCtx,
    hold: Option<LocalHold>,
    route: &Route,
    headers: &GateHeaders,
    ir: &ChatRequest,
    sink: &mut dyn DeltaSink,
) -> Result<Completion, GatewayError> {
    let result = crate::proxy::stream_once_on(
        state,
        hold.as_ref(),
        route,
        headers.fallback_reason(),
        ir,
        ClientProto::Realtime.as_str(),
        // Charged by the key's identity, at record time: a session's calls
        // can outlive a rename of its key (realtime §11, `KeyRef`).
        ctx.key_ref(),
        // No deadline of the session's own: a response lasts as long as the
        // route's own request timeout allows (a visible upstream setting),
        // and the client can cancel it at any time.
        Duration::MAX,
        sink,
        None,
    )
    .await;
    drop(hold);
    result
}

/// The gate's routing and admission for one response's chat call.
async fn open(
    state: &SharedState,
    alias: &str,
    ir: &ChatRequest,
) -> Result<crate::gate::Opened, crate::gate::OpenFailed> {
    crate::gate::resolve(state, alias, crate::gate::RouteCheck::Text(super::PATH))
        .await?
        .using(crate::gate::request_facets(ir, None))?
        .admit(state)
        .await
}

/// Forwards the deltas the listener gets, as they arrive, and carries the
/// response's stop into the call.
struct Forward<'a> {
    gen: u64,
    tx: &'a Tx,
    stop: &'a StopSignal,
}

impl DeltaSink for Forward<'_> {
    fn on_delta(&mut self, d: &StreamDelta) {
        let keep = matches!(
            d,
            StreamDelta::TextDelta(_)
                | StreamDelta::ToolCallStart { .. }
                | StreamDelta::ToolCallArgsDelta { .. }
                | StreamDelta::Stop(_)
                | StreamDelta::Usage(_)
        );
        if keep {
            let _ = self.tx.send((self.gen, Msg::Delta(d.clone())));
        }
    }

    fn stop(&self) -> Option<StopSignal> {
        Some(self.stop.clone())
    }
}
