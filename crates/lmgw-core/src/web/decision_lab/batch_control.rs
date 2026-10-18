//! A set run's controls (labs-store design §3.3): Cancel (the job stops
//! after the calls in flight; a second Cancel while it still runs drops
//! them), Resume (a new job for the items without a run, after a cancel, a
//! refusal or a restart, and with `retry_failed` for the items whose run
//! failed) and Delete (the run, its items, their runs and files; a live
//! job's calls in flight are dropped first).

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::decision_lab::{BatchResume, BatchStatus};
use lmgw_api_types::lab_store::LabKind;

use super::super::chat::err_json;
use super::super::chat_extract::{ChatOptJson, ChatPath};
use super::super::labs_store::store_failed;
use super::batches::{no_batch, view_of};
use super::groups::bad;
use crate::labs::{self, batch, batch_view};
use crate::state::SharedState;
use crate::store;

const LAB: LabKind = LabKind::Decision;

/// `POST /decision-lab/api/batches/{id}/cancel`.
pub async fn cancel(State(state): State<SharedState>, ChatPath(id): ChatPath<i64>) -> Response {
    let b = match store::lab_batch(&state.db, Some(LAB), id).await {
        Ok(Some(b)) => b,
        Ok(None) => return no_batch(id),
        Err(e) => return store_failed("reading the set run", e),
    };
    let Some(job) = batch::live_job(&state, &b) else {
        return err_json(
            StatusCode::CONFLICT,
            "batch_not_running",
            format!("set run {id} is not running"),
        );
    };
    // The first Cancel is graceful: the calls in flight finish and are kept.
    // A second one while the job still runs drops them, as a call can hang.
    if state.jobs.cancel_requested(job) == Some(true) {
        state.jobs.request_abort(job);
    } else {
        state.jobs.request_cancel(job);
    }
    view_of(&state, id).await
}

/// `POST /decision-lab/api/batches/{id}/resume`.
pub async fn resume(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatOptJson(req): ChatOptJson<BatchResume>,
) -> Response {
    let view = match batch_view::one(&state, LAB, id).await {
        Ok(Some(v)) => v,
        Ok(None) => return no_batch(id),
        Err(e) => return store_failed("reading the set run", e),
    };
    match view.status {
        BatchStatus::Running => {
            return err_json(
                StatusCode::CONFLICT,
                "batch_running",
                format!("set run {id} is running"),
            )
        }
        BatchStatus::Done if !(req.retry_failed && view.failed > 0) => {
            return err_json(
                StatusCode::CONFLICT,
                "batch_done",
                format!(
                    "every item of set run {id} has a run{}; a new run is a new set run",
                    if req.retry_failed {
                        " and none failed"
                    } else {
                        " (retry_failed asks the failed ones again)"
                    }
                ),
            )
        }
        _ => {}
    }
    let concurrency = req.concurrency.unwrap_or(view.concurrency);
    if concurrency == 0 {
        return bad("concurrency: at least 1");
    }
    let b = match store::lab_batch(&state.db, Some(LAB), id).await {
        Ok(Some(b)) => b,
        Ok(None) => return no_batch(id),
        Err(e) => return store_failed("reading the set run", e),
    };
    match batch::spawn(&state, &b, concurrency, req.retry_failed).await {
        Ok(batch::Started::Started(_)) => view_of(&state, id).await,
        Ok(batch::Started::Running(job)) => err_json(
            StatusCode::CONFLICT,
            "batch_running",
            format!("set run {id} is running (job {job})"),
        ),
        Err(e) => store_failed("starting the set run's job", e),
    }
}

/// `POST /decision-lab/api/batches/{id}/delete`: answers the lab's usage
/// after it.
pub async fn delete(State(state): State<SharedState>, ChatPath(id): ChatPath<i64>) -> Response {
    let b = match store::lab_batch(&state.db, Some(LAB), id).await {
        Ok(Some(b)) => b,
        Ok(None) => return no_batch(id),
        Err(e) => return store_failed("reading the set run", e),
    };
    batch::abort_live(&state, std::slice::from_ref(&b));
    match labs::delete::delete_batch(&state.db, &state.data_dir, LAB, id).await {
        Ok(0) => no_batch(id),
        Ok(_) => match labs::usage::usage(&state.db, &state.data_dir, LAB).await {
            Ok(u) => Json(u).into_response(),
            Err(e) => store_failed("reading the lab's usage", e),
        },
        Err(e) => store_failed("deleting the set run", e),
    }
}
