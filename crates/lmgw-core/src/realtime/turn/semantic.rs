//! `semantic_vad`: when a pause ends the turn, with Smart Turn's help
//! (realtime design §6.3).
//!
//! **The rule** (decided on the owner's recordings: 161 incomplete pauses
//! and 109 true ends of their own speech, German and English). Speech versus
//! silence stays Silero's — this is a part of [`ServerVad`]'s pause, blip
//! guard included. Once a pause has lasted [`PROBE_AFTER_MS`], the turn's
//! last 8 s *including those 200 ms of silence* are scored once
//! ([`ScoreRequest`]; "variant B": on real speech it separates as well as
//! the audio up to the pause start, and leaves fewer true ends with a low
//! score). Then:
//! - **p ≥ threshold** — commit now;
//! - **p ≥ floor** — commit when the silence reaches the floor window (the
//!   plain 500 ms): the model is unsure, and a plain `server_vad` would have
//!   committed there too;
//! - **below the floor** — keep waiting. Voice that resumes (the detector's
//!   `resume_ms` rule) continues the turn and the next pause is scored
//!   afresh; otherwise the turn commits when the silence reaches the
//!   eagerness's maximum wait.
//!
//! What the data says about it, and the per-eagerness defaults: §6.3.
//!
//! Where the rule leaves room, it takes the reading that waits — a false
//! "complete" cuts the user off, a false "incomplete" only waits longer:
//! - **A pending score is awaited**, bounded by the maximum wait; no other
//!   window races it. It lands ~20–50 ms after the probe.
//! - **One score per pause.** A second would see nearly the same audio.
//! - **No score** — the scorer failed or answered something that is not a
//!   number — and the pause commits on the plain silence window, which is
//!   exactly `semantic_vad` before Smart Turn ([`Probe::Failed`]).
//! - **After a barge-in** the detector's post-interrupt window is a floor
//!   under every one of these (§6.5): nothing commits before it.
//! - **Stale answers are dropped.** A request carries an id that is never
//!   reused within the session — the detector counts them, so a switch to
//!   `server_vad` and back does not start over (WP6 review); an answer for
//!   a pause that has ended (voice resumed, turn committed or discarded,
//!   buffer cleared) changes nothing.
//!
//! The module never runs the model, so it has no ONNX Runtime dependency
//! and tests inject the scores. The detector hands each request out
//! ([`ServerVad::take_score`]); the session scores the span from its
//! 16 kHz ring on the blocking pool and answers ([`ServerVad::scored`]).
//!
//! [`ServerVad`]: super::server_vad::ServerVad
//! [`ServerVad::take_score`]: super::server_vad::ServerVad::take_score
//! [`ServerVad::scored`]: super::server_vad::ServerVad::scored

use std::fmt;

use super::mel::WINDOW_SECONDS;

/// Silence before a pause is scored (§6.3, "after 200 ms of silence"). An
/// algorithm constant: the measured rule scored there, and its audio
/// includes exactly this much of the pause.
pub const PROBE_AFTER_MS: u32 = 200;

/// The least audio a pause is scored on (fix package B6): the
/// [`PROBE_AFTER_MS`] of the pause and as much again before it. Smart Turn
/// pads a shorter span with zeros on the left to its 8 s, and near-silence
/// scores as a finished turn; such a pause gets no score and falls back to
/// the plain silence window (`scorer`). An algorithm constant.
pub const MIN_SCORED_MS: u32 = 2 * PROBE_AFTER_MS;

/// The rule's knobs for one session — the settings' row for its eagerness
/// (`realtime.semantic_vad`, §12) plus the floor window.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticParams {
    /// A score at or above this commits at once, `0.0..=1.0`.
    pub threshold: f32,
    /// A score at or above this (and below the threshold) commits at the
    /// floor window; `floor == threshold` turns the middle band off.
    pub floor: f32,
    /// Where a score between the floor and the threshold commits
    /// (`realtime.semantic_floor_window_ms`).
    pub floor_window_ms: u32,
    /// Where any pause commits at the latest.
    pub max_wait_ms: u32,
}

/// Why the rule's parameters are refused.
#[derive(Debug, Clone, PartialEq)]
pub enum SemanticParamError {
    /// A threshold or floor outside `0.0..=1.0`, or not a number.
    Range(&'static str, f32),
    /// The floor above the threshold.
    FloorAboveThreshold { floor: f32, threshold: f32 },
    /// The floor window past the maximum wait (fix package B6).
    FloorWindowAboveMaxWait {
        floor_window_ms: u32,
        max_wait_ms: u32,
    },
}

impl fmt::Display for SemanticParamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Range(what, v) => write!(
                f,
                "semantic_vad {what} {v} is not in 0.0..=1.0 (realtime.semantic_vad)"
            ),
            Self::FloorAboveThreshold { floor, threshold } => write!(
                f,
                "semantic_vad floor {floor} is above its threshold {threshold} \
                 (realtime.semantic_vad; equal turns the floor off)"
            ),
            Self::FloorWindowAboveMaxWait {
                floor_window_ms,
                max_wait_ms,
            } => write!(
                f,
                "semantic_vad floor window {floor_window_ms} ms is above its max wait \
                 {max_wait_ms} ms (realtime.semantic_floor_window_ms, realtime.semantic_vad)"
            ),
        }
    }
}

impl SemanticParams {
    pub fn validate(&self) -> Result<(), SemanticParamError> {
        for (what, v) in [("threshold", self.threshold), ("floor", self.floor)] {
            if !(0.0..=1.0).contains(&v) {
                return Err(SemanticParamError::Range(what, v));
            }
        }
        if self.floor > self.threshold {
            return Err(SemanticParamError::FloorAboveThreshold {
                floor: self.floor,
                threshold: self.threshold,
            });
        }
        if self.floor_window_ms > self.max_wait_ms {
            return Err(SemanticParamError::FloorWindowAboveMaxWait {
                floor_window_ms: self.floor_window_ms,
                max_wait_ms: self.max_wait_ms,
            });
        }
        Ok(())
    }
}

/// Score the span `start..end` of the input timeline: the turn's last 8 s,
/// up to [`PROBE_AFTER_MS`] into the pause. Answer with `id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoreRequest {
    pub id: u64,
    pub start: u64,
    pub end: u64,
}

/// Which part of the rule ended a turn — the §11 timing line names it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EndRule {
    /// The score reached the threshold.
    Threshold { p: f32 },
    /// The score was between the floor and the threshold: the floor window.
    Floor { p: f32 },
    /// The maximum wait: a score below the floor, or none had come yet.
    MaxWait { p: Option<f32> },
    /// No score could be had: the plain silence window.
    Fallback,
}

/// How a `semantic_vad` turn ended: the rule, and whether the
/// post-interrupt window (§6.5) held it past where the rule would have
/// committed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurnEnd {
    pub rule: EndRule,
    pub held: bool,
}

impl fmt::Display for TurnEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.rule {
            EndRule::Threshold { p } => write!(f, "semantic_vad threshold, p {p:.2}")?,
            EndRule::Floor { p } => write!(f, "semantic_vad floor window, p {p:.2}")?,
            EndRule::MaxWait { p: Some(p) } => write!(f, "semantic_vad max wait, p {p:.2}")?,
            EndRule::MaxWait { p: None } => write!(f, "semantic_vad max wait, no score yet")?,
            EndRule::Fallback => write!(f, "semantic_vad silence fallback, no score")?,
        }
        if self.held {
            write!(f, "; held by post_interrupt_silence_ms")?;
        }
        Ok(())
    }
}

/// What an answer decided, for the session's DEBUG line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// At or above the threshold: the turn ends now.
    Now,
    /// Between floor and threshold: at this much silence (ms).
    FloorWindow(u32),
    /// Below the floor: at the maximum wait (ms), unless voice resumes.
    MaxWait(u32),
    /// No score: at the plain silence window.
    Fallback,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Probe {
    /// Not asked yet.
    Waiting,
    /// Asked; no answer yet.
    Pending,
    Scored(f32),
    Failed,
}

#[derive(Debug, Clone, Copy)]
struct Pause {
    id: u64,
    /// Where the pause began on the timeline.
    start: u64,
    probe: Probe,
}

/// The rule's state inside one detector: the open pause and its score.
#[derive(Debug)]
pub(super) struct Semantic {
    params: SemanticParams,
    probe_after: u64,
    floor_window: u64,
    max_wait: u64,
    /// Smart Turn's input, 8 s, in samples.
    window: u64,
    rate: u64,
    pause: Option<Pause>,
    request: Option<ScoreRequest>,
}

impl Semantic {
    pub fn new(params: &SemanticParams, rate: u64) -> Result<Self, SemanticParamError> {
        params.validate()?;
        let samples = |ms: u32| u64::from(ms) * rate / 1000;
        Ok(Self {
            params: params.clone(),
            probe_after: samples(PROBE_AFTER_MS),
            floor_window: samples(params.floor_window_ms),
            max_wait: samples(params.max_wait_ms),
            window: u64::from(WINDOW_SECONDS) * rate,
            rate,
            pause: None,
            request: None,
        })
    }

    /// New knobs from a `session.update`; the pause and its score are kept
    /// — the new thresholds apply to it from here.
    pub fn set_params(&mut self, params: &SemanticParams) -> Result<(), SemanticParamError> {
        let next = Self::new(params, self.rate)?;
        self.params = next.params;
        self.floor_window = next.floor_window;
        self.max_wait = next.max_wait;
        Ok(())
    }

    /// An unvoiced frame ending at `end` left `silence` samples of silence
    /// in the turn that starts at `turn_start`. `ask`: whether the pause
    /// may be scored now — not while the barge-in word check keeps the turn
    /// open (§6.4); its request then comes on the first frame after. `ids`:
    /// the detector's request counter, which outlives this rule — a switch
    /// to `server_vad` and back makes a new one (WP6 review), and a stale
    /// answer must still match no pause of it.
    pub fn on_silence(
        &mut self,
        end: u64,
        silence: u64,
        turn_start: u64,
        ask: bool,
        ids: &mut u64,
    ) {
        let pause = self.pause.get_or_insert_with(|| {
            let id = *ids;
            *ids = ids.wrapping_add(1);
            Pause {
                id,
                start: end.saturating_sub(silence),
                probe: Probe::Waiting,
            }
        });
        if ask && pause.probe == Probe::Waiting && silence >= self.probe_after {
            pause.probe = Probe::Pending;
            let to = pause.start + self.probe_after;
            self.request = Some(ScoreRequest {
                id: pause.id,
                start: turn_start.max(to.saturating_sub(self.window)),
                end: to,
            });
        }
    }

    /// The voice resumed (or the turn ended): no pause, and any answer
    /// still to come is stale.
    pub fn clear(&mut self) {
        self.pause = None;
        self.request = None;
    }

    pub fn take_request(&mut self) -> Option<ScoreRequest> {
        self.request.take()
    }

    /// Where the open pause began on the timeline.
    pub fn pause_start(&self) -> Option<u64> {
        self.pause.map(|p| p.start)
    }

    /// Request `id` answered: `Some(p)`, or `None` when it could not be
    /// scored (a non-finite `p` counts as none). What it decided; `None`
    /// for a stale or repeated answer.
    pub fn answer(&mut self, id: u64, p: Option<f32>) -> Option<Decision> {
        let pause = self.pause.as_mut()?;
        if pause.id != id || pause.probe != Probe::Pending {
            return None;
        }
        pause.probe = match p.filter(|p| p.is_finite()) {
            Some(p) => Probe::Scored(p),
            None => Probe::Failed,
        };
        let ms = |samples: u64| (samples * 1000 / self.rate) as u32;
        Some(match pause.probe {
            Probe::Scored(p) if p >= self.params.threshold => Decision::Now,
            Probe::Scored(p) if p >= self.params.floor => {
                Decision::FloorWindow(ms(self.floor_window))
            }
            Probe::Scored(_) => Decision::MaxWait(ms(self.max_wait)),
            _ => Decision::Fallback,
        })
    }

    /// The silence the open pause ends the turn at, in samples; `None`:
    /// the plain window (no score could be had).
    pub fn window(&self) -> Option<u64> {
        let Some(pause) = self.pause else {
            return Some(self.max_wait);
        };
        match pause.probe {
            Probe::Waiting | Probe::Pending => Some(self.max_wait),
            Probe::Scored(p) if p >= self.params.threshold => Some(self.probe_after),
            Probe::Scored(p) if p >= self.params.floor => Some(self.floor_window),
            Probe::Scored(_) => Some(self.max_wait),
            Probe::Failed => None,
        }
    }

    /// The part of the rule that ends the open pause (`window`'s reason).
    pub fn rule(&self) -> EndRule {
        match self.pause.map(|p| p.probe) {
            Some(Probe::Scored(p)) if p >= self.params.threshold => EndRule::Threshold { p },
            Some(Probe::Scored(p)) if p >= self.params.floor => EndRule::Floor { p },
            Some(Probe::Scored(p)) => EndRule::MaxWait { p: Some(p) },
            Some(Probe::Failed) => EndRule::Fallback,
            Some(Probe::Waiting | Probe::Pending) | None => EndRule::MaxWait { p: None },
        }
    }
}

#[cfg(test)]
mod tests;
