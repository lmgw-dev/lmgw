//! The barge-in word check, on the arbiter's side (realtime design §6.4,
//! owner's decision 2026-10-01): a turn the gate started is not announced
//! until its words are known.
//!
//! **Why.** Measured on the owner's own recordings, voiced time cannot tell
//! an interruption from a backchannel: "Stopp" has 320 ms of evidence and
//! "Stop" 384, while "Mhm" has 544 and "Okay" 608. At `barge_in_min_ms`
//! 300 every interruption passes the gate — and so do 8 of 9 backchannels,
//! a cough and a laugh. Words can tell them apart.
//!
//! **How.** When the gate's evidence reaches `barge_in_min_ms`, the turn
//! opens in the detector, back-dated as before — but **unconfirmed**: no
//! `speech_started`, and the answer keeps playing. The detector keeps it
//! open whatever the silence (`ServerVad::keep_open`), and the arbiter asks
//! the session to transcribe its audio so far, pre-roll included
//! ([`CheckRequest`]). The session answers with a [`Verdict`]
//! ([`Arbiter::checked`]):
//! - **Cut** — the turn is announced now: its withheld `speech_started`, a
//!   gate turn as without the check, on the post-interrupt window from
//!   here. An ASR failure, a timeout or no ASR alias is a cut too: the
//!   duration rule decides then.
//! - **Backchannel** — empty, or nothing but backchannel words, while the
//!   answer still plays: the turn stays unconfirmed and the session keeps
//!   listening. It is checked again once its voiced evidence has grown by
//!   another `barge_in_min_ms`, and once more when its silence window
//!   passes with unchecked speech in it — so "Mhm, aber warte mal" still
//!   cuts. A re-check transcribes only what came since the last check's
//!   audio, with at least the pre-roll's length of overlap (E2): the upload
//!   does not grow with the turn. It starts at a pause — the last unvoiced
//!   frame at or before that overlap's start (B4 review M2): cut mid-word,
//!   "alles klar" came back as "Les klar." and "ja genau" as "Nau.", which
//!   no list holds, and cut the answer. The pause is looked for within one
//!   more pre-roll before the overlap's start only, and without one there
//!   the re-check starts at the overlap's start, as E2 had it (fix package
//!   B6): falling back to the turn's start let hum, music or a foreign
//!   radio grow every upload until the check timed out and cut. When the
//!   window has passed and nothing unchecked is left, the turn is
//!   **discarded**: never announced, never committed, its audio gone
//!   ([`DropReason::Checked`]).
//! - **Turn** — a backchannel, but nothing plays any more (the session
//!   judges that, E1): "Ja" or "Okay" after "Soll ich das so machen?" is
//!   the user's answer. The turn is announced as a **normal** one, on the
//!   plain silence window — and ended at once if that has passed already.
//!
//! **The window bounds it** (E1, E2): frames of an unconfirmed turn are the
//! detector's while they fall inside the window — whether the user
//! interrupted is the words' question then, and the cut is judged at the
//! instant the gate decided (the session keeps it, `audio_in::listen`).
//! Past the window's end the answer has played out and nothing is left to
//! cut ([`Arbiter::past_window`], B4 review M1):
//! - speech still going on — a voiced frame — or speech no check has heard
//!   yet **promotes** the turn to a normal one: the normal endpointer and
//!   the turn's own transcript take over, and a turn whose silence window
//!   had passed already ends in the same frame;
//! - a check still in flight decides: backchannel words are then the
//!   user's reply (`Turn`);
//! - a turn whose words were all checked and were a backchannel, silent
//!   since, is **discarded** — a "Mhm" whose silence ran past the answer's
//!   end used to be promoted, committed and answered.
//!
//! Humming or music Silero calls voice therefore cannot keep a turn
//! unconfirmed, and re-checked, for longer than the answer plays.
//!
//! At most one check is in flight per turn; a re-check that falls due
//! meanwhile is made when the verdict comes back. A verdict for any other
//! check — the turn was committed, cleared, promoted or discarded since —
//! changes nothing ([`Arbiter::awaits`] says so first).

use super::super::server_vad::{ServerVad, TurnEvent};
use super::{Arbiter, DropReason, Dropped, Judged, Outcome};

/// The audio of an unconfirmed turn, to transcribe (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRequest {
    /// Its id: the verdict names it.
    pub id: u64,
    /// The turn's audio so far, 24 kHz, pre-roll included.
    pub samples: Vec<i16>,
    /// The first check of the turn — the gate's trigger: the frame that
    /// asked for it is where the cut is judged.
    pub trigger: bool,
}

/// What the word check found (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Words that are not a backchannel — or no answer: the duration rule.
    Cut,
    /// Nothing, or backchannel words only, while the answer plays.
    Backchannel,
    /// Backchannel words, but nothing plays any more: a reply, announced as
    /// a normal turn.
    Turn,
}

/// What a verdict did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Checked {
    /// The turn events, in order: the withheld `speech_started` on a cut or
    /// a reply — and a reply's `speech_stopped` when its silence window has
    /// passed already.
    pub judged: Vec<Judged>,
    /// The turn discarded.
    pub dropped: Option<Dropped>,
    /// The next check, if one is due now.
    pub check: Option<CheckRequest>,
}

/// A turn the gate started whose words are being checked.
#[derive(Debug)]
pub(super) struct Unconfirmed {
    /// The check in flight, or the last one made.
    id: u64,
    in_flight: bool,
    /// The withheld `speech_started`.
    started: TurnEvent,
    /// The gate's onset, for the log.
    onset: u64,
    /// Voiced time since the onset.
    evidence_ms: u64,
    /// `evidence_ms` when the last check was made.
    checked_ms: u64,
    /// Where the last check's audio ended: a re-check starts there, less
    /// the pre-roll (module doc).
    checked_to: u64,
    /// Where each unvoiced frame of the turn since its onset began, from
    /// the last re-check's start on: where a re-check may start (module
    /// doc).
    quiet: Vec<u64>,
    /// Voiced frames after the last check's audio.
    unchecked: bool,
    /// The turn's silence window has passed.
    ended: bool,
}

impl Arbiter {
    /// The gate triggered in words mode: the turn opens unconfirmed — kept
    /// open by the detector — and its first check goes out (module doc).
    /// `quiet`: the unvoiced frames of the gate's evidence (module doc).
    pub(super) fn begin_check(
        &mut self,
        vad: &mut ServerVad,
        started: TurnEvent,
        (onset, quiet): (u64, Vec<u64>),
        evidence_ms: u64,
        to: u64,
    ) -> Outcome {
        vad.keep_open(true);
        self.unconfirmed = Some(Unconfirmed {
            id: 0,
            in_flight: false,
            started,
            onset,
            evidence_ms,
            checked_ms: 0,
            checked_to: 0,
            quiet,
            unchecked: true,
            ended: false,
        });
        Outcome {
            check: self.issue(vad, to, true),
            ..Outcome::default()
        }
    }

    /// Whether a turn's words are being checked.
    pub fn checking(&self) -> bool {
        self.unconfirmed.is_some()
    }

    /// Whether check `id` is the one its turn waits for: a verdict for any
    /// other changes nothing (module doc).
    pub fn awaits(&self, id: u64) -> bool {
        self.unconfirmed
            .as_ref()
            .is_some_and(|u| u.in_flight && u.id == id)
    }

    /// A frame of the unconfirmed turn past the window's end (module doc):
    /// promoted on voice or unchecked speech, the verdict awaited while a
    /// check is in flight, and otherwise — a backchannel, all of it
    /// checked — discarded.
    pub(super) fn past_window(
        &mut self,
        vad: &mut ServerVad,
        voiced: bool,
        from: u64,
        to: u64,
    ) -> Outcome {
        let Some(u) = self.unconfirmed.as_ref() else {
            return Outcome::default();
        };
        if voiced || u.unchecked {
            return self.promote(vad, voiced, from, to);
        }
        if u.in_flight {
            return self.unconfirmed_frame(vad, voiced, from, to);
        }
        vad.push_voiced(voiced, from, to);
        Outcome {
            dropped: self.discard(vad),
            ..Outcome::default()
        }
    }

    /// The unconfirmed turn is a normal turn from this frame on (module
    /// doc), announced now; one whose silence window has passed already
    /// ends in the same frame, after its start.
    fn promote(&mut self, vad: &mut ServerVad, voiced: bool, from: u64, to: u64) -> Outcome {
        let Some(u) = self.unconfirmed.take() else {
            return Outcome::default();
        };
        vad.push_voiced(voiced, from, to);
        vad.keep_open(false);
        self.gate.reset();
        self.onset = None;
        self.carry = None;
        let stopped = if vad.stop_due() {
            vad.end_turn().map(|event| Judged {
                event,
                barge_in: false,
            })
        } else {
            None
        };
        Outcome {
            judged: Some(Judged {
                event: u.started,
                barge_in: false,
            }),
            promoted: Some(self.ms(u.onset)),
            stopped,
            ..Outcome::default()
        }
    }

    /// A frame of the unconfirmed turn: the detector's, its evidence
    /// counted (module doc).
    pub(super) fn unconfirmed_frame(
        &mut self,
        vad: &mut ServerVad,
        voiced: bool,
        from: u64,
        to: u64,
    ) -> Outcome {
        // Kept open, and in speech: no event comes of it.
        vad.push_voiced(voiced, from, to);
        let frame_ms = self.ms(to - from);
        let step = self.step();
        let ended = vad.stop_due();
        let Some(u) = self.unconfirmed.as_mut() else {
            return Outcome::default();
        };
        if voiced {
            u.evidence_ms += frame_ms;
            u.unchecked = true;
        } else {
            u.quiet.push(from);
        }
        let mut out = Outcome::default();
        if ended && !u.ended {
            u.ended = true;
            if !u.in_flight {
                if u.unchecked {
                    out.check = self.issue(vad, to, false);
                } else {
                    out.dropped = self.discard(vad);
                }
            }
        } else if !ended {
            u.ended = false;
            if !u.in_flight && u.evidence_ms >= u.checked_ms + step {
                out.check = self.issue(vad, to, false);
            }
        }
        out
    }

    /// Check `id`'s verdict (module doc).
    pub fn checked(&mut self, vad: &mut ServerVad, id: u64, verdict: Verdict) -> Checked {
        let step = self.step();
        if !self.awaits(id) {
            return Checked::default();
        }
        let Some(u) = self.unconfirmed.as_mut() else {
            return Checked::default();
        };
        u.in_flight = false;
        let mut out = Checked::default();
        match verdict {
            Verdict::Cut => {
                let Some(u) = self.unconfirmed.take() else {
                    return out;
                };
                vad.keep_open(false);
                vad.arm_post_interrupt();
                out.judged.push(Judged {
                    event: u.started,
                    barge_in: true,
                });
            }
            Verdict::Turn => {
                let Some(u) = self.unconfirmed.take() else {
                    return out;
                };
                vad.keep_open(false);
                self.gate.reset();
                out.judged.push(Judged {
                    event: u.started,
                    barge_in: false,
                });
                // Its silence window passed while the words were checked:
                // it ends now, not at whenever the next frame comes.
                if vad.stop_due() {
                    out.judged.extend(vad.end_turn().map(|event| Judged {
                        event,
                        barge_in: false,
                    }));
                }
            }
            Verdict::Backchannel => {
                let to = vad.last_frame_end();
                if u.ended && !u.unchecked {
                    out.dropped = self.discard(vad);
                } else if u.ended || u.evidence_ms >= u.checked_ms + step {
                    out.check = self.issue(vad, to, false);
                }
            }
        }
        out
    }

    /// The re-check step: another `barge_in_min_ms` of voice, one frame at
    /// the least.
    fn step(&self) -> u64 {
        u64::from(self.params.min_ms).max(1)
    }

    /// The next check of the unconfirmed turn: its audio up to `to` — the
    /// whole turn for the first, and for a re-check what came since the
    /// last check's audio, from the last pause at least the pre-roll's
    /// length before it and at most twice that, else from exactly the
    /// pre-roll's length before it (module doc).
    fn issue(&mut self, vad: &ServerVad, to: u64, trigger: bool) -> Option<CheckRequest> {
        self.checks += 1;
        let id = self.checks;
        let u = self.unconfirmed.as_mut()?;
        u.id = id;
        u.in_flight = true;
        u.checked_ms = u.evidence_ms;
        u.unchecked = false;
        let start = vad.turn_start().unwrap_or(to);
        let from = if trigger {
            start
        } else {
            let pre_roll = vad.prefix_samples();
            let overlap = u.checked_to.saturating_sub(pre_roll);
            let lookback = overlap.saturating_sub(pre_roll);
            let pause = u
                .quiet
                .iter()
                .rev()
                .find(|&&q| q <= overlap)
                .filter(|&&q| q >= lookback)
                .copied();
            pause.unwrap_or(overlap).max(start)
        };
        // Pauses before this start can never be a later re-check's.
        u.quiet.retain(|&q| q >= from);
        u.checked_to = to;
        Some(CheckRequest {
            id,
            samples: vad.audio(from, to),
            trigger,
        })
    }

    /// The unconfirmed turn was no turn: gone, and the gate listens again.
    pub(super) fn discard(&mut self, vad: &mut ServerVad) -> Option<Dropped> {
        let u = self.unconfirmed.take()?;
        vad.discard_turn();
        self.gate.reset();
        Some(Dropped {
            why: DropReason::Checked,
            evidence_ms: u.evidence_ms,
            onset_ms: self.ms(u.onset),
        })
    }
}

#[cfg(test)]
mod tests;
