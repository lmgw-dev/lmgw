//! A start this computer cannot make (machine-check design §2.5, §2.6, D23,
//! WP5): a start on the graphics card that fails is blamed by the machine
//! check — its last result while the machine is as it read it, else the
//! card and its driver read again, the full check after it in the
//! background (D96); when the card, its driver or what lets models use it
//! is a problem for that row, that is the answer — `503 gpu_setup` with the
//! check's plain sentence, or the configured fallback with
//! `x-lmgw-fallback-reason: gpu_setup` — and while that check still
//! describes the machine, a hop with a fallback goes on to it at resolve
//! without a start, for the causes the swap can tell are still so (D95). A
//! failed engine download hands on the same way (`engine_download`, D69).
//! The rows each item concerns, pinned callers, warms and lmgw's own spec
//! are `gpu_setup_rows.rs`'s.
//!
//! Over HTTP on `support/gpu_world.rs` (its `fail_run` is a start that does
//! not come up), the machine stated by `support/machine_world.rs`: nothing
//! of the computer the suite runs on is read or run.

use lmgw_api_types::machine::MachineVerdict;
use lmgw_core::config::AuxKind;
use lmgw_core::runtime::Class;
use lmgw_core::store::NewCandidateAlias;
use lmgw_core::vram::Target;

use super::engine_download::{engines, registry, scripted, ENGINE};
use super::gpu_hold::cloud_upstream;
use super::*;
use crate::support::fallbacks::set_fallback;
use crate::support::gpu_world::Gpu;
use crate::support::machine_world::Machine;

pub(super) const RESTART: &str = "'m1' could not start on the graphics card of the computer \
                                  lmgw runs on. Restart the computer to finish the graphics \
                                  driver update.";

/// A gateway whose check reads `machine`, with rows `m1` and `m2` on the
/// card, checked once: ready.
pub(super) async fn world(machine: &Machine) -> (Gpu, Gw) {
    let g = Gpu::new(24 * GIB, 6, 2).await;
    machine.state_on(&g.state);
    g.model("m1", 8 * GIB).await;
    g.model("m2", 8 * GIB).await;
    let c = lmgw_core::machine::check(&g.state, true).await;
    assert_eq!(c.verdict, MachineVerdict::Ready, "{c:#?}");
    let gw = common::serve(g.state.clone()).await;
    (g, gw)
}

/// The driver's libraries updated under a running lmgw, its module not:
/// a restart is due, and every start on the card fails.
pub(super) fn driver_updated(g: &Gpu, machine: &Machine) {
    machine.m.facts.set_driver_libraries("600.20.03");
    g.world()
        .fail_run
        .extend(["m1".to_string(), "m2".to_string()]);
}

pub(super) async fn ask(gw: &Gw, model: &str, guest: bool) -> reqwest::Response {
    let mut req = gw
        .client()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}));
    if guest {
        req = req.header("x-lmgw-background", "on");
    }
    req.send().await.unwrap()
}

pub(super) fn h(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

/// A 200 the cloud alias `cloud` answered for `reason`.
pub(super) async fn from_the_cloud(resp: reqwest::Response, reason: &str) {
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    assert_eq!(h(&resp, "x-lmgw-fallback").as_deref(), Some("cloud"));
    assert_eq!(h(&resp, "x-lmgw-fallback-reason").as_deref(), Some(reason));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(
        v["choices"][0]["message"]["content"], "from the cloud",
        "{v}"
    );
}

/// A `503 gpu_setup` whose message starts with `starts` and has every one
/// of `parts`.
pub(super) async fn not_ready(resp: reqwest::Response, starts: &str, parts: &[&str]) {
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_setup", "{body}");
    assert_eq!(body["error"]["type"], "api_error", "{body}");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.starts_with(starts), "{msg}");
    for p in parts {
        assert!(msg.contains(p), "{p:?} missing from: {msg}");
    }
}

/// The newest request-log row for `alias`, once there are more than
/// `before`.
async fn newest(state: &SharedState, alias: &str, before: usize) -> store::RequestLogRow {
    let mut wait = common::patience::Wait::new("the request-log row");
    loop {
        let rows = store::query_logs(
            &state.db,
            &store::LogFilter {
                alias: Some(alias.into()),
                limit: 1000,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if rows.len() > before {
            return rows.into_iter().next().unwrap();
        }
        wait.again(None).await;
    }
}

pub(super) fn verdict(g: &Gpu) -> MachineVerdict {
    g.state.machine.summary().unwrap().verdict
}

/// No fallback: the failed start reads the card and its driver again, and
/// the answer is the check's plain sentence — a restart, so nothing to see
/// in lmgw's window — Podman's own words kept; the full check follows in
/// the background, and the status says the new verdict, the log row the
/// code.
#[tokio::test]
async fn a_failed_start_after_a_driver_update_answers_gpu_setup() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw) = world(&machine).await;
    let runs = g.state.machine.runs();
    driver_updated(&g, &machine);

    let resp = ask(&gw, "m1", false).await;
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_setup", "{body}");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.starts_with(RESTART), "{msg}");
    assert!(!msg.contains("Open lmgw"), "a restart: {msg}");
    for p in [
        "its start said: starting chat model 'm1' failed",
        "m1 failed to start (test)",
    ] {
        assert!(msg.contains(p), "{p:?} missing from: {msg}");
    }
    assert_eq!(g.runs(), ["m1"]);
    g.state.machine.settled().await;
    assert_eq!(g.state.machine.blame_reads(), 1, "read again, once");
    assert_eq!(
        g.state.machine.runs(),
        runs + 1,
        "and checked in full, once"
    );
    assert_eq!(verdict(&g), MachineVerdict::NeedsRestart);
    let row = newest(&g.state, "m1", 0).await;
    assert_eq!(row.status, 503);
    assert_eq!(row.error_kind.as_deref(), Some("gpu_setup"));
    assert_eq!(row.fallback_reason, None);
    assert_eq!(
        lmgw_core::telemetry::outcome_of(row.status, row.error_kind.as_deref()),
        "upstream_error",
        "a failure, not a refusal"
    );

    // No fallback to go on to: the next request tries its start again,
    // since the verdict may be out of date; the machine is as the check
    // read it, so its verdict answers without reading anything again.
    not_ready(ask(&gw, "m1", false).await, RESTART, &[]).await;
    assert_eq!(g.runs(), ["m1", "m1"]);
    g.state.machine.settled().await;
    assert_eq!(g.state.machine.blame_reads(), 1);
    assert_eq!(g.state.machine.runs(), runs + 1);
}

/// The same refusal on `/v1/messages`, in Anthropic's error shape: a 503
/// whose type is `api_error` (never `overloaded_error`: no retry window
/// sets a computer up), the plain sentence as its message.
#[tokio::test]
async fn gpu_setup_on_the_messages_route_has_anthropics_shape() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw) = world(&machine).await;
    driver_updated(&g, &machine);

    let resp = gw
        .client()
        .post(format!("{gw}/v1/messages"))
        .json(&json!({
            "model": "m1",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error", "{body}");
    assert_eq!(body["error"]["type"], "api_error", "{body}");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.starts_with(RESTART), "{msg}");
    assert_eq!(g.runs(), ["m1"]);
}

/// A start that fails on a machine the last check found nothing wrong
/// with, and that is as that check read it, is the start's own 502, as
/// before — nothing read or run again for it.
#[tokio::test]
async fn a_failed_start_on_a_ready_machine_keeps_its_own_error() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw) = world(&machine).await;
    let (runs, probes) = (g.state.machine.runs(), machine.probes());
    g.world().fail_run.insert("m1".into());

    let resp = ask(&gw, "m1", false).await;
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "upstream", "{body}");
    g.state.machine.settled().await;
    assert_eq!(g.state.machine.runs(), runs, "the last check stands");
    assert_eq!(g.state.machine.blame_reads(), 0);
    assert_eq!(machine.probes(), probes);
    assert_eq!(verdict(&g), MachineVerdict::Ready);
}

/// With a fallback: the failed start hands on at admission; then, while the
/// check describes the machine, the hop goes on at resolve with no start;
/// a hop without a fallback still tries its start.
#[tokio::test]
async fn a_fallback_answers_gpu_setup_and_the_next_request_starts_nothing() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw) = world(&machine).await;
    g.cloud("cloud", None).await;
    set_fallback(&g.state, "local_models", "m1", Some("cloud")).await;
    driver_updated(&g, &machine);

    from_the_cloud(ask(&gw, "m1", false).await, "gpu_setup").await;
    assert_eq!(g.runs(), ["m1"], "one start, which failed");
    let row = newest(&g.state, "m1", 0).await;
    assert_eq!(row.status, 200);
    assert_eq!(row.fallback_reason.as_deref(), Some("gpu_setup"));

    // Once the check in the background has said so.
    g.state.machine.settled().await;
    let runs = g.state.machine.runs();
    from_the_cloud(ask(&gw, "m1", false).await, "gpu_setup").await;
    assert_eq!(g.runs(), ["m1"], "swapped at resolve: nothing started");
    assert_eq!(g.state.machine.runs(), runs, "and nothing checked");

    not_ready(
        ask(&gw, "m2", false).await,
        "'m2' could not start on the graphics card of the computer lmgw runs on. Restart the \
         computer",
        &[],
    )
    .await;
    assert_eq!(g.runs(), ["m1", "m2"]);
}

/// A restart since the check: the machine is not as the check read it, so
/// the start is tried — it works, and the check runs again by itself.
#[tokio::test]
async fn a_machine_changed_since_the_check_gets_its_start_tried() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw) = world(&machine).await;
    g.cloud("cloud", None).await;
    set_fallback(&g.state, "local_models", "m1", Some("cloud")).await;
    driver_updated(&g, &machine);
    from_the_cloud(ask(&gw, "m1", false).await, "gpu_setup").await;
    g.state.machine.settled().await;
    assert_eq!(verdict(&g), MachineVerdict::NeedsRestart);

    // The restart: the new module, a new boot; the start works again.
    machine
        .m
        .facts
        .set_kernel_module("600.20.03")
        .set_boot_id("0f6c1e9a-5b7d-4e2a-9c3f-000000000002");
    g.world().fail_run.clear();
    let resp = ask(&gw, "m1", false).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(h(&resp, "x-lmgw-fallback"), None);
    assert_eq!(g.runs(), ["m1", "m1"]);
    let mut wait = common::patience::Wait::new("the check after the start");
    while verdict(&g) != MachineVerdict::Ready {
        wait.again(None).await;
    }
}

/// A fallback that cannot answer (it does not resolve) does not keep the
/// hop from being tried: the start, checked again, and the error naming
/// why the fallback did not answer.
#[tokio::test]
async fn a_fallback_that_cannot_answer_leaves_the_start_to_be_tried() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw) = world(&machine).await;
    set_fallback(&g.state, "local_models", "m1", Some("nowhere")).await;
    driver_updated(&g, &machine);

    for n in 1..=2 {
        not_ready(
            ask(&gw, "m1", false).await,
            RESTART,
            &["(fallback 'nowhere' does not resolve)"],
        )
        .await;
        assert_eq!(g.runs().len(), n, "tried each time");
    }
}

/// NVIDIA's toolkit gone: a row that gives its container the NVIDIA card
/// is blamed on it; a row on the render nodes alone (a Vulkan engine) is
/// not, and keeps its own error.
#[tokio::test]
async fn a_missing_toolkit_is_the_reason_only_for_rows_given_the_nvidia_card() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw) = world(&machine).await;
    g.row(
        NewLocalModel {
            model_id: "vk".into(),
            gguf_path: "vk.gguf".into(),
            params: Default::default(),
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: Some(vec!["--device".into(), "/dev/dri".into()]),
            warm_start: false,
            fallback: None,
            capabilities_override: None,
            ladder: vec![],
        },
        4 * GIB,
    )
    .await;
    machine
        .m
        .facts
        .uninstall(lmgw_machine::cdi::own::NVIDIA_CTK);
    *machine.owner.lock().unwrap() = None;
    g.world()
        .fail_run
        .extend(["m1".to_string(), "vk".to_string()]);

    not_ready(
        ask(&gw, "m1", false).await,
        "'m1' could not start on the graphics card of the computer lmgw runs on. One piece is \
         missing so models can use the graphics card.",
        &["Open lmgw there to see what to do."],
    )
    .await;
    g.state.machine.settled().await;
    assert_eq!(verdict(&g), MachineVerdict::NeedsSteps);
    let resp = ask(&gw, "vk", false).await;
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "upstream", "{body}");
}

/// What the resolve-time swap reads: a row on the processor is never
/// swapped, nor one that is up; a GPU row is, while the check names a
/// problem.
#[tokio::test]
async fn a_row_on_the_processor_or_one_that_is_up_is_never_swapped() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw) = world(&machine).await;
    g.aux("emb", AuxKind::Embed, GIB).await;
    sqlx::query("UPDATE aux_models SET backend = 'cpu' WHERE model_id = 'emb'")
        .execute(&g.state.db)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();
    assert_eq!(ask(&gw, "m2", false).await.status(), 200, "m2 is up");
    machine.m.facts.set_driver_libraries("600.20.03");
    let c = lmgw_core::machine::check(&g.state, true).await;
    assert_eq!(c.verdict, MachineVerdict::NeedsRestart);

    let at = |class, id: &str| {
        lmgw_core::machine::problem_at_resolve(
            &g.state,
            &g.state.snapshot(),
            &Target {
                class,
                model_id: id.into(),
            },
        )
    };
    assert!(at(Class::Chat, "m1").is_some_and(|p| p.starts_with(
        "could not start on the graphics card of the computer lmgw runs on. Restart the computer"
    )));
    assert_eq!(at(Class::Aux, "emb"), None, "on the processor");
    assert_eq!(at(Class::Chat, "m2"), None, "up");
}

/// A candidate alias whose primary cannot start: its own fallback answers,
/// for `gpu_setup`.
#[tokio::test]
async fn a_candidate_aliases_fallback_answers_its_primarys_gpu_setup() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw) = world(&machine).await;
    g.cloud("cloud", None).await;
    g.candidate(NewCandidateAlias {
        alias: "pick".into(),
        candidates: vec!["m1".into()],
        background: false,
        fallback: Some("cloud".into()),
        capabilities_disabled: vec![],
        capabilities_enabled: vec![],
        enabled: true,
        notes: String::new(),
    })
    .await;
    driver_updated(&g, &machine);

    let resp = ask(&gw, "pick", false).await;
    assert_eq!(h(&resp, "x-lmgw-candidate"), None);
    from_the_cloud(resp, "gpu_setup").await;
    assert_eq!(g.runs(), ["m1"]);
}

/// Background traffic for a plain model: the row's fallback answers a
/// start that fails for the machine, as an owner's request is answered.
#[tokio::test]
async fn a_guests_fallback_answers_gpu_setup() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw) = world(&machine).await;
    g.cloud("cloud", None).await;
    set_fallback(&g.state, "local_models", "m1", Some("cloud")).await;
    driver_updated(&g, &machine);

    from_the_cloud(ask(&gw, "m1", true).await, "gpu_setup").await;
    assert_eq!(g.runs(), ["m1"]);
}

/// A test state's machine is blank (no card read), and a start that fails
/// there is never blamed on it: no check runs.
#[tokio::test]
async fn a_blank_machine_is_never_blamed() {
    let g = Gpu::new(24 * GIB, 2, 2).await;
    g.model("m1", 8 * GIB).await;
    let gw = common::serve(g.state.clone()).await;
    g.world().fail_run.insert("m1".into());
    let resp = ask(&gw, "m1", false).await;
    assert_eq!(resp.status(), 502);
    g.state.machine.settled().await;
    assert_eq!(g.state.machine.runs(), 0);
    assert_eq!(g.state.machine.blame_reads(), 0);
}

/// A failed engine download (D36): its own code without a fallback, and the
/// fallback with `engine_download` with one.
#[tokio::test]
async fn a_failed_engine_download_hands_on_to_the_fallback() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _reg = registry(&f).await;
    let (_api, _release) = scripted(&f, Some("manifest unknown: manifest unknown"));
    engines(&f, ENGINE, "ghcr.io/ggml-org/llama.cpp:server-cuda").await;

    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "engine_download", "{body}");

    let _cloud = cloud_upstream(&f, "cloud").await;
    set_fallback(&f.state, "local_models", "chat-model", Some("cloud")).await;
    let before = log_count(&f, "chat-model").await;
    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(fallback_reason(&resp), Some("engine_download"));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "from the cloud");
    let row = newest_log(&f, "chat-model", before).await;
    assert_eq!(row.fallback_reason.as_deref(), Some("engine_download"));
    assert!(f.runs().is_empty(), "nothing started");
}
