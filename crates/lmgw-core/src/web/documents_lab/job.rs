//! An async run's job (WP12): the route answered 202 with a job; a task
//! holds the cell's claim (the group is in flight) until the job has ended,
//! then keeps the job's result as the run, the same body a sync request gets
//! (`GET /v1/documents/jobs/{id}/result`), with the route's status for it.
//! A cancelled job is kept as its state with 409 `job_cancelled`, as the
//! result route answers it. The page follows the job meanwhile at
//! `/v1/documents/jobs/{id}` and cancels it with `DELETE` there.
//!
//! A gateway that stops while the job runs ends the task with it: the job is
//! `interrupted` after the restart and the group has no run, which the
//! history shows as stopped. Keeping such a job's finished files as the run
//! is not built yet.

use std::time::Instant;

use axum::body::Bytes;
use axum::http::StatusCode;
use lmgw_api_types::documents::jobs::JobState;

use super::kept::{self, Came, Cell};
use crate::extract::convert::jobs::answer;
use crate::labs::claims::CellClaim;
use crate::principal::Principal;
use crate::state::SharedState;

/// Wait for job `job` on a task of its own, then keep its result as `cell`'s
/// run. `accepted` is the route's 202, whose headers the run keeps.
pub(super) fn follow(
    state: SharedState,
    who: Principal,
    cell: Cell,
    accepted: Came,
    job: String,
    started: Instant,
    claim: CellClaim,
) {
    tokio::spawn(async move {
        // Every change until the job ends: the watch's sender goes with it.
        if let Some(mut rx) = state.convert_jobs.watch(&job) {
            while rx.changed().await.is_ok() {}
        }
        let came = result(&state, &who, &job, accepted, started).await;
        kept::keep(&state, cell, came).await;
        drop(claim);
    });
}

/// The ended job's result as the route answers it, read as `who`, the
/// owner who ran the lab (its request was the owner's, security round A).
async fn result(
    state: &SharedState,
    who: &Principal,
    job: &str,
    accepted: Came,
    started: Instant,
) -> Came {
    let mut said = None;
    let (status, text) = match answer::load(state, who, job).await {
        None => {
            let gone = lmgw_api_types::OpenAiError::new(
                format!("conversion job {job} was deleted before its result was read"),
                "invalid_request_error",
                "job_not_found",
            );
            (
                StatusCode::NOT_FOUND,
                serde_json::to_string(&gone).unwrap_or_default(),
            )
        }
        Some(l) if l.state() == JobState::Cancelled => {
            said = Some((
                "job_cancelled".to_string(),
                format!("conversion job {job} was cancelled; its results were deleted"),
            ));
            (
                StatusCode::CONFLICT,
                serde_json::to_string(&answer::status(&l)).unwrap_or_default(),
            )
        }
        Some(l) => {
            let (status, body) = answer::full(state, &l).await;
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
                serde_json::to_string(&body).unwrap_or_default(),
            )
        }
    };
    Came {
        status,
        headers: accepted.headers,
        response_type: Some("application/json".into()),
        bytes: Bytes::from(text),
        read_error: None,
        latency_ms: started.elapsed().as_millis() as i64,
        said,
    }
}
