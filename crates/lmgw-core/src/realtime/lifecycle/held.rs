//! The hold (voice-audio-input design §3.2): a response that hears the
//! user's audio starts at the commit, before the turn's transcript is in,
//! and its output is held until the transcript says the turn had words.
//!
//! **What is held.** The response's output, in arrival order: its chat frames
//! other than `state`, its deltas but the usage, its synthesized clauses, the
//! unspoken tail and its end ([`Msg::Finished`]). Neither a reply nor its end
//! reaches the client before the verdict. **What passes:** the marks, the
//! plan, the speaker's TTS reports, the voice list, the note that a turn went
//! as its transcript, whether the attempt carried the audio, the usage so far
//! (a veto's or a cut's `response.done` carries it, WP3 review #5), that a
//! server-side call was sent (a cut abandons it, realtime-server-tools §2.5)
//! and `state` frames — timings and model states stay live. A held message
//! takes its arrival mark when it arrives (a delta's first token, a clause's
//! synthesis); what reached the client counts from the release (`timing`).
//! Generation, clause cutting and synthesis go on: the writer's progress does
//! not move, so synthesis stops at its lead.
//!
//! **The verdict** comes once the response's last audio turn is
//! transcribed (`hearing`):
//! - **released** when any turn it answers has words, or an audio turn's
//!   transcription failed (the model heard it; the reply plays and its user
//!   message says so): the queue is replayed in order through the normal
//!   path, then output flows live;
//! - **vetoed**, quietly, when every turn it answers came back without
//!   words: the queue is dropped, the call stopped, and the response ends
//!   as a cancel does (`response.done {cancelled, reason: no_words}`) — no
//!   `error`, no note, no row, no reply. When a turn among them failed to
//!   transcribe and no model heard it (`hearing`), the failure was said as
//!   with audio input off, and the reason is `transcription_failed`.
//!
//! **A cut during the hold** — a barge-in (the user went on speaking,
//! whatever `interrupt_response` says: nobody heard it, WP3 review #6), a
//! `response.cancel` — ends a response nobody heard: its queue goes with
//! it. A response whose hold ended with nothing released is remembered
//! (`Bound.unreleased`) until its responder's last word, and what it still
//! says is dropped, not relayed — its notes and memory too (WP3 review #9):
//! no bubble and no note for a reply nobody heard. Only what the session
//! keeps whoever read it passes: the voice list, the marks, and whether the
//! attempt carried the audio, which the transcript's verdict reads.

use std::time::Instant;

use super::super::responder::Msg;
use super::{Active, Core};
use crate::ir::StreamDelta;

/// The `response.done` reason of a vetoed response (module doc).
pub(crate) const NO_WORDS: &str = "no_words";
/// …and of one vetoed because a turn it answers failed to transcribe, and
/// no model heard it (module doc).
pub(crate) const NOT_TRANSCRIBED: &str = "transcription_failed";

/// A response's hold (module doc).
#[derive(Default)]
pub(crate) struct Held {
    /// The output that came while held, in arrival order.
    queue: Vec<Msg>,
    /// Released: output flows live.
    released: bool,
    /// Its end came, held: its responder says nothing more.
    finished: bool,
}

impl Held {
    /// Still holding.
    pub fn holding(&self) -> bool {
        !self.released
    }
}

/// What passes a hold (module doc).
fn passes(msg: &Msg) -> bool {
    match msg {
        Msg::Mark(_)
        | Msg::Planned { .. }
        | Msg::Tts(_)
        | Msg::Voices { .. }
        | Msg::Input { .. }
        | Msg::Refused { .. }
        | Msg::Carried(_)
        | Msg::ToolSent(_)
        | Msg::Delta(StreamDelta::Usage(_)) => true,
        Msg::ChatFrame { event, .. } => *event == "state",
        Msg::Delta(_)
        | Msg::Clause { .. }
        | Msg::Unspoken(_)
        | Msg::SpeakerDone
        | Msg::ToolRunning { .. }
        | Msg::ToolDone { .. }
        | Msg::Finished(_) => false,
    }
}

/// What a response whose hold released nothing still passes (module doc):
/// what the session keeps, whichever response read it.
fn kept_by_the_session(msg: &Msg) -> bool {
    matches!(msg, Msg::Mark(_) | Msg::Voices { .. } | Msg::Carried(_))
}

/// Output a client would see: what a held response's wait is measured from.
fn output(msg: &Msg) -> bool {
    matches!(
        msg,
        Msg::Delta(StreamDelta::TextDelta(_) | StreamDelta::ToolCallStart { .. })
            | Msg::Clause { .. }
    )
}

impl Core {
    /// `msg` of response `gen` through the hold (module doc): `None` when it
    /// was queued, or dropped for a response whose hold released nothing.
    pub(super) fn hold(&mut self, gen: u64, msg: Msg) -> Option<Msg> {
        if let Some(b) = self.bound.as_mut().filter(|b| b.unreleased.contains(&gen)) {
            // Its responder's last word: nothing of it comes any more.
            if matches!(msg, Msg::Finished(_)) {
                b.unreleased.remove(&gen);
            }
            return kept_by_the_session(&msg).then_some(msg);
        }
        if passes(&msg) {
            return Some(msg);
        }
        let Some(active) = self.active.as_mut().filter(|a| a.output.gen == gen) else {
            return Some(msg);
        };
        let Some(held) = active.held.as_mut().filter(|h| h.holding()) else {
            return Some(msg);
        };
        // Its arrival marks (module doc).
        match &msg {
            Msg::Delta(StreamDelta::TextDelta(_) | StreamDelta::ToolCallStart { .. }) => {
                active.timing.token()
            }
            Msg::Clause { .. } => active.timing.audio(),
            _ => {}
        }
        if output(&msg) {
            active.timing.held_output();
        }
        held.finished |= matches!(msg, Msg::Finished(_));
        held.queue.push(msg);
        None
    }

    /// Release the active response's hold: its queue replayed in order
    /// through the normal path, then its output flows live.
    pub(in crate::realtime) fn release(&mut self) {
        let sid = self.id().to_string();
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let gen = active.output.gen;
        let Some(held) = active.held.as_mut().filter(|h| h.holding()) else {
            return;
        };
        held.released = true;
        let queue = std::mem::take(&mut held.queue);
        active.timing.released = Some(Instant::now());
        tracing::debug!(
            "realtime {sid}: response {} is released — the turn it heard had words ({} held \
             message(s) replayed)",
            active.output.id,
            queue.len()
        );
        for m in queue {
            self.on_responder(gen, m);
        }
    }

    /// Veto the active response (module doc): every turn it answers came
    /// back without words — `reason` [`NO_WORDS`], or [`NOT_TRANSCRIBED`]
    /// when one of them failed to transcribe and no model heard it.
    pub(in crate::realtime) fn veto(&mut self, reason: &'static str) {
        let sid = self.id().to_string();
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if let Some(held) = active.held.as_mut() {
            held.queue.clear();
        }
        let what = if reason == NO_WORDS {
            "heard only noise — every turn it answers came back without words"
        } else {
            "answers no words — a turn it answers failed to transcribe, and no model heard it"
        };
        tracing::info!(
            "realtime {sid}: response {} {what}; it is cancelled quietly, and nothing is written",
            active.output.id
        );
        self.cancel_active(reason);
    }

    /// Whether the active response is still held: a turn that starts now
    /// cuts it whatever `interrupt_response` says (module doc).
    pub(in crate::realtime) fn holding(&self) -> bool {
        self.active
            .as_ref()
            .and_then(|a| a.held.as_ref())
            .is_some_and(Held::holding)
    }

    /// `active` ended: if its hold never released, what it still says is
    /// dropped (module doc) — until its end, unless that came already.
    pub(super) fn held_over(&mut self, active: &Active) {
        if active
            .held
            .as_ref()
            .is_some_and(|h| h.holding() && !h.finished)
        {
            if let Some(b) = self.bound.as_mut() {
                b.unreleased.insert(active.output.gen);
            }
        }
    }
}
