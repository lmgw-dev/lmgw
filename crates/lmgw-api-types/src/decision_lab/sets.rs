//! Question sets (labs-store design §3.2): a named list of questions, the
//! model it is meant for, and one threshold per question, measured on a
//! labelled set run or not yet. A set is the Decision lab's preset of kind
//! `question_set` (`lab_store::presets`).

use serde::{Deserialize, Serialize};

use super::form::{check_questions, QuestionDraft};
use crate::systemone::Ordered;

/// Where a threshold was measured: a set run, and what the cut gave there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Measured {
    /// The set run it was measured on.
    pub batch_id: i64,
    /// The share of the run's answered items at or above the threshold.
    pub coverage: f64,
    /// Of those, the share of the labelled ones answered correctly. A cut
    /// with no labelled item at or above it is not written (409
    /// `no_labels`).
    pub accuracy: f64,
    /// How many labelled items the accuracy is over: at least 1.
    pub labelled: u64,
}

/// A question's threshold: an answer whose confidence (`choice`, `score`)
/// or certainty, max(p, 1 − p) (`noul`), is below it needs attention.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Threshold {
    /// From 0 to 1. 0 is "never needs attention", what an unmeasured
    /// question has.
    pub value: f64,
    /// Where the value was measured; `null`: not measured.
    pub measured: Option<Measured>,
}

/// A saved list of questions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct QuestionSet {
    /// In the order asked.
    pub questions: Vec<QuestionDraft>,
    /// The model loading the set picks; blank picks none.
    pub default_model: String,
    /// Per question id; a question with none has 0, not measured.
    pub thresholds: Ordered<Threshold>,
}

impl QuestionSet {
    /// The questions as [`check_questions`] checks a form's, and every
    /// threshold naming one of them, between 0 and 1. The message names the
    /// field.
    pub fn check(&self) -> Result<(), String> {
        check_questions(&self.questions)?;
        for (id, t) in self.thresholds.iter() {
            if !self.questions.iter().any(|q| q.id == id) {
                return Err(format!("thresholds: '{id}' is not a question of the set"));
            }
            if !(0.0..=1.0).contains(&t.value) {
                return Err(format!(
                    "thresholds: '{id}' is {}, not between 0 and 1",
                    t.value
                ));
            }
        }
        Ok(())
    }

    /// Question `id`'s threshold value: 0 when it has none.
    pub fn threshold(&self, id: &str) -> f64 {
        self.thresholds.get(id).map_or(0.0, |t| t.value)
    }

    /// Question `id`.
    pub fn question(&self, id: &str) -> Option<&QuestionDraft> {
        self.questions.iter().find(|q| q.id == id)
    }

    /// This set as an update of `before`: a threshold whose question is not
    /// `before`'s question of that id, whole (its type, options or levels,
    /// its text), or whose value is not the stored one, loses its
    /// measurement. Its value stays, and reads "not measured": a measurement
    /// holds for the question and the value it was taken at.
    pub fn unmeasure_changed(&mut self, before: &QuestionSet) {
        let questions = &self.questions;
        for (id, t) in self.thresholds.0.iter_mut() {
            let now = questions.iter().find(|q| q.id == *id);
            if now.is_none()
                || now != before.question(id)
                || before.thresholds.get(id).map(|b| b.value) != Some(t.value)
            {
                t.measured = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision_lab::{OptionDraft, QuestionKind};

    fn set() -> QuestionSet {
        QuestionSet {
            questions: vec![QuestionDraft {
                id: "team".into(),
                instructions: "Which team?".into(),
                kind: QuestionKind::Choice {
                    options: vec![OptionDraft {
                        name: "billing".into(),
                        description: String::new(),
                    }],
                },
            }],
            default_model: "decide/laya".into(),
            thresholds: Ordered::default(),
        }
    }

    #[test]
    fn a_threshold_names_a_question_and_lies_between_0_and_1() {
        let mut s = set();
        assert_eq!(s.check(), Ok(()));
        assert_eq!(s.threshold("team"), 0.0, "unmeasured");
        s.thresholds = [(
            "team",
            Threshold {
                value: 0.72,
                measured: None,
            },
        )]
        .into_iter()
        .collect();
        assert_eq!(s.check(), Ok(()));
        assert_eq!(s.threshold("team"), 0.72);
        s.thresholds.0[0].1.value = 1.5;
        assert!(s.check().unwrap_err().contains("not between 0 and 1"));
        s.thresholds.0[0].0 = "urgency".into();
        s.thresholds.0[0].1.value = 0.5;
        assert!(s
            .check()
            .unwrap_err()
            .contains("'urgency' is not a question"));
        s.questions.clear();
        assert_eq!(s.check().unwrap_err(), "questions: add at least one");
    }

    #[test]
    fn an_update_that_changes_a_question_drops_its_measurement_and_keeps_its_value() {
        let measured = |batch_id| Threshold {
            value: 0.7,
            measured: Some(Measured {
                batch_id,
                coverage: 0.8,
                accuracy: 0.95,
                labelled: 40,
            }),
        };
        let mut before = set();
        before.questions.push(QuestionDraft {
            id: "angry".into(),
            instructions: "Upset?".into(),
            kind: QuestionKind::Noul {
                yes: String::new(),
                no: String::new(),
            },
        });
        before.thresholds = [("team", measured(1)), ("angry", measured(2))]
            .into_iter()
            .collect();
        let mut after = before.clone();
        after.unmeasure_changed(&before);
        assert_eq!(after, before, "nothing changed: both stay measured");

        // A new option is a new question; so is new text.
        if let QuestionKind::Choice { options } = &mut after.questions[0].kind {
            options.push(OptionDraft {
                name: "technical".into(),
                description: String::new(),
            });
        }
        after.unmeasure_changed(&before);
        let team = after.thresholds.get("team").unwrap();
        assert_eq!((team.value, &team.measured), (0.7, &None));
        assert!(after.thresholds.get("angry").unwrap().measured.is_some());
        after.questions[1].instructions = "Is the customer upset?".into();
        after.unmeasure_changed(&before);
        assert!(after.thresholds.get("angry").unwrap().measured.is_none());

        // A moved threshold is not the one that was measured.
        let mut moved = before.clone();
        moved.thresholds.0[0].1.value = 0.9;
        moved.unmeasure_changed(&before);
        let team = moved.thresholds.get("team").unwrap();
        assert_eq!((team.value, &team.measured), (0.9, &None));
        assert!(moved.thresholds.get("angry").unwrap().measured.is_some());
    }
}
