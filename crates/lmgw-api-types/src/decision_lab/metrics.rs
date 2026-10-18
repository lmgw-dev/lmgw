//! A set run's metrics (labs-store design §3.4), computed from its stored
//! runs per question: counts, accuracy against the labels, the confusion
//! matrix, the reliability bins and the full threshold curve.
//!
//! What one answer predicts and whether a run is failed, unreadable or
//! answered ([`predict`], [`outcome`]) is one rule for the server's metrics,
//! its `readable` judgement and its CSV export, and for the page's items
//! table: pure over these types, as [`super::batch::read_label`] is.

use serde::{Deserialize, Serialize};

use super::form::{QuestionDraft, QuestionKind};
use crate::systemone::{Answer, SystemOneResponse};

/// What one answer predicts.
#[derive(Debug, Clone, PartialEq)]
pub struct Prediction {
    /// The class, spelled as a label: the winner (`choice`), the nearest
    /// level's index (`score`), `yes` when P(yes) ≥ 0.5 else `no` (`noul`).
    pub class: String,
    /// The predicted probability the reliability bins sort by: the winner's
    /// (`choice`), the nearest level's (`score`), P(yes) (`noul`).
    pub probability: f64,
    /// What a threshold cuts on: the answer's `confidence` (`choice`,
    /// `score`), the certainty max(p, 1 − p) (`noul`).
    pub confidence: f64,
    /// A `score`'s expected level, fractional.
    pub score: Option<f64>,
}

/// One item's outcome for one question.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// No run yet.
    Pending,
    /// The run is no 2xx.
    Failed,
    /// The run is a 2xx without a readable answer to the question.
    Unreadable,
    Answered(Prediction),
}

/// The classes of `q` in matrix order, spelled as labels.
pub fn classes(q: &QuestionDraft) -> Vec<String> {
    match &q.kind {
        QuestionKind::Choice { options } => options.iter().map(|o| o.name.clone()).collect(),
        QuestionKind::Score { levels } => (0..levels.len()).map(|i| i.to_string()).collect(),
        QuestionKind::Noul { .. } => vec!["yes".into(), "no".into()],
    }
}

/// The certainty of a `noul` answer, the lab's own measure (Decision 26).
pub fn certainty(p_yes: f64) -> f64 {
    p_yes.max(1.0 - p_yes)
}

/// What `a` predicts for `q`; `None` when it is no answer of `q`'s type,
/// names a class `q` does not have, or lacks the probability of the class
/// it predicts.
pub fn predict(q: &QuestionDraft, a: &Answer) -> Option<Prediction> {
    match (&q.kind, a) {
        (QuestionKind::Choice { options }, Answer::Choice(c)) => {
            if !options.iter().any(|o| o.name == c.choice) {
                return None;
            }
            Some(Prediction {
                class: c.choice.clone(),
                probability: *c.probabilities.get(&c.choice)?,
                confidence: c.confidence,
                score: None,
            })
        }
        (QuestionKind::Score { levels }, Answer::Score(s)) => {
            if levels.is_empty() || !s.score.is_finite() {
                return None;
            }
            let nearest = s.score.round().clamp(0.0, (levels.len() - 1) as f64) as usize;
            let class = nearest.to_string();
            Some(Prediction {
                probability: *s.probabilities.get(&class)?,
                class,
                confidence: s.confidence,
                score: Some(s.score),
            })
        }
        (QuestionKind::Noul { .. }, Answer::Noul(n)) => {
            if !n.noul.is_finite() {
                return None;
            }
            Some(Prediction {
                class: if n.noul >= 0.5 { "yes" } else { "no" }.into(),
                probability: n.noul,
                confidence: certainty(n.noul),
                score: None,
            })
        }
        _ => None,
    }
}

/// Whether `status` is a 2xx: a run that is not is failed, everywhere.
pub fn is_2xx(status: i64) -> bool {
    (200..300).contains(&status)
}

/// A run's answer as the metrics read it: its stored `text` parsed when the
/// run answered `http_status` 2xx; `None` for no run, a failed one, or a 2xx
/// whose text is no System One answer (which [`outcome`] reads as
/// unreadable).
pub fn read_response(http_status: Option<i64>, text: Option<&str>) -> Option<SystemOneResponse> {
    http_status
        .filter(|s| is_2xx(*s))
        .and(text)
        .and_then(|t| serde_json::from_str(t).ok())
}

/// Question `q`'s outcome in a run that answered `http_status` with
/// `response` ([`read_response`]); `None` status is no run yet.
pub fn outcome(
    q: &QuestionDraft,
    http_status: Option<i64>,
    response: Option<&SystemOneResponse>,
) -> Outcome {
    let Some(status) = http_status else {
        return Outcome::Pending;
    };
    if !is_2xx(status) {
        return Outcome::Failed;
    }
    response
        .and_then(|r| r.answers.get(&q.id))
        .and_then(|a| predict(q, a))
        .map_or(Outcome::Unreadable, Outcome::Answered)
}

/// Whether `a` answers `q` as `label` says; `None` when it is no answer of
/// `q`.
pub fn correct(q: &QuestionDraft, a: &Answer, label: &str) -> Option<bool> {
    predict(q, a).map(|p| p.class == label)
}

/// How many reliability bins a request that names none gets.
pub const DEFAULT_BINS: u32 = 10;

/// The query of `GET /decision-lab/api/batches/{id}/metrics`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MetricsQuery {
    /// One question's id; absent, every question of the set.
    pub question: Option<String>,
    /// How many equal-width reliability bins over 0 to 1; absent,
    /// [`DEFAULT_BINS`]. At least 1, no maximum: only the bins that hold an
    /// answer are answered, so any number costs what the answers do.
    pub bins: Option<u32>,
}

/// One reliability bin that holds an answer: the labelled answers whose
/// predicted probability falls in `[lower, upper)` (the last bin includes
/// 1). `lower` is `index / bins` and `upper` `(index + 1) / bins`, the very
/// values an answer is placed by. A bin the list skips holds none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReliabilityBin {
    /// Its place among `bins`, from 0.
    pub index: u32,
    pub lower: f64,
    pub upper: f64,
    /// At least 1.
    pub count: u64,
    /// The mean predicted probability.
    pub mean_prediction: f64,
    /// The observed rate of what was predicted: the share whose winner
    /// (`choice`) or nearest level (`score`) is the label; for `noul`, the
    /// share labelled yes (the bins sort by P(yes)).
    pub observed: f64,
}

/// One point of the threshold curve: what a cut at `threshold` keeps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CurvePoint {
    /// A confidence (certainty for `noul`) some answer has.
    pub threshold: f64,
    /// Answered items at or above it.
    pub covered: u64,
    /// `covered` over every answered item.
    pub coverage: f64,
    /// Labelled items among `covered`.
    pub labelled: u64,
    /// Of those, the ones answered correctly.
    pub correct: u64,
    /// `correct` over `labelled`; `null` when none is labelled.
    pub accuracy: Option<f64>,
}

/// One question's metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct QuestionMetrics {
    pub id: String,
    /// `choice`, `score` or `noul`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The classes in matrix order, spelled as labels are kept: the options'
    /// names, the levels' indices from `"0"`, or `yes` and `no`.
    pub classes: Vec<String>,
    /// Items in the run.
    pub items: u64,
    /// Items whose run answered this question.
    pub answered: u64,
    /// Items whose run is no 2xx.
    pub failed: u64,
    /// Items whose run is a 2xx without a readable answer to this question
    /// (none of its type, a winner the question does not offer, the nearest
    /// level's probability missing).
    pub unreadable: u64,
    /// Items with an expected answer for this question.
    pub labelled: u64,
    /// Items both answered and labelled: what `accuracy` is over.
    pub scored: u64,
    /// The share of `scored` answered correctly: the winner (`choice`),
    /// P(yes) ≥ 0.5 (`noul`), the nearest level (`score`) equal to the
    /// label; `null` when nothing is scored.
    pub accuracy: Option<f64>,
    /// `score` only: the mean absolute distance, in levels, between the
    /// expected level (the fractional `score`) and the label.
    pub mae: Option<f64>,
    /// Rows the labels, columns the predictions, both in `classes` order.
    pub confusion: Vec<Vec<u64>>,
    /// The non-empty ones of `bins` equal-width bins over the predicted
    /// probability, lowest first: the winner's (`choice`), the nearest
    /// level's (`score`), P(yes) (`noul`). Labelled answers only.
    pub reliability: Vec<ReliabilityBin>,
    /// One point per distinct confidence (`choice`, `score`) or certainty,
    /// max(p, 1 − p) (`noul`), lowest first.
    pub curve: Vec<CurvePoint>,
}

/// What `GET /decision-lab/api/batches/{id}/metrics` answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatchMetrics {
    pub batch_id: i64,
    pub bins: u32,
    /// In the set's order.
    pub questions: Vec<QuestionMetrics>,
}

#[cfg(test)]
mod tests;
