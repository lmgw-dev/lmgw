//! A run from start to end (benchmark design §3.2, §3.3): the
//! `JobKind::Benchmark` executor around the engine's [`run_suite`].
//!
//! **Start** (§3.2): take the GPU lease — from here on no local model of any
//! class is admitted — refuse if the hold came on meanwhile, empty the card
//! ([`super::drain`]), measure the idle baseline, start the bench container
//! ([`super::launcher`]) and time it to healthy (the load phase), then hand
//! the running server to the engine.
//!
//! **End** (§3.3), on every path — done, failed, canceled, aborted, and a
//! panic in the run, which is caught: the bench container is removed, *then*
//! the lease is released (in that order, so no
//! model is admitted onto a card the bench container still occupies), and the
//! row is finalised with whatever was measured. The models the start stopped
//! are not restarted (decision 9).
//!
//! **The hold** switching on mid-run ends it from outside
//! ([`abort_for_hold`]): the run is marked aborted, its job canceled, and its
//! container removed at once — `hold_sweep` would not see it, it is not a
//! registry entry.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::FutureExt;
use lmgw_api_types::bench::{LoadResult, Phase};
use lmgw_api_types::bench_ops::{BenchArgs, BenchJobDetail, BenchStatus};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::drain::{empty_the_card, Drained};
use super::identity::build_identity;
use super::launcher::{self, BenchContainer, BenchLauncher, LoadError};
use super::lease::LeaseGuard;
use super::plan::{self, Prepared};
use super::run::{BenchSink, Progress, Record};
use super::state::{CurrentRun, OnCard, Stranded};
use super::{run_suite, Bench, BenchTarget, Sampler, SuiteEnd};
use crate::agent::Cancel;
use crate::jobs::{JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress};
use crate::state::SharedState;
use crate::store::{self, NewBenchRun};

/// A run's job input: its row and the request it was started with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobInput {
    pub run_id: i64,
    pub args: BenchArgs,
}

/// The `benchmark` job kind.
pub struct BenchmarkExecutor;

#[async_trait]
impl JobExecutor for BenchmarkExecutor {
    fn kind(&self) -> JobKind {
        JobKind::Benchmark
    }

    async fn run(&self, ctx: JobCtx, input: Value) -> Result<JobOutcome, String> {
        let input: JobInput =
            serde_json::from_value(input).map_err(|e| format!("benchmark input: {e}"))?;
        let ended = run(&ctx, &input).await;
        let value = json!({
            "run_id": input.run_id,
            "status": ended.status.as_str(),
            "status_reason": ended.reason,
            "error": ended.error,
        });
        Ok(match ended.status {
            BenchStatus::Done => JobOutcome::Done(value),
            BenchStatus::Canceled | BenchStatus::Aborted => JobOutcome::CanceledWith(value),
            _ => JobOutcome::FailedWith {
                error: ended.error.unwrap_or_else(|| "the run failed".into()),
                value,
            },
        })
    }
}

/// How a run ended, as the row stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub status: BenchStatus,
    pub reason: Option<String>,
    pub error: Option<String>,
}

impl Ended {
    fn done() -> Self {
        Self {
            status: BenchStatus::Done,
            reason: None,
            error: None,
        }
    }

    fn canceled() -> Self {
        Self {
            status: BenchStatus::Canceled,
            reason: Some("canceled".into()),
            error: None,
        }
    }

    fn failed(e: impl Into<String>) -> Self {
        Self {
            status: BenchStatus::Failed,
            reason: None,
            error: Some(e.into()),
        }
    }
}

/// Where the engine (and the start sequence) report: the job's progress,
/// and the row after every phase.
struct Sink<'a> {
    ctx: &'a JobCtx,
    detail: Value,
    run_id: i64,
}

#[async_trait]
impl BenchSink for Sink<'_> {
    async fn progress(&self, p: Progress) {
        self.ctx
            .progress(JobProgress {
                done: p.done,
                total: (p.total > 0).then_some(p.total),
                stage: p.stage,
                detail: self.detail.clone(),
            })
            .await;
    }

    async fn phase_done(&self, _phase: Phase, record: &Record) {
        save(&self.ctx.state, self.run_id, record).await;
    }
}

async fn save(state: &SharedState, run_id: i64, record: &Record) {
    if let Err(e) = store::set_bench_run_record(
        &state.db,
        run_id,
        &record.results,
        &record.probes,
        &record.timeline,
        &record.gpu,
    )
    .await
    {
        tracing::warn!("benchmark run {run_id}: recording its results: {e}");
    }
}

async fn stage(sink: &Sink<'_>, line: impl Into<String>) {
    sink.progress(Progress {
        stage: line.into(),
        done: 0,
        total: 0,
    })
    .await;
}

/// The whole run, start to end, with the end's guarantees (module doc).
async fn run(ctx: &JobCtx, input: &JobInput) -> Ended {
    let st = &ctx.state;
    let run_id = input.run_id;
    let model_id = input.args.model_id.trim().to_string();
    st.bench.begin(CurrentRun {
        run_id,
        job_id: ctx.id,
        model_id: model_id.clone(),
        ..Default::default()
    });
    // §3.2 step 2, before anything else: from here on nothing local is
    // admitted, so the card the drain empties stays empty.
    let lease = LeaseGuard::take(st, run_id, &model_id);
    crate::vram::broadcast(st);

    let sink = Sink {
        ctx,
        detail: serde_json::to_value(BenchJobDetail {
            run_id,
            model_id: model_id.clone(),
        })
        .unwrap_or_default(),
        run_id,
    };
    let launcher = st.bench.launcher(st);
    let cancel = Cancel::flag(ctx.cancel_signal());
    let mut record = Record::default();
    let mut container: Option<String> = None;
    // A panic in the run is caught here, so the end below still runs: the
    // container removed, the row finalised, the job finished. Unwinding
    // through the job's task instead would leave the container on the card
    // and the row and the job `running` — refusing every later run until
    // the next restart (review finding 4).
    let outcome = AssertUnwindSafe(drive(
        ctx,
        input,
        launcher.as_ref(),
        &cancel,
        &sink,
        &mut record,
        &mut container,
    ))
    .catch_unwind()
    .await
    .unwrap_or_else(|panic| {
        let why = panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "no message".into());
        tracing::error!("benchmark run {run_id} panicked: {why}");
        Ended::failed(format!(
            "the run stopped on an internal error (a panic: {why})"
        ))
    });

    // The end (§3.3). From here the run is ending: a hold switched on now
    // finds nothing to abort — the run measured all it was going to, and
    // removes its own container just below.
    let abort = st.bench.end(run_id);
    // The container first, then the lease.
    let mut left_behind = None;
    if let Some(name) = &container {
        stage(&sink, "removing the bench container").await;
        match remove_retrying(launcher.as_ref(), name, st.bench.tuning().remove_backoff).await {
            // Its processes leave attribution's list as tombstones, counted
            // as lmgw's for as long as the driver still lists them.
            Ok(()) => st.bench.set_on_card(run_id, OnCard::Off),
            // Still on the card, perhaps. The lease is released all the
            // same — holding it would refuse every local model until a
            // restart — but the container is remembered: the next run
            // removes it before it measures, `bench_plan` warns about it,
            // and the next boot's sweep collects it. Its processes stay
            // lmgw's in attribution, as tombstones (§13 decision 48).
            Err(e) => {
                tracing::error!("benchmark run {run_id}: {e}");
                left_behind = Some(format!(
                    "the bench container {name} could not be removed ({e}) and may still hold                      GPU memory — the next run removes it first, lmgw's next start sweeps it,                      or remove it by hand: podman rm -f {name}"
                ));
                st.bench.strand(Stranded {
                    run_id,
                    name: name.clone(),
                    error: e,
                });
            }
        }
    }
    lease.release();
    drop(lease);
    crate::vram::broadcast(st);

    let mut ended = settle(outcome, abort, ctx.canceled());
    if let Some(note) = left_behind {
        ended.error = Some(match ended.error.take() {
            Some(e) => format!("{e}; {note}"),
            None => note,
        });
    }
    save(st, run_id, &record).await;
    if let Err(e) = store::finish_bench_run(
        &st.db,
        run_id,
        ended.status,
        ended.reason.as_deref(),
        ended.error.as_deref(),
    )
    .await
    {
        tracing::warn!("benchmark run {run_id}: finalising the row: {e}");
    }
    tracing::info!(
        "benchmark run {run_id} ('{model_id}') ended {}{}",
        ended.status.as_str(),
        ended
            .error
            .as_deref()
            .map(|e| format!(": {e}"))
            .unwrap_or_default()
    );
    ended
}

/// How a run ended, from how its measuring ended (`outcome`), what aborted
/// it from outside before it began to end (`abort`), and whether its job was
/// asked to cancel. A run that measured everything is `done` whatever came
/// after: an abort or a cancel that lands once the last phase has finished
/// ended nothing.
fn settle(outcome: Ended, abort: Option<&'static str>, canceled: bool) -> Ended {
    if outcome.status == BenchStatus::Done {
        return outcome;
    }
    match abort {
        Some(reason) => Ended {
            status: BenchStatus::Aborted,
            reason: Some(reason.to_string()),
            error: Some(match reason {
                "hold" => "the GPU hold was switched on during the run".to_string(),
                "shutdown" => "lmgw was shut down during the run".to_string(),
                other => format!("ended by {other}"),
            }),
        },
        None if canceled => Ended::canceled(),
        None => outcome,
    }
}

/// Start → load → suite. `container` is set just before `podman run`, so
/// the end removes whatever may have been created.
async fn drive(
    ctx: &JobCtx,
    input: &JobInput,
    launcher: &dyn BenchLauncher,
    cancel: &Cancel,
    sink: &Sink<'_>,
    record: &mut Record,
    container: &mut Option<String>,
) -> Ended {
    let st = &ctx.state;
    // §3.2 step 1 again, now that the lease is taken: a hold switched on
    // between `bench_start` and here saw no run to abort.
    if st.snapshot().settings.hold.active {
        st.bench.abort("hold");
        return Ended::canceled();
    }
    let snap = st.snapshot();
    let prep = match plan::prepare(st, &snap, &input.args).await {
        Ok(p) => p,
        Err(b) => return Ended::failed(b.message),
    };

    stage(sink, "stopping every model on the GPU").await;
    match empty_the_card(st, cancel, sink).await {
        Drained::Empty => {}
        Drained::Canceled => return Ended::canceled(),
        Drained::Failed(e) => return Ended::failed(e),
    }
    let tuning = st.bench.tuning();
    // An earlier run's container that would not go (§13 decision 48): it is
    // not a registry entry, so the drain did not see it — and a run beside
    // it measures nothing true (decision 27).
    for s in st.bench.stranded() {
        stage(
            sink,
            format!(
                "removing benchmark run {}'s container {}, left on the card",
                s.run_id, s.name
            ),
        )
        .await;
        match remove_retrying(launcher, &s.name, tuning.remove_backoff).await {
            Ok(()) => st.bench.unstrand(&s.name),
            Err(e) => {
                return Ended::failed(format!(
                    "benchmark run {}'s container {} is still on the card and could not be                      removed ({e}) — every number beside it would be wrong; remove it by hand                      (podman rm -f {})",
                    s.run_id, s.name, s.name
                ))
            }
        }
    }

    let probe = st.vram.probe();
    let sampler = match tuning.sample_interval {
        Some(every) => Sampler::with_interval(probe, every),
        None => Sampler::start(probe),
    };
    let ended = load_and_measure(
        ctx,
        input,
        &prep,
        launcher,
        cancel,
        sink,
        &sampler,
        tuning.baseline_window,
        record,
        container,
    )
    .await;
    // Whatever ended the run, the timeline and GPU facts so far are kept
    // (the engine refreshes them itself when it got that far).
    sampler.end_phase();
    record.timeline = sampler.timeline();
    record.gpu = sampler.gpu_identity();
    sampler.fill_energy_summary(&mut record.results.energy);
    ended
}

#[allow(clippy::too_many_arguments)]
async fn load_and_measure(
    ctx: &JobCtx,
    input: &JobInput,
    prep: &Prepared,
    launcher: &dyn BenchLauncher,
    cancel: &Cancel,
    sink: &Sink<'_>,
    sampler: &Sampler,
    baseline_window: Duration,
    record: &mut Record,
    container: &mut Option<String>,
) -> Ended {
    let st = &ctx.state;
    let run_id = input.run_id;
    // §3.2 step 4: the card empty, idle, before anything is loaded.
    stage(sink, "measuring the idle baseline").await;
    let from = Instant::now();
    if cancel
        .guard(tokio::time::sleep(baseline_window))
        .await
        .is_none()
    {
        return Ended::canceled();
    }
    let baseline = sampler.baseline(from, Instant::now()).await;
    record.results.energy.baseline_vram_bytes = baseline.vram_used_bytes;
    record.results.energy.idle_power_w = baseline.idle_power_w;

    // §3.4: the container, as the row (plus overrides) renders it.
    let snap = st.snapshot();
    let prefix = snap.settings.container_prefix.clone();
    let port = match launcher.free_port() {
        Ok(p) => p,
        Err(e) => return Ended::failed(format!("no free host port could be allocated: {e}")),
    };
    let (_spec, argv) = match plan::render(prep, &prefix, port, &st.data_dir, run_id) {
        Ok(r) => r,
        Err(e) => return Ended::failed(e),
    };
    let image = prep.runtime.image.clone();
    // Raced against the cancel, as every podman call before the container
    // exists is: podman can stall on its storage lock, and the owner's
    // cancel (or the hold's abort) is what bounds the wait, not a guessed
    // timeout (review finding 5).
    let build = match cancel.guard(launcher::image_facts(launcher, &image)).await {
        None => return Ended::canceled(),
        Some(Ok((id, labels))) => build_identity(&image, Some(id), labels),
        Some(Err(e)) => {
            return Ended::failed(format!(
                "the image '{image}' is not on this machine ({e}) — pull or build it first"
            ))
        }
    };
    let command_line = launcher::command_line(&argv);
    if let Err(e) = store::set_bench_run_identity(
        &st.db,
        run_id,
        &NewBenchRun {
            model: &prep.model,
            build: &build,
            settings: &prep.settings,
            settings_hash: &prep.settings_hash,
            command_line: &command_line,
            params: &prep.params,
            notes: "",
        },
    )
    .await
    {
        tracing::warn!("benchmark run {run_id}: recording its identity: {e}");
    }

    // Named before `podman run`, so every end — and the hold's abort, which
    // reads it — removes whatever the run may have created.
    let name = launcher::container_name(&prefix, run_id);
    st.bench.set_container(run_id, &name);
    *container = Some(name.clone());
    let bc = BenchContainer {
        launcher,
        name: name.clone(),
    };

    // The load phase (§4.3): `podman run` → `/health` 200.
    sampler.mark(Phase::Load);
    stage(sink, format!("starting the bench container {name}")).await;
    st.bench.set_on_card(run_id, OnCard::Loading);
    // Right before `podman run`, after the last await: a hold that aborted
    // the run just now removed a container that did not exist yet, and a
    // `podman run` after it would put one on the card anyway.
    if cancel.is_raised() {
        return Ended::canceled();
    }
    let t0 = Instant::now();
    match cancel.guard(bc.launch(&argv)).await {
        Some(Ok(())) => {}
        Some(Err(e)) => return Ended::failed(bc.load_failure(&e).await),
        // Dropped mid-run (the child is killed with it): the container may
        // or may not exist, and the end removes it by name either way.
        None => return Ended::canceled(),
    }
    stage(
        sink,
        format!("loading '{}' (waiting for /health)", prep.row.model_id),
    )
    .await;
    let timeout = Duration::from_secs(snap.settings.vram.load_timeout_seconds);
    match bc.await_healthy(&st.http, port, timeout, cancel).await {
        Ok(()) => {}
        Err(LoadError::Canceled) => return Ended::canceled(),
        Err(LoadError::Failed(e)) => return Ended::failed(bc.load_failure(&e).await),
    }
    let load_ms = t0.elapsed().as_millis() as u64;
    // Loaded: from here its processes are lmgw's share of the card (§3.4),
    // read now rather than by the next status frame, as a model container's
    // are once it is ready.
    st.bench.set_on_card(
        run_id,
        OnCard::Ready {
            generation: crate::runtime::registry::outside_generation(),
        },
    );
    st.vram.cache_pids(st);
    sampler.settle(Instant::now()).await;
    let used = sampler.latest().and_then(|s| s.vram_used);
    record.results.load = Some(LoadResult {
        ms: load_ms,
        vram_used_bytes: used,
        vram_bytes: used
            .zip(baseline.vram_used_bytes)
            .map(|(u, b)| u.saturating_sub(b)),
    });
    record.results.phases_done.push(Phase::Load);
    record.timeline = sampler.timeline();
    record.gpu = sampler.gpu_identity();
    save(st, run_id, record).await;

    // The suite (§4.4), against the live server.
    let bench = Bench {
        http: st.http.clone(),
        target: BenchTarget {
            base_url: format!("http://127.0.0.1:{port}"),
            reasoning: prep.caps.reasoning,
            tools: prep.caps.tools,
            projector: prep.caps.projector,
            kv_unified: prep.llama_params().map(|p| p.effective_kv_unified()),
        },
        params: prep.params.clone(),
        sampler,
        cancel: cancel.clone(),
    };
    let end = run_suite(&bench, record, sink).await;
    // `/props` `build_info`, where the engine reports it (§2.2).
    if let Some(info) = record
        .results
        .server
        .as_ref()
        .and_then(|s| s.build_info.clone())
    {
        let mut build = build;
        build.build_info = Some(info);
        if let Err(e) = store::set_bench_run_build(&st.db, run_id, &build).await {
            tracing::warn!("benchmark run {run_id}: recording its build_info: {e}");
        }
    }
    match end {
        SuiteEnd::Done => Ended::done(),
        SuiteEnd::Canceled => Ended::canceled(),
        SuiteEnd::Failed(e) => Ended::failed(e),
    }
}

/// How many times the end of a run tries `podman rm -f` before it gives up
/// and reports the container as left behind (§13 decision 48). Named in the
/// error it gives, so the bound is visible where it bites.
const REMOVE_ATTEMPTS: u32 = 3;

/// `podman rm -f name`, tried [`REMOVE_ATTEMPTS`] times, waiting `backoff`
/// before the second attempt and twice as long before each later one.
async fn remove_retrying(
    launcher: &dyn BenchLauncher,
    name: &str,
    backoff: Duration,
) -> Result<(), String> {
    let mut pause = backoff;
    let mut last = String::new();
    for attempt in 1..=REMOVE_ATTEMPTS {
        match launcher::remove(launcher, name).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!("removing {name}, attempt {attempt} of {REMOVE_ATTEMPTS}: {e}");
                last = e;
            }
        }
        if attempt < REMOVE_ATTEMPTS {
            tokio::time::sleep(pause).await;
            pause = pause.saturating_mul(2);
        }
    }
    Err(format!("{last}; tried {REMOVE_ATTEMPTS} times"))
}

/// The GPU hold switched on (§3.3): end the run in flight, if there is one —
/// mark it aborted (reason `hold`), cancel its job, and remove its container
/// now rather than when the run notices, because `hold_sweep` does not see
/// it and the owner asked for the card back. Returns the run's id.
pub async fn abort_for_hold(state: &SharedState) -> Option<i64> {
    abort(state, "hold").await
}

/// lmgw is quitting (`lifecycle::shutdown`): the same as for the hold, with
/// reason `shutdown` — the bench container is not a registry entry, so the
/// shutdown's `stop_all` would leave it on the card until the next boot.
pub async fn abort_for_shutdown(state: &SharedState) -> Option<i64> {
    abort(state, "shutdown").await
}

async fn abort(state: &SharedState, reason: &'static str) -> Option<i64> {
    let run = state.bench.abort(reason)?;
    state.jobs.request_cancel(run.job_id);
    let prefix = state.snapshot().settings.container_prefix.clone();
    let name = run
        .container
        .clone()
        .unwrap_or_else(|| launcher::container_name(&prefix, run.run_id));
    let launcher = state.bench.launcher(state);
    match launcher::remove(launcher.as_ref(), &name).await {
        Ok(()) => {
            state.bench.set_on_card(run.run_id, OnCard::Off);
            tracing::info!(
                "{reason}: benchmark run {} aborted, its container {name} removed",
                run.run_id
            )
        }
        Err(e) => tracing::warn!("{reason}: removing benchmark container {name}: {e}"),
    }
    Some(run.run_id)
}

/// Boot (§3.3): every bench container of this instance that podman created
/// before this process started is a leftover — no run survives a restart —
/// and is removed before anything is admitted. One created since is this
/// process's own run's, and so is the running run's, by name: neither is
/// touched (§13 decision 45).
pub async fn boot_sweep(state: &SharedState) {
    let launcher: Arc<dyn BenchLauncher> = state.bench.launcher(state);
    let prefix = state.snapshot().settings.container_prefix.clone();
    let born = state.started_at_utc.timestamp();
    match launcher::leftovers(launcher.as_ref(), &prefix, born).await {
        Ok(names) => {
            let running = state
                .bench
                .current()
                .map(|c| launcher::container_name(&prefix, c.run_id));
            for name in names.into_iter().filter(|n| Some(n) != running.as_ref()) {
                match launcher::remove(launcher.as_ref(), &name).await {
                    Ok(()) => tracing::info!("removed the leftover benchmark container {name}"),
                    Err(e) => tracing::warn!("removing the leftover benchmark container: {e}"),
                }
            }
        }
        Err(e) => tracing::warn!("listing leftover benchmark containers: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review finding 3: a run that measured everything stays `done` when
    /// the hold came on as it finished; one cut short by the hold is
    /// `aborted`, one cut short by the owner `canceled`.
    #[test]
    fn a_finished_run_stays_done_whatever_lands_after_it() {
        assert_eq!(settle(Ended::done(), Some("hold"), true), Ended::done());
        let aborted = settle(Ended::canceled(), Some("hold"), true);
        assert_eq!(aborted.status, BenchStatus::Aborted);
        assert_eq!(aborted.reason.as_deref(), Some("hold"));
        assert_eq!(
            settle(Ended::failed("prefill: x"), None, true),
            Ended::canceled()
        );
        assert_eq!(
            settle(Ended::failed("prefill: x"), None, false),
            Ended::failed("prefill: x")
        );
    }
}
