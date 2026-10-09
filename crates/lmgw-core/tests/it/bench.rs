//! Benchmark runs through the real router and ops (benchmark design §9,
//! WP2): `bench_plan` / `bench_start` / `bench_run` / `bench_runs` /
//! `bench_cancel` over `POST /api/op/…`, the GPU lease as a client and the
//! admission net see it, the hold aborting a run, the boot sweep, and
//! reconciliation leaving bench containers alone.
//!
//! The GPU world is `support/gpu_world.rs`'s (chat rows on sparse files, a
//! fake registry podman, wiremock model containers, cloud aliases), with the
//! scripted-energy probe of `support/llama_fake.rs` as the card. "The bench
//! container" is WP1's fake llama-server: a [`FakeLauncher`] hands out its
//! port and answers the podman verbs a run uses, recording each one. The
//! engine's own behaviour is `bench_engine.rs`'s.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_api_types::bench::{Phase, ProbeKind, SuiteParams};
use lmgw_core::bench::Tuning;
use lmgw_core::config::{AuxKind, HoldFallbackMode, LlamaParams};
use lmgw_core::runtime::registry::Registry;
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAuxModel, NewLocalModel};
use serde_json::{json, Value};

use crate::common::{self, Gw};
use crate::support::bench_fake::{FakeLauncher, HeldProbe, PsRunner};
use crate::support::gpu_world::{Gpu, GIB};
use crate::support::llama_fake::{self, Config, FakeLlama, ScriptedGpu};

// ---------------------------------------------------------------------------
// The world
// ---------------------------------------------------------------------------

/// Suite v1 shrunk so a run takes seconds (the engine suite's `fast`).
fn fast(p: &mut SuiteParams) {
    p.generate_tokens = 32;
    p.stream_prompt_tokens = 96;
    p.mixed_steady_ms = 300;
}

struct World {
    g: Gpu,
    gw: Gw,
    launcher: Arc<FakeLauncher>,
    llama: FakeLlama,
}

impl World {
    /// A 24 GiB card, `qwen` (the model runs benchmark) and `other` (a
    /// second chat row), and the fake llama-server with `cfg`.
    async fn new(cfg: Config) -> Self {
        Self::on(cfg, None).await
    }

    /// [`Self::new`] on the world's own card of `small` bytes — whose free
    /// memory follows what is loaded, so admission queues — instead of the
    /// scripted-energy one.
    async fn on(cfg: Config, small: Option<u64>) -> Self {
        let g = Gpu::new(small.unwrap_or(24 * GIB), 2, 5).await;
        if small.is_none() {
            g.state
                .vram
                .set_probe(ScriptedGpu::new(200, true, 0) as Arc<dyn lmgw_core::vram::GpuProbe>);
        }
        g.model("qwen", GIB).await;
        g.model("other", GIB).await;
        let llama = llama_fake::start(cfg).await;
        let port: u16 = llama.url.rsplit(':').next().unwrap().parse().unwrap();
        let launcher = Arc::new(FakeLauncher {
            port,
            ..Default::default()
        });
        g.state.bench.set_launcher_for_tests(launcher.clone());
        g.state.bench.set_tuning_for_tests(Tuning {
            baseline_window: Duration::from_millis(60),
            sample_interval: Some(Duration::from_millis(20)),
            params: Some(fast),
            remove_backoff: Duration::from_millis(10),
        });
        let gw = common::serve(g.state.clone()).await;
        Self {
            g,
            gw,
            launcher,
            llama,
        }
    }

    fn state(&self) -> &SharedState {
        &self.g.state
    }

    fn prefix(&self) -> String {
        self.state().snapshot().settings.container_prefix.clone()
    }

    async fn op(&self, name: &str, args: Value) -> (u16, Value) {
        let resp = self
            .gw
            .client()
            .post(format!("{}/api/op/{name}", self.gw))
            .json(&args)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    async fn ok(&self, name: &str, args: Value) -> Value {
        let (status, body) = self.op(name, args).await;
        assert_eq!(status, 200, "{name}: {body}");
        body
    }

    async fn refused(&self, name: &str, args: Value) -> String {
        let (status, body) = self.op(name, args).await;
        assert_eq!(status, 400, "{name} should be refused: {body}");
        body["message"].as_str().unwrap().to_string()
    }

    /// Start a run; `(run_id, job_id)`.
    async fn start(&self, args: Value) -> (i64, i64) {
        let v = self.ok("bench_start", args).await;
        (v["run_id"].as_i64().unwrap(), v["job_id"].as_i64().unwrap())
    }

    /// Wait until the job is no longer live.
    async fn finished(&self, job_id: i64) {
        common::patience::until(&format!("benchmark job {job_id} finishes"), || {
            self.state().jobs.live_one(job_id).is_none()
        })
        .await;
    }

    /// Wait until the job's stage satisfies `pred`; the stage.
    async fn stage(&self, job_id: i64, pred: impl Fn(&str) -> bool) -> String {
        let mut last = String::new();
        let mut wait = common::patience::Wait::new(format!("job {job_id} reaches the stage"));
        loop {
            match self.state().jobs.live_one(job_id) {
                Some(j) if pred(&j.stage) => return j.stage,
                Some(j) => last = j.stage,
                None => panic!("job {job_id} ended while waiting (last stage '{last}')"),
            }
            wait.again(Some(&last)).await;
        }
    }

    async fn run(&self, id: i64) -> Value {
        self.ok("bench_run", json!({"id": id})).await
    }

    async fn chat(&self, model: &str) -> reqwest::Response {
        self.gw
            .client()
            .post(format!("{}/v1/chat/completions", self.gw))
            .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap()
    }
}

fn header<'r>(resp: &'r reqwest::Response, name: &str) -> Option<&'r str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

/// A run held in its prefill phase: the fake never answers `/completion`,
/// so the probes finish and the run then waits there until it is ended.
fn hanging() -> Config {
    Config {
        hang_completion: true,
        per_slot_ctx: 2048,
        ..Config::default()
    }
}

fn quick() -> Config {
    Config {
        per_slot_ctx: 2048,
        token_delay: Duration::from_millis(1),
        ..Config::default()
    }
}

// ---------------------------------------------------------------------------
// A whole run, and the next one compared with it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_run_is_stored_whole_and_the_next_one_is_compared_with_it() {
    let w = World::new(quick()).await;
    let prefix = w.prefix();

    let plan = w.ok("bench_plan", json!({"model_id": "qwen"})).await;
    assert_eq!(plan["blocked"], Value::Null, "{plan}");
    assert_eq!(plan["points"]["provisional"], true);
    let cl = plan["command_line"].as_str().unwrap();
    assert!(cl.contains(&format!("{prefix}-bench-<run id>")), "{cl}");
    assert!(
        cl.contains("lmgw.bench=<run id>") && cl.contains("<port>:8080"),
        "{cl}"
    );
    assert_eq!(plan["build"]["engine_slug"], "official-master");
    assert_eq!(
        plan["probes"].as_array().unwrap().len(),
        ProbeKind::ALL.len()
    );

    let (first, job) = w
        .start(json!({"model_id": "qwen", "repetitions": 1, "notes": "baseline"}))
        .await;
    w.finished(job).await;
    let d = w.run(first).await;
    let run = &d["run"];
    assert_eq!(run["status"], "done", "{}", run["error"]);
    assert_eq!(run["notes"], "baseline");
    assert_eq!(run["results"]["complete"], true);
    assert!(run["results"]["load"]["ms"].as_u64().is_some());
    assert_eq!(run["results"]["server"]["per_slot_ctx"], 2048);
    assert!(!run["results"]["prefill"].as_array().unwrap().is_empty());
    assert!(!run["results"]["decode"].as_array().unwrap().is_empty());
    assert_eq!(run["probes"]["probes"].as_array().unwrap().len(), 9);
    assert_eq!(run["gpu"]["name"], "FakeGPU 4090");
    assert_eq!(run["build"]["build_info"], "b11226-0c6a6a7", "from /props");
    assert!(run["results"]["energy"]["idle_power_w"].as_f64().is_some());
    let name = format!("{prefix}-bench-{first}");
    let cl = run["command_line"].as_str().unwrap();
    assert!(cl.contains(&format!("--name {name}")), "{cl}");
    assert!(cl.contains(&format!("lmgw.bench={first}")), "{cl}");
    assert_eq!(d["comparison"], Value::Null, "nothing to compare with yet");

    // The container was run with the bench label, and removed at the end.
    let run_argv = w.launcher.verb("run");
    assert_eq!(run_argv.len(), 1);
    assert!(run_argv[0].contains(&format!("lmgw.bench={first}")));
    assert!(w.launcher.removed(&name));
    assert!(
        w.state().snapshot().gpu_lease.is_none(),
        "the lease is released"
    );
    assert!(w.llama.seen.completions.lock().unwrap().len() > 5);

    // A second run of the same settings compares with the first.
    let (second, job) = w.start(json!({"model_id": "qwen", "repetitions": 1})).await;
    w.finished(job).await;
    let d = w
        .ok("bench_run", json!({"id": second, "threshold_pct": 50.0}))
        .await;
    assert_eq!(d["run"]["status"], "done");
    let c = &d["comparison"];
    assert_eq!(c["base_run_id"], first);
    assert_eq!(c["comparable"], true, "{c}");
    assert_eq!(c["threshold_pct"], 50.0);
    assert!(c["metrics"].as_array().unwrap().len() >= 8);

    let runs = w.ok("bench_runs", json!({"model_id": "qwen"})).await;
    let rows = runs["runs"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], second, "newest first");
    assert_eq!(rows[0]["previous_id"], first);
    assert!(rows[0]["probes_judged"].as_u64().unwrap() > 0);
    assert_eq!(runs["threshold_pct"], 5.0);

    // Notes, then delete.
    w.ok("bench_run_set", json!({"id": first, "notes": "kept"}))
        .await;
    assert_eq!(w.run(first).await["run"]["notes"], "kept");
    w.ok("bench_delete", json!({"id": first})).await;
    let msg = w.refused("bench_run", json!({"id": first})).await;
    assert!(msg.contains("no benchmark run"), "{msg}");
}

// ---------------------------------------------------------------------------
// Why a run cannot start
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_plan_names_why_a_run_cannot_start() {
    let w = World::new(quick()).await;
    let reason = |v: &Value| {
        v["blocked"]["reason"]
            .as_str()
            .unwrap_or("none")
            .to_string()
    };

    let v = w.ok("bench_plan", json!({"model_id": "nope"})).await;
    assert_eq!(reason(&v), "row_missing");
    let v = w
        .ok("bench_plan", json!({"model_id": "qwen", "rung": 3}))
        .await;
    assert_eq!(reason(&v), "rung_out_of_range");

    w.g.row(
        NewLocalModel {
            model_id: "off".into(),
            gguf_path: "off.gguf".into(),
            params: LlamaParams::default(),
            args: vec![],
            idle_seconds: 0,
            enabled: false,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            ladder: vec![],
        },
        GIB,
    )
    .await;
    let v = w.ok("bench_plan", json!({"model_id": "off"})).await;
    assert_eq!(reason(&v), "row_disabled");

    store::insert_aux_model(&w.state().db, &aux("e5"))
        .await
        .unwrap();
    w.state().reload_snapshot().await.unwrap();
    let v = w.ok("bench_plan", json!({"model_id": "e5"})).await;
    assert_eq!(reason(&v), "not_chat");

    // An image override podman would read as a flag never reaches podman
    // (review finding 8).
    let v = w
        .ok(
            "bench_plan",
            json!({"model_id": "qwen", "image": "--privileged"}),
        )
        .await;
    assert_eq!(reason(&v), "invalid_image");
    let msg = w
        .refused(
            "bench_start",
            json!({"model_id": "qwen", "image": "-v/:/host"}),
        )
        .await;
    assert!(msg.contains("read as a flag"), "{msg}");
    let v = w
        .ok("bench_plan", json!({"model_id": "qwen", "image": "a b"}))
        .await;
    assert_eq!(reason(&v), "invalid_image");
    assert!(
        !w.launcher
            .calls()
            .iter()
            .flatten()
            .any(|a| a.starts_with('-') && (a == "--privileged" || a == "-v/:/host")),
        "{:?}",
        w.launcher.calls()
    );

    w.launcher.image_missing.store(true, Ordering::SeqCst);
    let v = w.ok("bench_plan", json!({"model_id": "qwen"})).await;
    assert_eq!(reason(&v), "image_missing");
    let msg = w.refused("bench_start", json!({"model_id": "qwen"})).await;
    assert!(msg.contains("not on this machine"), "{msg}");
    w.launcher.image_missing.store(false, Ordering::SeqCst);

    w.ok("hold_set", json!({"active": true})).await;
    let v = w.ok("bench_plan", json!({"model_id": "qwen"})).await;
    assert_eq!(reason(&v), "hold");
    let msg = w.refused("bench_start", json!({"model_id": "qwen"})).await;
    assert!(msg.contains("GPU hold is on"), "{msg}");
    w.ok("hold_set", json!({"active": false})).await;

    let v = w.ok("bench_plan", json!({"model_id": "qwen"})).await;
    assert_eq!(reason(&v), "none", "{}", v["blocked"]);
    // Refusals never reach podman.
    assert!(w.launcher.verb("run").is_empty());
}

fn aux(id: &str) -> NewAuxModel {
    NewAuxModel {
        model_id: id.into(),
        gguf_path: format!("{id}.gguf"),
        kind: AuxKind::Embed,
        pooling: None,
        ctx_size: None,
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: HoldFallbackMode::Inherit,
        hold_fallback: None,
    }
}

// ---------------------------------------------------------------------------
// The lease, one run at a time, and cancel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn during_a_run_the_card_is_the_benchmarks_and_cancel_keeps_what_it_measured() {
    let w = World::new(hanging()).await;
    w.g.cloud("cloud", None).await;
    w.g.row(
        NewLocalModel {
            model_id: "fb".into(),
            gguf_path: "fb.gguf".into(),
            params: LlamaParams::default(),
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: HoldFallbackMode::Alias,
            hold_fallback: Some("cloud".into()),
            capabilities_override: None,
            ladder: vec![],
        },
        GIB,
    )
    .await;
    store::insert_aux_model(&w.state().db, &aux("e5"))
        .await
        .unwrap();
    w.state().reload_snapshot().await.unwrap();

    let (run_id, job) = w
        .start(json!({"model_id": "qwen", "repetitions": 1, "phases": ["probes", "prefill"]}))
        .await;
    w.stage(job, |s| s.starts_with("prefill")).await;
    let lease = w.state().snapshot().gpu_lease.clone().expect("the lease");
    assert_eq!(lease.run_id, run_id);
    let vram =
        w.gw.client()
            .get(format!("{}/api/vram", w.gw))
            .send()
            .await
            .unwrap();
    let vram: Value = vram.json().await.unwrap();
    assert_eq!(vram["benchmark"]["run_id"], run_id, "{vram}");

    // A chat request for a local model without a fallback: 503 gpu_benchmark.
    let resp = w.chat("other").await;
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_benchmark", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&format!("run {run_id}")),
        "{body}"
    );

    // With a fallback: the fallback answers, and says why.
    let resp = w.chat("fb").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lmgw-fallback"), Some("cloud"));
    assert_eq!(header(&resp, "x-lmgw-fallback-reason"), Some("benchmark"));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "from the cloud");
    let logged = || async {
        store::query_logs(
            &w.state().db,
            &store::LogFilter {
                alias: Some("fb".into()),
                limit: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    common::patience::until_async("the fallback's row", || async {
        !logged().await.is_empty()
    })
    .await;
    assert_eq!(
        logged().await[0].fallback_reason.as_deref(),
        Some("benchmark")
    );

    // An aux request is refused the same way — named as the client asked for
    // it, `embed/e5`, not the row's bare id `e5`.
    let resp =
        w.gw.client()
            .post(format!("{}/v1/embeddings", w.gw))
            .json(&json!({"model": "embed/e5", "input": "hi"}))
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_benchmark", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("'embed/e5'"),
        "{body}"
    );

    // The admission net refuses a caller that resolves its own route.
    let err = lmgw_core::vram::admit(w.state(), &w.g.route("other"), "other")
        .await
        .unwrap_err();
    assert_eq!(err.code(), "gpu_benchmark", "{err}");
    assert!(w.g.runs().is_empty(), "nothing local was started");

    // One run at a time.
    let v = w.ok("bench_plan", json!({"model_id": "other"})).await;
    assert_eq!(v["blocked"]["reason"], "run_going");
    assert_eq!(v["blocked"]["run_id"], run_id);
    let msg = w.refused("bench_start", json!({"model_id": "other"})).await;
    assert!(msg.contains(&format!("run {run_id} is going")), "{msg}");
    let msg = w.refused("bench_delete", json!({"id": run_id})).await;
    assert!(msg.contains("still going"), "{msg}");

    // Cancel: the run ends canceled, keeping its probes.
    let v = w.ok("bench_cancel", json!({"run_id": run_id})).await;
    assert_eq!(v["run_id"], run_id);
    w.finished(job).await;
    let run = w.run(run_id).await["run"].clone();
    assert_eq!(run["status"], "canceled", "{run}");
    assert_eq!(run["results"]["complete"], false);
    let done: Vec<&str> = run["results"]["phases_done"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert_eq!(done, vec!["load", "probes"]);
    assert_eq!(run["probes"]["probes"].as_array().unwrap().len(), 9);
    assert!(w
        .launcher
        .removed(&format!("{}-bench-{run_id}", w.prefix())));
    assert!(w.state().snapshot().gpu_lease.is_none());

    // Local traffic flows again.
    assert_eq!(w.chat("other").await.status(), 200);
}

#[tokio::test]
async fn the_hold_switching_on_aborts_the_run_and_removes_its_container() {
    // The measured prefill hangs, the unmeasured requests before it do not;
    // removing the container breaks the request in flight at once, before
    // the run's cancel poll sees the abort (review finding 9).
    let w = World::new(Config {
        hang_n_predict: Some(1),
        ..quick()
    })
    .await;
    *w.launcher.rm_kills.lock().unwrap() = Some(w.llama.seen.killed.clone());
    let (run_id, job) = w
        .start(json!({"model_id": "qwen", "repetitions": 1, "phases": ["prefill"]}))
        .await;
    w.stage(job, |s| s.starts_with("prefill")).await;
    common::patience::until("the measured prefill hangs", || {
        w.llama.seen.hanging.load(Ordering::SeqCst) > 0
    })
    .await;
    let v = w.ok("hold_set", json!({"active": true})).await;
    assert_eq!(v["benchmark_aborted"], run_id, "{v}");
    let name = format!("{}-bench-{run_id}", w.prefix());
    assert!(
        w.launcher.removed(&name),
        "hold_set removed the container itself"
    );
    w.finished(job).await;
    let run = w.run(run_id).await["run"].clone();
    assert_eq!(run["status"], "aborted", "{run}");
    assert_eq!(run["status_reason"], "hold");
    assert_eq!(
        run["results"]["phase_errors"],
        json!([]),
        "the broken connection is the abort, not a phase error: {run}"
    );
    assert!(w.state().snapshot().gpu_lease.is_none());
}

/// Poll `GET /api/vram` until `ready` holds (the view reads a PID in the
/// background the first time it sees a container).
async fn until_vram(w: &World, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
    let get = || async {
        w.gw.client()
            .get(format!("{}/api/vram", w.gw))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    let mut wait = common::patience::Wait::new(what);
    loop {
        let v = get().await;
        if ready(&v) {
            return v;
        }
        wait.again(Some(&v)).await;
    }
}

/// The bench container's processes are lmgw's share of the card (§3.4), like
/// a model container's, so outside use stays measured during a run — and
/// once the run removed it, a process the driver still lists is a
/// tombstone, still lmgw's, until the driver drops it. (WP2 switched the
/// outside verdict off under the lease instead; that was rejected.)
#[tokio::test]
async fn a_runs_container_is_lmgws_share_of_the_card() {
    let w = World::on(hanging(), Some(24 * GIB)).await;
    // The run's process on the card, as `podman inspect` and the driver will
    // see it once the container runs (the fake launcher does not touch the
    // card): 3 GiB under the container name run 1 gets. And a game's 2 GiB.
    let name = format!("{}-bench-1", w.prefix());
    {
        let mut g = w.g.world();
        g.attribution = true;
        g.outside = 2 * GIB;
        g.names.insert(name.clone(), "bench".into());
        g.pids.insert("bench".into(), 6_000_000);
        g.size.insert("bench".into(), 3 * GIB);
        g.loaded.insert("bench".into());
    }
    let (run_id, job) = w
        .start(json!({"model_id": "qwen", "repetitions": 1, "phases": ["prefill"]}))
        .await;
    assert_eq!(run_id, 1, "the name above is run 1's");
    w.stage(job, |s| s.starts_with("prefill")).await;

    let v = until_vram(&w, "the run's container is attributed", |v| {
        !v["lmgw_share_bytes"].is_null()
    })
    .await;
    assert_eq!(v["lmgw_share_bytes"], 3 * GIB, "{v}");
    assert_eq!(v["outside_share_bytes"], 2 * GIB, "{v}");
    assert!(v["external_trigger_reason"].is_null(), "{v}");
    assert_eq!(v["benchmark"]["run_id"], run_id, "{v}");
    let status = lmgw_core::ops::status(w.state()).await.unwrap();
    assert_eq!(status["vram"]["lmgw_share_bytes"], 3 * GIB, "{status}");

    // Removed, but the driver still lists the process: a tombstone.
    w.ok("bench_cancel", json!({})).await;
    w.finished(job).await;
    assert!(w.launcher.removed(&name));
    let v = until_vram(&w, "after the run", |v| !v["lmgw_share_bytes"].is_null()).await;
    assert_eq!(v["lmgw_share_bytes"], 3 * GIB, "{v}");
    assert_eq!(v["outside_share_bytes"], 2 * GIB, "{v}");
    assert!(v["benchmark"].is_null(), "{v}");
    // The driver drops it: gone for good, and the game is still outside.
    w.g.world().loaded.remove("bench");
    let v = until_vram(&w, "the process left the card", |v| {
        v["lmgw_share_bytes"] == 0
    })
    .await;
    assert_eq!(v["outside_share_bytes"], 2 * GIB, "{v}");
}

// ---------------------------------------------------------------------------
// Emptying the card
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_busy_model_is_waited_for_and_then_stopped() {
    let w = World::new(quick()).await;
    // `other` is resident, and serving a request (the claim is held).
    let held = lmgw_core::vram::admit(w.state(), &w.g.route("other"), "other")
        .await
        .unwrap()
        .unwrap();
    let plan = w.ok("bench_plan", json!({"model_id": "qwen"})).await;
    let stops = plan["stops"].as_array().unwrap();
    assert_eq!(stops.len(), 1, "{plan}");
    assert_eq!(stops[0]["model_id"], "other");
    assert_eq!(stops[0]["busy"], true);

    let (run_id, job) = w
        .start(json!({"model_id": "qwen", "repetitions": 1, "phases": ["probes"]}))
        .await;
    let stage = w.stage(job, |s| s.starts_with("waiting for")).await;
    assert_eq!(stage, "waiting for chat/other to finish its request");
    assert!(w.g.stops().is_empty(), "nothing is killed mid-request");
    assert!(
        w.launcher.verb("run").is_empty(),
        "the bench waits for an empty card"
    );

    drop(held);
    w.finished(job).await;
    assert_eq!(w.g.stops(), vec!["other".to_string()]);
    assert_eq!(w.run(run_id).await["run"]["status"], "done");
}

/// A start decided before the lease — a guest's here, holding the admission
/// gate in its ledger read — finishes deciding before the drain looks: it is
/// refused, and the run loads only after it (§13 decision 44). Before, the
/// drain looked once at an empty registry and went on, and the guest's
/// container came up beside the bench container.
#[tokio::test]
async fn the_drain_waits_for_a_start_decided_before_the_lease() {
    let w = World::new(quick()).await;
    let probe = HeldProbe::wrap(w.state().vram.probe());
    w.state()
        .vram
        .set_probe(probe.clone() as Arc<dyn lmgw_core::vram::GpuProbe>);
    probe.hold_next(None);
    let (state, route) = (w.state().clone(), w.g.route("other"));
    let guest = tokio::spawn(async move {
        lmgw_core::vram::start_background(&state, &route, "bg")
            .await
            .map(|_| ())
    });
    probe.held().await;

    let (run_id, job) = w
        .start(json!({"model_id": "qwen", "repetitions": 1, "phases": ["probes"]}))
        .await;
    w.stage(job, |s| s == "stopping every model on the GPU")
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        w.launcher.verb("run").is_empty(),
        "the drain waits for the decision in flight"
    );
    probe.release();
    let err = guest.await.unwrap().unwrap_err();
    assert_eq!(err.code(), "gpu_benchmark", "{err}");
    w.finished(job).await;
    assert_eq!(w.run(run_id).await["run"]["status"], "done");
    assert!(w.g.runs().is_empty(), "the guest's model never started");
}

#[tokio::test]
async fn a_request_queued_for_room_is_refused_once_the_lease_is_taken() {
    // A card that holds one model: `other` is resident and busy, so a
    // request for `qwen` queues for room.
    let w = World::on(quick(), Some(GIB + GIB / 2)).await;
    let held = lmgw_core::vram::admit(w.state(), &w.g.route("other"), "other")
        .await
        .unwrap()
        .unwrap();
    let state = w.state().clone();
    let route = w.g.route("qwen");
    let queued = tokio::spawn(async move { lmgw_core::vram::admit(&state, &route, "qwen").await });
    common::patience::until("the request queues", || w.state().vram.waits_begun() > 0).await;

    let (run_id, job) = w
        .start(json!({"model_id": "qwen", "repetitions": 1, "phases": ["probes"]}))
        .await;
    // The waiter leaves the queue refused instead of starting a container
    // once `other` is stopped.
    let err = queued.await.unwrap().unwrap_err();
    assert_eq!(err.code(), "gpu_benchmark", "{err}");
    drop(held);
    w.finished(job).await;
    assert_eq!(w.run(run_id).await["run"]["status"], "done");
    assert_eq!(
        w.g.runs(),
        vec!["other".to_string()],
        "qwen was never started"
    );
}

/// The port `free_port` handed out can be taken before podman binds it
/// (per-model containers §10.4): the husk is collected and the container
/// runs once more on a fresh port, whose command line the run stores.
#[tokio::test]
async fn a_port_taken_before_podman_bound_it_is_retried_once_on_a_fresh_one() {
    let w = World::new(quick()).await;
    let decoy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let decoy_port = decoy.local_addr().unwrap().port();
    drop(decoy);
    *w.launcher.decoy.lock().unwrap() = Some(decoy_port);
    w.launcher.taken.store(1, Ordering::SeqCst);
    let (run_id, job) = w.start(json!({"model_id": "qwen", "repetitions": 1})).await;
    w.finished(job).await;
    let run = w.run(run_id).await["run"].clone();
    assert_eq!(run["status"], "done", "{}", run["error"]);

    let name = format!("{}-bench-{run_id}", w.prefix());
    let seen: Vec<Vec<String>> = w
        .launcher
        .calls()
        .into_iter()
        .filter(|c| matches!(c[0].as_str(), "run" | "rm"))
        .collect();
    let verbs: Vec<&str> = seen.iter().map(|c| c[0].as_str()).collect();
    assert_eq!(verbs[..3], ["run", "rm", "run"], "{seen:?}");
    assert_eq!(seen[1].last(), Some(&name), "the husk, by name");
    let published = |c: &[String]| c[c.iter().position(|a| a == "-p").unwrap() + 1].clone();
    let real = format!("127.0.0.1:{}:8080", w.launcher.port);
    assert_eq!(published(&seen[0]), format!("127.0.0.1:{decoy_port}:8080"));
    assert_eq!(published(&seen[2]), real);
    let cl = run["command_line"].as_str().unwrap();
    assert!(cl.contains(&real), "the line that ran is stored: {cl}");
}

/// Once: a second taken port fails the run with podman's words.
#[tokio::test]
async fn a_port_taken_twice_fails_the_run_with_podman_s_words() {
    let w = World::new(quick()).await;
    w.launcher.taken.store(2, Ordering::SeqCst);
    let (run_id, job) = w.start(json!({"model_id": "qwen"})).await;
    w.finished(job).await;
    let run = w.run(run_id).await["run"].clone();
    assert_eq!(run["status"], "failed");
    let err = run["error"].as_str().unwrap();
    assert!(err.contains("address already in use"), "{err}");
    assert_eq!(w.launcher.verb("run").len(), 2, "one retry, not a loop");
    assert!(w
        .launcher
        .removed(&format!("{}-bench-{run_id}", w.prefix())));
}

#[tokio::test]
async fn a_load_that_dies_fails_the_run_with_the_classifiers_hint() {
    let w = World::new(quick()).await;
    // Nothing answers on the port the container gets, and podman says it
    // exited: the load phase reads the log tail.
    let (port, _held) = common::refusing_port();
    let launcher = Arc::new(FakeLauncher {
        port,
        ..Default::default()
    });
    launcher.exited.store(true, Ordering::SeqCst);
    w.state().bench.set_launcher_for_tests(launcher.clone());
    let (run_id, job) = w.start(json!({"model_id": "qwen"})).await;
    w.finished(job).await;
    let run = w.run(run_id).await["run"].clone();
    assert_eq!(run["status"], "failed");
    let err = run["error"].as_str().unwrap();
    assert!(err.contains("exited with code 1"), "{err}");
    assert!(err.contains("not enough VRAM"), "{err}");
    assert!(err.contains("cudaMalloc failed"), "{err}");
    assert!(launcher.removed(&format!("{}-bench-{run_id}", w.prefix())));
}

#[tokio::test]
async fn boot_interrupts_running_rows_and_sweeps_leftover_bench_containers() {
    let w = World::new(quick()).await;
    let prefix = w.prefix();
    let db = &w.state().db;
    let model = lmgw_api_types::bench::ModelIdentity {
        model_id: "qwen".into(),
        ..Default::default()
    };
    let id = store::insert_bench_run(
        db,
        &store::NewBenchRun {
            model: &model,
            build: &Default::default(),
            settings: &Default::default(),
            settings_hash: "",
            command_line: "",
            params: &SuiteParams::v1(1, vec![Phase::Load]),
            notes: "",
        },
    )
    .await
    .unwrap();
    assert_eq!(store::interrupt_running_bench_runs(db).await.unwrap(), 1);
    let row = store::get_bench_run(db, id).await.unwrap().unwrap();
    assert_eq!(row.status.as_str(), "interrupted");

    // The leftover carries this instance's and a chat model's labels, like
    // every bench container: reconciliation must neither adopt nor remove
    // it; the bench sweep removes it.
    let leftover = format!("{prefix}-bench-99");
    let labels: HashMap<&str, String> = HashMap::from([
        ("lmgw.instance", prefix.clone()),
        ("lmgw.class", "chat".into()),
        ("lmgw.model", "qwen".into()),
        ("lmgw.bench", "99".into()),
    ]);
    let runner = Arc::new(PsRunner {
        ps: serde_json::to_string(&json!([
            {"Names": [leftover], "Labels": labels, "State": "running", "Created": 0}
        ]))
        .unwrap(),
        calls: Mutex::new(Vec::new()),
    });
    w.state().set_runtime_for_tests(Arc::new(Registry::new(
        runner.clone(),
        reqwest::Client::new(),
    )));
    w.launcher.leftovers.lock().unwrap().push(leftover.clone());

    lmgw_core::runtime::lifecycle::boot(w.state()).await;
    assert!(w.launcher.removed(&leftover), "the bench sweep removed it");
    let registry_touched: Vec<Vec<String>> = runner
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.contains(&leftover))
        .cloned()
        .collect();
    assert!(
        registry_touched.is_empty(),
        "reconciliation handled the bench container: {registry_touched:?}"
    );
    assert!(w.state().runtime().list().is_empty(), "nothing adopted");
}

/// Boot runs unawaited (§13 decision 45): until reconciliation has adopted
/// what a previous lmgw left running, a run is refused — its drain would not
/// see those containers — and the boot sweep never removes a bench container
/// created since this process started.
#[tokio::test]
async fn a_run_waits_for_boot_and_the_sweep_spares_this_processes_containers() {
    let w = World::new(quick()).await;
    w.state().vram.set_boot_settled(false);
    let v = w.ok("bench_plan", json!({"model_id": "qwen"})).await;
    assert_eq!(v["blocked"]["reason"], "booting", "{v}");
    let msg = w.refused("bench_start", json!({"model_id": "qwen"})).await;
    assert!(msg.contains("still adopting"), "{msg}");
    assert!(w.launcher.verb("run").is_empty());

    // A leftover of a previous lmgw, and one podman created after this
    // process started: only the first is swept.
    let prefix = w.prefix();
    let (old, young) = (format!("{prefix}-bench-98"), format!("{prefix}-bench-99"));
    w.launcher
        .leftovers
        .lock()
        .unwrap()
        .extend([old.clone(), young.clone()]);
    w.launcher
        .created
        .lock()
        .unwrap()
        .insert(young.clone(), w.state().started_at_utc.timestamp() + 1);
    lmgw_core::bench::boot_sweep(w.state()).await;
    assert!(w.launcher.removed(&old));
    assert!(!w.launcher.removed(&young), "created by this process");

    w.state().vram.set_boot_settled(true);
    let v = w.ok("bench_plan", json!({"model_id": "qwen"})).await;
    assert_eq!(v["blocked"], Value::Null, "{v}");
}

/// A panic in the run (review finding 4) ends it like any failure: the
/// container is removed, the row and the job are finalised `failed`, and the
/// next run is not refused as "a run is going".
#[tokio::test]
async fn a_run_that_panics_still_cleans_up_and_frees_the_next_run() {
    let w = World::new(quick()).await;
    *w.launcher.panic_on.lock().unwrap() = Some("run".into());
    let (run_id, job) = w.start(json!({"model_id": "qwen"})).await;
    w.finished(job).await;
    let run = w.run(run_id).await["run"].clone();
    assert_eq!(run["status"], "failed", "{run}");
    let err = run["error"].as_str().unwrap();
    assert!(
        err.contains("panic") && err.contains("fake podman"),
        "{err}"
    );
    assert!(w
        .launcher
        .removed(&format!("{}-bench-{run_id}", w.prefix())));
    assert!(w.state().snapshot().gpu_lease.is_none());
    assert!(w.state().bench.current().is_none());
    let row = store::get_job(&w.state().db, job).await.unwrap().unwrap();
    assert_eq!(row.status, "failed");

    *w.launcher.panic_on.lock().unwrap() = None;
    let (next, job) = w
        .start(json!({"model_id": "qwen", "repetitions": 1, "phases": ["probes"]}))
        .await;
    w.finished(job).await;
    assert_eq!(w.run(next).await["run"]["status"], "done");
}

/// A container whose removal fails (review finding 6) is tried a few times,
/// then named on the row and in `bench_plan`'s warnings; the next run removes
/// it before it measures, and fails naming it while it still cannot.
#[tokio::test]
async fn a_container_that_will_not_go_is_reported_and_removed_by_the_next_run() {
    let w = World::new(quick()).await;
    let args = json!({"model_id": "qwen", "repetitions": 1, "phases": ["probes"]});
    w.launcher.rm_fails.store(true, Ordering::SeqCst);
    let (first, job) = w.start(args.clone()).await;
    w.finished(job).await;
    let name = format!("{}-bench-{first}", w.prefix());
    let run = w.run(first).await["run"].clone();
    assert_eq!(run["status"], "done", "it measured everything: {run}");
    let err = run["error"].as_str().unwrap();
    assert!(
        err.contains(&name) && err.contains("could not be removed") && err.contains("3 times"),
        "{err}"
    );
    assert_eq!(w.launcher.verb("rm").len(), 3, "tried three times");
    assert!(
        w.state().snapshot().gpu_lease.is_none(),
        "the lease is not kept"
    );
    let plan = w.ok("bench_plan", args.clone()).await;
    let warned = plan["warnings"].as_array().unwrap();
    assert!(
        warned.iter().any(|x| x.as_str().unwrap().contains(&name)),
        "{plan}"
    );

    // Still failing: the next run will not measure beside it.
    let (second, job) = w.start(args.clone()).await;
    w.finished(job).await;
    let run = w.run(second).await["run"].clone();
    assert_eq!(run["status"], "failed", "{run}");
    assert!(
        run["error"].as_str().unwrap().contains("still on the card"),
        "{run}"
    );
    assert!(
        w.launcher.verb("run").len() == 1,
        "the second run never loaded"
    );

    // podman recovers: the next run removes it first, and measures.
    w.launcher.rm_fails.store(false, Ordering::SeqCst);
    let (third, job) = w.start(args.clone()).await;
    w.finished(job).await;
    assert_eq!(w.run(third).await["run"]["status"], "done");
    assert!(w.launcher.removed(&name));
    let plan = w.ok("bench_plan", args).await;
    assert!(plan["warnings"].as_array().unwrap().is_empty(), "{plan}");
}

/// podman stalled on its storage lock during `podman run` (review finding
/// 5): the cancel still ends the run at once, and the end removes the
/// container by name, since the dropped `podman run` may have created it.
#[tokio::test]
async fn cancel_ends_a_run_whose_podman_run_is_stuck() {
    let w = World::new(quick()).await;
    *w.launcher.stall_on.lock().unwrap() = Some("run".into());
    let (run_id, job) = w.start(json!({"model_id": "qwen"})).await;
    w.stage(job, |s| s.starts_with("starting the bench container"))
        .await;
    w.ok("bench_cancel", json!({"run_id": run_id})).await;
    w.finished(job).await;
    let run = w.run(run_id).await["run"].clone();
    assert_eq!(run["status"], "canceled", "{run}");
    assert!(w
        .launcher
        .removed(&format!("{}-bench-{run_id}", w.prefix())));
    assert!(w.state().snapshot().gpu_lease.is_none());
}

/// Review finding 7: one row whose JSON does not decode no longer fails the
/// list or a comparison — it is listed with what could be read and marked
/// `unreadable`, and compared with nothing; the previous comparable run is
/// one lookup that steps past it (and past a GPU column that is not JSON);
/// `limit=0` is refused.
#[tokio::test]
async fn an_unreadable_run_degrades_instead_of_failing_the_list() {
    let w = World::new(quick()).await;
    let db = &w.state().db;
    let model = lmgw_api_types::bench::ModelIdentity {
        model_id: "qwen".into(),
        gguf_path: "qwen.gguf".into(),
        gguf_size: 10,
        ..Default::default()
    };
    let gpu = |name: &str| lmgw_api_types::bench::GpuIdentity {
        name: Some(name.into()),
        ..Default::default()
    };
    let mut ids = Vec::new();
    for card in ["RTX 4090", "RTX 4090", "RTX 4090", "RTX 4090", "RTX 3090"] {
        let id = store::insert_bench_run(
            db,
            &store::NewBenchRun {
                model: &model,
                build: &Default::default(),
                settings: &Default::default(),
                settings_hash: "h",
                command_line: "",
                params: &SuiteParams::v1(1, vec![Phase::Load]),
                notes: "",
            },
        )
        .await
        .unwrap();
        store::set_bench_run_record(
            db,
            id,
            &Default::default(),
            &Default::default(),
            &Default::default(),
            &gpu(card),
        )
        .await
        .unwrap();
        store::finish_bench_run(
            db,
            id,
            lmgw_api_types::bench_ops::BenchStatus::Done,
            None,
            None,
        )
        .await
        .unwrap();
        ids.push(id);
    }
    let (r0, r1, r2, r3, r4) = (ids[0], ids[1], ids[2], ids[3], ids[4]);
    // r0's GPU column is not JSON at all; r2's results are cut off.
    sqlx::query("UPDATE bench_runs SET gpu = 'not json' WHERE id = ?1")
        .bind(r0)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("UPDATE bench_runs SET results = '{\"prefill\": [' WHERE id = ?1")
        .bind(r2)
        .execute(db)
        .await
        .unwrap();

    let runs = w.ok("bench_runs", json!({})).await;
    let rows = runs["runs"].as_array().unwrap();
    assert_eq!(rows.len(), 5, "{runs}");
    let row = |id: i64| rows.iter().find(|r| r["id"] == id).unwrap().clone();
    assert!(row(r2)["unreadable"][0]
        .as_str()
        .unwrap()
        .starts_with("results:"));
    assert_eq!(row(r2)["previous_id"], Value::Null, "compared with nothing");
    assert_eq!(row(r3)["previous_id"], r1, "stepped past the unreadable r2");
    assert_eq!(row(r1)["previous_id"], Value::Null, "r0 is unreadable");
    assert_eq!(row(r4)["previous_id"], Value::Null, "another GPU");
    assert!(row(r0)["unreadable"][0]
        .as_str()
        .unwrap()
        .starts_with("gpu:"));
    assert_eq!(row(r1).get("unreadable"), None, "absent when readable");

    let d = w.run(r2).await;
    assert_eq!(d["comparison"], Value::Null);
    assert_eq!(d["run"]["unreadable"].as_array().unwrap().len(), 1);
    let d = w.run(r3).await;
    assert_eq!(d["comparison"]["base_run_id"], r1, "{d}");

    let page = w.ok("bench_runs", json!({"limit": 2})).await;
    assert_eq!(page["runs"].as_array().unwrap().len(), 2);
    assert_eq!(page["more"], true);
    let msg = w.refused("bench_runs", json!({"limit": 0})).await;
    assert!(msg.contains("at least 1"), "{msg}");
}
