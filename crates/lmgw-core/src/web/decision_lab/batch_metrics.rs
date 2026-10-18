//! A set run's metrics (labs-store design §3.4) and "Use as threshold":
//! `GET /decision-lab/api/batches/{id}/metrics` computes them from the
//! stored runs with `labs::metrics`, per question of the set as run; `POST
//! /decision-lab/api/batches/{id}/threshold` measures one cut the same way
//! and writes its value and measurement into the saved set.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::decision_lab::{
    BatchMetrics, Measured, MetricsQuery, QuestionDraft, Threshold, ThresholdWrite, DEFAULT_BINS,
};
use lmgw_api_types::lab_store::{LabKind, LabPresetBody};
use lmgw_api_types::systemone::Ordered;

use super::super::chat::err_json;
use super::super::chat_extract::{ChatJson, ChatPath, ChatQuery};
use super::super::labs_store::store_failed;
use super::batches::no_batch;
use super::groups::bad;
use crate::labs::metrics::{self, ItemOutcome};
use crate::labs::{batch_view, presets};
use crate::state::SharedState;
use crate::store::{self, LabBatchAnswer, PresetWrite};

const LAB: LabKind = LabKind::Decision;

/// Each item's outcome for each of `questions`, in item order, read on the
/// blocking pool (every answer is parsed once).
async fn outcomes(
    state: &SharedState,
    id: i64,
    questions: Vec<QuestionDraft>,
) -> Result<Vec<Vec<ItemOutcome>>, Response> {
    let answers = store::lab_batch_answers(&state.db, id)
        .await
        .map_err(|e| store_failed("reading the set run's answers", e))?;
    tokio::task::spawn_blocking(move || judge(&questions, &answers))
        .await
        .map_err(|e| store_failed("reading the set run's answers", e))
}

/// Per question, each item's label and outcome.
fn judge(questions: &[QuestionDraft], answers: &[LabBatchAnswer]) -> Vec<Vec<ItemOutcome>> {
    let mut per: Vec<Vec<ItemOutcome>> = questions
        .iter()
        .map(|_| Vec::with_capacity(answers.len()))
        .collect();
    for a in answers {
        let expected: Ordered<String> = a
            .expected
            .as_deref()
            .and_then(|t| serde_json::from_str(t).ok())
            .unwrap_or_default();
        let response = metrics::read_response(a.http_status, a.response.as_deref());
        for (q, out) in questions.iter().zip(per.iter_mut()) {
            out.push(ItemOutcome {
                expected: expected.get(&q.id).cloned(),
                outcome: metrics::outcome(q, a.http_status, response.as_ref()),
            });
        }
    }
    per
}

/// `GET /decision-lab/api/batches/{id}/metrics?question=&bins=`.
pub async fn metrics(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatQuery(q): ChatQuery<MetricsQuery>,
) -> Response {
    let b = match store::lab_batch(&state.db, Some(LAB), id).await {
        Ok(Some(b)) => b,
        Ok(None) => return no_batch(id),
        Err(e) => return store_failed("reading the set run", e),
    };
    let bins = q.bins.unwrap_or(DEFAULT_BINS);
    if bins == 0 {
        return bad("bins: at least 1");
    }
    let set = batch_view::snapshot(&b);
    let questions: Vec<QuestionDraft> = match q.question.as_deref() {
        None => set.questions.clone(),
        Some(qid) => match set.question(qid) {
            Some(question) => vec![question.clone()],
            None => {
                return err_json(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    format!("set run {id}'s set has no question '{qid}'"),
                )
            }
        },
    };
    let per = match outcomes(&state, id, questions.clone()).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let questions = questions
        .iter()
        .zip(&per)
        .map(|(question, items)| metrics::question(question, items, bins))
        .collect();
    Json(BatchMetrics {
        batch_id: id,
        bins,
        questions,
    })
    .into_response()
}

/// The 409 for a threshold the saved set cannot take.
fn set_conflict(code: &'static str, message: String) -> Response {
    err_json(StatusCode::CONFLICT, code, message)
}

/// `POST /decision-lab/api/batches/{id}/threshold`: measure the cut at
/// `value` for `question` on this run and write both into the saved set.
pub async fn threshold(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<ThresholdWrite>,
) -> Response {
    if !(0.0..=1.0).contains(&req.value) {
        return bad(format!("value: {} is not between 0 and 1", req.value));
    }
    let b = match store::lab_batch(&state.db, Some(LAB), id).await {
        Ok(Some(b)) => b,
        Ok(None) => return no_batch(id),
        Err(e) => return store_failed("reading the set run", e),
    };
    let ran = batch_view::snapshot(&b);
    let Some(question) = ran.question(&req.question).cloned() else {
        return err_json(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("set run {id}'s set has no question '{}'", req.question),
        );
    };
    let gone = || {
        set_conflict(
            "set_deleted",
            format!(
                "set run {id} ran an unsaved set, or one deleted since: save the set and run it \
                 to measure a threshold"
            ),
        )
    };
    let Some(preset_id) = b.preset_id else {
        return gone();
    };
    let row = match store::lab_preset(&state.db, Some(LAB), preset_id).await {
        Ok(Some(r)) => r,
        Ok(None) => return gone(),
        Err(e) => return store_failed("reading the set", e),
    };
    let mut set = match presets::question_set(&row) {
        Ok(s) => s,
        Err(e) => return store_failed("reading the set", e),
    };
    if set.question(&question.id) != Some(&question) {
        return set_conflict(
            "set_changed",
            format!(
                "question '{}' of the set '{}' changed since set run {id} ran it: run the set \
                 again to measure its threshold",
                question.id, row.name
            ),
        );
    }
    let per = match outcomes(&state, id, vec![question.clone()]).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let point = metrics::at_threshold(&per[0], req.value);
    let Some(accuracy) = point.accuracy else {
        return set_conflict(
            "no_labels",
            format!(
                "set run {id} has no labelled answer of '{}' at or above {}: a threshold there \
                 measures nothing; label items, or pick a lower cut",
                question.id, req.value
            ),
        );
    };
    let t = Threshold {
        value: req.value,
        measured: Some(Measured {
            batch_id: id,
            coverage: point.coverage,
            accuracy,
            labelled: point.labelled,
        }),
    };
    match set.thresholds.0.iter_mut().find(|(k, _)| *k == question.id) {
        Some(slot) => slot.1 = t,
        None => set.thresholds.0.push((question.id.clone(), t)),
    }
    let body = match presets::body_text(&LabPresetBody::QuestionSet(set)) {
        Ok(b) => b,
        Err(e) => return store_failed("writing the set", e),
    };
    match store::replace_lab_preset_body(&state.db, preset_id, &row.body, &body).await {
        Ok(PresetWrite::Written(r)) => match presets::view(&r) {
            Ok(v) => Json(v).into_response(),
            Err(e) => store_failed("reading the set", e),
        },
        Ok(PresetWrite::Changed | PresetWrite::Missing | PresetWrite::NameTaken) => set_conflict(
            "set_changed",
            format!(
                "the set '{}' changed or went while the threshold was measured; try again",
                row.name
            ),
        ),
        Err(e) => store_failed("writing the set", e),
    }
}
