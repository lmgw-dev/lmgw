//! The Decision lab's built-in examples (labs-store design §2.8), offered in
//! its Examples menu. English, because Laya is English-only.

use super::form::{DecisionForm, OptionDraft, QuestionDraft, QuestionKind, StateMode};

/// One example: what the menu calls it, and the form it loads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Example {
    pub name: &'static str,
    pub form: DecisionForm,
}

fn choice(id: &str, instructions: &str, options: &[(&str, &str)]) -> QuestionDraft {
    QuestionDraft {
        id: id.into(),
        instructions: instructions.into(),
        kind: QuestionKind::Choice {
            options: options
                .iter()
                .map(|(name, description)| OptionDraft {
                    name: (*name).into(),
                    description: (*description).into(),
                })
                .collect(),
        },
    }
}

fn score(id: &str, instructions: &str, levels: &[&str]) -> QuestionDraft {
    QuestionDraft {
        id: id.into(),
        instructions: instructions.into(),
        kind: QuestionKind::Score {
            levels: levels.iter().map(|l| (*l).into()).collect(),
        },
    }
}

fn noul(id: &str, instructions: &str, yes: &str, no: &str) -> QuestionDraft {
    QuestionDraft {
        id: id.into(),
        instructions: instructions.into(),
        kind: QuestionKind::Noul {
            yes: yes.into(),
            no: no.into(),
        },
    }
}

/// Ticket triage, sentiment, and "is the user done talking?".
pub fn examples() -> Vec<Example> {
    vec![
        Example {
            name: "Ticket triage",
            form: DecisionForm {
                state_mode: StateMode::Text,
                state: "Hi, I was charged twice for order A-1001 this month. The account is \
                        jane@shop.example. Please refund one of the two payments."
                    .into(),
                images: Vec::new(),
                questions: vec![
                    choice(
                        "team",
                        "Which team should handle this ticket?",
                        &[
                            ("billing", "payments, refunds, invoices"),
                            ("technical", "bugs, errors, outages"),
                            ("account", "sign-in, profile, access"),
                            ("other", ""),
                        ],
                    ),
                    score(
                        "urgency",
                        "How urgent is this ticket?",
                        &["can wait a week", "within a few days", "today", "right now"],
                    ),
                    noul(
                        "needs_human",
                        "Does a person have to handle this ticket?",
                        "a person must decide or act",
                        "a help-centre answer resolves it",
                    ),
                ],
            },
        },
        Example {
            name: "Sentiment",
            form: DecisionForm {
                state_mode: StateMode::Text,
                state: "The kettle boils fast and looks great, but the lid started to stick \
                        after two weeks and the handle gets hot."
                    .into(),
                images: Vec::new(),
                questions: vec![
                    choice(
                        "sentiment",
                        "What is the overall sentiment of this product review?",
                        &[("positive", ""), ("neutral", ""), ("negative", "")],
                    ),
                    score(
                        "intensity",
                        "How strongly is that sentiment expressed?",
                        &["barely", "mildly", "clearly", "very", "strongly"],
                    ),
                ],
            },
        },
        Example {
            name: "Is the user done talking?",
            form: DecisionForm {
                state_mode: StateMode::Json,
                state: r#"[{"role": "assistant", "content": "Which day suits you?"}, {"role": "user", "content": "Thursday works, but only after"}]"#
                    .into(),
                images: Vec::new(),
                questions: vec![noul(
                    "done",
                    "Has the user finished their turn?",
                    "a complete thought that expects a reply",
                    "it breaks off or announces more",
                )],
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision_lab::form::Variant;
    use crate::systemone::SystemOneRequest;

    /// Each example passes the form's check and builds a body that reads as
    /// the wire's request, in every variant.
    #[test]
    fn every_example_builds_a_system_one_request() {
        let all = examples();
        assert_eq!(all.len(), 3);
        for ex in all {
            ex.form
                .check()
                .unwrap_or_else(|e| panic!("{}: {e}", ex.name));
            for v in Variant::ALL {
                let body = ex.form.body("decide/laya", v, &[]).unwrap();
                let req: SystemOneRequest = serde_json::from_str(&body)
                    .unwrap_or_else(|e| panic!("{} ({v:?}): {e}\n{body}", ex.name));
                assert_eq!(req.model, "decide/laya");
                assert_eq!(req.questions.len(), ex.form.questions.len());
            }
        }
    }
}
