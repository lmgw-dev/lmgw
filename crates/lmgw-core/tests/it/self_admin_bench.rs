//! `lmgw__bench_start`: the text of the tool is the `bench_start` op's
//! [`BenchStarted`] with a `next_step` beside it, read into its type and
//! written out again. The GPU world is `background_admission`'s (a fake card,
//! a fake llama-server held in its prefill phase) as `bench.rs` builds it.

use std::sync::Arc;
use std::time::Duration;

use lmgw_api_types::bench::SuiteParams;
use lmgw_api_types::bench_ops::BenchStarted;
use lmgw_api_types::WithNextStep;
use lmgw_core::bench::Tuning;
use serde_json::json;

use crate::support::bench_fake::FakeLauncher;
use crate::support::gpu_world::{Gpu, GIB};
use crate::support::llama_fake::{self, Config, ScriptedGpu};
use crate::support::self_admin::{parse, read_back, Admin};

fn fast(p: &mut SuiteParams) {
    p.generate_tokens = 32;
    p.stream_prompt_tokens = 96;
    p.mixed_steady_ms = 300;
}

#[tokio::test]
async fn a_started_run_says_how_to_follow_and_end_it() {
    let g = Gpu::new(24 * GIB, 2, 5).await;
    g.state
        .vram
        .set_probe(ScriptedGpu::new(200, true, 0) as Arc<dyn lmgw_core::vram::GpuProbe>);
    g.model("qwen", GIB).await;
    // Held in the prefill phase: the run stays alive until the test ends it.
    let llama = llama_fake::start(Config {
        hang_completion: true,
        per_slot_ctx: 2048,
        ..Config::default()
    })
    .await;
    let port: u16 = llama.url.rsplit(':').next().unwrap().parse().unwrap();
    g.state.bench.set_launcher_for_tests(Arc::new(FakeLauncher {
        port,
        ..Default::default()
    }));
    g.state.bench.set_tuning_for_tests(Tuning {
        baseline_window: Duration::from_millis(60),
        sample_interval: Some(Duration::from_millis(20)),
        params: Some(fast),
        remove_backoff: Duration::from_millis(10),
    });
    let a = Admin::new(&g.state).await;

    let text = a
        .text(
            "lmgw__bench_start",
            json!({"model_id": "qwen", "repetitions": 1, "phases": "probes, prefill"}),
        )
        .await;
    let started: WithNextStep<BenchStarted> = read_back(&text);
    let run_id = started.answer.run_id;
    assert!(run_id > 0 && started.answer.job_id > 0, "{text}");
    assert_eq!(
        started.next_step,
        format!(
            "follow it with lmgw__bench_run id={run_id} (status, and the comparison once \
             done); lmgw__bench_cancel ends it early"
        )
    );
    assert!(parse(&text)["stops"].is_array(), "{text}");

    // One run at a time.
    let (failed, said) = a
        .call("lmgw__bench_start", json!({"model_id": "qwen"}))
        .await;
    assert!(failed, "a second run while one is live is refused: {said}");
    a.text("lmgw__bench_cancel", json!({"run_id": run_id}))
        .await;
}
