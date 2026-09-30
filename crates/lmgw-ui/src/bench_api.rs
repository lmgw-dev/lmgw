//! Typed client for the benchmark ops (benchmark design §8.1), and the one
//! live run as the jobs feed reports it.
//!
//! Every op is `POST /api/op/<name>` with its `lmgw_api_types::bench_ops`
//! argument type as the body and its answer type back, so a field renamed on
//! either side is a compile error here rather than an empty column on the
//! Benchmarks page.

use lmgw_api_types::bench_ops::{
    BenchArgs, BenchCancelArgs, BenchDeleteArgs, BenchDone, BenchJobDetail, BenchPlan,
    BenchRunArgs, BenchRunDetail, BenchRunSetArgs, BenchRunsArgs, BenchRunsResponse, BenchStarted,
    JOB_KIND,
};
use lmgw_api_types::JobRow;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::api::Result;

async fn op<T: DeserializeOwned, B: Serialize>(name: &str, body: &B) -> Result<T> {
    crate::api::post(format!("/api/op/{name}"), body).await
}

/// What a start would do, and whether it can (the modal's preview).
pub async fn plan(args: &BenchArgs) -> Result<BenchPlan> {
    op("bench_plan", args).await
}

pub async fn start(args: &BenchArgs) -> Result<BenchStarted> {
    op("bench_start", args).await
}

/// Every run, newest first, each judged against its previous comparable run
/// at `threshold_pct`.
pub async fn runs(threshold_pct: f64) -> Result<BenchRunsResponse> {
    op(
        "bench_runs",
        &BenchRunsArgs {
            threshold_pct: Some(threshold_pct),
            ..Default::default()
        },
    )
    .await
}

/// One run whole. `timeline`: the 500 ms samples too — the bulk of a run,
/// asked for only where the timeline chart is drawn.
pub async fn run(id: i64, threshold_pct: f64, timeline: bool) -> Result<BenchRunDetail> {
    op(
        "bench_run",
        &BenchRunArgs {
            id,
            threshold_pct: Some(threshold_pct),
            timeline: Some(timeline),
        },
    )
    .await
}

pub async fn cancel(run_id: i64) -> Result<BenchDone> {
    op(
        "bench_cancel",
        &BenchCancelArgs {
            run_id: Some(run_id),
        },
    )
    .await
}

pub async fn set_notes(id: i64, notes: String) -> Result<BenchDone> {
    op("bench_run_set", &BenchRunSetArgs { id, notes }).await
}

pub async fn delete(id: i64) -> Result<BenchDone> {
    op("bench_delete", &BenchDeleteArgs { id }).await
}

/// The run going now, off the jobs feed (`kind = "benchmark"`, one at a
/// time: its key is the whole GPU).
#[derive(Clone, Debug, PartialEq)]
pub struct LiveBench {
    pub job_id: i64,
    pub run_id: i64,
    pub model_id: String,
    /// "stopping every model on the GPU", "prefill: 8192 tokens, 2/3", …
    pub stage: String,
    pub done: u64,
    pub total: Option<u64>,
    pub percent: Option<u64>,
}

impl LiveBench {
    /// The phase the stage names ("prefill", "decode", …), or the setup step
    /// in a word.
    pub fn phase_word(&self) -> String {
        match self.stage.split_once(':') {
            Some((head, _)) if !head.contains(' ') => head.to_string(),
            _ => "setting up".to_string(),
        }
    }
}

/// The benchmark job on the feed, if one is running.
pub fn live(jobs: &[JobRow]) -> Option<LiveBench> {
    jobs.iter().find(|j| j.kind == JOB_KIND).map(|j| {
        let d: BenchJobDetail = serde_json::from_value(j.detail.clone()).unwrap_or_default();
        LiveBench {
            job_id: j.id,
            run_id: d.run_id,
            model_id: d.model_id,
            stage: j.stage.clone(),
            done: j.done,
            total: j.total,
            percent: j.percent,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn job(kind: &str, stage: &str, detail: serde_json::Value) -> JobRow {
        serde_json::from_value(json!({
            "id": 7, "kind": kind, "key": "gpu", "label": "Benchmark m", "status": "running",
            "done": 3, "total": 15, "percent": 20, "stage": stage, "detail": detail,
            "error": null, "created_at": "", "started_at": null, "finished_at": null
        }))
        .unwrap()
    }

    #[test]
    fn the_benchmark_job_is_found_with_its_run() {
        let jobs = vec![
            job("build_run", "x", json!({})),
            job(
                JOB_KIND,
                "prefill: 2048 tokens, 1/3",
                json!({"run_id": 12, "model_id": "qwen"}),
            ),
        ];
        let l = live(&jobs).unwrap();
        assert_eq!((l.run_id, l.model_id.as_str(), l.job_id), (12, "qwen", 7));
        assert_eq!(l.phase_word(), "prefill");
        assert_eq!(live(&jobs[..1]), None);
    }

    #[test]
    fn a_setup_stage_is_not_a_phase() {
        let mut l = live(&[job(
            JOB_KIND,
            "waiting for chat/x to finish its request",
            json!({}),
        )])
        .unwrap();
        assert_eq!(l.phase_word(), "setting up");
        l.stage = "loading 'm' (waiting for /health)".into();
        assert_eq!(l.phase_word(), "setting up");
    }
}
