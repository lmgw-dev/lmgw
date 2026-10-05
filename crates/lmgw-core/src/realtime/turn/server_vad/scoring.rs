//! The detector's side of `semantic_vad` (realtime design §6.3): the score
//! requests its pauses make, the answers that may end the turn between two
//! frames, and the record of which part of the rule ended it. The rule
//! itself is `super::super::semantic`.

use super::super::semantic::{Decision, ScoreRequest, TurnEnd};
use super::{ServerVad, State, TurnEvent};

impl ServerVad {
    /// The score request the last frame made, if any: at most one per
    /// pause, and none while the word check keeps the turn open (§6.4).
    pub fn take_score(&mut self) -> Option<ScoreRequest> {
        self.semantic.as_mut()?.take_request()
    }

    /// Request `id`'s answer: `Some(p)`, or `None` when the pause could not
    /// be scored. What it decided — `None` for a stale or repeated answer —
    /// and the turn's end when the answer ends it now: the silence it
    /// already has reaches the window the answer chose, and the last frame
    /// was unvoiced.
    ///
    /// After voiced frames too few to resume the turn (`resume_ms`), the
    /// answer ends nothing yet (WP6 review M2): ended there, the commit
    /// would fall inside a word that may be resuming, split it, and its
    /// second half would barge into the first half's answer. The next
    /// unvoiced frame commits then, or enough voice clears the pause.
    pub fn scored(&mut self, id: u64, p: Option<f32>) -> (Option<Decision>, Option<TurnEvent>) {
        let Some(decision) = self.semantic.as_mut().and_then(|s| s.answer(id, p)) else {
            return (None, None);
        };
        let quiet = matches!(self.state, State::Speech { voiced_run: 0, .. });
        let ended = (quiet && self.stop_due() && !self.keep_open)
            .then(|| self.end_turn())
            .flatten();
        (Some(decision), ended)
    }

    /// How the last turn ended, when the `semantic_vad` rule ended it — for
    /// the §11 timing line; taken once.
    pub fn take_end(&mut self) -> Option<TurnEnd> {
        self.last_end.take()
    }

    /// The rule's reason for ending the open pause now, `None` without
    /// `semantic_vad` or while the word check keeps the turn open.
    pub(super) fn turn_end(&self) -> Option<TurnEnd> {
        let s = self.semantic.as_ref().filter(|_| !self.keep_open)?;
        let own = s.window().unwrap_or(self.silence);
        Some(TurnEnd {
            rule: s.rule(),
            held: self.post_armed && self.post_silence > own,
        })
    }

    /// Whether this detector runs `semantic_vad`'s rule.
    pub fn semantic(&self) -> bool {
        self.semantic.is_some()
    }

    /// Where the open pause began on the timeline, while `semantic_vad`'s
    /// rule has one: what its score request may still reach back from.
    pub fn pause_start(&self) -> Option<u64> {
        self.semantic.as_ref()?.pause_start()
    }
}
