//! `server_vad`: Silero probabilities to turns, on the session's 24 kHz
//! input timeline (realtime §6.2, §6.5).
//!
//! Why it looks like this:
//! - **Onset** needs `onset_ms` of consecutive voiced frames (96 ms, as in
//!   the earlier prototype), so one voiced frame of noise is not a turn.
//!   The turn is back-dated to the first frame of that run.
//! - **End** after `silence_duration_ms` of unvoiced frames (OpenAI's
//!   default 500 ms), or `post_interrupt_silence_ms` (1500 ms) for the turn
//!   that follows a barge-in (§6.5): a user who interrupts pauses to
//!   rephrase, and a tight window turned the interjection into the
//!   whole prompt.
//! - **Blip guard.** Inside a turn only `resume_ms` (~96 ms) of sustained
//!   voice resets the silence count; shorter voiced blips neither reset it
//!   nor add to it. The earlier prototype's live bug was a single blip holding
//!   a turn open forever.
//! - **Pre-roll.** The committed segment starts `prefix_padding_ms` before
//!   the onset, so the first word survives detector lag, but never before
//!   the previous commit (that audio was already committed) or before what
//!   was retained. `audio_start_ms` includes the padding and `audio_end_ms`
//!   the silence window, as OpenAI defines them.
//! - **Bounded retention.** While idle the ring keeps only the pre-roll for
//!   the current voiced run (plus anything [`ServerVad::hold_from`] asks
//!   for); during a turn it grows with the turn, like the input buffer
//!   (§10.4). Memory follows the audio received, never a parameter.
//!
//! The detector never sees the model or a clock: the session feeds it 24 kHz
//! PCM ([`ServerVad::push_audio`]) and one probability per Silero frame
//! with that frame's span on the same timeline ([`ServerVad::push_frame`]).
//! Setting `post_interrupt_silence_ms` equal to `silence_duration_ms`
//! restores plain OpenAI timing (§6.2 parity).
//!
//! **`semantic_vad`** (§6.3) is this detector with Smart Turn deciding when
//! a pause ends the turn ([`ServerVadParams::semantic`], `super::semantic`):
//! onset, blip guard, pre-roll and retention stay as they are, and
//! `silence_duration_ms` is then the plain window a pause falls back to
//! when it cannot be scored (`scoring`).

use std::collections::VecDeque;
use std::fmt;

use super::semantic::{Semantic, SemanticParamError, SemanticParams, TurnEnd};

/// Detector parameters; the first three are OpenAI's `server_vad` fields.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerVadParams {
    /// Probability at or above which a frame is voiced, `0.0..=1.0`.
    pub threshold: f32,
    /// Audio kept before the onset (OpenAI default 300).
    pub prefix_padding_ms: u32,
    /// Silence that ends a turn (OpenAI default 500).
    pub silence_duration_ms: u32,
    /// Silence that ends the turn after a barge-in (lmgw, §6.5).
    pub post_interrupt_silence_ms: u32,
    /// Consecutive voice that confirms an onset (prototype: 96).
    pub onset_ms: u32,
    /// Sustained voice that resets the silence count (prototype: 96).
    pub resume_ms: u32,
    /// The timeline's rate (the session's input rate, 24000).
    pub sample_rate: u32,
    /// `semantic_vad` with Smart Turn (§6.3): when a pause ends the turn is
    /// the rule's, and `silence_duration_ms` is the window a pause with no
    /// score falls back to. `None`: plain `server_vad`.
    pub semantic: Option<SemanticParams>,
}

impl Default for ServerVadParams {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            prefix_padding_ms: 300,
            silence_duration_ms: 500,
            post_interrupt_silence_ms: 1500,
            onset_ms: 96,
            resume_ms: 96,
            sample_rate: 24_000,
            semantic: None,
        }
    }
}

/// Rejected parameters.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamError {
    /// `threshold` must be a number in `0.0..=1.0`.
    Threshold(f32),
    /// `sample_rate` must not be zero.
    ZeroRate,
    /// The `semantic_vad` rule's knobs (§6.3).
    Semantic(SemanticParamError),
}

impl fmt::Display for ParamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Threshold(t) => write!(f, "turn_detection.threshold {t} is not in 0.0..=1.0"),
            Self::ZeroRate => write!(f, "sample rate must not be zero"),
            Self::Semantic(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ParamError {}

/// A committed stretch of input audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Timeline index of `samples[0]`.
    pub start_sample: u64,
    /// The 24 kHz PCM, pre-roll and trailing silence included.
    pub samples: Vec<i16>,
}

/// What a frame (or [`ServerVad::begin_at`]) changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEvent {
    /// `input_audio_buffer.speech_started`. `audio_start_ms` includes the
    /// prefix padding; `onset_ms` is the first voiced frame.
    SpeechStarted { audio_start_ms: u64, onset_ms: u64 },
    /// `input_audio_buffer.speech_stopped` plus the segment to commit.
    /// `audio_end_ms` includes the silence window.
    SpeechStopped { audio_end_ms: u64, segment: Segment },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle {
        run_start: Option<u64>,
        run_len: u64,
    },
    Speech {
        start: u64,
        voiced_run: u64,
        silence: u64,
    },
}

const IDLE: State = State::Idle {
    run_start: None,
    run_len: 0,
};

/// The `server_vad` turn detector.
#[derive(Debug)]
pub struct ServerVad {
    threshold: f32,
    rate: u64,
    prefix: u64,
    silence: u64,
    post_silence: u64,
    onset: u64,
    resume: u64,
    state: State,
    ring: VecDeque<i16>,
    /// Timeline index of `ring[0]`; `ring_base + ring.len()` is all audio pushed.
    ring_base: u64,
    /// No segment starts before the last commit or clear.
    floor: u64,
    last_frame_end: u64,
    hold: Option<u64>,
    post_armed: bool,
    /// The open turn does not end by itself: its silence is counted, and
    /// the barge-in word check decides (`hooks`).
    keep_open: bool,
    /// `semantic_vad`'s rule and its open pause (`scoring`).
    semantic: Option<Semantic>,
    /// How the last turn ended, when the `semantic_vad` rule ended it.
    last_end: Option<TurnEnd>,
    /// The next score request's id: here, not in the rule, so that a switch
    /// to `server_vad` and back never issues an id twice (`scoring`).
    score_ids: u64,
}

impl ServerVad {
    pub fn new(params: &ServerVadParams) -> Result<Self, ParamError> {
        if !(0.0..=1.0).contains(&params.threshold) {
            return Err(ParamError::Threshold(params.threshold));
        }
        if params.sample_rate == 0 {
            return Err(ParamError::ZeroRate);
        }
        let rate = u64::from(params.sample_rate);
        let samples = |ms: u32| u64::from(ms) * rate / 1000;
        let semantic = params
            .semantic
            .as_ref()
            .map(|p| Semantic::new(p, rate))
            .transpose()
            .map_err(ParamError::Semantic)?;
        Ok(Self {
            threshold: params.threshold,
            rate,
            prefix: samples(params.prefix_padding_ms),
            silence: samples(params.silence_duration_ms),
            post_silence: samples(params.post_interrupt_silence_ms),
            onset: samples(params.onset_ms),
            resume: samples(params.resume_ms),
            state: IDLE,
            ring: VecDeque::new(),
            ring_base: 0,
            floor: 0,
            last_frame_end: 0,
            hold: None,
            post_armed: false,
            keep_open: false,
            semantic,
            last_end: None,
            score_ids: 0,
        })
    }

    /// Applies a `session.update`'s new parameters in place (lmgw, WP2): the
    /// session's detector lives as long as the session, and a new one would
    /// restart the timeline at zero. The retained audio, an open turn and
    /// the silence counted so far are kept; the new windows apply from the
    /// next frame on. The rate must stay the timeline's own.
    pub fn set_params(&mut self, params: &ServerVadParams) -> Result<(), ParamError> {
        let next = Self::new(params)?;
        self.threshold = next.threshold;
        self.rate = next.rate;
        self.prefix = next.prefix;
        self.silence = next.silence;
        self.post_silence = next.post_silence;
        self.onset = next.onset;
        self.resume = next.resume;
        // The pause and its score survive a retune; a switch to or from
        // `semantic_vad` starts or drops the rule.
        match (self.semantic.as_mut(), params.semantic.as_ref()) {
            (Some(s), Some(p)) => s.set_params(p).map_err(ParamError::Semantic)?,
            _ => self.semantic = next.semantic,
        }
        self.trim();
        Ok(())
    }

    fn ms(&self, sample: u64) -> u64 {
        (u128::from(sample) * 1000 / u128::from(self.rate)) as u64
    }

    /// Appends received input audio to the timeline.
    pub fn push_audio(&mut self, samples: &[i16]) {
        self.ring.extend(samples);
        self.trim();
    }

    /// Feeds one Silero frame's probability; `start..end` is the frame's
    /// span on the input timeline. An empty span is ignored.
    pub fn push_frame(&mut self, prob: f32, start: u64, end: u64) -> Option<TurnEvent> {
        self.push_voiced(self.is_voiced(prob), start, end)
    }

    /// Whether `prob` counts as voice (`threshold` inclusive; NaN is not
    /// >= anything, so it counts as unvoiced).
    pub fn is_voiced(&self, prob: f32) -> bool {
        prob >= self.threshold
    }

    /// [`Self::push_frame`] with the verdict already taken — the barge-in
    /// arbiter's way to feed a frame it judged itself (`super::arbiter`).
    pub fn push_voiced(&mut self, voiced: bool, start: u64, end: u64) -> Option<TurnEvent> {
        if end <= start {
            return None;
        }
        let len = end - start;
        self.last_frame_end = end;
        let event = match self.state {
            State::Idle { run_start, run_len } => {
                if !voiced {
                    self.state = IDLE;
                    None
                } else {
                    let run_start = run_start.unwrap_or(start);
                    let run_len = run_len.saturating_add(len);
                    self.state = State::Idle {
                        run_start: Some(run_start),
                        run_len,
                    };
                    (run_len >= self.onset).then(|| self.start_turn(run_start))
                }
            }
            State::Speech {
                start: seg_start,
                voiced_run,
                silence,
            } => {
                if voiced {
                    let voiced_run = voiced_run.saturating_add(len);
                    let silence = if voiced_run >= self.resume {
                        // The voice resumed: the pause is over.
                        if let Some(s) = self.semantic.as_mut() {
                            s.clear();
                        }
                        0
                    } else {
                        silence
                    };
                    self.state = State::Speech {
                        start: seg_start,
                        voiced_run,
                        silence,
                    };
                    None
                } else {
                    let silence = silence.saturating_add(len);
                    let ask = !self.keep_open;
                    if let Some(s) = self.semantic.as_mut() {
                        s.on_silence(end, silence, seg_start, ask, &mut self.score_ids);
                    }
                    if silence >= self.window() && !self.keep_open {
                        let ended = self.turn_end();
                        Some(self.stop_turn(seg_start, end, ended))
                    } else {
                        self.state = State::Speech {
                            start: seg_start,
                            voiced_run: 0,
                            silence,
                        };
                        None
                    }
                }
            }
        };
        self.trim();
        event
    }

    fn padded_start(&self, onset: u64) -> u64 {
        onset
            .saturating_sub(self.prefix)
            .max(self.floor)
            .max(self.ring_base)
    }

    fn start_turn(&mut self, onset: u64) -> TurnEvent {
        let start = self.padded_start(onset);
        self.state = State::Speech {
            start,
            voiced_run: 0,
            silence: 0,
        };
        TurnEvent::SpeechStarted {
            audio_start_ms: self.ms(start),
            onset_ms: self.ms(onset),
        }
    }

    fn stop_turn(&mut self, start: u64, end: u64, ended: Option<TurnEnd>) -> TurnEvent {
        let samples = self.audio(start, end);
        self.state = IDLE;
        self.floor = end;
        self.post_armed = false;
        self.keep_open = false;
        self.last_end = ended;
        if let Some(s) = self.semantic.as_mut() {
            s.clear();
        }
        TurnEvent::SpeechStopped {
            audio_end_ms: self.ms(end),
            segment: Segment {
                start_sample: start,
                samples,
            },
        }
    }

    /// What is retained of `start..end` (frames may run ahead of the audio).
    pub fn audio(&self, start: u64, end: u64) -> Vec<i16> {
        let audio_end = self.ring_base + self.ring.len() as u64;
        let index = |x: u64| (x.clamp(self.ring_base, audio_end) - self.ring_base) as usize;
        let (from, to) = (index(start), index(end));
        self.ring.range(from..to.max(from)).copied().collect()
    }

    /// Ends the open turn at the last frame's end, as `semantic_vad` does on
    /// a high score (§6.3); `None` while idle.
    pub fn end_turn(&mut self) -> Option<TurnEvent> {
        let State::Speech { start, .. } = self.state else {
            return None;
        };
        // The rule's end only when its window has passed (a score's answer);
        // half duplex and the like end it for reasons of their own.
        let ended = self.stop_due().then(|| self.turn_end()).flatten();
        Some(self.stop_turn(start, self.last_frame_end, ended))
    }

    /// Silence counted towards the open turn's end, in samples: 0 while
    /// idle and after the voice resumes; blips leave it as it is.
    pub fn silence(&self) -> u64 {
        match self.state {
            State::Speech { silence, .. } => silence,
            State::Idle { .. } => 0,
        }
    }

    /// Where the open turn's segment starts (pre-roll included).
    pub fn turn_start(&self) -> Option<u64> {
        match self.state {
            State::Speech { start, .. } => Some(start),
            State::Idle { .. } => None,
        }
    }

    /// Drops uncommitted audio and all detector state
    /// (`input_audio_buffer.clear`); the timeline keeps counting.
    pub fn reset(&mut self) {
        self.ring_base += self.ring.len() as u64;
        self.ring.clear();
        self.floor = self.ring_base;
        self.last_frame_end = self.last_frame_end.max(self.ring_base);
        self.state = IDLE;
        self.hold = None;
        self.post_armed = false;
        self.keep_open = false;
        self.last_end = None;
        if let Some(s) = self.semantic.as_mut() {
            s.clear();
        }
    }

    pub fn in_speech(&self) -> bool {
        matches!(self.state, State::Speech { .. })
    }

    /// Samples currently retained (pre-roll or the open turn).
    pub fn retained(&self) -> usize {
        self.ring.len()
    }

    /// Gives back the ring's room beyond twice what it holds, or `keep`
    /// samples, whichever is more: after a long turn is committed, or a
    /// large append, memory follows what is retained (lmgw, WP2 review R2).
    pub fn release(&mut self, keep: usize) {
        let want = self.ring.len().max(keep);
        if self.ring.capacity() > 2 * want {
            self.ring.shrink_to(want);
        }
    }

    /// The ring's allocated room, in samples.
    pub fn capacity(&self) -> usize {
        self.ring.capacity()
    }

    fn trim(&mut self) {
        let State::Idle { run_start, .. } = self.state else {
            return;
        };
        let mut keep = run_start.unwrap_or(self.last_frame_end);
        if let Some(h) = self.hold {
            keep = keep.min(h);
        }
        let keep = keep.saturating_sub(self.prefix).max(self.floor);
        if keep > self.ring_base {
            let n = ((keep - self.ring_base) as usize).min(self.ring.len());
            self.ring.drain(..n);
            self.ring_base += n as u64;
        }
    }
}

mod hooks;
mod scoring;
#[cfg(test)]
mod tests;
