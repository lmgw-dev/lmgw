//! The native route's async jobs over HTTP (document-conversion design
//! §7.3): `GET /v1/documents/jobs/{id}` (its state), `GET
//! /v1/documents/jobs/{id}/result` (the sync answer once it has ended, or
//! one file's entry with `?index=` as soon as that file has one), `DELETE
//! /v1/documents/jobs/{id}` (cancel a running job: its tools are stopped and
//! its results deleted; an ended job is deleted with its results).
//!
//! A job another key made is a 404, as a job that does not exist; the owner
//! sees every job.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use lmgw_api_types::documents::jobs::{JobFileState, JobResultQuery, JobState};

use crate::error::GatewayError;
use crate::extract::convert::budget::answers;
use crate::extract::convert::jobs::{self, answer, results};
use crate::proxy::RequestCtx;
use crate::state::SharedState;

fn refused(status: u16, code: &'static str, message: String) -> Response {
    super::refusal(&GatewayError::Refused {
        status,
        code,
        message,
    })
}

fn refusal(e: &GatewayError) -> Response {
    (e.http_status(), Json(e.to_openai_json())).into_response()
}

fn not_found(id: &str) -> Response {
    refused(
        404,
        "job_not_found",
        format!("no conversion job {id} for this key: it never existed, was deleted after its retention, or another key made it"),
    )
}

/// `GET /v1/documents/jobs/{id}`.
pub async fn get(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    Path(id): Path<String>,
) -> Response {
    match answer::load(&state, &ctx.principal, &id).await {
        Some(l) => Json(answer::status(&l)).into_response(),
        None => not_found(&id),
    }
}

/// `GET /v1/documents/jobs/{id}/result`.
pub async fn result(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    Path(id): Path<String>,
    Query(q): Query<JobResultQuery>,
    version: axum::http::Version,
    conn: Option<axum::Extension<axum::extract::ConnectInfo<crate::server::Conn>>>,
) -> Response {
    let to = super::send::To::on(conn.as_ref().map(|c| &c.0 .0), version);
    let Some(l) = answer::load(&state, &ctx.principal, &id).await else {
        return not_found(&id);
    };
    let state_now = l.state();
    if state_now == JobState::Cancelled {
        return refused(
            409,
            "job_cancelled",
            format!("conversion job {id} was cancelled; its results were deleted"),
        );
    }
    if let Some(index) = q.index {
        let Some(f) = l.files.iter().find(|f| f.idx == i64::from(index)) else {
            return refused(
                404,
                "file_not_found",
                format!(
                    "conversion job {id} has {} file(s); there is no index {index}",
                    l.files.len()
                ),
            );
        };
        if JobFileState::parse(&f.state) == JobFileState::Pending {
            return refused(
                409,
                "file_pending",
                format!("file {index} of conversion job {id} is not converted yet; ask its state at /v1/documents/jobs/{id}"),
            );
        }
        let share = match hold(&ctx).await {
            Ok(s) => s,
            Err(e) => return refusal(&super::never_fits(&e)),
        };
        let entry = answer::entry(&state, &l, f).await;
        return super::send::json(StatusCode::OK, entry, share, to).await;
    }
    if state_now == JobState::Running {
        return refused(
            409,
            "job_running",
            format!(
                "conversion job {id} is still running ({} of {} files done); fetch a finished file with ?index=, or wait for its state to end",
                l.files.iter().filter(|f| JobFileState::parse(&f.state) != JobFileState::Pending).count(),
                l.files.len()
            ),
        );
    }
    // The stored answer is read into memory: it holds a share of the
    // answers in flight as a sync answer does (`budget::answers`).
    let share = match hold(&ctx).await {
        Ok(s) => s,
        Err(e) => return refusal(&super::never_fits(&e)),
    };
    let (_, body) = answer::full(&state, &l).await;
    super::send::json(StatusCode::OK, body, share, to).await
}

/// A stored answer's share: the output budget, as the caller's.
async fn hold(ctx: &RequestCtx) -> Result<answers::Share, answers::NeverFits> {
    let a = answers::global();
    a.hold(&super::caller(ctx).share, a.output_budget()).await
}

/// `DELETE /v1/documents/jobs/{id}`.
pub async fn delete(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    Path(id): Path<String>,
) -> Response {
    let Some(l) = answer::load(&state, &ctx.principal, &id).await else {
        return not_found(&id);
    };
    if l.state() == JobState::Running && jobs::cancel(&state, &id).await {
        return match answer::load(&state, &ctx.principal, &id).await {
            Some(l) => Json(answer::status(&l)).into_response(),
            None => not_found(&id),
        };
    }
    // An ended job (or one no task runs any more): deleted with its results.
    let before = answer::status(&l);
    if let Err(e) = crate::store::convert_jobs::delete_job(&state.db, &id).await {
        return refused(
            500,
            "internal",
            format!("deleting conversion job {id}: {e}"),
        );
    }
    let gone = results::remove(&state.data_dir, &id).await;
    state.convert_jobs.ledger.sub(gone);
    (StatusCode::OK, Json(before)).into_response()
}
