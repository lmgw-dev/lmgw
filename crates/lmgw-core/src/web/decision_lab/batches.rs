//! Set runs (labs-store design §3.3): `POST /decision-lab/api/batches/preview`
//! reads a file and shows its top (and with a mapping, every problem a start
//! would refuse); `POST /decision-lab/api/batches` writes the run and its
//! items in several short write transactions (`store::insert_lab_batch`:
//! not ready until the last) and starts its job, both as a task of its own
//! that a client gone away does not stop; the list, one run and its items in
//! pages. The file is read on the blocking pool, and its size is
//! bounded by nothing but the body, which these routes do not limit.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::decision_lab::{
    BatchItemPage, BatchItemsQuery, BatchList, BatchPreview, BatchStart, DecisionForm, QuestionSet,
};
use lmgw_api_types::lab_store::{LabCell, LabKind, LabPresetKind};

use super::super::chat::err_json;
use super::super::chat_extract::{ChatJson, ChatPath, ChatQuery};
use super::super::labs_store::store_failed;
use super::groups::bad;
use crate::labs::{batch, batch_view, ingest, presets, views};
use crate::state::SharedState;
use crate::store::{self, NewLabBatch, NewLabItem};

const LAB: LabKind = LabKind::Decision;

/// The 404 for a set run that is not there.
pub(super) fn no_batch(id: i64) -> Response {
    err_json(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("the decision lab has no set run {id}"),
    )
}

/// Set run `id` as its view, or the answer that says why not.
pub(super) async fn view_of(state: &SharedState, id: i64) -> Response {
    match batch_view::one(state, LAB, id).await {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => no_batch(id),
        Err(e) => store_failed("reading the set run", e),
    }
}

/// Run `f` on the blocking pool: a file is parsed whole.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Response> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| store_failed("reading the file", e))
}

pub async fn preview(ChatJson(req): ChatJson<BatchPreview>) -> Response {
    match blocking(move || ingest::preview(&req)).await {
        Ok(p) => Json(p).into_response(),
        Err(r) => r,
    }
}

/// The set a start names: a saved one (and its id) or one sent whole.
async fn set_of(
    state: &SharedState,
    req: &BatchStart,
) -> Result<(QuestionSet, Option<i64>), Response> {
    match (req.preset_id, &req.set) {
        (Some(_), Some(_)) => Err(bad(
            "name a saved set (preset_id) or send one (set), not both",
        )),
        (None, None) => Err(bad("set: pick a question set to run")),
        (None, Some(s)) => Ok((s.clone(), None)),
        (Some(id), None) => {
            let row = store::lab_preset(&state.db, Some(LAB), id)
                .await
                .map_err(|e| store_failed("reading the set", e))?
                .ok_or_else(|| {
                    err_json(
                        StatusCode::NOT_FOUND,
                        "not_found",
                        format!("the decision lab has no preset {id}"),
                    )
                })?;
            if row.kind != LabPresetKind::QuestionSet.as_str() {
                return Err(bad(format!(
                    "preset {id} is a {}, not a question set",
                    row.kind
                )));
            }
            let set =
                presets::question_set(&row).map_err(|e| store_failed("reading the set", e))?;
            Ok((set, Some(id)))
        }
    }
}

pub async fn start(
    State(state): State<SharedState>,
    ChatJson(req): ChatJson<BatchStart>,
) -> Response {
    let (set, preset_id) = match set_of(&state, &req).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if let Err(e) = set.check() {
        return bad(format!("set: {e}"));
    }
    let model = match req.model.trim() {
        "" => set.default_model.trim().to_string(),
        m => m.to_string(),
    };
    if model.is_empty() {
        return bad("model: pick a model to ask (the set names no default model)");
    }
    if !crate::modelinfo::decision::answers(&state, &model).await {
        return err_json(
            StatusCode::NOT_FOUND,
            "not_found",
            format!(
                "'{model}' answers no /v1/systemone: pick a model of GET /decision-lab/api/models"
            ),
        );
    }
    let concurrency = req.concurrency.unwrap_or(1);
    if concurrency == 0 {
        return bad("concurrency: at least 1");
    }

    let questions = set.questions.clone();
    let mapping = req.mapping.clone();
    let built = blocking(move || {
        let table = ingest::parse(req.format, &req.text);
        let items = table.items(&questions, &mapping)?;
        Ok::<_, Vec<lmgw_api_types::decision_lab::BatchProblem>>(
            items
                .into_iter()
                .map(|item| {
                    let form = DecisionForm {
                        state_mode: mapping.state_mode,
                        state: item.state,
                        images: Vec::new(),
                        questions: questions.clone(),
                    };
                    NewLabItem {
                        title: form.title(),
                        form: serde_json::to_string(&form).unwrap_or_default(),
                        expected: (!item.expected.is_empty())
                            .then(|| serde_json::to_string(&item.expected).unwrap_or_default()),
                        key: item.key,
                    }
                })
                .collect::<Vec<_>>(),
        )
    })
    .await;
    let items = match built {
        Ok(Ok(items)) => items,
        Ok(Err(problems)) => {
            return err_json(
                StatusCode::BAD_REQUEST,
                "bad_input",
                ingest::message(&problems),
            )
        }
        Err(r) => return r,
    };

    let snapshot = match serde_json::to_string(&set) {
        Ok(s) => s,
        Err(e) => return store_failed("writing the set run", e),
    };
    let plan = match serde_json::to_string(&[LabCell {
        model: model.clone(),
        variant: lmgw_api_types::decision_lab::Variant::AsWritten
            .as_str()
            .into(),
    }]) {
        Ok(p) => p,
        Err(e) => return store_failed("writing the set run", e),
    };
    let mapping = serde_json::to_string(&req.mapping).unwrap_or_default();
    let source = match req.source_name.trim() {
        "" => "pasted text",
        s => s,
    }
    .to_string();
    // The writes and the job's start run as a task of their own: a client
    // that goes away half way (a page left during a long write) does not
    // leave a run half written until the next boot, and the run it asked
    // for starts.
    let st = state.clone();
    let written = tokio::spawn(async move {
        let b = NewLabBatch {
            lab: LAB,
            preset_id,
            preset_snapshot: &snapshot,
            model: &model,
            source_name: &source,
            mapping: &mapping,
            concurrency: i64::from(concurrency),
            plan: &plan,
        };
        let id = match store::insert_lab_batch(&st.db, &b, &items).await {
            Ok(Some((id, how))) => {
                tracing::debug!(
                    "decision lab: set run {id} of {} items written in {} transactions, the \
                     longest {:?}",
                    items.len(),
                    how.transactions,
                    how.longest
                );
                id
            }
            Ok(None) => {
                return Err(err_json(
                    StatusCode::CONFLICT,
                    "batch_deleted",
                    "the set run was deleted (a Clear) while its items were written; start it \
                     again",
                ))
            }
            Err(e) => return Err(store_failed("writing the set run", e)),
        };
        let row = match store::lab_batch(&st.db, Some(LAB), id).await {
            Ok(Some(b)) => b,
            Ok(None) => return Err(no_batch(id)),
            Err(e) => return Err(store_failed("reading the set run", e)),
        };
        batch::spawn(&st, &row, concurrency, false)
            .await
            .map_err(|e| store_failed("starting the set run's job", e))?;
        Ok(id)
    })
    .await;
    match written {
        Ok(Ok(id)) => view_of(&state, id).await,
        Ok(Err(r)) => r,
        Err(e) => store_failed("writing the set run", e),
    }
}

/// `GET /decision-lab/api/batches`: every set run, newest first.
pub async fn list(State(state): State<SharedState>) -> Response {
    let read = async {
        let rows = store::lab_batches(&state.db, LAB).await?;
        batch_view::views(&state, rows).await
    };
    match read.await {
        Ok(batches) => Json(BatchList { batches }).into_response(),
        Err(e) => store_failed("reading the set runs", e),
    }
}

/// `GET /decision-lab/api/batches/{id}`.
pub async fn get(State(state): State<SharedState>, ChatPath(id): ChatPath<i64>) -> Response {
    view_of(&state, id).await
}

/// `GET /decision-lab/api/batches/{id}/items`: a page of items in order.
pub async fn items(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatQuery(q): ChatQuery<BatchItemsQuery>,
) -> Response {
    match store::lab_batch(&state.db, Some(LAB), id).await {
        Ok(Some(_)) => {}
        Ok(None) => return no_batch(id),
        Err(e) => return store_failed("reading the set run", e),
    }
    let after = q.after.map(|a| i64::try_from(a).unwrap_or(i64::MAX));
    let read = async {
        let rows = store::lab_batch_items(&state.db, id, after, q.limit).await?;
        let total = store::count_lab_batch_items(&state.db, id).await?;
        let next_after = match rows.last().and_then(|r| r.item_ix) {
            Some(last) if store::lab_batch_item_after(&state.db, id, last).await? => {
                Some(last.max(0) as u64)
            }
            _ => None,
        };
        let mut items = views::groups(&state.db, rows, &|a| views::api_href(a.id)).await?;
        views::mark_in_flight(&mut items, &state.lab_cells);
        Ok::<_, crate::error::GatewayError>(BatchItemPage {
            items,
            total,
            next_after,
        })
    };
    match read.await {
        Ok(page) => Json(page).into_response(),
        Err(e) => store_failed("reading the set run's items", e),
    }
}
