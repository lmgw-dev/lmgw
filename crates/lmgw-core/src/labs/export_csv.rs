//! The Decision lab's flat table (labs-store design §1.7): one row per run
//! and question, a failed run one row with the question columns empty. A set
//! run's item (§3.4) is named by `batch_id`, `item_ix` and `item_key` (empty
//! for a group of the history), so a row can be matched back to its source
//! row, and has a row per question of its set whatever its run answered:
//! its label for the question as `expected`, and whether the answer matches
//! it as `correct`, judged as the metrics judge it (`labs::metrics`), empty
//! when the run failed or did not answer the question readably.

use lmgw_api_types::decision_lab::{DecisionForm, QuestionDraft};
use lmgw_api_types::lab_store::{LabGroupView, LabRunView};
use lmgw_api_types::systemone::{Answer, Ordered, SystemOneResponse};
use serde_json::value::RawValue;

use super::csv::csv_line;
use super::metrics;

/// The columns, in order.
pub const DECISION_COLUMNS: [&str; 22] = [
    "group_id",
    "batch_id",
    "item_ix",
    "item_key",
    "run_id",
    "created_at",
    "model_asked",
    "answered_by",
    "upstream_model",
    "variant",
    "http_status",
    "error_message",
    "latency_ms",
    "input_tokens",
    "question_id",
    "question_type",
    "answer",
    "expected",
    "correct",
    "confidence",
    "probabilities",
    "state",
];

/// The header record.
pub fn decision_header() -> String {
    csv_line(&DECISION_COLUMNS)
}

/// The state the run sent: a JSON string's text, any other value's JSON
/// as sent; empty when the request is no object with a `state`.
fn state_of(request: &str) -> String {
    let Ok(body) = serde_json::from_str::<Ordered<Box<RawValue>>>(request) else {
        return String::new();
    };
    match body.get("state") {
        Some(raw) => {
            serde_json::from_str::<String>(raw.get()).unwrap_or_else(|_| raw.get().to_string())
        }
        None => String::new(),
    }
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

/// The answer columns of one answer: the type, the answer (the option, the
/// expected level, or P(yes)), the confidence (for `noul` the lab's
/// certainty, max(p, 1 − p)) and the probabilities' JSON in option order.
fn answer_columns(a: &Answer) -> [String; 4] {
    match a {
        Answer::Choice(c) => [
            "choice".into(),
            c.choice.clone(),
            c.confidence.to_string(),
            serde_json::to_string(&c.probabilities).unwrap_or_default(),
        ],
        Answer::Score(s) => [
            "score".into(),
            s.score.to_string(),
            s.confidence.to_string(),
            serde_json::to_string(&s.probabilities).unwrap_or_default(),
        ],
        Answer::Noul(n) => [
            "noul".into(),
            n.noul.to_string(),
            n.noul.max(1.0 - n.noul).to_string(),
            String::new(),
        ],
    }
}

/// The `expected` and `correct` columns of question `id`'s answer `a` in
/// group `g`: empty unless `g` is a set run's item with a label for it;
/// `correct` empty too when there is no readable answer.
fn label_columns(
    g: &LabGroupView,
    question: Option<&QuestionDraft>,
    id: &str,
    a: Option<&Answer>,
) -> [String; 2] {
    let Some(label) = g.item.as_ref().and_then(|i| i.expected.get(id)) else {
        return [String::new(), String::new()];
    };
    let correct = question
        .zip(a)
        .and_then(|(q, a)| metrics::correct(q, a, label));
    [label.clone(), opt(correct)]
}

fn run_rows(g: &LabGroupView, form: Option<&DecisionForm>, r: &LabRunView, out: &mut String) {
    let item = g.item.as_ref();
    let head = [
        g.id.to_string(),
        opt(item.map(|i| i.batch_id)),
        opt(item.map(|i| i.item_ix)),
        item.and_then(|i| i.key.clone()).unwrap_or_default(),
        opt(r.id),
        r.created_at.clone(),
        r.model_asked.clone(),
        r.answered_by.clone(),
        opt(r.upstream_model.as_ref()),
        r.variant.clone(),
        r.http_status.to_string(),
        opt(r.error_message.as_ref()),
        r.latency_ms.to_string(),
        opt(r.input_tokens),
    ];
    let state = state_of(&r.request);
    let answered = (200..300)
        .contains(&r.http_status)
        .then(|| {
            r.response
                .as_deref()
                .and_then(|t| serde_json::from_str::<SystemOneResponse>(t).ok())
        })
        .flatten();
    let mut row = |id: &str, columns: [String; 4], labels: [String; 2]| {
        let [kind, answer, confidence, probabilities] = columns;
        let [expected, correct] = labels;
        let mut fields: Vec<String> = head.to_vec();
        fields.extend([
            id.to_string(),
            kind,
            answer,
            expected,
            correct,
            confidence,
            probabilities,
            state.clone(),
        ]);
        out.push_str(&csv_line(&fields));
    };
    match (item.and(form), answered) {
        // A set run's item: a row per question of its set, answered or not,
        // so each label is in the table.
        (Some(form), answered) => {
            for q in &form.questions {
                let a = answered.as_ref().and_then(|resp| resp.answers.get(&q.id));
                let columns = match a {
                    Some(a) => answer_columns(a),
                    None => [
                        q.kind.type_name().to_string(),
                        String::new(),
                        String::new(),
                        String::new(),
                    ],
                };
                row(&q.id, columns, label_columns(g, Some(q), &q.id, a));
            }
        }
        (None, Some(resp)) if !resp.answers.is_empty() => {
            for (id, a) in resp.answers.iter() {
                row(id, answer_columns(a), label_columns(g, None, id, Some(a)));
            }
        }
        (None, _) => {
            let mut fields: Vec<String> = head.to_vec();
            fields.extend(std::iter::repeat_n(String::new(), 7));
            fields.push(state);
            out.push_str(&csv_line(&fields));
        }
    }
}

/// The records of group `g`: its runs in cell order.
pub fn decision_rows(g: &LabGroupView) -> String {
    // A set run's item needs its questions to judge an answer and to name
    // each label; the history's groups have no labels to judge against.
    let form = g
        .item
        .as_ref()
        .and_then(|_| serde_json::from_value::<DecisionForm>(g.form.clone()).ok());
    let mut out = String::new();
    for r in &g.runs {
        run_rows(g, form.as_ref(), r, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_is_a_strings_text_or_the_json_as_sent() {
        assert_eq!(
            state_of(r#"{"model": "m", "state": "a \"q\"\nb"}"#),
            "a \"q\"\nb"
        );
        assert_eq!(
            state_of(r#"{"state": {"b": 1, "a": 2}, "model": "m"}"#),
            r#"{"b": 1, "a": 2}"#
        );
        assert_eq!(state_of("not json"), "");
    }

    #[test]
    fn a_noul_answers_its_p_yes_with_the_labs_certainty() {
        let a: Answer = serde_json::from_str(r#"{"type": "noul", "noul": 0.25}"#).unwrap();
        let [kind, answer, confidence, probabilities] = answer_columns(&a);
        assert_eq!(
            (kind.as_str(), answer.as_str(), confidence.as_str()),
            ("noul", "0.25", "0.75")
        );
        assert_eq!(probabilities, "");
    }
}
