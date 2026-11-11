//! A container start's pinned and seeded settings (headless server design
//! §2.2): the start writes them before the snapshot is first read, every
//! settings save refuses a change of a pinned one, the settings answer lists
//! them, and Settings → About gets the stop budget (D20).
//!
//! The real initialisation through its host seams, on a real data dir, with
//! a podman that answers nothing and no GPU.

use std::sync::{Arc, Mutex};

use lmgw_api_types::RESTART_COMMAND;
use lmgw_core::config::deployment::{
    ContainerEnv, Deployment, PinnedSource, Start, StartFacts, IMAGE_BIND_ADDR, PINNED_CODE,
};
use lmgw_core::config::DevicesListen;
use lmgw_core::erased::Erased;
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::vram::nvml::NoTelemetry;
use serde_json::{json, Value};

use crate::common::{serve, Gw};
use crate::support::self_admin::Admin;

/// A podman with nothing in it; every call recorded.
#[derive(Default)]
struct EmptyPodman {
    calls: Mutex<Vec<Vec<String>>>,
}

impl CommandRunner for EmptyPodman {
    fn run<'a>(
        &'a self,
        _program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(args.to_vec());
            let stdout = if args.first().map(String::as_str) == Some("ps") {
                "[]".to_string()
            } else {
                String::new()
            };
            Ok(CmdOutput {
                status: 0,
                stdout,
                stderr: String::new(),
            })
        })
    }
}

/// A container start on `data` with `env`.
async fn start(data: &std::path::Path, env: ContainerEnv) -> SharedState {
    AppState::init_started_with_host(
        data.to_path_buf(),
        false,
        Start {
            deployment: Deployment::Container,
            env,
        },
        Arc::new(EmptyPodman::default()),
        Arc::new(NoTelemetry("test: no GPU".into())),
        Arc::new(lmgw_machine::fake::FakeFacts::new()),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_pinned_variable_is_written_and_a_later_start_without_it_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let env = ContainerEnv {
        bind_addr: Some("0.0.0.0:18787".parse().unwrap()),
        webrtc_port: Some(18790),
        ..Default::default()
    };
    let first = start(dir.path(), env).await;
    let s = &first.snapshot().settings;
    assert_eq!(s.bind_addr, "0.0.0.0:18787", "the snapshot reads it pinned");
    assert_eq!(s.realtime.webrtc.webrtc_port, Some(18790));
    let stored = lmgw_core::store::load_settings(&first.db).await.unwrap();
    assert_eq!(stored.bind_addr, "0.0.0.0:18787", "and the store has it");
    assert_eq!(first.deployment(), Deployment::Container);
    first.db.close().await;
    drop(first);

    let again = start(dir.path(), ContainerEnv::default()).await;
    let s = &again.snapshot().settings;
    assert_eq!(s.bind_addr, "0.0.0.0:18787", "not the desktop default");
    assert_eq!(s.realtime.webrtc.webrtc_port, Some(18790));
    // The rows say the variables were not set this time.
    let facts = again.start_facts();
    use PinnedSource::{ImageDefault as Image, Kept};
    assert_eq!(
        facts.sources,
        [Kept, Kept, Image, Image, Kept, Image, Kept, Image]
    );
}

/// Two listeners on one TCP port end the container start naming both rows
/// and the unit's variables (headless server design §2.1), not at the
/// second bind: here the TLS address on the address and port the unit pins
/// for the plain listener.
#[tokio::test]
async fn a_start_whose_listeners_share_a_tcp_port_ends_naming_both() {
    let dir = tempfile::tempdir().unwrap();
    let env = ContainerEnv {
        bind_addr: Some("0.0.0.0:18443".parse().unwrap()),
        tls_cert: Some("/etc/lmgw/tls/tls.crt".into()),
        tls_key: Some("/etc/lmgw/tls/tls.key".into()),
        tls_listen: Some("0.0.0.0:18443".parse().unwrap()),
        ..Default::default()
    };
    let e = AppState::init_started_with_host(
        dir.path().to_path_buf(),
        false,
        Start {
            deployment: Deployment::Container,
            env,
        },
        Arc::new(EmptyPodman::default()),
        Arc::new(NoTelemetry("test: no GPU".into())),
        Arc::new(lmgw_machine::fake::FakeFacts::new()),
    )
    .await
    .err()
    .expect("the start ends");
    let e = format!("{e:#}");
    assert!(
        e.contains("the bind address 0.0.0.0:18443")
            && e.contains("the TLS address 0.0.0.0:18443")
            && e.contains("one TCP port")
            && e.contains("(in the unit: LMGW_BIND_ADDR, LMGW_TLS_LISTEN)"),
        "{e}"
    );
}

#[tokio::test]
async fn a_data_dir_whose_first_start_died_before_seeding_is_seeded_by_the_next() {
    // A database without the marker: what a start that ended after the
    // database was created and before the seeds' write leaves (and what a
    // desktop data dir copied into a container is).
    let dir = tempfile::tempdir().unwrap();
    let first = AppState::init_started_with_host(
        dir.path().to_path_buf(),
        false,
        Start::desktop(),
        Arc::new(EmptyPodman::default()),
        Arc::new(NoTelemetry("test: no GPU".into())),
        Arc::new(lmgw_machine::fake::FakeFacts::new()),
    )
    .await
    .unwrap();
    assert!(dir.path().join("lmgw.sqlite").exists());
    assert!(!first.snapshot().settings.container_seeded);
    first.db.close().await;
    drop(first);

    let seeded = start(dir.path(), ContainerEnv::default()).await;
    let s = seeded.snapshot().settings.clone();
    assert!(s.container_seeded);
    assert!(s.auth_enabled, "seeded although the database was there");
    assert_eq!(s.devices.listen, DevicesListen::All);
    assert_eq!(s.bind_addr, IMAGE_BIND_ADDR);
    use PinnedSource::{ImageDefault as Image, Kept};
    assert_eq!(
        seeded.start_facts().sources,
        [Image, Kept, Image, Image, Image, Image, Kept, Image]
    );
}

#[tokio::test]
async fn a_seeded_variable_applies_to_a_fresh_data_dir_only() {
    let dir = tempfile::tempdir().unwrap();
    let fresh = start(dir.path(), ContainerEnv::default()).await;
    let s = fresh.snapshot().settings.clone();
    assert!(s.auth_enabled, "Require API key on in the image (ruling 9)");
    assert_eq!(s.devices.listen, DevicesListen::All);
    assert_eq!(s.bind_addr, IMAGE_BIND_ADDR);
    // The owner switches it off on the dashboard; the stored value rules
    // from then on, a seed variable or not.
    let mut off = s.clone();
    off.auth_enabled = false;
    off.devices.listen = DevicesListen::Loopback;
    lmgw_core::store::save_settings(&fresh.db, &off)
        .await
        .unwrap();
    fresh.db.close().await;
    drop(fresh);

    let env = ContainerEnv {
        require_api_key: Some(true),
        devices_listen: Some(DevicesListen::All),
        ..Default::default()
    };
    let later = start(dir.path(), env).await;
    let s = &later.snapshot().settings;
    assert!(!s.auth_enabled);
    assert_eq!(s.devices.listen, DevicesListen::Loopback);
}

#[tokio::test]
async fn a_desktop_start_pins_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::init_started_with_host(
        dir.path().to_path_buf(),
        false,
        Start {
            deployment: Deployment::Desktop,
            env: ContainerEnv {
                bind_addr: Some("0.0.0.0:18787".parse().unwrap()),
                require_api_key: Some(true),
                ..Default::default()
            },
        },
        Arc::new(EmptyPodman::default()),
        Arc::new(NoTelemetry("test: no GPU".into())),
        Arc::new(lmgw_machine::fake::FakeFacts::new()),
    )
    .await
    .unwrap();
    let s = &state.snapshot().settings;
    let desktop = lmgw_core::config::Settings::default();
    assert_eq!(s.bind_addr, desktop.bind_addr);
    assert_eq!(s.auth_enabled, desktop.auth_enabled);
    assert_eq!(state.deployment(), Deployment::Desktop);
    assert_eq!(state.stop_budget(), None);
}

/// A test gateway in container mode, told `stop_timeout` by its unit.
async fn container_gateway(stop_timeout: Option<u64>) -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    state.set_start_for_tests(StartFacts {
        deployment: Deployment::Container,
        stop_timeout,
        sources: [PinnedSource::Variable; 8],
    });
    state
}

async fn post(gw: &Gw, path: &str, body: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn every_save_path_refuses_a_pinned_field() {
    let state = container_gateway(Some(95)).await;
    let gw = serve(state.clone()).await;
    let bind = state.snapshot().settings.bind_addr.clone();
    let port = state.snapshot().settings.realtime.webrtc.webrtc_port;

    // The dashboard's full save.
    let (status, body) = post(
        &gw,
        "/api/op/settings_set_full",
        json!({ "bind_addr": "0.0.0.0:9", "retention_days": 3 }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], json!(PINNED_CODE), "{body}");
    let said = body["message"].as_str().unwrap();
    assert!(
        said.starts_with("Settings → Network & access → Gateway → Bind address: ")
            && said.contains("LMGW_BIND_ADDR")
            && said.contains(RESTART_COMMAND)
            && !said.contains("(bind_addr)"),
        "{said}"
    );
    let (status, body) = post(
        &gw,
        "/api/op/settings_set_full",
        json!({ "realtime": { "webrtc_port": 3478 } }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("LMGW_WEBRTC_PORT"),
        "{body}"
    );

    // The patch op, on /api/op, with the same code.
    let (status, body) = post(
        &gw,
        "/api/op/settings_set",
        json!({ "realtime": { "webrtc_port": 3478 } }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], json!(PINNED_CODE), "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("LMGW_WEBRTC_PORT"),
        "{body}"
    );

    // And as lmgw__settings_set.
    let admin = Admin::new(&state).await;
    let (failed, text) = admin
        .call(
            "lmgw__settings_set",
            json!({ "realtime": { "webrtc_port": 3478 } }),
        )
        .await;
    assert!(failed, "{text}");
    assert!(text.contains("LMGW_WEBRTC_PORT"), "{text}");

    let s = &state.snapshot().settings;
    assert_eq!(s.bind_addr, bind, "a refused save changed the bind address");
    assert_eq!(s.realtime.webrtc.webrtc_port, port);
    assert_ne!(s.retention_days, 3, "a refused save is refused whole");

    // A save that carries a pinned field at its value, and anything else,
    // goes through.
    let (status, body) = post(
        &gw,
        "/api/op/settings_set_full",
        json!({ "bind_addr": bind, "retention_days": 3 }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(state.snapshot().settings.retention_days, 3);
}

#[tokio::test]
async fn the_desktop_saves_its_listener_settings_as_before() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let (status, body) = post(
        &gw,
        "/api/op/settings_set_full",
        json!({ "bind_addr": "127.0.0.1:18797" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(
        &gw,
        "/api/op/settings_set",
        json!({ "realtime": { "webrtc_port": 18798 } }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let s = &state.snapshot().settings;
    assert_eq!(s.bind_addr, "127.0.0.1:18797");
    assert_eq!(s.realtime.webrtc.webrtc_port, Some(18798));
}

async fn settings_full(gw: &Gw) -> lmgw_api_types::SettingsFull {
    gw.client()
        .get(format!("{gw}/api/settings-full"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_settings_answer_lists_the_pinned_settings_and_the_stop_budget() {
    let state = container_gateway(Some(95)).await;
    let gw = serve(state.clone()).await;
    let full = settings_full(&gw).await;
    assert_eq!(full.deployment, Deployment::Container);
    let pinned: Vec<(&str, &str)> = full
        .pinned
        .iter()
        .map(|p| (p.setting.as_str(), p.variable.as_str()))
        .collect();
    assert_eq!(
        pinned,
        [
            ("bind_addr", "LMGW_BIND_ADDR"),
            ("realtime.webrtc_port", "LMGW_WEBRTC_PORT"),
            ("tls.cert_path", "LMGW_TLS_CERT"),
            ("tls.key_path", "LMGW_TLS_KEY"),
            ("tls.listen", "LMGW_TLS_LISTEN"),
            ("public_url", "LMGW_PUBLIC_URL"),
            ("realtime.webrtc_tls_port", "LMGW_WEBRTC_TLS_PORT"),
            ("trusted_proxies", "LMGW_TRUSTED_PROXIES"),
        ]
    );
    assert!(full.pinned[0].why.contains(RESTART_COMMAND));
    assert_eq!(full.pinned[0].source, PinnedSource::Variable);
    assert!(!full.dev);
    let budget = full.stop_budget.expect("container mode compares");
    assert_eq!(budget.unit_seconds, Some(95));
    assert_eq!(
        budget.needed_seconds, 95,
        "the defaults fit the shipped unit"
    );
    assert!(!budget.over);

    // A longer unload timeout no longer fits: the save says so, and so
    // does the answer.
    let (status, body) = post(
        &gw,
        "/api/op/settings_set_full",
        json!({ "vram": { "unload_timeout_seconds": 120 } }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("with an unload timeout of 120 s a quit can take 155 s"),
        "{body}"
    );
    let budget = settings_full(&gw).await.stop_budget.unwrap();
    assert!(budget.over);
    assert_eq!(budget.needed_seconds, 155);

    // The desktop: nothing pinned, no unit to compare with.
    let desktop = AppState::init_for_tests().await.unwrap();
    let full = settings_full(&serve(desktop).await).await;
    assert_eq!(full.deployment, Deployment::Desktop);
    assert!(full.pinned.is_empty());
    assert_eq!(full.stop_budget, None);
}

#[tokio::test]
async fn without_a_stop_timeout_the_budget_says_it_cannot_compare() {
    let state = container_gateway(None).await;
    let budget = state.stop_budget().unwrap();
    assert!(!budget.over);
    assert!(
        budget.sentence.contains("LMGW_STOP_TIMEOUT is not set"),
        "{}",
        budget.sentence
    );
}

/// `realtime.webrtc_enabled`'s save sentence (§6.2): in container mode it
/// names the restart command on the host, through both save paths; on the
/// desktop it is what it was.
#[tokio::test]
async fn the_webrtc_switch_says_how_its_next_start_comes() {
    let note = "the WebRTC calls switch applies at the next start of lmgw";
    let state = container_gateway(Some(95)).await;
    let gw = serve(state.clone()).await;
    let (status, body) = post(
        &gw,
        "/api/op/settings_set_full",
        json!({ "realtime": { "webrtc_enabled": false } }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let said = body["message"].as_str().unwrap();
    assert!(
        said.contains(&format!("{note} ({RESTART_COMMAND} on the host)")),
        "{said}"
    );
    let (status, body) = post(
        &gw,
        "/api/op/settings_set",
        json!({ "realtime": { "webrtc_enabled": true } }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.to_string()
            .contains(&format!("{note} ({RESTART_COMMAND} on the host)")),
        "{body}"
    );

    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let (status, body) = post(
        &gw,
        "/api/op/settings_set_full",
        json!({ "realtime": { "webrtc_enabled": false } }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let said = body["message"].as_str().unwrap();
    assert!(said.contains(note), "{said}");
    assert!(!said.contains("on the host"), "{said}");
}
