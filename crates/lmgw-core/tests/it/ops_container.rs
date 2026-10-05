//! `ops::container` — the per-model-container operator surface (design §8):
//! the `target`/`model` grammar, per-model start/stop/apply/logs, group
//! start/stop/apply, and the busy-refusal/override contract.
//!
//! Runs against a fake `CommandRunner` (no podman is executed) and one
//! wiremock server standing in for every container's `/health` — the fake
//! port allocator always hands out that one server's port, so every
//! `acquire` in a test answers healthy without needing a distinct mock per
//! model (this file is about the ops grammar, not port bookkeeping, which
//! `runtime_registry.rs`/`runtime_lifecycle.rs` already cover).

use std::sync::{Arc, Mutex};

use lmgw_core::config::{AuxKind, Settings};
use lmgw_core::ops;
use lmgw_core::runtime::descriptor::model_runtime;
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry};
use lmgw_core::runtime::{container_name, lifecycle, Class};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAuxModel, NewLocalModel};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// A minimal fake podman: every verb succeeds, `logs` returns canned text,
// every call is recorded so a test can assert on the argv it was given.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Vec<String>>>,
    logs: Mutex<String>,
}

fn ok(stdout: &str) -> CmdOutput {
    CmdOutput {
        status: 0,
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

#[async_trait::async_trait]
impl CommandRunner for Fake {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        assert_eq!(program, "podman", "the registry only ever shells podman");
        self.calls.lock().unwrap().push(args.to_vec());
        match args[0].as_str() {
            "run" => Ok(ok("c0ffee\n")),
            "logs" => Ok(ok(&self.logs.lock().unwrap().clone())),
            _ => Ok(ok("")),
        }
    }
}

impl Fake {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    fn verb(&self, v: &str) -> Vec<Vec<String>> {
        self.calls().into_iter().filter(|c| c[0] == v).collect()
    }

    /// Names passed to `podman stop`, in order.
    fn stopped_names(&self) -> Vec<String> {
        self.verb("stop")
            .into_iter()
            .map(|c| c.last().cloned().unwrap_or_default())
            .collect()
    }

    fn run_count(&self) -> usize {
        self.verb("run").len()
    }

    fn set_logs(&self, text: &str) {
        *self.logs.lock().unwrap() = text.to_string();
    }
}

/// A single mock server every fake `podman run` "starts" — it answers
/// `/health` (llama), `/v1/models` (audio) and `/sdcpp/v1/capabilities`
/// (image) with 200, so the registry's readiness poll succeeds regardless of
/// which model or class acquired it.
async fn healthy() -> MockServer {
    let server = MockServer::start().await;
    for p in ["/health", "/v1/models"] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"supported_modes":["img_gen"],"current_mode":"img_gen"}"#),
        )
        .mount(&server)
        .await;
    server
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    state: SharedState,
    podman: Arc<Fake>,
    _models_dir: tempfile::TempDir,
    _server: MockServer,
}

/// A gateway with the given chat/aux models (`(model_id, warm_start)`), a
/// stateful fake podman, and a fixed healthy mock every start lands on. Chat
/// model ids get an on-disk `.gguf` so the group-apply pre-flight finds
/// nothing wrong with them by default — a test wanting a "missing file"
/// finding just omits writing one for that id.
async fn fixture(chat: &[(&str, bool)], aux: &[(&str, bool)]) -> Fixture {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let server = healthy().await;
    let port = server.address().port();

    let podman = Arc::new(Fake::default());
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        podman.clone(),
        reqwest::Client::new(),
        Arc::new(move || Ok(port)),
    )));

    let mut s = Settings::default();
    s.router.models_dir = dir.path().display().to_string();
    s.aux_router.models_dir = dir.path().display().to_string();
    s.vram.load_timeout_seconds = 5;
    s.vram.unload_timeout_seconds = 5;
    store::save_settings(&state.db, &s).await.unwrap();

    for (model_id, warm_start) in chat {
        std::fs::write(dir.path().join(format!("{model_id}.gguf")), b"not a gguf").unwrap();
        store::insert_local_model(
            &state.db,
            &NewLocalModel {
                model_id: (*model_id).into(),
                gguf_path: format!("{model_id}.gguf"),
                params: Default::default(),
                args: vec![],
                idle_seconds: 0,
                enabled: true,
                public: true,
                image: None,
                extra_run_args: None,
                warm_start: *warm_start,
                hold_fallback_mode: Default::default(),
                hold_fallback: None,
                capabilities_override: None,
                ladder: vec![],
            },
        )
        .await
        .unwrap();
    }
    for (model_id, warm_start) in aux {
        store::insert_aux_model(
            &state.db,
            &NewAuxModel {
                model_id: (*model_id).into(),
                gguf_path: format!("{model_id}.gguf"),
                kind: AuxKind::Embed,
                pooling: None,
                ctx_size: None,
                args: vec![],
                idle_seconds: 0,
                enabled: true,
                image: None,
                extra_run_args: None,
                warm_start: *warm_start,
                hold_fallback_mode: Default::default(),
                hold_fallback: None,
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();

    Fixture {
        state,
        podman,
        _models_dir: dir,
        _server: server,
    }
}

/// Skip writing a `.gguf` for `model_id` after [`fixture`] created one, so a
/// group-apply pre-flight has something to complain about.
fn make_missing(f: &Fixture, model_id: &str) {
    std::fs::remove_file(f._models_dir.path().join(format!("{model_id}.gguf"))).unwrap();
}

// ---------------------------------------------------------------------------
// Per-model actions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn per_model_start_status_and_stop_round_trip() {
    let f = fixture(&[("m1", false)], &[]).await;

    let out = ops::container(&f.state, None, Some("m1"), "start", false, None)
        .await
        .unwrap();
    assert_eq!(out["ok"], true);
    assert!(out["port"].as_u64().unwrap() > 0);
    assert_eq!(f.podman.run_count(), 1);

    let status = ops::container(&f.state, None, Some("m1"), "status", false, None)
        .await
        .unwrap();
    assert_eq!(status["class"], "chat");
    assert_eq!(status["model_id"], "m1");
    assert_eq!(status["runtime"]["state"], "ready");
    assert_eq!(status["runtime"]["model_id"], "m1");

    let stop = ops::container(&f.state, None, Some("m1"), "stop", false, None)
        .await
        .unwrap();
    assert_eq!(stop["ok"], true);
    assert_eq!(
        f.podman.stopped_names(),
        vec![container_name("lmgw", Class::Chat, "m1")]
    );

    let status = ops::container(&f.state, None, Some("m1"), "status", false, None)
        .await
        .unwrap();
    assert!(status["runtime"].is_null(), "{status}");
}

#[tokio::test]
async fn per_model_logs_reads_the_containers_recent_output() {
    let f = fixture(&[("m1", false)], &[]).await;
    f.podman.set_logs("booting\nready to serve\n");

    ops::container(&f.state, None, Some("m1"), "start", false, None)
        .await
        .unwrap();
    let out = ops::container(&f.state, None, Some("m1"), "logs", false, Some(5))
        .await
        .unwrap();
    assert_eq!(out["tail"], 5);
    assert_eq!(out["container"], container_name("lmgw", Class::Chat, "m1"));
    assert!(
        out["logs"].as_str().unwrap().contains("ready to serve"),
        "{out}"
    );

    // `--tail <n>` reached podman verbatim.
    let last_logs_call = f.podman.verb("logs").pop().unwrap();
    assert!(
        last_logs_call.contains(&"5".to_string()),
        "{last_logs_call:?}"
    );
}

#[tokio::test]
async fn per_model_logs_work_before_the_model_has_ever_started() {
    let f = fixture(&[("m1", false)], &[]).await;
    // Default tail (60) applies when the caller doesn't pass one.
    let out = ops::container(&f.state, None, Some("m1"), "logs", false, None)
        .await
        .unwrap();
    assert_eq!(out["tail"], 60);
    assert_eq!(out["container"], container_name("lmgw", Class::Chat, "m1"));
    assert!(
        f.podman.verb("run").is_empty(),
        "logs must not start anything"
    );
}

#[tokio::test]
async fn per_model_apply_on_a_running_model_recreates_it_with_fresh_argv() {
    let f = fixture(&[("m1", false)], &[]).await;
    ops::container(&f.state, None, Some("m1"), "start", false, None)
        .await
        .unwrap();
    assert_eq!(f.podman.run_count(), 1);

    let out = ops::container(&f.state, None, Some("m1"), "apply", false, None)
        .await
        .unwrap();
    assert_eq!(out["ok"], true);
    assert_eq!(f.podman.run_count(), 2, "apply stops then starts again");
    assert_eq!(
        f.podman.stopped_names(),
        vec![container_name("lmgw", Class::Chat, "m1")]
    );
}

/// §3.6: "running → stop + start (fresh argv); **not running → no-op**".
///
/// Apply exists to make a *running* container pick up an edit. A stopped model
/// has nothing to pick up — its argv is rendered from the current
/// configuration at the next start, whenever that is — so starting it here
/// would answer "apply my edit" with an unasked-for VRAM allocation. The
/// response says so instead, and nothing is started.
#[tokio::test]
async fn per_model_apply_on_a_stopped_model_is_a_no_op() {
    let f = fixture(&[("m1", false)], &[]).await;
    for action in ["apply", "restart"] {
        let out = ops::container(&f.state, None, Some("m1"), action, false, None)
            .await
            .unwrap();
        assert_eq!(out["ok"], true, "{action}: {out}");
        assert_eq!(out["applied"], false, "{action}: {out}");
        assert_eq!(out["running"], false, "{action}: {out}");
        let message = out["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("not running") && message.contains("action=start"),
            "the no-op has to say what it did not do, and how to start it: {message}"
        );
    }
    assert_eq!(f.podman.run_count(), 0, "apply must not start a cold model");
    assert!(f.podman.stopped_names().is_empty());

    // …and the model is still startable, so the no-op did not leave anything
    // half-decided behind.
    ops::container(&f.state, None, Some("m1"), "start", false, None)
        .await
        .unwrap();
    assert_eq!(f.podman.run_count(), 1);
}

#[tokio::test]
async fn stop_refuses_a_busy_model_and_override_forces_it() {
    let f = fixture(&[("m1", false)], &[]).await;
    ops::container(&f.state, None, Some("m1"), "start", false, None)
        .await
        .unwrap();

    // Hold an in-flight claim the way a real request would, directly against
    // the registry — `ops::container`'s own `start` drops its guard
    // immediately (design §3.2: "warm it", not "claim it").
    let snap = f.state.snapshot();
    let rt = model_runtime(&snap, Class::Chat, "m1").unwrap();
    let spec = lifecycle::acquire_spec(&f.state, &snap, &rt);
    let guard = f.state.runtime().acquire(&spec).await.unwrap();

    let err = ops::container(&f.state, None, Some("m1"), "stop", false, None)
        .await
        .unwrap_err();
    assert!(err.contains("still serving 1 request"), "{err}");
    assert!(err.contains("override=true"), "{err}");
    assert!(f.podman.stopped_names().is_empty());

    let out = ops::container(&f.state, None, Some("m1"), "stop", true, None)
        .await
        .unwrap();
    assert_eq!(out["ok"], true);
    assert_eq!(
        f.podman.stopped_names(),
        vec![container_name("lmgw", Class::Chat, "m1")]
    );

    drop(guard);
}

// ---------------------------------------------------------------------------
// Errors name the offender
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_model_names_itself_in_the_error() {
    let f = fixture(&[("m1", false)], &[]).await;
    let err = ops::container(&f.state, None, Some("nope"), "status", false, None)
        .await
        .unwrap_err();
    assert!(err.contains("nope"), "{err}");
}

#[tokio::test]
async fn unknown_target_names_itself_in_the_error() {
    let f = fixture(&[("m1", false)], &[]).await;
    let err = ops::container(&f.state, Some("bogus"), None, "status", false, None)
        .await
        .unwrap_err();
    assert!(err.contains("bogus"), "{err}");
}

#[tokio::test]
async fn a_model_id_shared_across_classes_needs_target_to_disambiguate() {
    let f = fixture(&[("shared", false)], &[("shared", false)]).await;

    let err = ops::container(&f.state, None, Some("shared"), "status", false, None)
        .await
        .unwrap_err();
    assert!(err.contains("more than one class"), "{err}");
    assert!(err.contains("chat") && err.contains("aux"), "{err}");

    let out = ops::container(&f.state, Some("aux"), Some("shared"), "status", false, None)
        .await
        .unwrap();
    assert_eq!(out["class"], "aux");
}

#[tokio::test]
async fn target_and_model_that_disagree_are_rejected() {
    let f = fixture(&[("m1", false)], &[]).await;
    let err = ops::container(&f.state, Some("aux"), Some("m1"), "status", false, None)
        .await
        .unwrap_err();
    assert!(err.contains("m1") && err.contains("chat"), "{err}");
}

// ---------------------------------------------------------------------------
// Group actions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn group_status_reports_the_runtime_list_and_vram_no_legacy_shape() {
    let f = fixture(&[("a", false), ("b", false)], &[]).await;
    ops::container(&f.state, None, Some("a"), "start", false, None)
        .await
        .unwrap();

    let out = ops::container(&f.state, Some("chat"), None, "status", false, None)
        .await
        .unwrap();
    assert_eq!(out["target"], "chat");
    assert!(
        out.get("state").is_none(),
        "the old single-status shape must be gone: {out}"
    );
    let runtime = out["runtime"].as_array().unwrap();
    assert_eq!(runtime.len(), 1);
    assert_eq!(runtime[0]["model_id"], "a");
    assert!(out["vram"].is_object());
}

#[tokio::test]
async fn group_stop_stops_every_running_member_of_the_class() {
    let f = fixture(&[("a", false), ("b", false)], &[]).await;
    for id in ["a", "b"] {
        ops::container(&f.state, None, Some(id), "start", false, None)
            .await
            .unwrap();
    }
    assert_eq!(f.state.runtime().list().len(), 2);

    let out = ops::container(&f.state, Some("chat"), None, "stop", false, None)
        .await
        .unwrap();
    assert_eq!(out["ok"], true);
    let stopped: Vec<&str> = out["stopped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["model_id"].as_str().unwrap())
        .collect();
    assert!(stopped.contains(&"a") && stopped.contains(&"b"), "{out}");
    assert!(f.state.runtime().list().is_empty());
}

#[tokio::test]
async fn group_start_only_warms_the_warm_start_flagged_models() {
    let f = fixture(&[("warm", true), ("cold", false)], &[]).await;

    let out = ops::container(&f.state, Some("chat"), None, "start", false, None)
        .await
        .unwrap();
    assert_eq!(out["ok"], true);
    let started: Vec<&str> = out["started"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["model_id"].as_str().unwrap())
        .collect();
    assert_eq!(started, vec!["warm"]);
    assert_eq!(f.podman.run_count(), 1);

    let live = f.state.runtime().list();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].model_id, "warm");
    assert_eq!(live[0].in_flight, 0, "warm means resident, not claimed");
}

#[tokio::test]
async fn group_apply_reports_the_pre_flight_and_recreates_only_running_members() {
    let f = fixture(&[("clean", false), ("broken", false)], &[]).await;
    make_missing(&f, "broken");

    ops::container(&f.state, None, Some("clean"), "start", false, None)
        .await
        .unwrap();
    assert_eq!(f.podman.run_count(), 1);

    let out = ops::container(&f.state, Some("chat"), None, "apply", false, None)
        .await
        .unwrap();
    assert_eq!(out["ok"], true);
    assert_eq!(out["models_enabled"], 2);
    assert_eq!(out["models_with_problems"], 1);
    let problems = out["problems"].as_array().unwrap();
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0]["model_id"], "broken");
    assert!(
        problems[0]["issues"][0]
            .as_str()
            .unwrap()
            .contains("missing from the models dir"),
        "{out}"
    );

    // Only "clean" was running, so only "clean" gets recreated — "broken"
    // having a problem does not make apply try to start it.
    let recreated: Vec<&str> = out["recreated"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["model_id"].as_str().unwrap())
        .collect();
    assert_eq!(recreated, vec!["clean"]);
    assert_eq!(
        f.podman.run_count(),
        2,
        "apply recreated the one running member"
    );
}

// ---------------------------------------------------------------------------
// `ops::status` — the same runtime shape, in the gateway-wide surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gateway_status_carries_runtime_and_drops_the_old_containers_key() {
    let f = fixture(&[("m1", false)], &[]).await;
    ops::container(&f.state, None, Some("m1"), "start", false, None)
        .await
        .unwrap();

    let status = ops::status(&f.state).await.unwrap();
    assert!(status.get("containers").is_none(), "{status}");
    let runtime = status["runtime"].as_array().expect("runtime array");
    assert_eq!(runtime.len(), 1);
    assert_eq!(runtime[0]["model_id"], "m1");
    assert_eq!(runtime[0]["class"], "chat");
}

// ---------------------------------------------------------------------------
// The image class's pre-flight (image-generation design §4)
// ---------------------------------------------------------------------------

/// An image row's pre-flight answers three questions the other classes never
/// have to: does every file it names exist, is every key a flag sd-server
/// has, and does it name exactly one of the two ways to load a pipeline. All
/// three arrive through the same `models_with_problems` shape group apply has
/// always used, and `target=image` is a group like any other.
#[tokio::test]
async fn group_apply_reports_the_image_pre_flight() {
    let f = fixture(&[], &[]).await;
    let dir = f._models_dir.path();
    std::fs::write(dir.join("z.gguf"), b"weights").unwrap();

    let mut s = f.state.snapshot().settings.clone();
    s.image.models_dir = dir.display().to_string();
    store::save_settings(&f.state.db, &s).await.unwrap();

    // A row with nothing wrong with it.
    store::insert_image_model(
        &f.state.db,
        &store::NewImageModel {
            model_id: "clean".into(),
            files: serde_json::json!({"diffusion_model": "z.gguf"})
                .as_object()
                .cloned()
                .unwrap(),
            args: serde_json::json!({"diffusion_fa": true})
                .as_object()
                .cloned()
                .unwrap(),
            modes: vec!["img_gen".into()],
            enabled: true,
            idle_seconds: 300,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // One of each finding: a missing file, a key sd-server does not have, and
    // no way to load a pipeline at all.
    store::insert_image_model(
        &f.state.db,
        &store::NewImageModel {
            model_id: "broken".into(),
            files: serde_json::json!({"vae": "nowhere/ae.safetensors"})
                .as_object()
                .cloned()
                .unwrap(),
            args: serde_json::json!({"cfg_scail": 1.0})
                .as_object()
                .cloned()
                .unwrap(),
            modes: vec!["img_gen".into()],
            enabled: true,
            idle_seconds: 300,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();

    let out = ops::container(&f.state, Some("image"), None, "apply", false, None)
        .await
        .unwrap();
    assert_eq!(out["ok"], true);
    assert_eq!(out["target"], "image");
    assert_eq!(out["models_enabled"], 2);
    assert_eq!(out["models_with_problems"], 1);

    let problems = out["problems"].as_array().unwrap();
    assert_eq!(problems.len(), 1, "{out}");
    assert_eq!(problems[0]["class"], "image");
    assert_eq!(problems[0]["model_id"], "broken");
    let issues: Vec<&str> = problems[0]["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        issues.iter().any(|i| i.contains("cfg_scail")),
        "the unknown key is named: {issues:?}"
    );
    assert!(
        issues.iter().any(|i| i.contains("neither 'model'")),
        "a row with no pipeline to load is refused: {issues:?}"
    );
    assert!(
        issues
            .iter()
            .any(|i| i.contains("nowhere/ae.safetensors") && i.contains("missing")),
        "the missing file is named: {issues:?}"
    );
}

/// The image class is a group and a model like any other: `target=image`
/// filters it, `model=<id>` finds its class from the `image_models` table
/// without being told, and start / stop / apply / logs / status all reach the
/// one container that row owns.
///
/// Nothing here is image-specific mechanism — that is the point. WP1 made
/// `Class::Image` real everywhere the lifecycle is generic, so this asserts
/// the fourth class fell out of that rather than needing a fourth code path.
#[tokio::test]
async fn the_image_class_starts_stops_and_applies_like_every_other() {
    let f = fixture(&[], &[]).await;
    let dir = f._models_dir.path();
    std::fs::write(dir.join("z.gguf"), b"weights").unwrap();
    let mut s = f.state.snapshot().settings.clone();
    s.image.models_dir = dir.display().to_string();
    store::save_settings(&f.state.db, &s).await.unwrap();
    store::insert_image_model(
        &f.state.db,
        &store::NewImageModel {
            model_id: "z-image".into(),
            files: serde_json::json!({"diffusion_model": "z.gguf"})
                .as_object()
                .cloned()
                .unwrap(),
            modes: vec!["img_gen".into()],
            enabled: true,
            idle_seconds: 300,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();

    // `podman run` is also how the class's `--help` vocabulary is probed
    // (WP1: the sd-server binary links libcuda directly, so the throwaway
    // needs the device flags), so count only the runs that name *this* row's
    // container.
    let name = container_name(
        &f.state.snapshot().settings.container_prefix,
        Class::Image,
        "z-image",
    );
    let starts = || {
        f.podman
            .verb("run")
            .into_iter()
            .filter(|c| c.contains(&name))
            .count()
    };

    // The class is found from the id alone.
    let out = ops::container(&f.state, None, Some("z-image"), "start", false, None)
        .await
        .unwrap();
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["class"], "image");
    assert_eq!(starts(), 1, "the container was never started");

    let status = ops::container(&f.state, None, Some("z-image"), "status", false, None)
        .await
        .unwrap();
    assert_eq!(status["class"], "image");
    assert_eq!(status["engine"], "sdcpp");
    assert_eq!(status["runtime"]["state"], "ready");

    // Apply recreates: one more stop and one more run, on the image-class name.
    let applied = ops::container(&f.state, None, Some("z-image"), "apply", false, None)
        .await
        .unwrap();
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(starts(), 2);
    assert_eq!(f.podman.stopped_names(), vec![name.clone()]);

    // The group verbs see it under its own target and not under another's.
    let group = ops::container(&f.state, Some("image"), None, "status", false, None)
        .await
        .unwrap();
    assert_eq!(group["target"], "image");
    assert_eq!(
        group["runtime"].as_array().map(Vec::len),
        Some(1),
        "{group}"
    );
    let chat = ops::container(&f.state, Some("chat"), None, "status", false, None)
        .await
        .unwrap();
    assert_eq!(chat["runtime"].as_array().map(Vec::len), Some(0), "{chat}");

    let stopped = ops::container(&f.state, Some("image"), None, "stop", false, None)
        .await
        .unwrap();
    assert_eq!(stopped["ok"], true, "{stopped}");
    assert!(f.state.runtime().list().is_empty());

    // Logs are per model, and the image class is no exception.
    f.podman.set_logs("sd-server: loading tensors completed\n");
    let logs = ops::container(&f.state, None, Some("z-image"), "logs", false, Some(5))
        .await
        .unwrap();
    assert!(
        logs["logs"].as_str().unwrap().contains("loading tensors"),
        "{logs}"
    );
    assert!(model_runtime(&f.state.snapshot(), Class::Image, "z-image").is_some());
}
