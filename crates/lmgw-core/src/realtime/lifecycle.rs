//! One response at a time (realtime design §4.2, §4.3, §7.5, §19): what
//! `response.create` and `response.cancel` do, and how a response ends.
//!
//! The phases of a response ([`Phase`], §4.3):
//! - **Pending** ([`Pending`]) — a turn the detector committed with
//!   `create_response` on; its automatic response is owed and not created
//!   yet. New speech defers it; it is never dropped while its turns have
//!   words.
//! - **Generating** — `response.created` went out and the model call runs.
//!   While the transcripts of committed turns are still being made the call
//!   waits for them (awaiting transcripts, §4.1: "ASR if pending"), so a
//!   manual-mode `commit` + `response.create` answers what was said.
//!   Another `response.create` is refused with
//!   `conversation_already_has_active_response`, echoing the client's
//!   `event_id` (`@openai/agents` matches errors to its own ids).
//! - **Closing** — generation has ended; in text mode the items are closed
//!   (with audio, the function calls are, and every clause before the end
//!   has been synthesized), and the call is still reading its last chunks.
//!   A `response.create` now is **queued** and starts right after this
//!   response's `response.done`: the stock client sends its post-tool
//!   follow-up exactly then (§2.3). Server-side calls keep a response here
//!   while they run; once nothing of it plays, speech is a turn, not a
//!   barge-in (`producing`, realtime-server-tools §2.5).
//! - **Playing** — the call has returned, and the writer is draining the
//!   response's last output (paced audio, §8.2). The writer's drained
//!   acknowledgement for this generation ends it: only then do the closing
//!   events and `response.done` go out (`ending`), so a barge-in while the
//!   answer plays still has a response to cancel.
//!
//! **Cancel** is cooperative (`cancel`): the call is stopped, never aborted,
//! and still writes its usage row. A truncate of the item still being spoken
//! is a client-side stop of the rest (`truncate`, §4.3). A **barge-in** —
//! the user's turn starting while the response is in progress — cancels it
//! with `turn_detected` (`interrupt`, §6.4).
//!
//! **No response starts while the user's turn is open** (§4.3, owner's
//! decision Q3): a `response.create` that arrives then, or that was queued
//! behind a response that ends then, is carried and starts once the turn is
//! over (`pending`).
//!
//! Each response **snapshots** the session's config as it is created (§4.2
//! step 4), with the `response.create`'s own overrides on top, so a
//! `session.update` while it waits or runs applies to the next one.
//! Out-of-band responses (`conversation: "none"`, `input`) are refused by
//! name (§19).
//!
//! **A response that hears the user's audio** (voice-audio-input design
//! §3.1, `hearing`) launches at the commit, without waiting for the turn's
//! transcript, and its output is held until the transcript is in (`held`).
//!
//! A failure mid-response — a policy refusal, the gate, the upstream, a
//! context that does not fit (`context_length_exceeded` from the per-send
//! fit, §7.5), the transcripts it waited for failing — is an `error` plus
//! `response.done {failed}`; the session stays open, and the client can
//! delete items and try again.

use super::output::{AudioOut, Output};
use super::protocol::{ErrorObject, ResponseCreateParams};
use super::render;
use super::responder;
use super::session::Core;
use crate::error::GatewayError;
use crate::ir::{Completion, FinishReason, Usage};
use crate::proxy::StopHandle;

mod bound;
mod cancel;
mod ending;
mod hearing;
mod held;
mod interrupt;
mod pending;
mod producing;
mod refusal;
mod snapshot;
#[cfg(test)]
mod tests;
mod timing;
mod truncate;

pub(crate) use hearing::RowTx;
pub(crate) use interrupt::Interruption;
pub(crate) use pending::Pending;
use snapshot::Snapshot;
use timing::Timing;
pub(crate) use timing::TurnTiming;

/// The response in flight.
pub(crate) struct Active {
    output: Output,
    phase: Phase,
    /// The `response.create`'s own id, echoed on an error about this
    /// response.
    event_id: Option<String>,
    /// The `response.create` that arrived while this one was closing.
    queued: Option<Create>,
    /// The model call's cooperative stop (`cancel`). Dropped with the
    /// response — a session that ends drops it — which stops the call too;
    /// the call's task is detached, never aborted, so it always writes its
    /// row (§4.3).
    call: Option<StopHandle>,
    /// The config the call renders with, kept while awaiting transcripts.
    waiting: Option<Snapshot>,
    /// The committed turns whose transcripts this response waits for.
    awaited: Vec<String>,
    /// The turns it answers by debt (`pending`): what a barge-in owes again
    /// when the client heard nothing of it (`interrupt`).
    answers: Vec<String>,
    /// A client's `response.create` as it came, for a barge-in that cuts
    /// the response before the client heard any of it: it is held and
    /// starts again after the turn (`interrupt`, B3 review 1). `None` for
    /// the automatic response, whose turns are owed again instead.
    again: Option<Create>,
    /// Why generation ended, once the stream said (Closing).
    stop: Option<FinishReason>,
    /// The usage the upstream reported so far, for a response that ends
    /// before its call does (§4.3: `response.done` carries usage when known).
    usage: Usage,
    /// How the call ended (Playing): the drained acknowledgement finishes
    /// the response with it.
    ended: Option<Result<Completion, Failure>>,
    /// Its moments, for the §11 timing line (`timing`).
    timing: Timing,
    /// The parameter that named the voice it speaks with — its
    /// `response.create`'s own, or the session's (package B review 6).
    voice_param: &'static str,
    /// Its output is held for the transcript of a turn it heard as audio
    /// (`held`); `None` for a response that heard none.
    held: Option<held::Held>,
    /// Its speaker handed the last clause: what it says is all with the
    /// writer (`producing`, realtime-server-tools §2.5).
    spoken: bool,
    /// The automatic response that started first after server-side calls'
    /// results were in: it answers with them (`refusal`).
    answers_tools: bool,
}

/// Why a response failed: what the client is told, and what the log says.
pub(crate) struct Failure {
    error: ErrorObject,
    log: String,
}

impl Failure {
    /// The model call failed with `e`. A voice the first clause refused is
    /// about the voice `voice_param` named, not a permission (§5.3).
    fn of_call(e: &GatewayError, voice_param: &str) -> Self {
        let mut error = super::voice::voice_error(e, voice_param)
            .unwrap_or_else(|| ErrorObject::from_gateway(e));
        // A refusal for lmgw's own failure — a bound session's history write
        // the store refused (chat-voice §8.3) — is the server's, not a
        // permission.
        if matches!(e, GatewayError::Refused { status, .. } if *status >= 500) {
            error.kind = "server_error".into();
        }
        Self {
            error,
            log: e.to_string(),
        }
    }
}

/// The phases of §4.3 a created response goes through (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Generating, but the model call waits for committed turns' transcripts.
    AwaitingTranscripts,
    Generating,
    Closing,
    Playing,
}

impl Phase {
    /// Rendered: the model call has the conversation as it was.
    fn launched(self) -> bool {
        self != Self::AwaitingTranscripts
    }
}

/// A `response.create` as it arrived — or the automatic one.
#[derive(Debug, Clone)]
struct Create {
    event_id: Option<String>,
    params: Option<ResponseCreateParams>,
    /// The committed turns an automatic response is owed to (`pending`).
    answers: Vec<String>,
    /// The automatic response, owed to `answers` — not a client's.
    auto: bool,
}

impl Core {
    /// `response.create` (§4.3).
    pub(super) fn response_create(
        &mut self,
        event_id: Option<String>,
        params: Option<ResponseCreateParams>,
    ) {
        // A bound session's thread owns what a response could override
        // (chat-voice §8.1).
        if self.bound.is_some() {
            if let Err(e) = super::thread::check_create(params.as_ref()) {
                return self.error(e.for_event(event_id.as_deref()));
            }
        }
        let create = Create {
            event_id,
            params,
            answers: Vec::new(),
            auto: false,
        };
        // The voice as the settings say now (B2 review 5): the create is
        // judged against it.
        self.refresh_voice();
        // One is already held for the user's turn: it answers that turn, so
        // a second would be a second response (`pending`, §4.3) — whether
        // the turn is still open or its transcript is still being made —
        // unless it is absorbed into a cut response's own create, held again
        // (realtime-server-tools §2.5).
        if self.pending_carries() {
            return self.pending_second(create);
        }
        let Some(active) = &self.active else {
            if self.turn.is_some() {
                // The user is speaking: it starts after the turn (module
                // doc) — judged now, so a create that can never start gets
                // its error at once.
                if let Err(e) = self.snapshot(create.params.as_ref()) {
                    return self.error(e.for_event(create.event_id.as_deref()));
                }
                return self.pending_carry(create);
            }
            return self.start_response(create);
        };
        if matches!(active.phase, Phase::Closing | Phase::Playing) && active.queued.is_none() {
            // Judged now, not when it is due: an out-of-band request, say,
            // gets its error at once, echoing its id, instead of after the
            // response before it — and never takes the queue's one place
            // from an owed response that could start (WP1c review #2).
            if let Err(e) = self.snapshot(create.params.as_ref()) {
                return self.error(e.for_event(create.event_id.as_deref()));
            }
            if let Some(active) = self.active.as_mut() {
                active.queued = Some(create);
            }
            return;
        }
        self.refuse_second(create.event_id.as_deref());
    }

    fn start_response(&mut self, create: Create) {
        self.refresh_voice();
        let snap = match self.snapshot(create.params.as_ref()) {
            Ok(s) => s,
            Err(e) => return self.error(e.for_event(create.event_id.as_deref())),
        };
        // A bound response that would answer nothing (chat-voice §8.2).
        if let Some(e) = self.bound_refusal(&create.answers) {
            return self.error(e.for_event(create.event_id.as_deref()));
        }
        self.generation += 1;
        // A client's create, kept to start again if a barge-in cuts this
        // response before it is heard (`interrupt`).
        let again = (!create.auto).then(|| Create {
            answers: Vec::new(),
            ..create.clone()
        });
        let audio = snap.speaking.as_ref().map(|s| AudioOut {
            voice: s.echo.clone(),
            lead: s.lead,
        });
        let output = Output::new(
            self.generation,
            self.ids.response(),
            snap.modalities.clone(),
            snap.max_output_tokens,
            snap.metadata.clone(),
            audio,
        );
        tracing::debug!(
            "realtime {}: response {} on '{}'",
            self.id(),
            output.id,
            snap.alias
        );
        output.created(&self.conversation, &mut self.ob);
        let speaks = snap.speaking.is_some();
        let voice_param = snap
            .speaking
            .as_ref()
            .map_or("session.audio.output.voice", |s| s.voice_param);
        // Whichever response starts first renders the calls' results.
        let answers_tools = std::mem::take(&mut self.mcp.results_in) && create.auto;
        self.active = Some(Active {
            output,
            phase: Phase::AwaitingTranscripts,
            event_id: create.event_id,
            queued: None,
            call: None,
            waiting: Some(snap),
            // What it was created for, and every transcript it waits for.
            // A turn the chat model hears is not waited for: it goes as
            // audio (`hearing`).
            awaited: create
                .answers
                .iter()
                .cloned()
                .chain(
                    self.transcriber
                        .items()
                        .into_iter()
                        .filter(|id| !self.hears(id)),
                )
                .collect(),
            again,
            answers: create.answers,
            stop: None,
            usage: Usage::default(),
            ended: None,
            timing: Timing::new(self.last_turn.take(), speaks),
            voice_param,
            held: None,
            spoken: false,
            answers_tools,
        });
        if self.mcp.table.listing() {
            self.timing_tools_waiting();
        }
        if self.busy_for_launch() {
            tracing::debug!(
                "realtime {}: the response waits for the transcripts or MCP listings still being \
                 made",
                self.id()
            );
        } else {
            self.launch();
        }
    }

    /// Render the waiting response and start its model call — called once no
    /// committed turn is waiting for its transcript any more (§4.1).
    pub(super) fn launch(&mut self) {
        let Some(active) = self
            .active
            .as_mut()
            .filter(|a| a.phase == Phase::AwaitingTranscripts)
        else {
            return;
        };
        let Some(snap) = active.waiting.take() else {
            return;
        };
        // The turns it waited for all failed to transcribe: answering would
        // answer nothing that was said (R10).
        let hearing = self.bound.as_ref().map(|b| &b.hearing);
        if super::input::transcripts_failed(&self.conversation, &active.awaited, hearing) {
            let error = ErrorObject {
                kind: "server_error".into(),
                ..ErrorObject::invalid(
                    "transcription_failed",
                    "the speech this response answers could not be transcribed (see its \
                     conversation.item.input_audio_transcription.failed); say it again, or send \
                     the text",
                )
            };
            let log = "every turn it answers failed to transcribe".to_string();
            return self.end_call(Err(Failure { error, log }));
        }
        // A bound session's LLM step is the chat thread's own turn
        // (chat-voice §8.2), over the thread's history, not this
        // conversation.
        if self.bound.is_some() {
            let speaking = snap
                .speaking
                .as_ref()
                .map(|s| super::thread::turn::Speaking {
                    progress: self.out.progress(),
                    ahead: s
                        .ahead
                        .map(|a| a.as_secs() * u64::from(super::audio::resample::INPUT_RATE)),
                    longest_pause_ms: s.longest_pause_ms,
                    speed: s.speed,
                });
            if let Some(stop) = self.launch_bound(speaking) {
                self.launched(stop);
            }
            return;
        }
        // What it offers of the session's MCP tools is decided now that the
        // listings it waited for are in, and the calls of them are the
        // gateway's to run (realtime-server-tools §1.1, §2.1).
        let offer = match snap.mcp_offer(&self.mcp.table) {
            Ok(o) => o,
            Err(error) => {
                let log = format!("its MCP tools: {}", error.message);
                return self.end_call(Err(Failure { error, log }));
            }
        };
        let served = self.mcp.table.served(&offer, snap.parallel_tool_calls);
        active.output.serve_mcp(served.owners);
        let ir = render::render(&render::Input {
            alias: &snap.alias,
            instructions: &snap.instructions,
            items: self.conversation.items(),
            tools: &snap.tools,
            tool_choice: snap.tool_choice.as_ref(),
            parallel_tool_calls: snap.parallel_tool_calls,
            max_output_tokens: Some(snap.max_output_tokens),
            reasoning: snap.reasoning.as_ref(),
            speech_hint: snap.speaking.as_ref().and_then(|s| s.hint.as_deref()),
            written: &|id| self.conversation.written(id),
            mcp: render::McpInput {
                tools: &offer.tools,
                choice: offer.choice.as_ref(),
                only: offer.only.is_some(),
                call: &|c| render::mcp_of_session(c, &self.conversation, &self.mcp.table),
            },
        });
        let (stop, signal) = crate::proxy::stop_pair();
        // Detached: a cancel stops it through `stop`, never by aborting it.
        let progress = self.out.progress();
        let sid = self.session.id.clone().unwrap_or_else(|| "?".into());
        let speech = snap.speaking.map(|s| responder::Speech {
            label: format!("realtime {sid}"),
            proto: crate::ingress::ClientProto::Realtime,
            alias: s.tts,
            requested: s.requested,
            voice: s.voice,
            facts: s.facts,
            speed: s.speed,
            language: s.language,
            instructions: s.style.send,
            dropped: s.style.dropped,
            seed: Some(s.seed),
            ahead: s
                .ahead
                .map(|a| a.as_secs() * u64::from(super::audio::resample::INPUT_RATE)),
            longest_pause_ms: s.longest_pause_ms,
            progress: Some(progress),
        });
        // Held until the call has ended and written its row, also when the
        // session drops it at its end: a stopping server waits for it
        // (review F-2).
        let running = self
            .state
            .stops
            .running_at(self.state.stops.at_or_now(self.ctx.served_at));
        let job = responder::Job {
            state: self.state.clone(),
            ctx: self.ctx.clone(),
            gen: active.output.gen,
            session: sid,
            ir,
            tx: self.responder_tx.clone(),
            stop: signal,
            speech,
            tools: served.tools,
        };
        tokio::spawn(async move {
            let _running = running;
            responder::run(job).await
        });
        self.launched(stop);
    }

    /// The active response's model call started, `stop` its stop:
    /// Generating.
    fn launched(&mut self, stop: StopHandle) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.call = Some(stop);
        active.phase = Phase::Generating;
        // Every committed turn is in what it renders: the latest is its own
        // (`timing`), and no automatic response is owed any more.
        self.timing_launched();
        let settled = self.pending_answered();
        if let Some(active) = self.active.as_mut() {
            for id in settled {
                if !active.answers.contains(&id) {
                    active.answers.push(id);
                }
            }
        }
    }
}
