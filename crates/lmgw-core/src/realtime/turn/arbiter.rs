//! Who judges a frame of input (realtime design §6.4, §6.5): the
//! `server_vad` detector, or the barge-in evidence gate.
//!
//! Why there are two judges: clients stop playback on `speech_started`, so
//! while the client plays a response the event must not fire on echo, a
//! cough or a "mhm" — speech has to earn the interruption ([`BargeIn`]).
//! Everywhere else the detector's normal onset (96 ms) is right, and
//! OpenAI's. The rules, per Silero frame:
//! - **Inside the playing window, no turn open:** the gate decides. The
//!   detector only keeps its timeline moving ([`ServerVad::idle_frame`], so
//!   the gate's frames never add up to a normal onset) and holds the audio
//!   of pending evidence ([`ServerVad::hold_from`]). When the evidence
//!   reaches `barge_in_min_ms` the turn starts back-dated to where it began
//!   ([`ServerVad::begin_at`]): `audio_start_ms` stays pure sample math.
//!   With the word check on (`barge_in_check: "words"`), it starts
//!   unconfirmed, and its words decide whether it is announced at all
//!   (`check`).
//! - **Speech that does not earn it** (a backchannel) is never announced or
//!   committed: its held audio goes when the gate's evidence resets, and the
//!   session logs it ([`Dropped`]).
//! - **A turn already open** stays the detector's, gate or no gate (the
//!   user was talking before playback began, or `interrupt_response` is off
//!   and the turn after a barge-in is still going): the gate forgets its
//!   window meanwhile, so once that turn ends inside the same window the
//!   gate listens again — with no guard, the window having started long
//!   before.
//! - **Leaving the window mid-evidence** (the answer ended while the user
//!   started): the next normal onset within the gate's gap (800 ms) is
//!   back-dated to the gate's onset, so the utterance's start inside the
//!   window survives ([`Carry`]). The voice before the window's end counts
//!   towards that onset, so a short reply that straddles the end ("Nein",
//!   with less than the onset's 96 ms after it) is a turn too (B3 review,
//!   E1). The answer has played out by then: it is a normal turn, on the
//!   plain silence window — a reply, not an interruption.
//! - **A wall-clock gap** between two frames longer than that gap — a muted
//!   client sends nothing, so no unvoiced frames reset the evidence — resets
//!   it too: a mute cannot glue two utterances into one barge-in. It also
//!   ends a turn whose words are being checked (`check`): what its checks
//!   heard did not interrupt, and what follows the mute is a new utterance.
//! - **Half duplex** (`session.lmgw.half_duplex`, for clients without echo
//!   cancellation, §6.4): inside the window nothing is listened to — every
//!   frame counts as silence, so no turn starts and none is committed from
//!   what the microphone heard of the answer; a turn already open ends at
//!   the last frame before the window, so the echo neither holds it open
//!   nor is transcribed with it (B3 review 5). Not the turn whose start
//!   cancels that answer: the session leaves that answer's window out for
//!   it (`audio_in::listen`, live run 2 H1).
//! - **The turn that interrupts** — started by the gate, or started at all
//!   while a response the client has heard some of is in progress — ends on
//!   `post_interrupt_silence_ms` (§6.5), armed here, in the same frame, so
//!   even a turn that ends within the same append gets it. A response
//!   nobody heard yet (awaiting transcripts, generating before its first
//!   audio) interrupts nothing the user listened to: a turn then is a
//!   continuation, and keeps the plain window (B3 review 4). Nor does a
//!   turn that starts after the answer played out — carried over the
//!   window's end, or a gate turn whose words were still being checked
//!   (`check`): that is a reply (B3 review, E1).
//!
//! Pure: no clock, no model. The session says what surrounds each frame
//! ([`Context`]: inside the window or not, where the window began on the
//! input timeline, a wall-clock gap, a heard response in progress).

use super::barge_in::{BargeIn, BargeInFrame, BargeInParams};
use super::server_vad::{ServerVad, TurnEvent};

mod check;

pub use check::{CheckRequest, Checked, Verdict};

/// A frame captured inside the playing window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowFrame {
    /// Which window: a new value is a new one (the response generation).
    pub window: u64,
    /// Where the window began on the input timeline, in milliseconds: the
    /// gate's guard counts from here. An estimate the session refines as
    /// frames come (`audio_in::listen`).
    pub started_ms: u64,
}

/// What surrounds one frame — the session's knowledge, not the arbiter's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Context {
    /// `Some` when the frame was captured while the client played.
    pub window: Option<WindowFrame>,
    /// More than the gate's gap of wall time passed since the frame before
    /// was captured (module doc).
    pub wall_gap: bool,
    /// A response the client has heard some of is in progress: a turn that
    /// starts now interrupts it (B3 review 4).
    pub heard: bool,
}

/// A turn event, and whether the barge-in gate started it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judged {
    pub event: TurnEvent,
    pub barge_in: bool,
}

/// Speech inside the playing window that was deliberately not a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dropped {
    pub why: DropReason,
    /// Voiced time the gate had counted (0 for half duplex).
    pub evidence_ms: u64,
    /// Where it began on the input timeline.
    pub onset_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Below `barge_in_min_ms` before the evidence reset.
    Backchannel,
    /// `half_duplex`: input during playback is not listened to (reported
    /// once per window).
    HalfDuplex,
    /// The gate passed it, and the word check heard nothing or only
    /// backchannel words (`check`).
    Checked,
}

/// What one frame did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub judged: Option<Judged>,
    pub dropped: Option<Dropped>,
    /// Audio of an unconfirmed turn to check the words of (`check`).
    pub check: Option<CheckRequest>,
    /// An unconfirmed turn went on past the window's end before its words
    /// were known, and is a normal turn now (`check`): where it began on
    /// the input timeline, in milliseconds — for the log.
    pub promoted: Option<u64>,
    /// The promoted turn's end in the same frame, after its start: its
    /// silence window had passed already (`check`).
    pub stopped: Option<Judged>,
}

/// Evidence that left the window unfinished (module doc).
#[derive(Debug, Clone, Copy)]
struct Carry {
    /// The gate's onset sample.
    onset: u64,
    /// The last frame end a normal onset may come at and still be it.
    until: u64,
    evidence_ms: u64,
    /// Voice heard since the window ended, in samples: with the evidence
    /// before it, what a normal onset needs makes the turn (module doc).
    after: u64,
}

/// The arbiter between the detector and the gate (module doc).
#[derive(Debug)]
pub struct Arbiter {
    gate: BargeIn,
    params: BargeInParams,
    half_duplex: bool,
    /// The timeline's rate (24000).
    rate: u64,
    /// Where the gate's pending evidence began, as a sample.
    onset: Option<u64>,
    carry: Option<Carry>,
    /// The window whose ignored speech was reported (half duplex).
    reported: Option<u64>,
    /// The word check is on (`check`).
    words: bool,
    /// The gate's turn whose words are being checked.
    unconfirmed: Option<check::Unconfirmed>,
    /// Checks made so far: the next one's id. Never reset, so a verdict for
    /// a turn that is gone cannot match a later one.
    checks: u64,
    /// Where each unvoiced frame of the gate's pending evidence began: a
    /// re-check of the turn it becomes starts at one (`check`, B4 review
    /// M2).
    quiet: Vec<u64>,
}

impl Arbiter {
    pub fn new(params: BargeInParams, half_duplex: bool, rate: u32) -> Self {
        Self {
            gate: BargeIn::new(params),
            params,
            half_duplex,
            rate: u64::from(rate.max(1)),
            onset: None,
            carry: None,
            reported: None,
            words: false,
            unconfirmed: None,
            checks: 0,
            quiet: Vec::new(),
        }
    }

    /// New knobs from a `session.update`; the evidence so far is forgotten
    /// — but not a turn whose words are being checked: the detector keeps
    /// it open, and only its verdict ends that.
    pub fn set_params(&mut self, params: BargeInParams, half_duplex: bool) {
        if (params, half_duplex) != (self.params, self.half_duplex) {
            let mut next = Self::new(params, half_duplex, self.rate as u32);
            next.words = self.words;
            next.unconfirmed = self.unconfirmed.take();
            next.checks = self.checks;
            *self = next;
        }
    }

    /// Whether the gate's turns wait for the word check (`check`): the
    /// session's `barge_in_check` is `"words"` and it has an ASR alias.
    pub fn set_words(&mut self, words: bool) {
        self.words = words;
    }

    /// The gate's gap: unvoiced time — or wall time with no frames — that
    /// resets the evidence.
    pub fn gap_ms(&self) -> u32 {
        self.params.gap_reset_ms
    }

    /// Forget everything — a clear, a commit, a mode switch. The detector's
    /// hold, and a turn kept open for its words, go with its own reset.
    pub fn reset(&mut self) {
        self.gate.reset();
        self.onset = None;
        self.carry = None;
        self.reported = None;
        self.unconfirmed = None;
        self.quiet.clear();
    }

    fn ms(&self, sample: u64) -> u64 {
        (u128::from(sample) * 1000 / u128::from(self.rate)) as u64
    }

    /// Judge one Silero frame (`prob`, spanning `from..to` on the timeline).
    pub fn frame(
        &mut self,
        vad: &mut ServerVad,
        prob: f32,
        from: u64,
        to: u64,
        cx: Context,
    ) -> Outcome {
        let voiced = vad.is_voiced(prob);
        let mut gap_drop = None;
        if self.unconfirmed.is_some() {
            if !vad.in_speech() {
                // Its turn went with a detector reset the arbiter missed.
                self.unconfirmed = None;
            } else if cx.wall_gap {
                // A mute ends it (module doc): what follows is new.
                gap_drop = self.discard(vad);
            } else if cx.window.is_some() {
                return self.unconfirmed_frame(vad, voiced, from, to);
            } else {
                // Past the window's end: nothing plays any more.
                return self.past_window(vad, voiced, from, to);
            }
        }
        if cx.wall_gap {
            gap_drop = gap_drop.or(self.forget(vad));
        }
        let mut out = match cx.window {
            Some(w) if self.half_duplex => self.unheard(vad, voiced, from, to, w),
            Some(w) if !vad.in_speech() => self.gate_frame(vad, voiced, from, to, w),
            _ => self.detector_frame(vad, voiced, from, to, cx),
        };
        out.dropped = out.dropped.or(gap_drop);
        out
    }

    /// The evidence is gone (a wall-clock gap): what it was, if anything.
    fn forget(&mut self, vad: &mut ServerVad) -> Option<Dropped> {
        let evidence_ms = self.gate.evidence_ms();
        let onset = self.onset.take().or(self.carry.take().map(|c| c.onset));
        self.gate.reset();
        vad.hold_from(None);
        onset.map(|o| Dropped {
            why: DropReason::Backchannel,
            evidence_ms,
            onset_ms: self.ms(o),
        })
    }

    /// Half duplex, inside the window: heard as silence (module doc).
    fn unheard(
        &mut self,
        vad: &mut ServerVad,
        voiced: bool,
        from: u64,
        to: u64,
        w: WindowFrame,
    ) -> Outcome {
        self.gate.reset();
        self.onset = None;
        self.carry = None;
        vad.hold_from(None);
        // A turn open when the window began ends at the frame before it: for
        // a client without echo cancellation what follows is the answer's
        // echo, which must neither keep the turn open nor be transcribed
        // with it (B3 review 5).
        let ended = if vad.in_speech() {
            vad.end_turn()
        } else {
            None
        };
        let event = ended.or_else(|| vad.push_voiced(false, from, to));
        let dropped = (voiced && self.reported != Some(w.window)).then(|| {
            self.reported = Some(w.window);
            Dropped {
                why: DropReason::HalfDuplex,
                evidence_ms: 0,
                onset_ms: self.ms(from),
            }
        });
        Outcome {
            judged: event.map(|event| Judged {
                event,
                barge_in: false,
            }),
            dropped,
            ..Outcome::default()
        }
    }

    /// Inside the window, no turn open: the gate's frame.
    fn gate_frame(
        &mut self,
        vad: &mut ServerVad,
        voiced: bool,
        from: u64,
        to: u64,
        w: WindowFrame,
    ) -> Outcome {
        // Back inside a window (the next response plays): its own evidence
        // counts, not what left the last one.
        self.carry = None;
        let evidence_ms = self.gate.evidence_ms();
        let had = self.onset;
        let trigger = self.gate.push(BargeInFrame {
            voiced,
            frame_ms: self.ms(to - from) as u32,
            now_ms: self.ms(to),
            playing: true,
            window: w.window,
            playback_started_ms: w.started_ms,
        });
        if trigger.is_some() {
            let onset = had.unwrap_or(from);
            self.onset = None;
            // The hold first, so advancing the timeline keeps the pre-roll.
            vad.hold_from(Some(onset));
            vad.idle_frame(from, to);
            let event = vad.begin_at(onset);
            vad.hold_from(None);
            if self.words {
                let evidence = evidence_ms + self.ms(to - from);
                let mut quiet = std::mem::take(&mut self.quiet);
                quiet.retain(|&q| q >= onset);
                return self.begin_check(vad, event, (onset, quiet), evidence, to);
            }
            self.quiet.clear();
            vad.arm_post_interrupt();
            return Outcome {
                judged: Some(Judged {
                    event,
                    barge_in: true,
                }),
                ..Outcome::default()
            };
        }
        self.onset = self.gate.pending_onset_ms().map(|_| had.unwrap_or(from));
        match self.onset {
            Some(_) if !voiced => self.quiet.push(from),
            Some(_) => {}
            None => self.quiet.clear(),
        }
        let dropped = match (had, self.onset) {
            (Some(o), None) => Some(Dropped {
                why: DropReason::Backchannel,
                evidence_ms,
                onset_ms: self.ms(o),
            }),
            _ => None,
        };
        vad.hold_from(self.onset);
        vad.idle_frame(from, to);
        Outcome {
            dropped,
            ..Outcome::default()
        }
    }

    /// The detector's frame: outside the window, or a turn is open.
    fn detector_frame(
        &mut self,
        vad: &mut ServerVad,
        voiced: bool,
        from: u64,
        to: u64,
        cx: Context,
    ) -> Outcome {
        if let Some(onset) = self.onset.take() {
            // The window ended under the gate's pending evidence.
            self.carry = Some(Carry {
                onset,
                until: to + u64::from(self.params.gap_reset_ms) * self.rate / 1000,
                evidence_ms: self.gate.evidence_ms(),
                after: 0,
            });
        }
        self.gate.reset();
        let mut event = vad.push_voiced(voiced, from, to);
        let mut dropped = None;
        if voiced && event.is_none() && !vad.in_speech() {
            // Voice since the window ended adds to the evidence before it:
            // a short reply that straddles the end is a turn (module doc).
            let rate = self.rate;
            if let Some(c) = self.carry.as_mut() {
                c.after += to - from;
                if c.evidence_ms * rate / 1000 + c.after >= vad.onset_samples() {
                    event = Some(vad.begin_at(c.onset));
                }
            }
        }
        if matches!(event, Some(TurnEvent::SpeechStarted { .. })) {
            if let Some(c) = self.carry.take() {
                event = Some(vad.begin_at(c.onset));
            }
            // A carried turn starts after the answer played out: a reply,
            // on the plain window (module doc).
            if cx.heard {
                vad.arm_post_interrupt();
            }
        } else if let Some(c) = self.carry.filter(|c| to > c.until) {
            self.carry = None;
            dropped = Some(Dropped {
                why: DropReason::Backchannel,
                evidence_ms: c.evidence_ms,
                onset_ms: self.ms(c.onset),
            });
        }
        vad.hold_from(self.carry.map(|c| c.onset));
        Outcome {
            judged: event.map(|event| Judged {
                event,
                barge_in: false,
            }),
            dropped,
            ..Outcome::default()
        }
    }
}

#[cfg(test)]
mod tests;
