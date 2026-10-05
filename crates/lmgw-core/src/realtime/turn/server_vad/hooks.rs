//! The detector's hooks for barge-in (realtime design §6.4, §6.5): what the
//! arbiter (`super::super::arbiter`) does to the detector while the
//! barge-in gate, not the detector, judges the frames of the playing
//! window — keep the audio of pending evidence, start a turn back-dated to
//! where it began, and give the turn after an interruption its longer
//! silence window.

use super::{ServerVad, State, TurnEvent, IDLE};

impl ServerVad {
    /// Starts (or back-dates) the turn at a barge-in's onset (§6.4). Idle:
    /// a turn begins there. In a turn: its start moves earlier if the onset
    /// is earlier. Either way the start is padded and clamped like a normal
    /// onset, and the returned `SpeechStarted` carries the effective start.
    pub fn begin_at(&mut self, onset_sample: u64) -> TurnEvent {
        match self.state {
            State::Idle { .. } => self.start_turn(onset_sample),
            State::Speech {
                start,
                voiced_run,
                silence,
            } => {
                let start = start.min(self.padded_start(onset_sample));
                self.state = State::Speech {
                    start,
                    voiced_run,
                    silence,
                };
                TurnEvent::SpeechStarted {
                    audio_start_ms: self.ms(start),
                    onset_ms: self.ms(onset_sample),
                }
            }
        }
    }

    /// While idle, also retain audio from `sample - prefix` on (the session
    /// passes the barge-in gate's pending onset so [`begin_at`] can
    /// back-date into it); `None` drops the hold.
    ///
    /// [`begin_at`]: Self::begin_at
    pub fn hold_from(&mut self, sample: Option<u64>) {
        self.hold = sample;
        self.trim();
    }

    /// Makes the current turn, or the next one if idle, end on
    /// `post_interrupt_silence_ms` (§6.5). Cleared when that turn stops.
    pub fn arm_post_interrupt(&mut self) {
        self.post_armed = true;
    }

    /// The silence window the open turn ends on. With `semantic_vad` it is
    /// the rule's for the open pause (`scoring`) — never shorter than the
    /// post-interrupt window when that is armed — except while the word
    /// check keeps the turn open: that turn is judged as without Smart Turn
    /// (§6.4).
    pub(super) fn window(&self) -> u64 {
        let plain = if self.post_armed {
            self.post_silence
        } else {
            self.silence
        };
        let rule = self.semantic.as_ref().filter(|_| !self.keep_open);
        match rule.and_then(|s| s.window()) {
            Some(w) if self.post_armed => w.max(self.post_silence),
            Some(w) => w,
            None => plain,
        }
    }

    /// While `on`, the open turn does not end by itself (§6.4's word check):
    /// its silence is still counted, and [`Self::stop_due`] says when it
    /// would have ended. Off, it ends at the next unvoiced frame if its
    /// window has passed. Cleared when the turn stops or is discarded.
    pub fn keep_open(&mut self, on: bool) {
        self.keep_open = on;
    }

    /// The open turn's silence window has passed: it would have ended.
    pub fn stop_due(&self) -> bool {
        match self.state {
            State::Speech { silence, .. } => silence >= self.window(),
            State::Idle { .. } => false,
        }
    }

    /// Drop the open turn without a segment — speech the word check found
    /// was no turn (§6.4). Its audio goes, and no later segment starts
    /// before where it got to.
    pub fn discard_turn(&mut self) {
        if !self.in_speech() {
            return;
        }
        self.state = IDLE;
        self.floor = self.last_frame_end;
        self.post_armed = false;
        self.keep_open = false;
        if let Some(s) = self.semantic.as_mut() {
            s.clear();
        }
        self.trim();
    }

    /// The end of the last frame judged, on the timeline.
    pub fn last_frame_end(&self) -> u64 {
        self.last_frame_end
    }

    /// The voice a normal onset needs, in samples (`onset_ms`).
    pub fn onset_samples(&self) -> u64 {
        self.onset
    }

    /// The pre-roll kept before an onset, in samples
    /// (`prefix_padding_ms`).
    pub fn prefix_samples(&self) -> u64 {
        self.prefix
    }

    /// A frame the detector does not judge — the barge-in gate does (§6.4):
    /// the timeline advances and a voiced run restarts, so the gate's frames
    /// never add up to a normal onset; `hold_from` keeps what the gate may
    /// still back-date into. Idle only: an open turn is always the
    /// detector's.
    pub fn idle_frame(&mut self, start: u64, end: u64) {
        if end <= start || self.in_speech() {
            return;
        }
        self.last_frame_end = end;
        self.state = IDLE;
        self.trim();
    }
}
