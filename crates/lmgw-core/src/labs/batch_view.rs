//! A set run as the API's view (labs-store design §3.3): the set as run, its
//! progress from the store, and where it is, told from its job: running
//! while its job is live here, done once every item has a run, else
//! cancelled, interrupted (a restart, a failure, or a job row pruned since)
//! or not started. The job is found by its key (`lab-batch:<id>`), the
//! newest row of it, so a run whose row does not name its job yet (the
//! write after the spawn) still reads it; the row's `job_id` stands in once
//! the job rows are pruned. A run whose live job was asked to cancel says
//! so (`cancel_requested`): the next Cancel aborts the calls in flight.

use lmgw_api_types::decision_lab::batch::JOB_KIND;
use lmgw_api_types::decision_lab::{BatchMapping, BatchStatus, BatchView, QuestionSet};
use lmgw_api_types::lab_store::LabKind;

use super::batch::{self, job_key};
use crate::state::SharedState;
use crate::store::{self, DbResult, LabBatchProgress, LabBatchRow};

/// The set as run.
pub fn snapshot(b: &LabBatchRow) -> QuestionSet {
    serde_json::from_str(&b.preset_snapshot).unwrap_or_default()
}

/// The mapping as run.
pub fn mapping(b: &LabBatchRow) -> BatchMapping {
    serde_json::from_str(&b.mapping).unwrap_or_default()
}

/// Where `b` is: its status, its last job, and that job's error when it
/// failed.
async fn status(
    state: &SharedState,
    b: &LabBatchRow,
    p: &LabBatchProgress,
) -> DbResult<(BatchStatus, Option<i64>, Option<String>)> {
    if let Some(job) = batch::live_job(state, b) {
        return Ok((BatchStatus::Running, Some(job), None));
    }
    let job = store::latest_job_by_key(&state.db, JOB_KIND, &job_key(b.id)).await?;
    let job_id = job.as_ref().map(|j| j.id).or(b.job_id);
    let error = job
        .as_ref()
        .filter(|j| j.status == "failed")
        .and_then(|j| j.error.clone());
    if p.done >= b.item_count.max(0) as u64 {
        return Ok((BatchStatus::Done, job_id, error));
    }
    let status = match (job_id, job.as_ref().map(|j| j.status.as_str())) {
        // No job is known; items with runs say one ran, its row pruned and
        // never named on the run.
        (None, _) if p.done > 0 => BatchStatus::Interrupted,
        (None, _) => BatchStatus::NotStarted,
        (Some(_), Some("canceled")) => BatchStatus::Canceled,
        // Failed, pruned, or a row no job of this gateway runs.
        _ => BatchStatus::Interrupted,
    };
    Ok((status, job_id, error))
}

/// The set runs `rows` as views, in the order given.
pub async fn views(state: &SharedState, rows: Vec<LabBatchRow>) -> DbResult<Vec<BatchView>> {
    let ids: Vec<i64> = rows.iter().map(|b| b.id).collect();
    let mut progress = store::lab_batch_progress(&state.db, &ids).await?;
    let mut out = Vec::with_capacity(rows.len());
    for b in rows {
        let p = progress.remove(&b.id).unwrap_or_default();
        let (status, job_id, job_error) = status(state, &b, &p).await?;
        // A Cancel went to the job running it: the next one aborts.
        let cancel_requested = status == BatchStatus::Running
            && job_id.is_some_and(|j| state.jobs.cancel_requested(j) == Some(true));
        let preset_name = match b.preset_id {
            Some(id) => store::lab_preset(&state.db, None, id)
                .await?
                .map(|r| r.name),
            None => None,
        };
        out.push(BatchView {
            id: b.id,
            lab: LabKind::parse(&b.lab).unwrap_or(LabKind::Decision),
            preset_id: b.preset_id,
            preset_name,
            set: snapshot(&b),
            model: b.model.clone(),
            source_name: b.source_name.clone(),
            mapping: mapping(&b),
            concurrency: u32::try_from(b.concurrency).unwrap_or(1),
            status,
            total: b.item_count.max(0) as u64,
            done: p.done,
            failed: p.failed,
            unreadable: p.unreadable,
            labelled: p.labelled,
            job_id,
            job_error,
            cancel_requested,
            created_at: b.created_at,
        });
    }
    Ok(out)
}

/// Set run `id` of `lab` as its view; `None` when there is none.
pub async fn one(state: &SharedState, lab: LabKind, id: i64) -> DbResult<Option<BatchView>> {
    let Some(b) = store::lab_batch(&state.db, Some(lab), id).await? else {
        return Ok(None);
    };
    Ok(views(state, vec![b]).await?.pop())
}
