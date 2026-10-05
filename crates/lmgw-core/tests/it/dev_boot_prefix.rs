//! A fresh dev data dir never touches another prefix's containers
//! (chat-voice WP5 review B1).
//!
//! A fresh `LMGW_DATA_DIR` loads settings on the production
//! `container_prefix`, the installed app's. Only the entry point's
//! dev-instance step — the headless runner's `ensure_dev_instance_safety`, a
//! debug shell's refusal — makes the prefix the dev instance's own, and it
//! runs between `AppState::init_with` and `server::run`. Agent-container
//! reconciliation used to be spawned from `init_with`: it listed the
//! installed app's agent containers, removed every one, and swept its run
//! directories before the step had run.
//!
//! The real `init_with` (through its host seam) on a real, fresh data dir,
//! with a podman that answers like a box where the installed app runs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use lmgw_core::agents::container::{KIND_AGENT, LABEL_INSTANCE, LABEL_KIND, LABEL_RUN};
use lmgw_core::config::{default_container_prefix, dev_instance_override};
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner};
use lmgw_core::state::AppState;
use lmgw_core::store;
use lmgw_core::vram::nvml::NoTelemetry;
use serde_json::{json, Value};

/// A podman with the installed app's containers in it: an agent's service
/// container and a model's, both under the production prefix and older than
/// this process. `ps` applies its filters the way podman does (each
/// `--filter` ANDed); every call is recorded.
struct InstalledAppPodman {
    containers: Vec<Value>,
    calls: Mutex<Vec<Vec<String>>>,
}

impl InstalledAppPodman {
    fn new() -> Self {
        let prod = default_container_prefix();
        let old = chrono::Utc::now().timestamp() - 3600;
        Self {
            containers: vec![
                json!({
                    "Names": [format!("{prod}-agentsvc-board")],
                    "Labels": {LABEL_KIND: KIND_AGENT, LABEL_INSTANCE: prod, LABEL_RUN: "service"},
                    "State": "running",
                    "Created": old,
                }),
                json!({
                    "Names": [format!("{prod}-chat-talk")],
                    "Labels": {LABEL_INSTANCE: prod, "lmgw.class": "chat", "lmgw.model": "talk"},
                    "State": "running",
                    "Created": old,
                }),
            ],
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    fn matches(c: &Value, filter: &str) -> bool {
        let labels: HashMap<String, String> =
            serde_json::from_value(c["Labels"].clone()).unwrap_or_default();
        if let Some(kv) = filter.strip_prefix("label=") {
            return match kv.split_once('=') {
                Some((k, v)) => labels.get(k).is_some_and(|got| got == v),
                None => labels.contains_key(kv),
            };
        }
        if let Some(name) = filter.strip_prefix("name=") {
            return c["Names"][0].as_str().is_some_and(|n| n.contains(name));
        }
        panic!("a filter this fake does not know: {filter}");
    }
}

fn ok(stdout: String) -> CmdOutput {
    CmdOutput {
        status: 0,
        stdout,
        stderr: String::new(),
    }
}

#[async_trait]
impl CommandRunner for InstalledAppPodman {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        assert_eq!(program, "podman");
        self.calls.lock().unwrap().push(args.to_vec());
        if args.first().map(String::as_str) != Some("ps") {
            return Ok(ok(String::new()));
        }
        let filters: Vec<&str> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(f, _)| *f == "--filter")
            .map(|(_, v)| v.as_str())
            .collect();
        let rows: Vec<&Value> = self
            .containers
            .iter()
            .filter(|c| filters.iter().all(|f| Self::matches(c, f)))
            .collect();
        Ok(ok(serde_json::to_string(&rows).unwrap()))
    }
}

#[tokio::test]
async fn a_fresh_dev_data_dir_never_lists_or_removes_another_prefixs_containers() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    // The builds dir inside the scratch dir: a dev instance's default is the
    // shared ~/.cache/lmgw-dev/builds, and `init` spawns its boot sweep. The
    // prefix is left alone — the production default is the point.
    {
        let db = store::open(&data.join("lmgw.sqlite")).await.unwrap();
        let mut s = store::load_snapshot(&db).await.unwrap().settings;
        assert_eq!(s.container_prefix, default_container_prefix());
        s.builds_dir = Some(data.join("builds").display().to_string());
        store::save_settings(&db, &s).await.unwrap();
        db.close().await;
    }
    // A leftover run directory under the data dir's production leaf, which
    // the agent sweep would remove — the stand-in for the installed app's
    // `$XDG_RUNTIME_DIR/lmgw/lmgw/run-*`, which a test must not create.
    let leftover = data.join("agents").join("lmgw").join("run-7");
    std::fs::create_dir_all(&leftover).unwrap();
    let when = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::open(&leftover)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(when))
        .unwrap();

    let podman = Arc::new(InstalledAppPodman::new());
    let state = AppState::init_with_host(
        data.clone(),
        true,
        podman.clone(),
        Arc::new(NoTelemetry("test: no GPU".into())),
    )
    .await
    .unwrap();
    assert_eq!(
        state.snapshot().settings.container_prefix,
        default_container_prefix()
    );
    // Whatever `init` spawned gets to run: on this current-thread runtime a
    // spawned task runs only while the test waits, and this podman answers at
    // once. Nothing below depends on how long this is; it only gives a
    // regression the room to show itself.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        podman.calls(),
        Vec::<Vec<String>>::new(),
        "AppState::init_with ran podman while the prefix was still the installed app's"
    );
    assert!(
        leftover.exists(),
        "AppState::init_with swept run directories before the dev-instance step"
    );

    // An entry point that forgot its step: the server refuses to start.
    let refused = lmgw_core::server::run(
        state.clone(),
        "127.0.0.1:0".parse().unwrap(),
        std::future::pending(),
    )
    .await
    .expect_err("a dev instance on the production prefix must not serve");
    assert!(
        refused.to_string().contains("production default"),
        "{refused}"
    );
    assert!(podman.calls().is_empty(), "{:?}", podman.calls());

    // The headless runner's step, then the passes `server::run` spawns.
    let prefix = format!("lmgw-it-b1-{}", std::process::id());
    let (moved, _) = dev_instance_override(
        &state.snapshot().settings.container_prefix,
        Some(&prefix),
        None,
    )
    .expect("a fresh dir is on the default prefix");
    let mut s = state.snapshot().settings.clone();
    s.container_prefix = moved;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    lmgw_core::runtime::lifecycle::boot(&state).await;
    lmgw_core::agents::container::boot_reconcile(&state).await;

    let calls = podman.calls();
    let listings: Vec<&Vec<String>> = calls.iter().filter(|c| c[0] == "ps").collect();
    assert!(
        listings.len() >= 2,
        "model and agent reconciliation both list: {calls:?}"
    );
    let own = format!("label={LABEL_INSTANCE}={prefix}");
    for c in &listings {
        assert!(
            c.contains(&own),
            "a listing not scoped to this instance's prefix: {c:?}"
        );
    }
    let prod = format!("label={LABEL_INSTANCE}={}", default_container_prefix());
    assert!(
        !calls.iter().flatten().any(|a| a == &prod),
        "a call named the installed app's prefix: {calls:?}"
    );
    let touched: Vec<&Vec<String>> = calls
        .iter()
        .filter(|c| matches!(c[0].as_str(), "rm" | "stop" | "kill" | "wait"))
        .collect();
    assert!(
        touched.is_empty(),
        "nothing exists under the dev prefix, so nothing may be stopped or removed: {touched:?}"
    );
}

/// What `POST /api/op/settings_set_full` answered: status and JSON body.
async fn settings_save(gw: &crate::common::Gw, patch: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/settings_set_full"))
        .json(&patch)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The boot refusal's rule on the settings write too (chat-voice WP11 review
/// m1): a dev window cannot move its own instance onto the production
/// prefix, whose next model start would `--replace` the installed app's
/// container of the same name. Same message as `server::run`'s.
#[tokio::test]
async fn a_dev_instance_refuses_the_production_prefix_as_a_settings_edit() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let dev_prefix = format!("lmgw-it-m1-{}", std::process::id());
    {
        let db = store::open(&data.join("lmgw.sqlite")).await.unwrap();
        let mut s = store::load_snapshot(&db).await.unwrap().settings;
        s.builds_dir = Some(data.join("builds").display().to_string());
        s.container_prefix = dev_prefix.clone();
        store::save_settings(&db, &s).await.unwrap();
        db.close().await;
    }
    let podman = Arc::new(InstalledAppPodman::new());
    let state = AppState::init_with_host(
        data.clone(),
        true,
        podman.clone(),
        Arc::new(NoTelemetry("test: no GPU".into())),
    )
    .await
    .unwrap();
    assert!(state.dev());
    let gw = crate::common::serve(state.clone()).await;

    let prod = default_container_prefix();
    let want = lmgw_core::config::dev_prefix_refusal(true, &prod).unwrap();
    for spelled in [prod.clone(), format!(" {prod} ")] {
        let (status, body) = settings_save(&gw, json!({ "container_prefix": spelled })).await;
        assert_eq!(status, 400, "{spelled:?}: {body}");
        assert_eq!(body["code"], json!("dev_production_prefix"), "{body}");
        assert_eq!(body["message"], json!(want), "the boot refusal's message");
        assert_eq!(
            state.snapshot().settings.container_prefix,
            dev_prefix,
            "a refused save changed the prefix"
        );
    }
    // A save that carries the prefix among other fields is refused whole.
    let before = state.snapshot().settings.retention_days;
    let (status, body) = settings_save(
        &gw,
        json!({ "container_prefix": prod, "retention_days": before + 1 }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(state.snapshot().settings.retention_days, before);

    // Another dev prefix is a normal edit.
    let other = format!("{dev_prefix}-b");
    let (status, body) = settings_save(&gw, json!({ "container_prefix": other })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(state.snapshot().settings.container_prefix, other);
    assert!(podman.calls().is_empty(), "{:?}", podman.calls());
}

/// Production may move off its prefix and back: the rule is a dev
/// instance's only.
#[tokio::test]
async fn production_may_take_its_own_prefix_back() {
    let state = AppState::init_for_tests().await.unwrap();
    assert!(!state.dev());
    let gw = crate::common::serve(state.clone()).await;
    let prod = default_container_prefix();
    for prefix in [format!("{prod}-it-m1-away"), prod.clone()] {
        let (status, body) = settings_save(&gw, json!({ "container_prefix": prefix })).await;
        assert_eq!(status, 200, "{prefix}: {body}");
        assert_eq!(state.snapshot().settings.container_prefix, prefix);
    }
}

/// A fresh dev data dir on a prefix of its own, through the podman above:
/// the passes `server::serve` spawns ask only that.
async fn dev_state_on_own_prefix(
    data: &std::path::Path,
    prefix: &str,
) -> (lmgw_core::state::SharedState, Arc<InstalledAppPodman>) {
    {
        let db = store::open(&data.join("lmgw.sqlite")).await.unwrap();
        let mut s = store::load_snapshot(&db).await.unwrap().settings;
        s.builds_dir = Some(data.join("builds").display().to_string());
        s.container_prefix = prefix.to_string();
        store::save_settings(&db, &s).await.unwrap();
        db.close().await;
    }
    let podman = Arc::new(InstalledAppPodman::new());
    let state = AppState::init_with_host(
        data.to_path_buf(),
        true,
        podman.clone(),
        Arc::new(NoTelemetry("test: no GPU".into())),
    )
    .await
    .unwrap();
    (state, podman)
}

/// The shell's "Restart gateway" (chat-voice WP11 review m3): `bind` says
/// whether this process holds the port, and `serve`'s release signal lets
/// the restart bind the same port again without racing the old listener.
/// The background tasks run once per state, whatever the restarts.
#[tokio::test]
async fn the_server_reports_its_bind_and_releases_its_port_on_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = format!("lmgw-it-m3-{}", std::process::id());
    let (state, podman) = dev_state_on_own_prefix(dir.path(), &prefix).await;

    let listener = lmgw_core::server::bind(&state, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    // The port is this process's now: a second bind is refused, by name.
    let taken = lmgw_core::server::bind(&state, addr)
        .await
        .expect_err("a held port binds once");
    assert!(taken.to_string().contains(&addr.to_string()), "{taken}");

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (released_tx, released) = tokio::sync::oneshot::channel();
    let first = tokio::spawn(lmgw_core::server::serve(
        state.clone(),
        listener,
        async {
            let _ = stopped.await;
        },
        Some(released_tx),
    ));
    let page = format!("http://{addr}/");
    // No pooled connection: the second request must reach the second server.
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    assert_eq!(http.get(&page).send().await.unwrap().status(), 200);

    stop.send(()).unwrap();
    released
        .await
        .expect("the release fires, it is not dropped");
    // Released means bindable, whatever the first server still drains.
    let listener = lmgw_core::server::bind(&state, addr)
        .await
        .expect("the released port binds again at once");
    let (stop2, stopped2) = tokio::sync::oneshot::channel::<()>();
    let second = tokio::spawn(lmgw_core::server::serve(
        state.clone(),
        listener,
        async {
            let _ = stopped2.await;
        },
        None,
    ));
    assert_eq!(http.get(&page).send().await.unwrap().status(), 200);
    // The background tasks belong to the state, not to a server: the
    // restart started no second reaper, status tick or boot pass (one
    // agent-container reconciliation, not one per start).
    let agent = format!("label={LABEL_KIND}={KIND_AGENT}");
    let reconciled = || {
        podman
            .calls()
            .iter()
            .filter(|c| c.iter().any(|a| a == &agent))
            .count()
    };
    for _ in 0..200 {
        if reconciled() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(reconciled(), 1, "{:?}", podman.calls());
    assert!(
        !lmgw_core::server::spawn_background_tasks(state.clone()),
        "they ran already"
    );
    stop2.send(()).unwrap();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();

    let prod = format!("label={LABEL_INSTANCE}={}", default_container_prefix());
    assert!(
        !podman.calls().iter().flatten().any(|a| a == &prod),
        "{:?}",
        podman.calls()
    );
}
