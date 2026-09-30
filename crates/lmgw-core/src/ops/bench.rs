//! The benchmark ops (benchmark design §8.1): `bench_plan`, `bench_start`,
//! `bench_runs`, `bench_run`, `bench_cancel`, `bench_run_set` and
//! `bench_delete` — one function each, shared by the dashboard
//! (`POST /api/op/<name>`) and the `lmgw__bench_*` tools, taking and
//! returning the typed shapes of [`lmgw_api_types::bench_ops`].
//!
//! Everything below the ops layer is [`crate::bench`]'s; these functions pick
//! arguments apart and put refusals into words.

use lmgw_api_types::bench::ProbeOutcome;
use lmgw_api_types::bench_ops::{
    BenchArgs, BenchCancelArgs, BenchDeleteArgs, BenchDone, BenchPlan, BenchRun, BenchRunArgs,
    BenchRunDetail, BenchRunSetArgs, BenchRunSummary, BenchRunsArgs, BenchRunsResponse,
    BenchStarted, BenchStatus, JOB_KEY,
};
use serde_json::json;

use crate::bench::compare::{self, headline};
use crate::bench::identity::build_identity;
use crate::bench::launcher;
use crate::bench::plan;
use crate::bench::runner::JobInput;
use crate::error::GatewayError;
use crate::jobs::{self, JobKind, Spawn};
use crate::state::SharedState;
use crate::store::{self, NewBenchRun};

fn db(e: GatewayError) -> String {
    match e {
        GatewayError::BadRequest(m) | GatewayError::NotFound(m) => m,
        other => other.to_string(),
    }
}

/// `bench_plan`: what a start would do, and whether it can.
pub async fn bench_plan(state: &SharedState, args: BenchArgs) -> Result<BenchPlan, String> {
    if args.model_id.trim().is_empty() {
        return Err("model_id is required: a local chat model's id".into());
    }
    Ok(plan::plan(state, &args).await)
}

/// `bench_start` (§3.2): refused under the hold, while a run is going, or
/// for a row that cannot run; otherwise the row is written and the job
/// spawned — the job takes the lease and empties the card.
pub async fn bench_start(state: &SharedState, args: BenchArgs) -> Result<BenchStarted, String> {
    if args.model_id.trim().is_empty() {
        return Err("model_id is required: a local chat model's id".into());
    }
    let snap = state.snapshot();
    if let Some(b) = plan::blocked_now(state, &snap) {
        return Err(b.message);
    }
    let prep = plan::prepare(state, &snap, &args)
        .await
        .map_err(|b| b.message)?;
    let launcher = state.bench.launcher(state);
    let image = prep.runtime.image.clone();
    let (image_id, labels) = launcher::image_facts(launcher.as_ref(), &image)
        .await
        .map_err(|e| {
            format!(
                "the image '{image}' is not on this machine ({e}) — pull or build it first; a \
                 run would otherwise time the download as its load"
            )
        })?;
    let build = build_identity(&image, Some(image_id), labels);
    let stops = plan::stops(state).await;
    let run_id = store::insert_bench_run(
        &state.db,
        &NewBenchRun {
            model: &prep.model,
            build: &build,
            settings: &prep.settings,
            settings_hash: &prep.settings_hash,
            command_line: "",
            params: &prep.params,
            notes: args.notes.trim(),
        },
    )
    .await
    .map_err(db)?;
    let model_id = prep.row.model_id.clone();
    let label = if prep.rungs > 1 {
        format!("Benchmark {model_id} (rung {})", args.rung)
    } else {
        format!("Benchmark {model_id}")
    };
    let input = serde_json::to_value(JobInput {
        run_id,
        args: args.clone(),
    })
    .map_err(|e| e.to_string())?;
    match jobs::spawn(
        state,
        JobKind::Benchmark,
        Some(JOB_KEY.to_string()),
        label,
        input,
    )
    .await
    {
        Ok(Spawn::Started(job_id)) => {
            store::set_bench_run_job(&state.db, run_id, job_id)
                .await
                .map_err(db)?;
            let message = if stops.is_empty() {
                format!(
                    "benchmark run {run_id} of '{model_id}' started (job {job_id}); nothing \
                     else was on the GPU. Local models are refused or served by their fallback \
                     until it ends."
                )
            } else {
                format!(
                    "benchmark run {run_id} of '{model_id}' started (job {job_id}); it stops {} \
                     first ({}), waiting for busy ones to finish. Local models are refused or \
                     served by their fallback until it ends.",
                    stops.len(),
                    stops
                        .iter()
                        .map(|s| format!(
                            "{}/{}{}",
                            s.class,
                            s.model_id,
                            if s.busy { " (busy)" } else { "" }
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            Ok(BenchStarted {
                run_id,
                job_id,
                stops,
                message,
            })
        }
        Ok(Spawn::AlreadyRunning(job_id)) => {
            // Lost a race with another start: this row never ran.
            let _ = store::finish_bench_run(
                &state.db,
                run_id,
                BenchStatus::Failed,
                None,
                Some("another benchmark run started first"),
            )
            .await;
            let _ = store::delete_bench_run(&state.db, run_id).await;
            Err(format!(
                "a benchmark run is already going (job {job_id}) — one run at a time"
            ))
        }
        Err(e) => {
            let _ = store::finish_bench_run(&state.db, run_id, BenchStatus::Failed, None, Some(&e))
                .await;
            Err(e)
        }
    }
}

/// One run's table row.
fn summary(run: &BenchRun) -> BenchRunSummary {
    let judged = |o: &ProbeOutcome| {
        matches!(
            o,
            ProbeOutcome::Pass | ProbeOutcome::Fail | ProbeOutcome::Error
        )
    };
    BenchRunSummary {
        id: run.id,
        job_id: run.job_id,
        created_at: run.created_at.clone(),
        finished_at: run.finished_at.clone(),
        status: run.status,
        status_reason: run.status_reason.clone(),
        error: run.error.clone(),
        complete: run.results.complete,
        model_id: run.model.model_id.clone(),
        quant: run.model.quant.clone(),
        rung: run.model.rung,
        image_ref: run.image_ref.clone(),
        engine_slug: run.build.engine_slug.clone(),
        version: run.build.version.clone(),
        commit: run.build.commit.clone(),
        build_info: run.build.build_info.clone(),
        gpu_name: run.gpu.name.clone(),
        throttled: run.gpu.throttled,
        suite_version: run.suite_version,
        repetitions: run.params.repetitions,
        phases: run.params.phases.clone(),
        headline: headline(&run.results),
        probes_passed: run
            .probes
            .probes
            .iter()
            .filter(|p| p.outcome == ProbeOutcome::Pass)
            .count() as u32,
        probes_judged: run
            .probes
            .probes
            .iter()
            .filter(|p| judged(&p.outcome))
            .count() as u32,
        notes: run.notes.clone(),
        previous_id: None,
        regressions: None,
        improvements: None,
        unreadable: run.unreadable.clone(),
    }
}

/// `bench_runs`: summaries, newest first, each against its previous
/// comparable run.
pub async fn bench_runs(
    state: &SharedState,
    args: BenchRunsArgs,
) -> Result<BenchRunsResponse, String> {
    let threshold = compare::threshold(args.threshold_pct)?;
    if args.limit == Some(0) {
        return Err("limit must be at least 1 — omit it for every run".into());
    }
    let model = args
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    let mut rows = store::list_bench_runs(
        &state.db,
        model,
        args.before,
        args.limit.map(|l| l.saturating_add(1)),
    )
    .await
    .map_err(db)?;
    let more = args.limit.is_some_and(|l| rows.len() > l as usize);
    if let Some(l) = args.limit {
        rows.truncate(l as usize);
    }
    let mut runs = Vec::with_capacity(rows.len());
    for run in &rows {
        let mut s = summary(run);
        if run.status == BenchStatus::Done {
            let prev = store::previous_comparable_bench_run(&state.db, run)
                .await
                .map_err(db)?;
            if let Some(prev) = prev
                .as_ref()
                .and_then(|p| compare::previous_comparable(run, std::slice::from_ref(p)))
            {
                let c = compare::compare(run, prev, threshold);
                s.previous_id = Some(prev.id);
                s.regressions = Some(c.regressions);
                s.improvements = Some(c.improvements);
            }
        }
        runs.push(s);
    }
    Ok(BenchRunsResponse {
        runs,
        more,
        threshold_pct: threshold,
    })
}

/// `bench_run`: one whole run, and the comparison with its previous
/// comparable run (§6).
pub async fn bench_run(state: &SharedState, args: BenchRunArgs) -> Result<BenchRunDetail, String> {
    let threshold = compare::threshold(args.threshold_pct)?;
    let mut run = store::get_bench_run(&state.db, args.id)
        .await
        .map_err(db)?
        .ok_or_else(|| format!("no benchmark run {}", args.id))?;
    if args.timeline == Some(false) {
        run.timeline.samples.clear();
    }
    let prev = store::previous_comparable_bench_run(&state.db, &run)
        .await
        .map_err(db)?;
    // Judged again by the shared rule, which is what the Compare view uses:
    // the lookup and the rule cannot disagree about a pair.
    let comparison = prev
        .as_ref()
        .and_then(|p| compare::previous_comparable(&run, std::slice::from_ref(p)))
        .map(|p| compare::compare(&run, p, threshold));
    let live_job_id = state
        .bench
        .current()
        .filter(|c| c.run_id == run.id)
        .map(|c| c.job_id);
    Ok(BenchRunDetail {
        headline: headline(&run.results),
        run,
        comparison,
        threshold_pct: threshold,
        live_job_id,
    })
}

/// `bench_cancel`: the running run's cooperative cancel (§3.3) — in-flight
/// requests to the bench container are dropped, the container removed, the
/// row finalised `canceled` with what was measured.
pub async fn bench_cancel(state: &SharedState, args: BenchCancelArgs) -> Result<BenchDone, String> {
    let Some(cur) = state.bench.current() else {
        return Err("no benchmark run is going".into());
    };
    if let Some(id) = args.run_id.filter(|id| *id != cur.run_id) {
        return Err(format!(
            "run {id} is not the running one — benchmark run {} is",
            cur.run_id
        ));
    }
    let message = jobs::cancel(state, cur.job_id).await?;
    Ok(BenchDone {
        ok: true,
        run_id: cur.run_id,
        message: format!("benchmark run {}: {message}", cur.run_id),
    })
}

/// `bench_run_set`: the owner's notes.
pub async fn bench_run_set(
    state: &SharedState,
    args: BenchRunSetArgs,
) -> Result<BenchDone, String> {
    if !store::set_bench_run_notes(&state.db, args.id, args.notes.trim())
        .await
        .map_err(db)?
    {
        return Err(format!("no benchmark run {}", args.id));
    }
    Ok(BenchDone {
        ok: true,
        run_id: args.id,
        message: format!("notes of benchmark run {} saved", args.id),
    })
}

/// `bench_delete`: a finished run, by hand (§7: nothing prunes runs).
pub async fn bench_delete(state: &SharedState, args: BenchDeleteArgs) -> Result<BenchDone, String> {
    let run = store::get_bench_run(&state.db, args.id)
        .await
        .map_err(db)?
        .ok_or_else(|| format!("no benchmark run {}", args.id))?;
    if run.status == BenchStatus::Running {
        return Err(format!(
            "benchmark run {} is still going — cancel it first (bench_cancel)",
            args.id
        ));
    }
    store::delete_bench_run(&state.db, args.id)
        .await
        .map_err(db)?;
    Ok(BenchDone {
        ok: true,
        run_id: args.id,
        message: format!("benchmark run {} deleted", args.id),
    })
}

/// The tools' `next_step` after a start: where to watch it.
pub fn start_next_step(started: &BenchStarted) -> serde_json::Value {
    json!(format!(
        "follow it with lmgw__bench_run id={} (status, and the comparison once done); \
         lmgw__bench_cancel ends it early",
        started.run_id
    ))
}
