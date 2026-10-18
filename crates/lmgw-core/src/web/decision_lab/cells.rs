//! `POST /decision-lab/api/groups/{id}/cells/{cell}` (labs-store design
//! §2.6 steps 2 and 3): one cell of a stored group, dispatched in process
//! through the real `/v1/systemone` handler and recorded as a run.
//!
//! The page posts the cells one at a time, so each latency is the request's
//! own; Stop is the page not posting the next, and a cell in flight that is
//! aborted drops the handler's future, which writes the request log's 499
//! row as any disconnect does.
//!
//! A dispatched cell answers 200 whatever the route said (Decision 20): the
//! run carries the status, the envelope's code and message and the body
//! verbatim. Only the lab's own failures keep their status: an unknown group
//! or cell 404, a cell that already has a run or is being answered 409. The
//! cell is claimed before anything is built or sent
//! ([`crate::labs::claims`]), so two posts of one cell reach the model once.
//! A run the store could not keep is answered all the same, with
//! `store_error` saying why.
//!
//! A set run's job answers its items through the same path ([`answer`]):
//! each item is a group of one cell (labs-store design §3.3). Its mode
//! ([`AnswerMode`]) differs in two things: a refusal lmgw gives about its own
//! state before any model answers (the GPU hold, admission, the container's
//! start) is handed back unrecorded, since the next item would get the same,
//! and Resume's `retry_failed` replaces a failed run. An item's 2xx run is
//! judged as it is recorded: whether it reads as an answer to every question
//! of the item's form (`lab_runs.readable`).

use std::collections::HashMap;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::decision_lab::form::ENDPOINT;
use lmgw_api_types::decision_lab::{DecisionForm, ImageInput, Variant};
use lmgw_api_types::lab_store::{
    artifact_ref, artifact_ref_id, LabCellResult, LabKind, LabRunView,
};

use super::super::chat::err_json;
use super::super::chat_extract::ChatPath;
use super::super::labs_store::groups::no_group;
use super::super::labs_store::store_failed;
use super::{images, model_member};
use crate::egress::openai_wire::SystemOneBody;
use crate::labs::artifacts;
use crate::labs::record::{self, NewArtifact, NewRun, RecordError};
use crate::labs::run_facts::{answer_of, failure_of, gateway_headers};
use crate::labs::{metrics, views};
use crate::proxy::systemone::SameForEveryItem;
use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::store::{self, LabArtifactRow, LabGroupRow, LabRunData};

/// The bytes a cell sends, and the text its run keeps: the same request,
/// each image a data URL in the one and `lab-artifact:<id>` in the other.
struct CellBody {
    sent: Vec<u8>,
    stored: String,
}

/// The group's inputs by id: their file and type.
type Inputs = HashMap<i64, LabArtifactRow>;

/// An input artifact as the data URL a request carries: the URL as it was
/// typed when it came as one.
async fn data_url_of(state: &SharedState, a: &LabArtifactRow) -> Result<String, String> {
    let bytes = artifacts::read(&state.data_dir, &a.sha256).await?;
    Ok(artifacts::data_url_again(
        a.data_url_head.as_deref(),
        &a.mime,
        &bytes,
    ))
}

/// The cell's body from the stored form, in `variant`.
async fn from_form(
    state: &SharedState,
    g: &LabGroupRow,
    inputs: &Inputs,
    model: &str,
    variant: Variant,
) -> Result<CellBody, String> {
    let form: DecisionForm =
        serde_json::from_str(&g.form).map_err(|e| format!("the stored form does not read: {e}"))?;
    let mut sent = Vec::new();
    let mut stored = Vec::new();
    for img in &form.images {
        match img {
            ImageInput::Artifact { id, name } => {
                let a = inputs.get(id).ok_or_else(|| {
                    format!("the image '{name}' (artifact {id}) is not the group's")
                })?;
                sent.push(data_url_of(state, a).await?);
                stored.push(artifact_ref(*id));
            }
            ImageInput::DataUrl { url, .. } => {
                sent.push(url.clone());
                stored.push(url.clone());
            }
        }
    }
    Ok(CellBody {
        sent: form.body(model, variant, &sent)?.into_bytes(),
        stored: form.body(model, variant, &stored)?,
    })
}

/// The cell's body from the stored override: `model` rewritten, as the
/// route itself rewrites it (inserted first when the override has none), and
/// each of the group's `lab-artifact:<id>` in its `images` or `files` the
/// data URL again.
async fn from_override(
    state: &SharedState,
    over: &str,
    inputs: &Inputs,
    model: &str,
) -> Result<CellBody, String> {
    let over = model_member::ensure_model(over, model).map_err(|e| format!("override: {e}"))?;
    let over = over.as_ref();
    let elements = images::elements(over)?;
    let mut swaps = Vec::new();
    for e in &elements {
        if let Some(a) = artifact_ref_id(&e.value).and_then(|id| inputs.get(&id)) {
            swaps.push((e, data_url_of(state, a).await?));
        }
    }
    let resolved = images::splice(over, swaps);
    let with_model = |text: &str| {
        SystemOneBody::parse(text.as_bytes())
            .and_then(|b| b.with_model(model))
            .map_err(|e| format!("override: {e}"))
    };
    let stored = String::from_utf8(with_model(over)?).map_err(|e| e.to_string())?;
    Ok(CellBody {
        sent: with_model(&resolved)?,
        stored,
    })
}

/// The lab's 409: the cell has a run, or is being answered.
fn answered(message: String) -> Response {
    err_json(StatusCode::CONFLICT, "cell_answered", message)
}

pub(in crate::web) async fn run(
    State(state): State<SharedState>,
    ChatPath((id, cell)): ChatPath<(i64, u32)>,
) -> Response {
    let lab = LabKind::Decision;
    let g = match store::lab_group(&state.db, lab, id).await {
        Ok(Some(g)) => g,
        Ok(None) => return no_group(lab, id),
        Err(e) => return store_failed("reading the group", e),
    };
    match answer(&state, &g, cell, AnswerMode::default()).await {
        Ok(a) => Json(a.result).into_response(),
        Err(refused) => refused.into_response(),
    }
}

/// How [`answer`] treats a cell: the page's way by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AnswerMode {
    /// An answer that is the same for every item ([`SameForEveryItem`]: lmgw's
    /// own refusal before any model answers, or the upstream refusing lmgw's
    /// credential) is [`CellRefused::Gateway`], not a run: a set
    /// run's job, whose next item would get the same. The page records it,
    /// to show it.
    pub unrecorded_gateway_refusals: bool,
    /// The cell's failed run (no 2xx) is replaced by this one: a set run's
    /// Resume with `retry_failed`. A 2xx run stays the cell's.
    pub replace_failed: bool,
}

/// What [`answer`] answered.
#[derive(Debug)]
pub(crate) struct Answered {
    pub result: LabCellResult,
    /// A set run item's 2xx run: whether it reads as an answer to every
    /// question of the item's form.
    pub readable: Option<bool>,
}

/// Whether the answer `text` of a run that answered `status` reads as an
/// answer to every question of the set run item `g`'s form, by the metrics'
/// own rule ([`metrics::outcome`]); `None` for a group of the history, which
/// nothing judges.
fn readable(g: &LabGroupRow, status: u16, text: Option<&str>) -> Option<bool> {
    g.batch_id?;
    let form: DecisionForm = serde_json::from_str(&g.form).ok()?;
    let status = Some(i64::from(status));
    let response = metrics::read_response(status, text);
    Some(form.questions.iter().all(|q| {
        matches!(
            metrics::outcome(q, status, response.as_ref()),
            metrics::Outcome::Answered(_)
        )
    }))
}

/// Why a cell was not dispatched, or its run is another's: the lab's own
/// failures, which keep their status.
#[derive(Debug)]
pub(crate) enum CellRefused {
    /// The plan has no such cell: 404.
    NoCell(String),
    /// The cell has a run or is being answered: 409 `cell_answered`.
    Answered(String),
    /// The stored form or file no longer builds a request: 500
    /// `cell_unbuilt`.
    Unbuilt(String),
    /// The store failed reading what the cell needs: 500.
    Store(&'static str, String),
    /// lmgw refused before any model answered, about its own state, and the
    /// mode asked for no run of it ([`AnswerMode`]): its status, its
    /// envelope's code and message. Answered as it came.
    Gateway {
        status: StatusCode,
        code: Option<String>,
        message: String,
    },
}

impl std::fmt::Display for CellRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCell(m) | Self::Answered(m) | Self::Unbuilt(m) => f.write_str(m),
            Self::Store(what, e) => write!(f, "{what} failed: {e}"),
            Self::Gateway {
                status,
                code,
                message,
            } => {
                write!(f, "lmgw could not ask the model ({}", status.as_u16())?;
                if let Some(code) = code {
                    write!(f, " {code}")?;
                }
                write!(f, "): {message}")
            }
        }
    }
}

impl IntoResponse for CellRefused {
    fn into_response(self) -> Response {
        match self {
            Self::NoCell(m) => err_json(StatusCode::NOT_FOUND, "not_found", m),
            Self::Answered(m) => answered(m),
            Self::Unbuilt(m) => err_json(StatusCode::INTERNAL_SERVER_ERROR, "cell_unbuilt", m),
            Self::Store(what, e) => store_failed(what, e),
            Self::Gateway {
                status,
                code,
                message,
            } => (
                status,
                Json(lmgw_api_types::ApiError {
                    code: code.unwrap_or_else(|| "refused".into()),
                    message,
                    ..Default::default()
                }),
            )
                .into_response(),
        }
    }
}

/// Cell `cell` of the group `g`: claimed, built, dispatched in process
/// through `/v1/systemone`'s handler and recorded. The one path a cell
/// takes, posted by the page or answered by a set run's job
/// (`labs::batch`), each in its [`AnswerMode`].
pub(crate) async fn answer(
    state: &SharedState,
    g: &LabGroupRow,
    cell: u32,
    mode: AnswerMode,
) -> Result<Answered, CellRefused> {
    let id = g.id;
    let plan = views::plan_of(g);
    let Some(c) = plan.get(cell as usize) else {
        return Err(CellRefused::NoCell(format!(
            "group {id} has {} cells; there is no cell {cell}",
            plan.len()
        )));
    };
    // Held until the run is recorded (or the handler is dropped).
    let Some(_claim) = state.lab_cells.claim(id, i64::from(cell)) else {
        return Err(CellRefused::Answered(format!(
            "cell {cell} of group {id} is being answered; a re-ask is a new group"
        )));
    };
    match store::lab_run_exists(&state.db, id, i64::from(cell), mode.replace_failed).await {
        Ok(true) => {
            return Err(CellRefused::Answered(format!(
                "cell {cell} of group {id} has a run already; a re-ask is a new group"
            )))
        }
        Ok(false) => {}
        Err(e) => {
            return Err(CellRefused::Store(
                "reading the group's runs",
                e.to_string(),
            ))
        }
    }
    let inputs: Inputs = match store::lab_artifacts_of(&state.db, &[id]).await {
        Ok(rows) => rows
            .into_iter()
            .filter(|a| a.run_id.is_none())
            .map(|a| (a.id, a))
            .collect(),
        Err(e) => {
            return Err(CellRefused::Store(
                "reading the group's images",
                e.to_string(),
            ))
        }
    };
    let variant = Variant::parse(&c.variant).unwrap_or_default();
    let body = match &g.override_text {
        Some(over) => from_override(state, over, &inputs, &c.model).await,
        None => from_form(state, g, &inputs, &c.model, variant).await,
    };
    let body = match body {
        Ok(b) => b,
        Err(e) => {
            return Err(CellRefused::Unbuilt(format!(
                "cell {cell} of group {id} could not be built: {e}"
            )))
        }
    };

    let started = Instant::now();
    let resp = crate::proxy::systemone::dispatch(
        state.clone(),
        RequestCtx::default(),
        Bytes::from(body.sent),
    )
    .await;
    let status = resp.status();
    let (parts, answer) = resp.into_parts();
    let bytes = axum::body::to_bytes(answer, usize::MAX).await;
    let same_for_all = parts.extensions.get::<SameForEveryItem>().is_some();
    let latency_ms = started.elapsed().as_millis() as i64;
    let (bytes, read_error) = match bytes {
        Ok(b) => (b.to_vec(), None),
        Err(e) => (Vec::new(), Some(format!("reading the answer: {e}"))),
    };

    let headers = gateway_headers(&parts.headers);
    let response_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let (response, outputs) = match String::from_utf8(bytes) {
        Ok(t) => (Some(t), Vec::new()),
        // No text: kept as an output artifact instead.
        Err(e) => (
            None,
            vec![NewArtifact::Bytes {
                name: "response".into(),
                bytes: e.into_bytes().into(),
                declared_mime: response_type.clone(),
                data_url_head: None,
            }],
        ),
    };
    let (mut error_code, mut error_message) = if status.is_success() {
        (None, None)
    } else {
        failure_of(response.as_deref())
    };
    if same_for_all && mode.unrecorded_gateway_refusals && read_error.is_none() {
        return Err(CellRefused::Gateway {
            status,
            code: error_code,
            message: error_message.unwrap_or_default(),
        });
    }
    if let Some(e) = read_error {
        error_code.get_or_insert_with(|| "read_failed".into());
        error_message = Some(e);
    }
    let (upstream_model, input_tokens, output_tokens) = if status.is_success() {
        answer_of(response.as_deref())
    } else {
        (None, None, None)
    };
    let readable = if status.is_success() {
        readable(g, status.as_u16(), response.as_deref())
    } else {
        None
    };
    let data = LabRunData {
        group_id: id,
        cell: i64::from(cell),
        variant: c.variant.clone(),
        endpoint: ENDPOINT.into(),
        model_asked: c.model.clone(),
        answered_by: headers
            .get("x-lmgw-fallback")
            .cloned()
            .unwrap_or_else(|| c.model.clone()),
        fallback_reason: headers.get("x-lmgw-fallback-reason").cloned(),
        upstream_model,
        http_status: i64::from(status.as_u16()),
        error_code,
        error_message,
        request: body.stored,
        response,
        response_type,
        headers: serde_json::to_string(&headers).unwrap_or_else(|_| "{}".into()),
        input_tokens,
        output_tokens,
        latency_ms,
        readable,
    };
    let recorded = record::run(
        &state.db,
        &state.data_dir,
        NewRun {
            data: data.clone(),
            outputs,
            replace_failed: mode.replace_failed,
        },
    )
    .await;
    let result = match recorded {
        // Only a writer the claim does not see gets here: the run it kept
        // stands, and this one is the 409 it would have been.
        Err(RecordError::Taken(m)) => return Err(CellRefused::Answered(m)),
        Ok(r) => {
            let row = store::LabRunRow {
                id: r.id,
                created_at: r.created_at,
                data,
            };
            let outs = r
                .outputs
                .iter()
                .map(|a| views::artifact_view(a, views::api_href(a.id)))
                .collect();
            LabCellResult {
                run: views::run_view(&row, outs),
                store_error: None,
            }
        }
        Err(e) => {
            tracing::warn!("decision lab: cell {cell} of group {id} not kept: {e}");
            let row = store::LabRunRow {
                id: 0,
                created_at: chrono::Utc::now()
                    .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                    .to_string(),
                data,
            };
            let run = LabRunView {
                id: None,
                ..views::run_view(&row, Vec::new())
            };
            LabCellResult {
                run,
                store_error: Some(e.to_string()),
            }
        }
    };
    Ok(Answered { result, readable })
}
