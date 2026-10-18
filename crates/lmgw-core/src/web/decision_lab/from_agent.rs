//! "Save as labelled set" (labs-store design §4, Q4):
//! `POST /decision-lab/api/batches/from-agent` turns a batch agent's
//! finished apply run into a set run not yet run (`job_id` NULL, status
//! `not_started`); Resume on the set-run page runs it.
//!
//! The labels are the rows the apply job received: the checked rows with
//! the owner's edits (unchecked rows were not confirmed either way). Each
//! item's state is the prompt the classify call sent (`Row.prompt`, which
//! the received row carries; else the run it was applied from, when the job
//! names one), its expected answer the row's output field, its key the row's
//! id. Skipped, with the reason: a row the model was never asked about (no
//! state), and a row whose call failed with its output still the fallback
//! the failure gave it (no answer, and no label of the owner's). A label
//! that is no option of the set's question refuses the save naming each
//! row, as an unknown label refuses a file (§3.3). The rows are copied, so
//! job pruning later does not touch the set run.
//!
//! The set is the one stamped on the rows ([`Row::set`]: the set that
//! classified them), which must be one set: rows of several sets, or
//! stamped rows beside rows the model call classified, are refused naming
//! them. `preset_id` is an explicit override. Each item's form is the set
//! restricted to the output field's question, the request the agent sends,
//! and so is the run's snapshot: its measurement, token count and threshold
//! describe that request, and "Use as threshold" writes onto that question
//! of the saved set. The items are written as a file's are
//! (`store::insert_lab_batch`: short write transactions, not ready until
//! the last), as a task of their own.

use std::collections::HashMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::decision_lab::{
    agent_source_name, BatchFromAgent, BatchFromAgentSaved, BatchFromAgentSkip,
    BatchFromAgentSkipped, BatchMapping, BatchProblem, BatchProblemKind, DecisionForm,
    QuestionKind, QuestionSet, StateMode, Variant, AGENT_KEY_COLUMN, AGENT_STATE_COLUMN,
};
use lmgw_api_types::lab_store::{LabCell, LabKind, LabPresetKind};
use lmgw_api_types::systemone::Ordered;
use lmgw_api_types::AgentRowSet;
use serde_json::Value;

use super::super::chat::err_json;
use super::super::chat_extract::ChatJson;
use super::super::labs_store::store_failed;
use super::batches::no_batch;
use super::groups::bad;
use crate::agents::batch::{self, decide, Phase, Row};
use crate::agents::manifest::RunSpec;
use crate::agents::{self, Agent};
use crate::labs::{batch_view, presets};
use crate::state::SharedState;
use crate::store::{self, NewLabBatch, NewLabItem};

const LAB: LabKind = LabKind::Decision;

fn not_found(message: String) -> Response {
    err_json(StatusCode::NOT_FOUND, "not_found", message)
}

/// The rows of the run `base` (the classify run an apply names) by id, for
/// a received row that lost its prompt; empty when it is gone or another
/// agent's.
async fn base_rows(state: &SharedState, base: Option<i64>, agent_id: &str) -> HashMap<String, Row> {
    let Some(base) = base else {
        return HashMap::new();
    };
    let Ok(Some(job)) = store::get_job(&state.db, base).await else {
        return HashMap::new();
    };
    if job.kind != agents::JOB_KIND || job.key.as_deref() != Some(&agents::job_key(agent_id)) {
        return HashMap::new();
    }
    let result: Option<Value> = job
        .result
        .as_deref()
        .and_then(|r| serde_json::from_str(r).ok());
    batch::rows_of(state, base, result.as_ref())
        .into_iter()
        .map(|r| (r.id.clone(), r))
        .collect()
}

/// A received row that becomes an item: its index, the row, its state, its
/// label (none when empty) and the set that classified it.
struct Candidate<'r> {
    ix: usize,
    row: &'r Row,
    prompt: String,
    label: Option<String>,
    set: Option<&'r AgentRowSet>,
}

/// `ids` as the refusal names them: "row 18f1" or "rows 18f1, 18f2".
fn rows_named(ids: &[&str]) -> String {
    match ids {
        [one] => format!("row {one}"),
        many => format!("rows {}", many.join(", ")),
    }
}

/// The one set stamped on `items` (labs-store design §4: the set that
/// classified them), or the refusal: no row stamped (the model call
/// classified them, or the rows predate the stamp), or rows of several sets,
/// or stamped rows beside unstamped ones, naming each group's rows.
fn stamped_set(items: &[Candidate<'_>]) -> Result<i64, Response> {
    // In the rows' order, each set once.
    let mut groups: Vec<(Option<&AgentRowSet>, Vec<&str>)> = Vec::new();
    for c in items {
        let stamp = c.set;
        match groups
            .iter_mut()
            .find(|(s, _)| s.map(|s| s.id) == stamp.map(|s| s.id))
        {
            Some((_, ids)) => ids.push(&c.row.id),
            None => groups.push((stamp, vec![&c.row.id])),
        }
    }
    match groups.as_slice() {
        [(Some(set), _)] => Ok(set.id),
        [(None, _)] => Err(bad(
            "preset_id: the run's rows were not classified through a question set (the model \
             call classified them), so name the set to save them into",
        )),
        _ => {
            let named: Vec<String> = groups
                .iter()
                .map(|(set, ids)| match set {
                    Some(s) => format!("'{}' (set {}): {}", s.name, s.id, rows_named(ids)),
                    None => format!("the model call, no set: {}", rows_named(ids)),
                })
                .collect();
            Err(err_json(
                StatusCode::BAD_REQUEST,
                "mixed_sets",
                format!(
                    "the run's rows were not classified by one question set, so they are no one \
                     set's labels: {}. classify them again through one set, or pass \
                     preset_id to choose the set",
                    named.join("; ")
                ),
            ))
        }
    }
}

pub async fn save(
    State(state): State<SharedState>,
    ChatJson(req): ChatJson<BatchFromAgent>,
) -> Response {
    let job_id = req.job_id;
    let job = match store::get_job(&state.db, job_id).await {
        Ok(Some(j)) if j.kind == agents::JOB_KIND => j,
        Ok(_) => return not_found(format!("no agent run with job id {job_id}")),
        Err(e) => return store_failed("reading the agent run", e),
    };
    let input: batch::Input = match serde_json::from_str(&job.input) {
        Ok(i) => i,
        Err(e) => return bad(format!("run {job_id} has no input this build reads: {e}")),
    };
    if input.phase != Phase::Apply {
        return bad(format!(
            "run {job_id} is a {} run: save a finished apply run, whose rows are the reviewed \
             labels",
            input.phase.as_str()
        ));
    }
    if matches!(job.status.as_str(), "queued" | "running") {
        return err_json(
            StatusCode::CONFLICT,
            "run_live",
            format!("run {job_id} is still applying; save it once it has finished"),
        );
    }
    let agent_id = input.agent_id.clone();
    let agent = match store::get_agent(&state.db, &agent_id).await {
        Ok(Some(row)) => match Agent::from_row(row) {
            Ok(a) => a,
            Err(e) => return bad(format!("agent '{agent_id}' does not read: {e}")),
        },
        Ok(None) => return not_found(format!("agent '{agent_id}' is not there any more")),
        Err(e) => return store_failed("reading the agent", e),
    };
    let out = match &agent.manifest.run {
        RunSpec::Batch { item, .. } => item.output.clone(),
        _ => None,
    };
    let Some((out, field)) = out.and_then(|o| o.field.clone().map(|f| (o, f))) else {
        return bad(format!(
            "agent '{agent_id}' classifies into no single field, so its rows carry no label of \
             a choice question"
        ));
    };
    let fallback = match &out.fallback {
        Some(Value::String(f)) => Some(f.as_str()),
        _ => None,
    };

    // The rows that become items, in the order the apply job received them,
    // and the ones that do not.
    let base = base_rows(&state, input.base_job, &agent_id).await;
    let (mut candidates, mut skipped) = (Vec::new(), Vec::new());
    for (ix, row) in input.rows.iter().enumerate() {
        let prompt = row
            .prompt
            .clone()
            .or_else(|| base.get(&row.id).and_then(|b| b.prompt.clone()))
            .filter(|p| !p.trim().is_empty());
        let Some(prompt) = prompt else {
            skipped.push(BatchFromAgentSkipped {
                id: row.id.clone(),
                kind: BatchFromAgentSkip::NeverAsked,
                reason: match &row.error {
                    Some(e) => format!("the model was never asked about it: {e}"),
                    None => "the model was never asked about it: the row carries no prompt".into(),
                },
            });
            continue;
        };
        let label = row
            .output
            .get(&field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string);
        // The fallback a failed call was given is no answer, and the owner
        // left it as it was: no label either.
        if let Some(e) = &row.error {
            if label.is_none() || label.as_deref() == fallback {
                skipped.push(BatchFromAgentSkipped {
                    id: row.id.clone(),
                    kind: BatchFromAgentSkip::CallFailed,
                    reason: format!("its call failed: {e}"),
                });
                continue;
            }
        }
        // The received row's stamp, else the classify run's row's, as for
        // its prompt.
        let set = row
            .set
            .as_ref()
            .or_else(|| base.get(&row.id).and_then(|b| b.set.as_ref()));
        candidates.push(Candidate {
            ix,
            row,
            prompt,
            label,
            set,
        });
    }
    if candidates.is_empty() {
        return bad(format!(
            "run {job_id} received {} row(s) and none has an answer or a label to save: every \
             one was never asked or its call failed",
            input.rows.len()
        ));
    }

    // The set: the override, else the one that classified the rows.
    let set_id = match req.preset_id {
        Some(id) => id,
        None => match stamped_set(&candidates) {
            Ok(id) => id,
            Err(refused) => return refused,
        },
    };
    let preset = match store::lab_preset(&state.db, Some(LAB), set_id).await {
        Ok(Some(p)) => p,
        Ok(None) => return not_found(format!("the decision lab has no preset {set_id}")),
        Err(e) => return store_failed("reading the set", e),
    };
    if preset.kind != LabPresetKind::QuestionSet.as_str() {
        return bad(format!(
            "preset {set_id} is a {}, not a question set",
            preset.kind
        ));
    }
    let set = match presets::question_set(&preset) {
        Ok(s) => s,
        Err(e) => return store_failed("reading the set", e),
    };
    if let Err(e) = set.check() {
        return bad(format!("set: {e}"));
    }
    let question = match decide::question_of(&preset.name, &set, &field) {
        Ok(q) => q.clone(),
        Err(e) => return bad(e),
    };
    let QuestionKind::Choice { options } = &question.kind else {
        return bad(format!("question '{field}' is no choice"));
    };
    let model = set.default_model.trim().to_string();
    if model.is_empty() {
        return bad(format!(
            "the question set '{}' names no default model: pick one for it in the Decision lab",
            preset.name
        ));
    }
    if !crate::modelinfo::decision::answers(&state, &model).await {
        return not_found(format!(
            "'{model}' answers no /v1/systemone: pick a model of GET /decision-lab/api/models \
             for the set"
        ));
    }
    // What the agent asks: the output field's question alone.
    let asked = QuestionSet {
        questions: vec![question.clone()],
        default_model: set.default_model.clone(),
        thresholds: set
            .thresholds
            .iter()
            .filter(|(id, _)| *id == field)
            .map(|(id, t)| (id, t.clone()))
            .collect(),
    };
    // Changed means the output question was edited since the run asked it,
    // not any write to the set (a threshold write leaves it as it was). A
    // stamp without a digest reads as unchanged.
    let digest = crate::agents::batch::decide::question_digest(&question);
    let set_changed = candidates.iter().any(|c| {
        c.set
            .is_some_and(|s| s.id == set_id && !s.digest.is_empty() && s.digest != digest)
    });

    let names: Vec<&str> = options.iter().map(|o| o.name.as_str()).collect();
    let (mut items, mut problems) = (Vec::new(), Vec::new());
    for c in candidates {
        let mut expected = Ordered::default();
        if let Some(label) = c.label {
            if !names.contains(&label.as_str()) {
                problems.push(BatchProblem {
                    kind: BatchProblemKind::Label,
                    question: Some(field.clone()),
                    row: Some(c.ix as u64 + 1),
                    line: None,
                    column: Some(field.clone()),
                    value: Some(label.clone()),
                    message: format!(
                        "row {} ({}): '{label}' is no option of the set's question '{field}' \
                         ({}); add it to the set '{}' to keep this label",
                        c.ix + 1,
                        c.row.id,
                        names.join(", "),
                        preset.name
                    ),
                });
                continue;
            }
            expected.0.push((field.clone(), label));
        }
        let form = DecisionForm {
            state_mode: StateMode::Text,
            state: c.prompt,
            images: Vec::new(),
            questions: asked.questions.clone(),
        };
        items.push(NewLabItem {
            title: form.title(),
            form: serde_json::to_string(&form).unwrap_or_default(),
            expected: (!expected.is_empty())
                .then(|| serde_json::to_string(&expected).unwrap_or_default()),
            key: (!c.row.id.is_empty()).then(|| c.row.id.clone()),
        });
    }
    if !problems.is_empty() {
        let list: Vec<&str> = problems.iter().map(|p| p.message.as_str()).collect();
        return err_json(
            StatusCode::BAD_REQUEST,
            "bad_input",
            format!(
                "the run's rows cannot be saved as labels ({} problem{}): {}",
                problems.len(),
                if problems.len() == 1 { "" } else { "s" },
                list.join("; ")
            ),
        );
    }

    let snapshot = match serde_json::to_string(&asked) {
        Ok(s) => s,
        Err(e) => return store_failed("writing the set run", e),
    };
    let plan = match serde_json::to_string(&[LabCell {
        model: model.clone(),
        variant: Variant::AsWritten.as_str().into(),
    }]) {
        Ok(p) => p,
        Err(e) => return store_failed("writing the set run", e),
    };
    let mapping = BatchMapping {
        state: AGENT_STATE_COLUMN.into(),
        state_mode: StateMode::Text,
        key: Some(AGENT_KEY_COLUMN.into()),
        labels: Ordered(vec![(field.clone(), field.clone())]),
        score_labels: Ordered::default(),
    };
    let mapping = serde_json::to_string(&mapping).unwrap_or_default();
    let source = agent_source_name(job_id, &agent_id);
    // Written as a task of its own, as a file's start is: a client that goes
    // away half way does not leave the run half written until the next boot.
    let st = state.clone();
    let written = tokio::spawn(async move {
        let b = NewLabBatch {
            lab: LAB,
            preset_id: Some(set_id),
            preset_snapshot: &snapshot,
            model: &model,
            source_name: &source,
            mapping: &mapping,
            concurrency: 1,
            plan: &plan,
        };
        store::insert_lab_batch(&st.db, &b, &items).await
    })
    .await;
    let id = match written {
        Ok(Ok(Some((id, _)))) => id,
        Ok(Ok(None)) => {
            return err_json(
                StatusCode::CONFLICT,
                "batch_deleted",
                "the set run was deleted (a Clear) while its items were written; save it again",
            )
        }
        Ok(Err(e)) => return store_failed("writing the set run", e),
        Err(e) => return store_failed("writing the set run", e),
    };
    match batch_view::one(&state, LAB, id).await {
        Ok(Some(batch)) => Json(BatchFromAgentSaved {
            batch,
            skipped,
            set_changed,
        })
        .into_response(),
        Ok(None) => no_batch(id),
        Err(e) => store_failed("reading the set run", e),
    }
}
