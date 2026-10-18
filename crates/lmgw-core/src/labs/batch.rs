//! The set-run job (labs-store design §3.3): `JobKind::LabBatch`, keyed
//! `lab-batch:<id>`, so one job runs a set run at a time. The items and
//! their answers live in the lab store and the job only executes, because
//! job rows are pruned: the job answers the items that have no run (and with
//! `retry_failed` those whose run failed), through the same path as a lab
//! cell (`web::decision_lab::cells::answer`: the claim, the build, the
//! in-process `/v1/systemone` dispatch, the record), up to `concurrency` at
//! once. Admission and the model's slots are the only other bound.
//!
//! Cancel stops after the calls in flight; a second Cancel, a Delete and a
//! Clear abort them (`JobManager::request_abort`): their handler futures are
//! dropped, which writes the request log's 499 rows as the page's Stop does,
//! and their items stay without a run. A refusal lmgw gives about its own
//! state before any model answers (the GPU hold, admission, the container's
//! start) is no answer of the item: it is not recorded, no new item is
//! taken, and the job ends failed with that reason, so the run reads
//! interrupted and Resume continues once the cause is gone. A gateway
//! restart marks the job failed (`state.rs`, the boot's
//! `fail_orphaned_jobs`); the run then reads as interrupted, and Resume is a
//! new job for the rest.

pub use lmgw_api_types::decision_lab::batch::job_key;
use lmgw_api_types::decision_lab::BatchJobDetail as Detail;
use lmgw_api_types::lab_store::LabKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinSet;

use crate::erased::Erased;
use crate::jobs::{self, JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress, Spawn};
use crate::state::SharedState;
use crate::store::{self, LabBatchRow};
use crate::web::decision_lab::cells::{self, AnswerMode, CellRefused};

/// What the job is asked.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Input {
    batch_id: i64,
    concurrency: u32,
    /// Ask the items whose run failed again too, replacing that run.
    #[serde(default)]
    retry_failed: bool,
}

/// How a start or resume went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Started {
    Started(i64),
    /// A job runs the set run already; its id.
    Running(i64),
}

/// Start a job for set run `b`'s items without a run (with `retry_failed`,
/// and those whose run failed), `concurrency` at once.
///
/// The job's id and the concurrency are written onto the run after the job
/// is live. The run's status reads its job by its key (`lab-batch:<id>`),
/// not by that column, so a failure to write it costs nothing while the
/// job's row is kept and is logged rather than answered as a failed start
/// of a job that runs.
pub async fn spawn(
    state: &SharedState,
    b: &LabBatchRow,
    concurrency: u32,
    retry_failed: bool,
) -> Result<Started, String> {
    let label = format!("set run {} on {} ({} items)", b.id, b.model, b.item_count);
    let input = serde_json::to_value(Input {
        batch_id: b.id,
        concurrency,
        retry_failed,
    })
    .map_err(|e| e.to_string())?;
    match jobs::spawn(state, JobKind::LabBatch, Some(job_key(b.id)), label, input).await? {
        Spawn::Started(job) => {
            if let Err(e) =
                store::set_lab_batch_job(&state.db, b.id, job, Some(i64::from(concurrency))).await
            {
                tracing::warn!(
                    "decision lab: set run {} runs as job {job}, which its row does not name \
                     ({e}); its status reads the job by its key",
                    b.id
                );
            }
            Ok(Started::Started(job))
        }
        Spawn::AlreadyRunning(job) => Ok(Started::Running(job)),
    }
}

/// The live job running set run `b`, if one does in this gateway.
pub fn live_job(state: &SharedState, b: &LabBatchRow) -> Option<i64> {
    state
        .jobs
        .live_by_key(JobKind::LabBatch, &job_key(b.id))
        .map(|j| j.id)
}

/// Ask the live jobs of `batches` to stop and to drop the calls in flight:
/// a Delete and a Clear, which should not wait for a call that may hang.
pub fn abort_live(state: &SharedState, batches: &[LabBatchRow]) {
    for b in batches {
        if let Some(job) = live_job(state, b) {
            state.jobs.request_abort(job);
        }
    }
}

/// How one item went.
enum ItemAnswer {
    /// Its run is kept: whether the route answered other than 2xx, whether
    /// a 2xx does not read as an answer to every question, and whether it
    /// replaced a failed run.
    Kept {
        failed: bool,
        unreadable: bool,
        replaced: bool,
    },
    /// Its cell has a run already, or is being answered elsewhere.
    Elsewhere,
    /// Not answered, or answered and not kept: why.
    NotKept(String),
    /// An answer that would be the same for every item (lmgw refused, or the
    /// upstream refused its credential): why. The
    /// item has no new run.
    Refused(String),
}

/// Answer item group `id` of `lab`: its one cell, through the lab cell's
/// path, in the job's mode; `failed_run`, it has a failed run to replace.
async fn item(
    state: &SharedState,
    lab: LabKind,
    (id, failed_run): (i64, bool),
    mode: AnswerMode,
) -> ItemAnswer {
    let g = match store::lab_group(&state.db, lab, id).await {
        Ok(Some(g)) => g,
        Ok(None) => return ItemAnswer::NotKept(format!("item group {id} is gone")),
        Err(e) => return ItemAnswer::NotKept(format!("reading item group {id} failed: {e}")),
    };
    let ix = g.item_ix.unwrap_or_default() + 1;
    match cells::answer(state, &g, 0, mode).await {
        Ok(a) => match a.result.store_error {
            None => ItemAnswer::Kept {
                failed: !(200..300).contains(&a.result.run.http_status),
                unreadable: a.readable == Some(false),
                replaced: failed_run,
            },
            Some(e) => ItemAnswer::NotKept(e),
        },
        Err(CellRefused::Answered(_)) => ItemAnswer::Elsewhere,
        Err(e @ CellRefused::Gateway { .. }) => ItemAnswer::Refused(format!("item {ix}: {e}")),
        Err(e) => ItemAnswer::NotKept(e.to_string()),
    }
}

async fn report(ctx: &JobCtx, d: &mut Detail) {
    // The frame that first carries a raised flag skips the publish throttle:
    // a call in flight may hang, and no later report would send it.
    let raised = ctx.canceled() && !d.cancel_requested;
    d.cancel_requested = ctx.canceled();
    let p = JobProgress {
        done: d.done,
        total: Some(d.total),
        stage: "answering".into(),
        detail: serde_json::to_value(d).unwrap_or(Value::Null),
    };
    if raised {
        ctx.progress_now(p).await;
    } else {
        ctx.progress(p).await;
    }
}

/// Resolves once the job's cancel flag is up.
async fn cancel_news(ctx: &JobCtx) {
    while !ctx.canceled() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn progress_of(state: &SharedState, id: i64) -> Result<store::LabBatchProgress, String> {
    Ok(store::lab_batch_progress(&state.db, &[id])
        .await
        .map_err(|e| e.to_string())?
        .remove(&id)
        .unwrap_or_default())
}

/// How the loop over the items ended, before the store's count.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    /// Every item it took was answered (or found answered).
    Through,
    /// A cancel: the calls in flight finished.
    Canceled,
    /// An abort: the calls in flight were dropped.
    Aborted,
    /// lmgw refused an item before asking the model: why.
    Refused(String),
}

async fn run(ctx: &JobCtx, input: Input) -> Result<JobOutcome, String> {
    let state = &ctx.state;
    let b = store::lab_batch(&state.db, None, input.batch_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("set run {} no longer exists", input.batch_id))?;
    let lab = LabKind::parse(&b.lab).unwrap_or(LabKind::Decision);
    let pending = store::lab_batch_pending(&state.db, b.id, input.retry_failed)
        .await
        .map_err(|e| e.to_string())?;
    let p = progress_of(state, b.id).await?;
    let mut d = Detail {
        batch_id: b.id,
        total: b.item_count.max(0) as u64,
        done: p.done,
        failed: p.failed,
        unreadable: p.unreadable,
        not_kept: 0,
        retry_failed: input.retry_failed,
        cancel_requested: false,
    };
    report(ctx, &mut d).await;

    // Each item is a task of its own, so one that holds a database
    // connection mid-write keeps running while this loop writes the
    // progress; items polled as one stream would wait for the loop, and the
    // loop for their connection. A new item is taken only while no cancel
    // is asked and nothing was refused: the calls in flight finish and are
    // kept, unless an abort drops them.
    let mode = AnswerMode {
        unrecorded_gateway_refusals: true,
        replace_failed: input.retry_failed,
    };
    let concurrency = input.concurrency.max(1) as usize;
    let mut pending = pending.into_iter();
    let mut flight: JoinSet<ItemAnswer> = JoinSet::new();
    let mut why: Vec<String> = Vec::new();
    let mut refused: Option<String> = None;
    let mut aborted = std::pin::pin!(ctx.aborted());
    let ended = loop {
        while flight.len() < concurrency && !ctx.canceled() && refused.is_none() {
            let Some(next) = pending.next() else {
                break;
            };
            let state = state.clone();
            flight.spawn(async move { item(&state, lab, next, mode).await });
        }
        let joined = tokio::select! {
            biased;
            () = &mut aborted => {
                flight.abort_all();
                while flight.join_next().await.is_some() {}
                break Ended::Aborted;
            }
            // A cancel raised from outside is news of its own: the calls in
            // flight may hang, and no item would land to carry it.
            () = cancel_news(ctx), if !d.cancel_requested => {
                report(ctx, &mut d).await;
                continue;
            }
            joined = flight.join_next() => joined,
        };
        let Some(joined) = joined else {
            break match refused.take() {
                Some(r) => Ended::Refused(r),
                None if ctx.canceled() => Ended::Canceled,
                None => Ended::Through,
            };
        };
        let a = joined.unwrap_or_else(|e| ItemAnswer::NotKept(format!("an item's task: {e}")));
        match a {
            ItemAnswer::Kept {
                failed,
                unreadable,
                replaced,
            } => {
                // A replaced run was counted, as a failed one.
                if replaced {
                    d.failed = d.failed.saturating_sub(1);
                } else {
                    d.done += 1;
                }
                d.failed += u64::from(failed);
                d.unreadable += u64::from(unreadable);
            }
            ItemAnswer::Elsewhere => {}
            ItemAnswer::NotKept(m) => {
                d.not_kept += 1;
                if !why.contains(&m) {
                    why.push(m);
                }
            }
            ItemAnswer::Refused(m) => {
                refused.get_or_insert(m);
            }
        }
        report(ctx, &mut d).await;
    };

    // The store's count, items another post answered included (and after a
    // Delete, nothing).
    let p = progress_of(state, b.id).await?;
    d.done = p.done;
    d.failed = p.failed;
    d.unreadable = p.unreadable;
    let value = serde_json::to_value(&d).unwrap_or(Value::Null);
    let left = d.total.saturating_sub(d.done);
    match ended {
        Ended::Aborted => Ok(JobOutcome::CanceledWith(value)),
        Ended::Refused(reason) => Ok(JobOutcome::FailedWith {
            error: if input.retry_failed {
                format!(
                    "stopped at {reason}; Resume with \"retry failed\" runs the items without a \
                     run, and those with a failed one, once the cause is gone"
                )
            } else {
                format!(
                    "stopped at {reason}; {left} of {} items have no run: Resume runs the rest \
                     once the cause is gone",
                    d.total
                )
            },
            value,
        }),
        _ if left == 0 && why.is_empty() => Ok(JobOutcome::Done(value)),
        Ended::Canceled => Ok(JobOutcome::CanceledWith(value)),
        Ended::Through if left == 0 => Ok(JobOutcome::FailedWith {
            error: format!(
                "{} item(s) could not be asked again: {}",
                d.not_kept,
                why.join("; ")
            ),
            value,
        }),
        Ended::Through => {
            let why = if why.is_empty() {
                "they were being answered elsewhere".to_string()
            } else {
                why.join("; ")
            };
            Ok(JobOutcome::FailedWith {
                error: format!("{left} of {} items have no run: {why}", d.total),
                value,
            })
        }
    }
}

pub struct LabBatchExecutor;

impl JobExecutor for LabBatchExecutor {
    fn kind(&self) -> JobKind {
        JobKind::LabBatch
    }

    fn run<'a>(&'a self, ctx: JobCtx, input: Value) -> Erased<'a, Result<JobOutcome, String>> {
        Box::pin(async move {
            let input: Input =
                serde_json::from_value(input).map_err(|e| format!("lab_batch input: {e}"))?;
            run(&ctx, input).await
        })
    }
}
