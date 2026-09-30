//! Background jobs (quickdoc design §9c): one subsystem for every long-running
//! unit of work the gateway owns — persistent rows, typed progress, cancel, and
//! a single live feed the dashboard consumes.
//!
//! The shape it replaces was a per-feature `HashMap<i64, Progress>` behind a
//! `Mutex` next to a bespoke `tokio::spawn` (`hf.rs`'s download manager). That
//! works exactly once; ingestion, re-embedding and eval runs would each have
//! grown their own copy, each with its own progress type, its own SSE frame and
//! no cancel at all.
//!
//! **Split of duties.** The `jobs` row is the durable half: what was asked for,
//! how it ended, when. [`JobManager`] is the live half: the byte counters that
//! move hundreds of times a second, plus the cancel flag. Progress is written
//! back to the row on a throttle ([`PUBLISH_INTERVAL`]) so a finished or
//! interrupted job still reports where it got to.
//!
//! **Adding a kind** is a [`JobKind`] variant, one [`JobExecutor`] impl, and one
//! [`JobManager::register`] call. Nothing here needs to know what the kind does:
//! its request payload and its progress detail are both the executor's own
//! serde types, erased to JSON at this boundary. The generic feed still renders
//! it, because every job reports the same three universal things — units done,
//! units total (when knowable), and what stage it is in.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::SharedState;
use crate::store;

pub mod hf_download;

/// How often a running job's progress is written to its row and pushed to the
/// live feed. Progress callbacks fire per network chunk — thousands per second
/// on a fast link — so they are coalesced here rather than at each call site.
/// This is a sampling rate, not a cap: the in-memory snapshot is updated on
/// every callback, and every *status* transition publishes immediately.
pub const PUBLISH_INTERVAL: Duration = Duration::from_millis(500);

/// The job kinds the gateway knows about. Every one of them has an executor;
/// [`spawn`] still reports a missing one rather than silently ignoring it,
/// because registration is a runtime map a test may replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    HfDownload,
    Ingest,
    ReEmbed,
    EvalRun,
    GoldenGen,
    /// One run of one batch agent (agent-catalog §2.4): its key is
    /// `agent:<id>`, so the partial unique index on `(kind, key)` gives "one
    /// live run per agent" for free.
    AgentRun,
    /// One run of one container build (container-builds §5): its key is
    /// `build:<id>`, so the same index gives "one live run per build". Runs of
    /// *different* builds are serialized machine-wide by the executor's flock,
    /// not here — a run waiting for it is still `running`, with phase
    /// `waiting` in its detail.
    BuildRun,
    /// `podman pull` of a registry image in use (container-builds §8 **Pull
    /// update**): key `image:<reference>`, so one pull per reference at a
    /// time.
    ImagePull,
    /// One benchmark run (benchmark design §3.1): key `gpu` for every run,
    /// so the same index gives "one run at a time" — a second start is
    /// refused while one holds the card.
    Benchmark,
    /// Ingest a knowledge base's pending files (chat-complete design §9.2):
    /// key `kb:<id>`, so one live ingest per base.
    KbIngest,
    /// Re-embed a knowledge base (§9.2): key `kb:<id>` too — the knowledge
    /// ops keep the two kinds from running on one base at once.
    KbReembed,
}

impl JobKind {
    pub const ALL: [JobKind; 11] = [
        Self::HfDownload,
        Self::Ingest,
        Self::ReEmbed,
        Self::EvalRun,
        Self::GoldenGen,
        Self::AgentRun,
        Self::BuildRun,
        Self::ImagePull,
        Self::Benchmark,
        Self::KbIngest,
        Self::KbReembed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::HfDownload => "hf_download",
            Self::Ingest => "ingest",
            Self::ReEmbed => "re_embed",
            Self::EvalRun => "eval_run",
            Self::GoldenGen => "golden_gen",
            // Exactly `crate::agents::JOB_KIND`, which named this string
            // before there was an executor behind it; the assertion is in
            // `agents::batch`'s tests.
            Self::AgentRun => crate::agents::JOB_KIND,
            Self::BuildRun => "build_run",
            Self::ImagePull => "image_pull",
            // `lmgw_api_types::bench_ops::JOB_KIND`, which the dashboard
            // filters the live feed by.
            Self::Benchmark => lmgw_api_types::bench_ops::JOB_KIND,
            Self::KbIngest => "kb_ingest",
            Self::KbReembed => "kb_reembed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

impl std::fmt::Display for JobKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A progress report: three universal fields every consumer can render, plus
/// the kind's own typed payload.
///
/// The unit of `done`/`total` belongs to the kind (bytes for a download,
/// documents for an ingest, queries for an eval run) and `stage` names it in
/// passing. `total` is `Option` because "unknown" is a real answer — a server
/// that sends no `Content-Length` gets `None`, never an invented estimate.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobProgress {
    pub done: u64,
    pub total: Option<u64>,
    /// One short line: what the job is doing right now.
    pub stage: String,
    /// Kind-specific detail — the executor's own serde struct, erased to JSON.
    pub detail: Value,
}

impl JobProgress {
    pub fn percent(&self) -> Option<u64> {
        self.total
            .filter(|t| *t > 0)
            .map(|t| self.done.min(t) * 100 / t)
    }
}

/// A job as the API and the live feed see it: the durable row flattened
/// together with its progress.
#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    pub id: i64,
    pub kind: String,
    pub key: Option<String>,
    pub label: String,
    pub status: String,
    pub done: u64,
    pub total: Option<u64>,
    pub percent: Option<u64>,
    pub stage: String,
    pub detail: Value,
    pub error: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

impl JobView {
    pub fn from_row(row: &store::JobRow) -> Self {
        let p: JobProgress = serde_json::from_str(&row.progress).unwrap_or_default();
        Self {
            id: row.id,
            kind: row.kind.clone(),
            key: row.key.clone(),
            label: row.label.clone(),
            status: row.status.clone(),
            done: p.done,
            total: p.total,
            percent: p.percent(),
            stage: p.stage.clone(),
            detail: p.detail.clone(),
            error: row.error.clone(),
            created_at: row.created_at.clone(),
            started_at: row.started_at.clone(),
            finished_at: row.finished_at.clone(),
        }
    }

    fn apply(&mut self, p: &JobProgress) {
        self.done = p.done;
        self.total = p.total;
        self.percent = p.percent();
        self.stage = p.stage.clone();
        self.detail = p.detail.clone();
    }

    fn progress(&self) -> JobProgress {
        JobProgress {
            done: self.done,
            total: self.total,
            stage: self.stage.clone(),
            detail: self.detail.clone(),
        }
    }
}

/// How a job ended. Every variant but [`Self::Done`] is still an `Ok` return
/// from the executor: "it stopped early" and "it broke on the way out" are
/// outcomes the executor reports, not errors it failed to handle. A bare `Err`
/// remains the shorthand for a failure with nothing to keep.
#[derive(Debug, Clone, PartialEq)]
pub enum JobOutcome {
    /// Finished; the value is the kind's terminal payload, stored on the row.
    Done(Value),
    /// Stopped because [`JobCtx::canceled`] went true. The executor decides
    /// what to clean up before returning this.
    Canceled,
    /// Cancelled, but with work worth keeping. The row still ends `canceled`;
    /// the value is stored exactly as [`Self::Done`]'s is.
    ///
    /// A cancelled agent run keeps the rows it had classified and a cancelled
    /// apply reports what it wrote (agent-catalog §4.1) — throwing that away
    /// because the owner pressed Cancel would make Cancel destructive, which
    /// is the opposite of what it is for.
    CanceledWith(Value),
    /// Failed, but with work already done that the owner has to see. The row
    /// ends `failed` and carries `error` exactly as an `Err` return does — the
    /// difference is that `value` is stored alongside it instead of the result
    /// column being left NULL.
    ///
    /// An apply that wrote eight labels and then lost the connection on its
    /// final answer *did write eight labels*. Reporting only the message would
    /// tell the owner what broke and not what it changed, which is the one
    /// thing they need before deciding whether to retry (agent-catalog §4.1,
    /// §4.6).
    FailedWith { error: String, value: Value },
}

/// Result of [`spawn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spawn {
    Started(i64),
    /// A job with the same `(kind, key)` is already in flight; its id.
    AlreadyRunning(i64),
}

impl Spawn {
    pub fn id(self) -> i64 {
        match self {
            Self::Started(id) | Self::AlreadyRunning(id) => id,
        }
    }
}

/// Handle an executor uses to talk back to the subsystem.
pub struct JobCtx {
    pub state: SharedState,
    pub id: i64,
    cancel: Arc<AtomicBool>,
}

impl JobCtx {
    /// Cancellation is cooperative: an executor polls this at whatever boundary
    /// it can stop cleanly at (per chunk, per document, per query) and returns
    /// [`JobOutcome::Canceled`]. Aborting the task instead would leave
    /// half-written files and rows behind with nothing to undo them.
    pub fn canceled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// The flag itself, for an await that has to be *raced* against it rather
    /// than polled at a boundary — a model call that runs for minutes is the
    /// case [`Self::canceled`] alone cannot answer (agent-catalog §4.1). Shared,
    /// not copied: raising it here is seen there.
    pub fn cancel_signal(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    pub async fn progress(&self, p: JobProgress) {
        let state = self.state.clone();
        state.jobs.note_progress(&state, self.id, p).await;
    }

    /// Re-report the job's **current** counters with a new `detail`.
    ///
    /// For news that lives in the payload rather than in `done`/`total`: an
    /// agent run whose container is still pulling, printing diagnostics or
    /// logging its way through startup has produced no row yet, so the frame
    /// never changed and a dashboard watching `(id, done, status)` had nothing
    /// to re-read — the Run tab stayed empty for exactly the phase its owner
    /// needs it (container-runtime §3.2, final review). Sending a
    /// [`JobProgress::default`] instead would reset the counters to zero, which
    /// is why this reads them back off the live entry.
    ///
    /// Still throttled by [`PUBLISH_INTERVAL`] like every other report, so a
    /// chatty container costs two frames a second whatever it does.
    pub async fn progress_detail(&self, detail: Value) {
        let state = self.state.clone();
        state.jobs.note_detail(&state, self.id, detail).await;
    }

    /// A context with no live job behind it, for unit-testing an executor's
    /// pipeline directly.
    ///
    /// [`Self::progress`] then finds no live entry and returns without writing
    /// anything — which is exactly right when the jobs subsystem is not what is
    /// under test — while [`Self::canceled`] still follows the flag the test
    /// holds, so cancellation stays exercisable.
    #[cfg(test)]
    pub(crate) fn detached(state: SharedState, id: i64, cancel: Arc<AtomicBool>) -> Self {
        Self { state, id, cancel }
    }
}

/// What a job kind actually does.
#[async_trait::async_trait]
pub trait JobExecutor: Send + Sync + 'static {
    fn kind(&self) -> JobKind;

    /// `input` is the payload passed to [`spawn`], the executor's own type.
    /// Returning `Err` marks the job failed with that message.
    async fn run(&self, ctx: JobCtx, input: Value) -> Result<JobOutcome, String>;
}

struct Live {
    view: JobView,
    cancel: Arc<AtomicBool>,
    last_publish: Instant,
}

/// Executor registry plus the live state of running jobs.
pub struct JobManager {
    executors: RwLock<HashMap<JobKind, Arc<dyn JobExecutor>>>,
    live: Mutex<HashMap<i64, Live>>,
}

impl Default for JobManager {
    fn default() -> Self {
        Self::new()
    }
}

impl JobManager {
    pub fn new() -> Self {
        let m = Self {
            executors: RwLock::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
        };
        m.register(Arc::new(hf_download::HfDownloadExecutor));
        m.register(Arc::new(crate::quickdoc::ingest::IngestExecutor));
        m.register(Arc::new(crate::quickdoc::reembed::ReEmbedExecutor));
        m.register(Arc::new(crate::quickdoc::eval::EvalRunExecutor));
        m.register(Arc::new(crate::quickdoc::golden::GoldenGenExecutor));
        m.register(Arc::new(crate::agents::batch::AgentRunExecutor));
        m.register(Arc::new(crate::backends::run::BuildRunExecutor));
        m.register(Arc::new(crate::backends::pull::ImagePullExecutor));
        m.register(Arc::new(crate::bench::BenchmarkExecutor));
        m.register(Arc::new(crate::knowledge::ingest::KbIngestExecutor));
        m.register(Arc::new(crate::knowledge::reembed::KbReembedExecutor));
        m
    }

    /// Register (or replace) the executor for a kind. Runtime rather than
    /// compile-time registration so a later crate — or a test with a fake —
    /// can supply one without this module knowing about it.
    pub fn register(&self, exec: Arc<dyn JobExecutor>) {
        self.executors.write().unwrap().insert(exec.kind(), exec);
    }

    fn executor(&self, kind: JobKind) -> Option<Arc<dyn JobExecutor>> {
        self.executors.read().unwrap().get(&kind).cloned()
    }

    /// Kinds that currently have an executor, sorted by name.
    pub fn registered_kinds(&self) -> Vec<JobKind> {
        let mut v: Vec<JobKind> = self.executors.read().unwrap().keys().copied().collect();
        v.sort_by_key(|k| k.as_str());
        v
    }

    /// Every running job, oldest first.
    pub fn live(&self) -> Vec<JobView> {
        let mut v: Vec<JobView> = self
            .live
            .lock()
            .unwrap()
            .values()
            .map(|l| l.view.clone())
            .collect();
        v.sort_by_key(|j| j.id);
        v
    }

    pub fn live_of_kind(&self, kind: JobKind) -> Vec<JobView> {
        let mut v = self.live();
        v.retain(|j| j.kind == kind.as_str());
        v
    }

    pub fn live_by_key(&self, kind: JobKind, key: &str) -> Option<JobView> {
        self.live
            .lock()
            .unwrap()
            .values()
            .find(|l| l.view.kind == kind.as_str() && l.view.key.as_deref() == Some(key))
            .map(|l| l.view.clone())
    }

    pub fn live_one(&self, id: i64) -> Option<JobView> {
        self.live.lock().unwrap().get(&id).map(|l| l.view.clone())
    }

    /// Raise the cancel flag; `false` when no job with that id is running here.
    pub fn request_cancel(&self, id: i64) -> bool {
        match self.live.lock().unwrap().get(&id) {
            Some(l) => {
                l.cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    fn broadcast(&self, state: &SharedState) {
        state.telemetry.jobs(self.live());
    }

    async fn note_progress(&self, state: &SharedState, id: i64, p: JobProgress) {
        let due = {
            let mut live = self.live.lock().unwrap();
            let Some(entry) = live.get_mut(&id) else {
                return;
            };
            entry.view.apply(&p);
            let due = entry.last_publish.elapsed() >= PUBLISH_INTERVAL;
            if due {
                entry.last_publish = Instant::now();
            }
            due
        };
        if due {
            self.persist_progress(state, id, &p).await;
            self.broadcast(state);
        }
    }

    /// [`note_progress`](Self::note_progress) with only the `detail` replaced —
    /// the counters come off the live entry, so a payload-only report cannot
    /// rewind `done`. A job that is no longer live is a no-op, as everywhere.
    async fn note_detail(&self, state: &SharedState, id: i64, detail: Value) {
        let p = {
            let live = self.live.lock().unwrap();
            let Some(entry) = live.get(&id) else {
                return;
            };
            let mut p = entry.view.progress();
            p.detail = detail;
            p
        };
        self.note_progress(state, id, p).await;
    }

    async fn persist_progress(&self, state: &SharedState, id: i64, p: &JobProgress) {
        let json = serde_json::to_string(p).unwrap_or_else(|_| "{}".into());
        if let Err(e) = store::set_job_progress(&state.db, id, &json).await {
            tracing::warn!("recording progress for job {id}: {e}");
        }
    }
}

/// Queue and start a job. Returns [`Spawn::AlreadyRunning`] — not an error —
/// when `key` names a job that is already in flight, which is the duplicate
/// guard callers like the download queue rely on.
pub async fn spawn(
    state: &SharedState,
    kind: JobKind,
    key: Option<String>,
    label: String,
    input: Value,
) -> Result<Spawn, String> {
    let exec = state
        .jobs
        .executor(kind)
        .ok_or_else(|| format!("job kind '{kind}' has no executor in this build"))?;
    let input_json = serde_json::to_string(&input).map_err(|e| e.to_string())?;

    // A claim that finds the key taken looks the holder up — and the holder
    // may finish between the two statements. Then nothing holds the key any
    // more and the claim is simply tried again (three tries in all: a miss
    // every time means other spawns keep winning the key in that instant).
    let mut attempt = 0;
    let id = loop {
        attempt += 1;
        let claimed = store::claim_job(
            &state.db,
            kind.as_str(),
            key.as_deref(),
            &label,
            &input_json,
        )
        .await
        .map_err(|e| e.to_string())?;
        if let Some(id) = claimed {
            break id;
        }
        let key = key.clone().unwrap_or_default();
        match store::active_job_by_key(&state.db, kind.as_str(), &key)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(existing) => return Ok(Spawn::AlreadyRunning(existing.id)),
            None if attempt < 3 => continue,
            None => return Err(format!("'{kind}' job for {key} could not be claimed")),
        }
    };

    store::start_job(&state.db, id)
        .await
        .map_err(|e| e.to_string())?;
    let row = store::get_job(&state.db, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("job {id} vanished between insert and start"))?;

    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut live = state.jobs.live.lock().unwrap();
        live.insert(
            id,
            Live {
                view: JobView::from_row(&row),
                cancel: cancel.clone(),
                // Backdated so the executor's first progress report publishes
                // at once — that is the one carrying the total.
                last_publish: Instant::now()
                    .checked_sub(PUBLISH_INTERVAL)
                    .unwrap_or_else(Instant::now),
            },
        );
    }
    state.jobs.broadcast(state);

    let st = state.clone();
    tokio::spawn(async move {
        let ctx = JobCtx {
            state: st.clone(),
            id,
            cancel,
        };
        let (status, result, error) = match exec.run(ctx, input).await {
            Ok(JobOutcome::Done(v)) => (
                "done",
                Some(serde_json::to_string(&v).unwrap_or_else(|_| "null".into())),
                None,
            ),
            Ok(JobOutcome::Canceled) => ("canceled", None, None),
            Ok(JobOutcome::CanceledWith(v)) => (
                "canceled",
                Some(serde_json::to_string(&v).unwrap_or_else(|_| "null".into())),
                None,
            ),
            Ok(JobOutcome::FailedWith { error, value }) => {
                tracing::warn!("job {id} ({kind}) failed: {error}");
                (
                    "failed",
                    Some(serde_json::to_string(&value).unwrap_or_else(|_| "null".into())),
                    Some(error),
                )
            }
            Err(e) => {
                tracing::warn!("job {id} ({kind}) failed: {e}");
                ("failed", None, Some(e))
            }
        };
        // Flush the last progress the throttle may have swallowed, so a
        // finished row shows where it actually got to.
        let final_progress = st.jobs.live_one(id).map(|v| v.progress());
        if let Some(p) = final_progress {
            st.jobs.persist_progress(&st, id, &p).await;
        }
        if let Err(e) =
            store::finish_job(&st.db, id, status, result.as_deref(), error.as_deref()).await
        {
            tracing::warn!("recording outcome of job {id}: {e}");
        }
        st.jobs.live.lock().unwrap().remove(&id);
        st.jobs.broadcast(&st);
    });

    Ok(Spawn::Started(id))
}

/// Ask a job to stop. Cooperative: the executor observes the flag and finishes
/// on its own terms, so this returns as soon as the request is delivered.
pub async fn cancel(state: &SharedState, id: i64) -> Result<String, String> {
    if state.jobs.request_cancel(id) {
        return Ok(format!("cancel requested for job {id}"));
    }
    match store::get_job(&state.db, id)
        .await
        .map_err(|e| e.to_string())?
    {
        // A row left non-terminal by a crash: nothing is running it, so close
        // it here rather than leaving it to hold the (kind, key) guard.
        Some(row) if matches!(row.status.as_str(), "queued" | "running") => {
            store::finish_job(&state.db, id, "canceled", None, None)
                .await
                .map_err(|e| e.to_string())?;
            Ok(format!("job {id} was not running; marked canceled"))
        }
        Some(row) => Err(format!("job {id} already finished ({})", row.status)),
        None => Err(format!("no job {id}")),
    }
}

/// Merge the durable rows with live progress (fresher than the throttled
/// snapshot on the row) — the poll surface behind `GET /api/jobs`.
pub async fn list(
    state: &SharedState,
    kind: Option<JobKind>,
    active_only: bool,
    limit: i64,
) -> Result<Vec<JobView>, String> {
    let rows = store::list_jobs(&state.db, kind.map(|k| k.as_str()), active_only, limit)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|r| {
            state
                .jobs
                .live_one(r.id)
                .unwrap_or_else(|| JobView::from_row(r))
        })
        .collect())
}
