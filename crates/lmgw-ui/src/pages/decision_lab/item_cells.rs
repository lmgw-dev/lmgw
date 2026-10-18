//! What one item's cell in the set run's table says for one question
//! (labs-store design §3.4): the answer and its confidence, the expected
//! answer and whether the answer is correct. Whether a run is pending,
//! failed, unreadable or answered, and what an answer predicts, is the
//! server's own rule (`lmgw_api_types::decision_lab::metrics::outcome`),
//! which its metrics, its `readable` count and its export call too: a run
//! that is no 2xx is failed, a 2xx whose text is no answer of the
//! question's type (or names a class the question lacks, or lacks the
//! predicted class's probability) is unreadable. The confidence a threshold
//! cuts on is the answer's `confidence`, or for `noul` the certainty
//! max(p, 1 − p).

use lmgw_api_types::decision_lab::metrics::{outcome, read_response, Outcome};
use lmgw_api_types::decision_lab::{QuestionDraft, QuestionKind};
use lmgw_api_types::lab_store::LabRunView;
use lmgw_api_types::systemone::SystemOneResponse;

use super::answers::compact;

/// A label as kept (an option's name, a level's index, `yes` or `no`) as the
/// table shows it: a score's level by its text.
pub(super) fn label_text(q: &QuestionDraft, label: &str) -> String {
    match &q.kind {
        QuestionKind::Score { levels } => label
            .parse::<usize>()
            .ok()
            .and_then(|i| levels.get(i).map(|t| format!("{i} · {t}")))
            .unwrap_or_else(|| label.to_string()),
        _ => label.to_string(),
    }
}

/// An item's run as every question of its row reads it: the status the
/// rule takes and the answer parsed once (a 2xx's only).
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ReadRun {
    /// `None`: no run yet.
    pub status: Option<i64>,
    pub response: Option<SystemOneResponse>,
}

impl ReadRun {
    pub fn of(run: Option<&LabRunView>) -> Self {
        let status = run.map(|r| i64::from(r.http_status));
        Self {
            status,
            response: read_response(status, run.and_then(|r| r.response.as_deref())),
        }
    }
}

/// One cell of the items table.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Cell {
    /// No run yet.
    Pending,
    /// The run is no 2xx.
    Failed,
    /// A 2xx without a readable answer to this question.
    Unreadable,
    Answered {
        /// The compact answer ("billing 81%", "2.37 · today", "P(yes) 0.82").
        text: String,
        confidence: f64,
        /// The expected answer as shown, and whether the answer is it.
        expected: Option<(String, bool)>,
    },
}

/// Question `q`'s cell for an item whose run reads as `run`, with the
/// item's label `expected`.
pub(super) fn cell(q: &QuestionDraft, run: &ReadRun, expected: Option<&str>) -> Cell {
    match outcome(q, run.status, run.response.as_ref()) {
        Outcome::Pending => Cell::Pending,
        Outcome::Failed => Cell::Failed,
        Outcome::Unreadable => Cell::Unreadable,
        Outcome::Answered(p) => Cell::Answered {
            text: run
                .response
                .as_ref()
                .and_then(|r| r.answers.get(&q.id))
                .map(compact)
                .unwrap_or_default(),
            confidence: p.confidence,
            expected: expected.map(|l| (label_text(q, l), p.class == l)),
        },
    }
}

/// An expected answer of a cell without an answer to judge it against.
pub(super) fn expected_only(q: &QuestionDraft, expected: Option<&str>) -> Option<String> {
    expected.map(|l| label_text(q, l))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::decision_lab::examples;

    fn triage() -> Vec<QuestionDraft> {
        examples().into_iter().next().unwrap().form.questions
    }

    fn run(status: u16, response: Option<&str>) -> LabRunView {
        serde_json::from_value(serde_json::json!({
            "id": 1, "group_id": 1, "cell": 0, "variant": "as_written",
            "endpoint": "/v1/systemone", "model_asked": "m", "answered_by": "m",
            "fallback_reason": null, "upstream_model": null, "http_status": status,
            "error_code": null, "error_message": null, "request": "{}",
            "response": response, "response_type": null, "headers": {},
            "input_tokens": null, "output_tokens": null, "latency_ms": 1,
            "created_at": "", "artifacts": []
        }))
        .unwrap()
    }

    const ANSWERS: &str = r#"{"model": "m", "usage": {"input_tokens": 1, "output_tokens": 0}, "answers": {
        "team": {"type": "choice", "choice": "billing", "probabilities": {"billing": 0.81, "technical": 0.19}, "confidence": 0.7},
        "urgency": {"type": "score", "score": 2.4, "legend": {"2": "today"}, "probabilities": {"0": 0.1, "1": 0.1, "2": 0.5, "3": 0.3}, "confidence": 0.5},
        "needs_human": {"type": "noul", "noul": 0.3}}}"#;

    #[test]
    fn a_cell_reads_its_answer_and_marks_the_label() {
        let qs = triage();
        let r = ReadRun::of(Some(&run(200, Some(ANSWERS))));
        match cell(&qs[0], &r, Some("technical")) {
            Cell::Answered {
                text,
                confidence,
                expected,
            } => {
                assert_eq!(text, "billing 81%");
                assert_eq!(confidence, 0.7);
                assert_eq!(expected, Some(("technical".into(), false)));
            }
            other => panic!("{other:?}"),
        }
        match cell(&qs[1], &r, Some("2")) {
            Cell::Answered { expected, .. } => {
                assert_eq!(expected, Some(("2 · today".into(), true)));
            }
            other => panic!("{other:?}"),
        }
        match cell(&qs[2], &r, Some("no")) {
            Cell::Answered {
                confidence,
                expected,
                ..
            } => {
                assert_eq!(confidence, 0.7, "the certainty max(p, 1 − p)");
                assert_eq!(expected, Some(("no".into(), true)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn failed_and_unreadable_are_the_servers_reading() {
        let qs = triage();
        assert_eq!(
            cell(&qs[0], &ReadRun::of(None), Some("billing")),
            Cell::Pending
        );
        // Any 2xx reads, 200 is not special.
        assert!(matches!(
            cell(&qs[0], &ReadRun::of(Some(&run(203, Some(ANSWERS)))), None),
            Cell::Answered { .. }
        ));
        for failed in [404, 429, 500, 503] {
            assert_eq!(
                cell(
                    &qs[0],
                    &ReadRun::of(Some(&run(failed, Some(ANSWERS)))),
                    None
                ),
                Cell::Failed,
                "{failed}"
            );
        }
        // A 2xx whose text is no answer is unreadable, not failed.
        assert_eq!(
            cell(&qs[0], &ReadRun::of(Some(&run(200, Some("<html>")))), None),
            Cell::Unreadable
        );
        assert_eq!(
            cell(&qs[0], &ReadRun::of(Some(&run(200, None))), None),
            Cell::Unreadable
        );
        // A winner the question does not offer; a level without its
        // probability; another type.
        let odd = r#"{"model": "m", "usage": {"input_tokens": 1, "output_tokens": 0}, "answers": {
            "team": {"type": "choice", "choice": "sales", "probabilities": {"sales": 1.0}, "confidence": 1.0},
            "urgency": {"type": "score", "score": 1.2, "legend": {}, "probabilities": {"0": 0.5}, "confidence": 0.5},
            "needs_human": {"type": "choice", "choice": "billing", "probabilities": {"billing": 1.0}, "confidence": 1.0}}}"#;
        let r = ReadRun::of(Some(&run(200, Some(odd))));
        for q in &qs {
            assert_eq!(cell(q, &r, None), Cell::Unreadable, "{}", q.id);
        }
        assert_eq!(
            expected_only(&qs[1], Some("3")).as_deref(),
            Some("3 · right now")
        );
    }
}
