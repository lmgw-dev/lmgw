//! Smart Turn's answers in the session core (realtime design §6.3): each
//! score the detector asked for (`audio_in`, `scorer`) comes back here, is
//! handed to the detector — which may end the turn now — and is logged.
//!
//! - **One DEBUG line per score**: the probability, where the pause began,
//!   how long the score took, and what it decided.
//! - **A pause that could not be scored** — the model did not load, the run
//!   failed, or the answer was not a number — falls back to the plain
//!   silence window, `semantic_vad` as it was before Smart Turn. That is
//!   said at WARN once per session; every later one at DEBUG. A pause with
//!   too little audio to score, or one a newer pause overtook (`scorer`),
//!   is no failure: its DEBUG line says why.

use super::super::scorer::{ScoreDone, Unscored};
use super::super::session::Core;
use super::super::turn::semantic::{Decision, MIN_SCORED_MS};

impl Core {
    /// A score's answer (module doc).
    pub(in crate::realtime) fn on_turn_score(&mut self, done: ScoreDone) {
        let p = match &done.result {
            Ok(p) if p.is_finite() => Some(*p),
            Ok(p) => {
                self.warn_unscored(&format!("it answered {p}, not a probability"));
                None
            }
            Err(Unscored::Failed(e)) => {
                self.warn_unscored(e);
                None
            }
            // Not the model's failure: the plain window, at DEBUG below.
            Err(Unscored::Short(_) | Unscored::Overtaken) => None,
        };
        let (decision, appended) = self.input.turn_scored(done.id, p);
        let what = match decision {
            Some(Decision::Now) => "commits now (at or above the threshold)".to_string(),
            Some(Decision::FloorWindow(ms)) => {
                format!("commits at {ms} ms of silence (between the floor and the threshold)")
            }
            Some(Decision::MaxWait(ms)) => format!(
                "below the floor: waits, and commits at {ms} ms of silence unless speech resumes"
            ),
            Some(Decision::Fallback) => "no score: commits at the plain silence window".into(),
            None => {
                "its pause is over (speech resumed, or the turn ended) — it decides nothing".into()
            }
        };
        let score = match (&done.result, p) {
            (_, Some(p)) => format!("p {p:.3}"),
            (Err(Unscored::Short(ms)), _) => {
                format!("no score (only {ms} ms of audio, under the {MIN_SCORED_MS} ms it needs)")
            }
            (Err(Unscored::Overtaken), _) => {
                "no score (a newer pause's request overtook it)".to_string()
            }
            _ => "no score".to_string(),
        };
        tracing::debug!(
            "realtime {}: Smart Turn {score} for the pause from {} ms of the input ({} ms) — \
             {what}",
            self.id(),
            done.pause_ms,
            done.took.as_millis()
        );
        self.on_appended(appended, None, None);
    }

    /// Once per session (module doc).
    fn warn_unscored(&mut self, why: &str) {
        if std::mem::replace(&mut self.score_warned, true) {
            return tracing::debug!(
                "realtime {}: Smart Turn could not score a pause ({why})",
                self.id()
            );
        }
        tracing::warn!(
            "realtime {}: Smart Turn could not score a pause ({why}); semantic_vad commits every \
             pause it cannot score at the plain silence window of its eagerness \
             (realtime.semantic_vad.<eagerness>.silence_duration_ms), as before Smart Turn — \
             said once per session",
            self.id()
        );
    }
}
