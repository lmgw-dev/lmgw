//! How a response ends (realtime design §4.3, §9.1, §11): what the model
//! call reports, the drained acknowledgement, and the closing events.
//!
//! Generation ends with the stream's finish (Closing): in text mode the items
//! are closed then, because the stock client runs a tool as soon as its
//! call's `output_item.done` says `completed` and sends its follow-up right
//! away (§2.3). The call returning — its hold already released by the
//! responder (§9.1) — moves the response to Playing and queues the drained
//! marker behind its output. The writer's acknowledgement of that marker
//! finishes it: the closing events still open, `response.done`, and the
//! response that was waiting for it.
//!
//! **A response that speaks** keeps its message open past the finish
//! ([`Output::speaks`]): its closing events — `output_audio.done` and the
//! rest — are due when the paced audio ends, which is the drained
//! acknowledgement, so a barge-in until then still finds it open. Its
//! function calls close at the finish as in text mode: the client may run
//! its tool while the preamble plays. Each synthesized clause arrives as a
//! [`Msg::Clause`], in stream order with the deltas. Its TTS hold is dropped
//! by the responder after the last clause is synthesized, as the chat hold
//! is after the stream (§9.1).
//!
//! **Server-side calls** (realtime-server-tools §2.2–§2.4) stay open past
//! the finish: the responder runs them and reports each one's start and
//! result before its call returns, so `response.done` follows them all.
//! What closes when is `Output`'s rule (`output::closing`); this only calls
//! it.
//!
//! [`Output::speaks`]: super::super::output::Output::speaks

use serde_json::json;

use super::super::output;
use super::super::protocol::{ResponseStatus, ServerEvent, StatusDetails};
use super::super::responder::Msg;
use super::{Active, Core, Failure, Phase};
use crate::ir::{Completion, FinishReason, StreamDelta};

impl Core {
    /// What the response's model call reported — through the hold of a
    /// response that heard the user's audio (`held`).
    pub(in crate::realtime) fn on_responder(&mut self, gen: u64, msg: Msg) {
        let Some(msg) = self.hold(gen, msg) else {
            return;
        };
        // The session's knowledge, whichever response read it.
        let msg = match msg {
            Msg::Voices { alias, names } => return self.voices_read(&alias, names),
            // A bound turn's (chat-voice §8.2): relayed whatever became of
            // its response.
            Msg::ChatFrame { event, data } => return self.bound_frame(gen, event, data),
            Msg::Planned {
                chat,
                tts,
                thread,
                voice,
            } => return self.bound_planned(gen, (chat, tts), thread, *voice),
            Msg::Input { input, why } => return self.bound_input(gen, input, why),
            Msg::Refused { refused, note } => return self.bound_refused(gen, refused, note),
            Msg::Carried(carried) => return self.carried(gen, carried),
            other => other,
        };
        if self.bound.is_some() {
            self.bound_note(gen, &msg);
        }
        let Some(active) = self.active.as_mut().filter(|a| a.output.gen == gen) else {
            // A cancelled response's last words.
            return;
        };
        let (conv, ids, ob) = (&mut self.conversation, &self.ids, &mut self.ob);
        match msg {
            Msg::Delta(StreamDelta::TextDelta(t)) => {
                active.timing.text();
                active.output.text(conv, ids, ob, &t)
            }
            Msg::Delta(StreamDelta::ToolCallStart { index, id, name }) => {
                let mcp = active.output.call_start(conv, ids, ob, index, &id, &name);
                active.timing.call(mcp);
            }
            Msg::Delta(StreamDelta::ToolCallArgsDelta { index, fragment }) => {
                active.output.call_args(conv, ob, index, &fragment)
            }
            Msg::Clause { text, written, pcm } => {
                active.timing.audio();
                active.output.clause(conv, ids, ob, &text, written, &pcm)
            }
            Msg::Unspoken(raw) => active.output.unspoken(conv, &raw),
            Msg::Mark(m) => active.timing.mark(m),
            Msg::Delta(StreamDelta::Usage(u)) => active.usage.merge(&u),
            Msg::Delta(StreamDelta::Stop(reason)) => {
                // Every item is whole now (module doc) — a spoken message
                // closes when it has played, a server-side call with its
                // result.
                active.output.generated(conv, ids, ob, &reason);
                active.stop = Some(reason);
                active.phase = Phase::Closing;
            }
            Msg::SpeakerDone => active.spoken = true,
            Msg::ToolSent(call) => active.output.mcp_sent(call),
            Msg::ToolRunning { index, at } => {
                active.timing.tool_running(at);
                active.output.mcp_running(ob, index)
            }
            Msg::ToolDone { index, outcome, at } => {
                active.timing.tool_done(at);
                active.output.mcp_done(conv, ids, ob, index, outcome)
            }
            Msg::Delta(_)
            | Msg::Voices { .. }
            | Msg::Tts(_)
            | Msg::ChatFrame { .. }
            | Msg::Planned { .. }
            | Msg::Input { .. }
            | Msg::Refused { .. }
            | Msg::Carried(_) => {}
            Msg::Finished(result) => {
                // A bound session's voice is the thread's, which its client
                // does not set: a voice error names no parameter (WP11
                // binding review NIT 2).
                let param = if self.bound.is_some() {
                    ""
                } else {
                    active.voice_param
                };
                let result = (*result).map_err(|e| Failure::of_call(&e, param));
                self.end_call(result);
            }
        }
    }

    /// The model call is over (or never started): Playing, until the writer
    /// says this generation's output has all left.
    pub(super) fn end_call(&mut self, result: Result<Completion, Failure>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        // A response's items are whole once its call has returned, whether
        // or not the stream said why it stopped — a spoken message closes
        // when it has played.
        if let Ok(c) = &result {
            let (conv, ids, ob) = (&mut self.conversation, &self.ids, &mut self.ob);
            active.output.generated(conv, ids, ob, &c.finish_reason);
        }
        active.ended = Some(result);
        active.phase = Phase::Playing;
        // The call's stop is no longer anyone's to raise.
        active.call = None;
        self.ob.drained(active.output.gen);
    }

    /// The writer's drained acknowledgement for `gen` (§4.3). One for a
    /// response that is no longer active — cancelled while its output
    /// drained — is stale.
    pub(in crate::realtime) fn on_drained(&mut self, gen: u64) {
        let ended = self
            .active
            .as_mut()
            .filter(|a| a.output.gen == gen && a.phase == Phase::Playing)
            .and_then(|a| a.ended.take());
        let Some(ended) = ended else {
            return;
        };
        let Some(mut active) = self.active.take() else {
            return;
        };
        let (conv, ids, ob) = (&mut self.conversation, &self.ids, &mut self.ob);
        let status = match ended {
            Ok(c) => {
                active.output.finish_items(conv, ids, ob, &c.finish_reason);
                let (status, details) = response_status(&c.finish_reason);
                let usage = output::usage(&c.usage);
                active.output.done(conv, ob, status, details, Some(usage));
                status
            }
            // The stream failed after its finish: every item was already
            // announced whole — a call may have been run on that — so the
            // response is what it said it was, and the failure is the log's.
            // Its server-side calls still open were never run, or a failed
            // voice cut them off, and close as such (realtime-server-tools
            // §2.3).
            Err(f) if active.stop.is_some() => {
                let reason = active.stop.take().unwrap_or(FinishReason::Stop);
                tracing::warn!(
                    "realtime {}: response {} failed after its end of generation, and finishes \
                     as generated — {}",
                    self.session.id.as_deref().unwrap_or("?"),
                    active.output.id,
                    f.log
                );
                active.output.finish_items(conv, ids, ob, &reason);
                let (status, details) = response_status(&reason);
                let usage = output::known_usage(&active.usage);
                active.output.done(conv, ob, status, details, usage);
                status
            }
            Err(f) => {
                active.output.abandon(conv, ids, ob);
                tracing::info!(
                    "realtime {}: response {} failed — {}",
                    self.session.id.as_deref().unwrap_or("?"),
                    active.output.id,
                    f.log
                );
                let details = StatusDetails {
                    kind: ResponseStatus::Failed,
                    reason: None,
                    error: Some(json!({
                        "type": f.error.kind,
                        "code": f.error.code,
                        "message": f.error.message,
                    })),
                };
                ob.send(ServerEvent::error(
                    f.error.for_event(active.event_id.as_deref()),
                ));
                let usage = output::known_usage(&active.usage);
                active
                    .output
                    .done(conv, ob, ResponseStatus::Failed, Some(details), usage);
                ResponseStatus::Failed
            }
        };
        self.log_timing(&active, status);
        self.next_response(active);
    }

    /// The response is over: start the one that was waiting for it — the
    /// client's queued `response.create`, or else an automatic response
    /// owed to a turn that committed while it ran (§4.3). A queued create
    /// was judged when it was queued, but the session may have changed
    /// since: one that cannot start after all has had its error, and the
    /// owed response is next rather than stranded (WP1c review #2). While
    /// the user's turn is open — a barge-in ended it, or the user started
    /// talking as it played — the queued create is carried to after the
    /// turn instead (`pending`, owner's decision Q3).
    pub(super) fn next_response(&mut self, mut ended: Active) {
        // Its server-side calls' results are in for the next one (`refusal`).
        self.mcp.results_in |= ended.output.has_mcp();
        let queued = ended.queued.take();
        drop(ended);
        if let Some(create) = queued {
            if self.turn.is_some() {
                self.pending_carry(create);
            } else {
                self.start_response(create);
            }
        }
        if self.active.is_none() {
            self.pending_after_response();
        }
    }
}

/// A finished response's status: `incomplete` when the model was cut off,
/// with OpenAI's reason words.
fn response_status(reason: &FinishReason) -> (ResponseStatus, Option<StatusDetails>) {
    let reason = match reason {
        FinishReason::Length => "max_output_tokens",
        FinishReason::ContentFilter => "content_filter",
        _ => return (ResponseStatus::Completed, None),
    };
    (
        ResponseStatus::Incomplete,
        Some(StatusDetails {
            kind: ResponseStatus::Incomplete,
            reason: Some(reason.into()),
            error: None,
        }),
    )
}
