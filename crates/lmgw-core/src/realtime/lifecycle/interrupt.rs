//! Barge-in, on the response side (realtime design §4.3, §6.4): what the
//! input needs to know of the output, and what a turn that starts during a
//! response does to it.
//!
//! **The window the input is judged against** ([`Core::listen`]) is the
//! writer's record of the latest speaking response's playback
//! (`writer::playback`). While that response is still producing audio the
//! window is open-ended: a pause while the next clause is synthesized is
//! still the middle of the answer — and so it is while any of its audio
//! still waits in the writer (B3 review 8). Then the window ends where the
//! client's playback does, plus a **margin** (B3 review 2): the window is
//! the server's release time, and the client plays it a transit and its
//! output buffer later; the capture estimate is arrival-based, so the
//! client's input transit and buffer make it later still. The two delays
//! add — the earlier claim that they cancel was wrong — so the end used for
//! membership is pushed back by the liveness ping's round trip, when one
//! was measured, plus `echo_tail_ms` for what the round trip cannot see
//! (the client's audio buffers, Bluetooth, the room's reverberation). Both
//! the barge-in gate and half duplex judge by that end; the cut itself
//! (`PlayedOut`) still judges by the modelled end, since response.done goes
//! out there.
//!
//! **What a turn's start does** ([`Core::interruption`]) is decided at the
//! moment its deciding frame was *captured*, not when the core got to it:
//! - no response in progress — nothing to interrupt (the turn may still
//!   have been captured inside a window that ended since: the residual race
//!   of §6.4, a normal turn now);
//! - a response whose items are all closed after the end of generation —
//!   nothing a cancel could change; it finishes as generated (the same
//!   predicate as `response.cancel`, so a completed tool call is never
//!   marked cancelled, WP3 review H4);
//! - a spoken response whose audio had played out when the speech began —
//!   it is finishing, not interrupted;
//! - anything else is cut, when `interrupt_response` says so — or when it
//!   is held for a transcript, so nobody heard it (`held`).
//!
//! **The cut** ([`Core::interrupt`]) runs in a fixed order: the owed
//! response is deferred first (WP1c review H5, by the turn's start), so
//! nothing the cancel ends can start another response while the user
//! talks; then its queued audio is purged synchronously — so
//! `speech_started`, already queued, reaches the client before the
//! cancelled item's `output_audio.done`, and `@openai/agents` still has an
//! item to interrupt — and the purge says whether the client **heard**
//! anything of it; then what it owes is settled (`pending`); then the
//! cancel itself, `turn_detected`.
//!
//! **Heard** (B3 review 1) means audio samples or a transcript delta left
//! the writer, or a text delta went out — not that an item was announced. A
//! function call says nothing to the listener, and a client's
//! `response.create` (the `@openai/agents` tool follow-up) has no turns of
//! its own to owe. So a response cut before it was heard:
//! - owes its turns again, ahead of anything owed since;
//! - and, if a client created it, holds that `response.create` — or the
//!   one queued behind it, which renders the same conversation — to start
//!   again after the turn, answered even if the turn has no words (the
//!   client asked). A cough during a tool follow-up's generation therefore
//!   no longer strands the tool's result.
//!
//! A response the client was hearing is not resumed (owner's decision Q2):
//! the next response, if the turn has words, answers from what was heard.

use std::time::Duration;

use tokio::time::Instant;

use super::super::audio_in::{Listen, PlayView};
use super::{Active, Core, Phase};

/// How a turn that just started meets the active response (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Interruption {
    /// No response in progress.
    Idle,
    /// The response is cut. `into_ms`: how far into its playback the speech
    /// was captured, if it was playing.
    Cuts { id: String, into_ms: Option<u64> },
    /// Its items are all closed: it finishes as generated.
    Generated { id: String },
    /// Its audio had played out when the speech began.
    PlayedOut { id: String },
}

impl Core {
    /// What the input side needs to know of the output, for one append
    /// (module doc).
    pub(in crate::realtime) fn listen(&self) -> Listen {
        let margin = self.echo_margin();
        let view = self.out.playback().and_then(|pb| {
            let first = pb.first?;
            let producing = pb.ended.is_none()
                && self
                    .active
                    .as_ref()
                    .is_some_and(|a| a.output.gen == pb.gen && a.phase != Phase::Playing);
            // Audio of it still waits to leave: the answer is not over,
            // whatever the end of what left says (B3 review 8).
            let open = producing || pb.waiting;
            Some(PlayView {
                gen: pb.gen,
                first,
                end: if open {
                    None
                } else {
                    pb.end().map(|end| end + margin)
                },
            })
        });
        // What a turn starting now cancels (`Self::interruption`): a frame
        // inside the window has not played out, so only the closed items
        // and `interrupt_response` decide — and a held response nobody
        // heard is cut whatever it says (`held`).
        let interrupts = super::super::input::interrupt_response(&self.session) || self.holding();
        let cuts = self
            .active
            .as_ref()
            .filter(|a| a.cancellable() && interrupts)
            .map(|a| a.output.gen);
        Listen {
            view,
            heard: self.active.as_ref().is_some_and(|a| self.heard(a)),
            cuts,
        }
    }

    /// How far past the modelled playback end input still counts as heard
    /// during it (module doc): the ping's round trip, when one was
    /// measured, plus `echo_tail_ms`.
    pub(in crate::realtime) fn echo_margin(&self) -> Duration {
        let tail = self
            .session
            .lmgw
            .as_ref()
            .and_then(|l| l.echo_tail_ms)
            .unwrap_or(0);
        self.out.rtt().unwrap_or_default() + Duration::from_millis(u64::from(tail))
    }

    /// The margin as [`Self::echo_margin`] has it now, logged at DEBUG when
    /// it changed (E3): a new least round trip, or a session.update of
    /// `echo_tail_ms`.
    pub(in crate::realtime) fn note_margin(&mut self) {
        let margin = self.echo_margin();
        if self.margin_seen.replace(margin) != Some(margin) {
            tracing::debug!(
                "realtime {}: the barge-in window ends {} ms after the modelled playback end \
                 (the least of the last ping round trips, {}, plus echo_tail_ms)",
                self.id(),
                margin.as_millis(),
                // A loopback round trip is well under a millisecond: shown
                // in tenths, so a measured one never reads as missing.
                self.out
                    .rtt()
                    .map_or("not measured yet".to_string(), |r| format!(
                        "{:.1} ms",
                        r.as_secs_f64() * 1000.0
                    ))
            );
        }
    }

    /// Whether the client has heard anything of `a` (module doc).
    fn heard(&self, a: &Active) -> bool {
        a.output.text_sent() || self.out.heard(a.output.gen)
    }

    /// How a turn whose deciding frame was captured `at` meets the active
    /// response (module doc).
    pub(in crate::realtime) fn interruption(&self, at: Instant) -> Interruption {
        let Some(a) = &self.active else {
            return Interruption::Idle;
        };
        let id = a.output.id.clone();
        if !a.cancellable() {
            return Interruption::Generated { id };
        }
        let playback = self.out.playback().filter(|p| p.gen == a.output.gen);
        let into_ms = playback
            .and_then(|p| p.first)
            .filter(|first| at >= *first)
            .map(|first| at.duration_since(first).as_millis() as u64);
        let ended = playback.filter(|p| !p.waiting).and_then(|p| p.end());
        if a.phase == Phase::Playing && ended.is_some_and(|end| at >= end) {
            return Interruption::PlayedOut { id };
        }
        Interruption::Cuts { id, into_ms }
    }

    /// Cut the active response for the turn that just started, in the
    /// module doc's order — the turn's start has deferred the owed response
    /// already (`input::judged`).
    pub(in crate::realtime) fn interrupt(&mut self) {
        let Some(gen) = self.active.as_ref().map(|a| a.output.gen) else {
            return;
        };
        let sent = self.out.purge(gen);
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if sent.heard || active.output.text_sent() {
            let rid = active.output.id.clone();
            self.pending_interrupted(rid);
        } else {
            let answers = active.answers.clone();
            // The create queued behind it renders the same conversation and
            // more; either is started again only once.
            let again = active.queued.take().or_else(|| active.again.take());
            self.pending_reowe(answers);
            if let Some(create) = again {
                self.pending_carry_again(create);
            }
        }
        self.cancel_purged("turn_detected", &sent);
    }
}
