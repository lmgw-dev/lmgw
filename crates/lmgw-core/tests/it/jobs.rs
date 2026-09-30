//! The generalized Jobs subsystem (§9c), exercised through a fake executor
//! registered for a *reserved* kind — which is also the point: adding a kind is
//! one `JobExecutor` impl plus one `register` call, with nothing in the core
//! aware of what it does. No network, no container.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use lmgw_core::jobs::{self, JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress, Spawn};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use lmgw_core::telemetry::Event;

/// A job that reports progress until it is told to stop — by finishing or by
/// being cancelled.
struct Controlled {
    kind: JobKind,
    finish: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl JobExecutor for Controlled {
    fn kind(&self) -> JobKind {
        self.kind
    }

    async fn run(&self, ctx: JobCtx, input: Value) -> Result<JobOutcome, String> {
        if input.get("boom").is_some() {
            return Err("executor said no".into());
        }
        let total = input.get("total").and_then(Value::as_u64);
        let mut done = 0u64;
        self.started.store(true, Ordering::Relaxed);
        loop {
            if ctx.canceled() {
                return Ok(JobOutcome::Canceled);
            }
            if self.finish.load(Ordering::Relaxed) {
                break;
            }
            done += 1;
            ctx.progress(JobProgress {
                done,
                total,
                stage: "working".into(),
                detail: json!({ "step": done }),
            })
            .await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(JobOutcome::Done(json!({ "steps": done })))
    }
}

async fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..400 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

async fn wait_status(state: &SharedState, id: i64, want: &str) -> store::JobRow {
    for _ in 0..400 {
        let row = store::get_job(&state.db, id).await.unwrap().unwrap();
        if row.status == want {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {id} never reached '{want}'");
}

fn register(state: &SharedState, kind: JobKind) -> (Arc<AtomicBool>, Arc<AtomicBool>) {
    let finish = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    state.jobs.register(Arc::new(Controlled {
        kind,
        finish: finish.clone(),
        started: started.clone(),
    }));
    (finish, started)
}

#[tokio::test]
async fn job_runs_reports_progress_and_persists_its_outcome() {
    let state = AppState::init_for_tests().await.unwrap();
    let (finish, started) = register(&state, JobKind::Ingest);
    let mut feed = state.telemetry.subscribe();

    let spawned = jobs::spawn(
        &state,
        JobKind::Ingest,
        Some("corpus:axum@0.8".into()),
        "ingest axum@0.8".into(),
        json!({ "total": 10 }),
    )
    .await
    .unwrap();
    let id = match spawned {
        Spawn::Started(id) => id,
        other => panic!("expected a fresh job, got {other:?}"),
    };

    // The duplicate guard is the (kind, key) index: the second call reports the
    // job already in flight instead of starting a rival one.
    let again = jobs::spawn(
        &state,
        JobKind::Ingest,
        Some("corpus:axum@0.8".into()),
        "ingest axum@0.8".into(),
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(again, Spawn::AlreadyRunning(id));
    assert!(wait_for(|| started.load(Ordering::Relaxed)).await);
    assert_eq!(state.jobs.live().len(), 1);

    // Live progress is per-callback, not per-publish-tick.
    assert!(wait_for(|| state.jobs.live_one(id).is_some_and(|j| j.done > 0)).await);
    let view = state.jobs.live_one(id).unwrap();
    assert_eq!(view.kind, "ingest");
    assert_eq!(view.total, Some(10));
    assert_eq!(view.stage, "working");
    assert_eq!(view.detail["step"], view.done);
    assert_eq!(view.percent, Some(view.done.min(10) * 10));
    assert_eq!(
        state
            .jobs
            .live_by_key(JobKind::Ingest, "corpus:axum@0.8")
            .map(|j| j.id),
        Some(id)
    );

    // …and it reaches the shared live feed, which is what the dashboard reads.
    let mut saw_frame = false;
    while let Ok(ev) = feed.try_recv() {
        if let Event::Jobs(views) = ev {
            saw_frame |= views.iter().any(|j| j.id == id);
        }
    }
    assert!(saw_frame, "no jobs frame carried the running job");

    finish.store(true, Ordering::Relaxed);
    let row = wait_status(&state, id, "done").await;
    assert!(row.finished_at.is_some());
    assert!(row.error.is_none());
    let result: Value = serde_json::from_str(row.result.as_deref().unwrap()).unwrap();
    assert!(result["steps"].as_u64().unwrap() > 0);
    // The final progress is flushed past the publish throttle, so a finished
    // row still says how far it got.
    let progress: JobProgress = serde_json::from_str(&row.progress).unwrap();
    assert_eq!(progress.done, result["steps"].as_u64().unwrap());
    assert!(state.jobs.live().is_empty());

    // Same key again once the first finished: the guard covers live jobs only.
    let third = jobs::spawn(
        &state,
        JobKind::Ingest,
        Some("corpus:axum@0.8".into()),
        "ingest axum@0.8".into(),
        json!({}),
    )
    .await
    .unwrap();
    assert!(matches!(third, Spawn::Started(new) if new != id));
    let _ = jobs::cancel(&state, third.id()).await;
}

#[tokio::test]
async fn cancel_ends_the_job_as_canceled() {
    let state = AppState::init_for_tests().await.unwrap();
    let (_finish, started) = register(&state, JobKind::ReEmbed);

    let id = jobs::spawn(&state, JobKind::ReEmbed, None, "re-embed".into(), json!({}))
        .await
        .unwrap()
        .id();
    assert!(wait_for(|| started.load(Ordering::Relaxed)).await);

    let msg = jobs::cancel(&state, id).await.unwrap();
    assert!(msg.contains("cancel requested"), "{msg}");
    let row = wait_status(&state, id, "canceled").await;
    assert!(row.finished_at.is_some());
    assert!(row.error.is_none(), "cancelling is not a failure");
    assert!(state.jobs.live().is_empty());

    // Cancelling a finished job is an error, not a silent success.
    assert!(jobs::cancel(&state, id).await.is_err());
    assert!(jobs::cancel(&state, 9999).await.is_err());
}

#[tokio::test]
async fn executor_failure_is_recorded_and_every_declared_kind_can_run() {
    let state = AppState::init_for_tests().await.unwrap();
    register(&state, JobKind::Ingest);

    let id = jobs::spawn(
        &state,
        JobKind::Ingest,
        None,
        "doomed".into(),
        json!({ "boom": true }),
    )
    .await
    .unwrap()
    .id();
    let row = wait_status(&state, id, "failed").await;
    assert_eq!(row.error.as_deref(), Some("executor said no"));

    // Every kind the API will accept has an executor behind it — a declared
    // kind that cannot be spawned is a 400 nobody can act on. (`Ingest` was
    // replaced above by this test's fake, which is the point of the registry
    // being a runtime map.)
    let registered = state.jobs.registered_kinds();
    for kind in JobKind::ALL {
        assert!(registered.contains(&kind), "{kind} has no executor");
    }

    // …and spawning one leaves a real row rather than the refusal the last
    // build gave for `eval_run`. (It fails immediately: this state has no
    // corpus 1. What matters here is that it *ran*.)
    let id = jobs::spawn(
        &state,
        JobKind::EvalRun,
        None,
        "eval".into(),
        json!({ "corpus_id": 1 }),
    )
    .await
    .unwrap()
    .id();
    let row = wait_status(&state, id, "failed").await;
    assert!(
        row.error
            .as_deref()
            .unwrap_or_default()
            .contains("corpus 1"),
        "{row:?}"
    );
}

#[tokio::test]
async fn retention_trims_finished_rows_and_leaves_running_ones_alone() {
    let state = AppState::init_for_tests().await.unwrap();
    let (finish, _) = register(&state, JobKind::Ingest);
    finish.store(true, Ordering::Relaxed);

    let mut done_ids = Vec::new();
    for n in 0..3 {
        let id = jobs::spawn(&state, JobKind::Ingest, None, format!("run {n}"), json!({}))
            .await
            .unwrap()
            .id();
        wait_status(&state, id, "done").await;
        done_ids.push(id);
    }
    // One row the crash-recovery sweep would see: non-terminal, nothing running
    // it. Claimed directly so no task can finish it under the test.
    let live_row = store::claim_job(&state.db, "ingest", Some("held"), "held", "{}")
        .await
        .unwrap()
        .unwrap();

    // Age rule: nothing is old enough yet, so it removes nothing.
    assert_eq!(store::prune_jobs(&state.db, 1, 0).await.unwrap(), 0);
    // Count rule: keep the two newest finished rows.
    assert_eq!(store::prune_jobs(&state.db, 0, 2).await.unwrap(), 1);
    let left: Vec<i64> = store::list_jobs(&state.db, None, false, 0)
        .await
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert!(!left.contains(&done_ids[0]));
    assert!(left.contains(&done_ids[2]));
    assert!(left.contains(&live_row), "a live row is never trimmed");
    // Both rules off keeps everything — the "no limit" setting really means it.
    assert_eq!(store::prune_jobs(&state.db, 0, 0).await.unwrap(), 0);

    // Startup sweep: the held row is closed, freeing its (kind, key) guard.
    assert_eq!(store::fail_orphaned_jobs(&state.db).await.unwrap(), 1);
    let row = store::get_job(&state.db, live_row).await.unwrap().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.error.as_deref(), Some("interrupted by shutdown"));
    assert!(store::active_job_by_key(&state.db, "ingest", "held")
        .await
        .unwrap()
        .is_none());
}
