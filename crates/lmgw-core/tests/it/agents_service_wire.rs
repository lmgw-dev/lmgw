//! What the ops around an agent's app container answer over HTTP, through
//! `POST /api/op/{name}`: `agent_service_start`, `agent_service_stop`,
//! `agent_service_log`, `agent_dev_url_set`, and the ones that stop a running
//! app as a side effect (`agent_enable`, `agent_config_set` over a re-pointed
//! folder, `agent_token_rotate`). Each answer reads back through the type the
//! document describes for it and keeps the keys it has always sent, among them
//! the ones that are absent on one path and `null` on another.
//!
//! The container is a fake `podman` that starts a loopback server on the port
//! its argv publishes (`support::agent_world`), so nothing reaches past
//! loopback.

use lmgw_api_types as dto;
use lmgw_core::state::SharedState;
use serde_json::{json, Value};

use crate::agents_wire::{assert_keys, op};
use crate::common::{self, Gw};
use crate::support::agent_world::{chat, gateway, Podman};

const IMAGE: &str = "localhost/board:1";

/// A `container` agent that serves an app on port 8080, with a folder slot
/// `notes` and a secret.
fn board() -> String {
    json!({
        "schema_version": 1, "id": "board", "name": "Board",
        "model": { "alias": "m1" },
        "config": { "schema": { "type": "object", "properties": {
            "notes": { "type": "string", "format": "directory", "access": "rw" },
            "secret_token": { "type": "string", "format": "secret" }
        } } },
        "run": {
            "kind": "container", "image": IMAGE,
            "limits": { "memory_mb": 256, "cpus": 1.0, "pids": 64, "stop_grace_seconds": 1 },
            "service": { "port": 8080, "idle_seconds": 0, "start_timeout_seconds": 10 }
        }
    })
    .to_string()
}

fn podman() -> Podman {
    Podman::default()
        .serving()
        .with(IMAGE, Some("sha256:board"), None)
}

async fn installed(podman: Podman) -> (SharedState, Gw) {
    let (state, gw) = gateway(podman).await;
    let (status, live) = op(&gw, "agent_set", json!({ "manifest": board() })).await;
    assert_eq!(status, 200, "{live}");
    (state, gw)
}

async fn start(gw: &Gw) -> dto::AgentServiceStarted {
    let (status, live) = op(gw, "agent_service_start", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{live}");
    common::round_trips::<dto::AgentServiceStarted>("service_start", &live)
}

#[tokio::test]
async fn a_start_answers_the_container_the_port_and_where_the_app_answers() {
    let (_state, gw) = installed(podman()).await;

    let (status, live) = op(&gw, "agent_service_start", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{live}");
    let up = common::round_trips::<dto::AgentServiceStarted>("service_start", &live);
    assert_keys(
        &live,
        &[
            "ok",
            "id",
            "running",
            "container",
            "host_port",
            "origin",
            "message",
        ],
    );
    assert!(up.ok && up.running);
    assert!(up.container.contains("agentsvc-board"), "{live}");
    assert!(up.host_port > 0);
    let port = gw.base.rsplit(':').next().unwrap();
    assert_eq!(up.origin, format!("http://board.localhost:{port}/"));
    assert_eq!(
        up.message,
        format!(
            "'board' is serving on 127.0.0.1:{} as {}",
            up.host_port, up.container
        )
    );

    // Starting what is up is the same container.
    assert_eq!(start(&gw).await.container, up.container);
}

#[tokio::test]
async fn a_stop_answers_what_it_stopped_and_stopping_nothing_is_success() {
    let (_state, gw) = installed(podman()).await;
    let up = start(&gw).await;

    let (status, live) = op(&gw, "agent_service_stop", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{live}");
    let down = common::round_trips::<dto::AgentServiceStopped>("service_stop", &live);
    assert_keys(
        &live,
        &[
            "ok",
            "id",
            "running",
            "stopped",
            "cancelled_start",
            "message",
        ],
    );
    assert!(down.ok && !down.running && !down.cancelled_start);
    assert_eq!(down.stopped.as_deref(), Some(up.container.as_str()));
    assert_eq!(down.message, format!("stopped {}", up.container));

    // Nothing running: `stopped` is absent.
    let (status, live) = op(&gw, "agent_service_stop", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{live}");
    let none = common::round_trips::<dto::AgentServiceStopped>("service_stop, nothing", &live);
    assert!(live.get("stopped").is_none(), "{live}");
    assert_eq!(none.stopped, None);
    assert_eq!(none.message, "'board' had no app container running");
}

#[tokio::test]
async fn a_stop_cancels_a_start_that_is_still_in_flight() {
    let (_state, gw) = installed(podman().holding_start()).await;
    let starter = {
        let gw = gw.clone();
        tokio::spawn(async move { op(&gw, "agent_service_start", json!({ "id": "board" })).await })
    };
    common::patience::until_async("board's start is in flight", || async {
        let resp = gw
            .client()
            .get(format!("{gw}/api/agents/board"))
            .send()
            .await;
        let detail: Value = resp.unwrap().json().await.unwrap_or(Value::Null);
        detail["service"]["starting"] == json!(true)
    })
    .await;

    let (status, live) = op(&gw, "agent_service_stop", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{live}");
    let down = common::round_trips::<dto::AgentServiceStopped>("service_stop, cancelling", &live);
    assert!(down.cancelled_start);
    assert_eq!(
        down.message,
        format!(
            "a start was cancelled and {} collected",
            down.stopped.as_deref().unwrap()
        )
    );
    // The start that was cancelled is refused, not answered.
    let (status, refused) = starter.await.unwrap();
    assert_eq!(status, 400, "{refused}");
}

#[tokio::test]
async fn a_log_answers_the_tail_and_an_idle_app_has_no_container_key() {
    let (_state, gw) = installed(podman()).await;

    // Nothing running is an answer, not an error: no `container` key at all.
    let (status, live) = op(&gw, "agent_service_log", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{live}");
    let idle = common::round_trips::<dto::AgentServiceLog>("service_log, idle", &live);
    assert_keys(&live, &["ok", "id", "lines", "running", "log", "message"]);
    assert!(!idle.running && idle.container.is_none());
    assert_eq!(idle.log, "");
    assert_eq!(idle.lines, 0, "nothing was read");
    assert_eq!(
        idle.message,
        "'board' has no app container running, so there is no log to read"
    );

    let up = start(&gw).await;
    let (status, live) = op(
        &gw,
        "agent_service_log",
        json!({ "id": "board", "lines": 5 }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let tail = common::round_trips::<dto::AgentServiceLog>("service_log, running", &live);
    assert_keys(
        &live,
        &[
            "ok",
            "id",
            "lines",
            "running",
            "container",
            "log",
            "message",
        ],
    );
    // `lines` counts what came back: the fake's `tail=5` header and five lines.
    assert!(tail.running && tail.lines == 6);
    assert_eq!(tail.container.as_deref(), Some(up.container.as_str()));
    assert!(tail.log.starts_with("tail=5\nline 0\n"), "{live}");
    assert_eq!(
        tail.message,
        format!("the last 6 lines of '{}'", up.container)
    );

    // `0` is the whole log.
    let (_, live) = op(
        &gw,
        "agent_service_log",
        json!({ "id": "board", "lines": 0 }),
    )
    .await;
    let whole = common::round_trips::<dto::AgentServiceLog>("service_log, whole", &live);
    // The fake's whole log: the `tail=-1` header and 40 lines.
    assert_eq!(whole.lines, 41);
    assert_eq!(
        whole.message,
        format!("the whole log of '{}' (41 lines)", up.container)
    );
}

#[tokio::test]
async fn a_dev_url_answers_what_it_stopped_when_set_and_nothing_when_cleared() {
    let (_state, gw) = installed(podman()).await;
    let up = start(&gw).await;

    // Setting one stops the app that was started from the image.
    let (status, live) = op(
        &gw,
        "agent_dev_url_set",
        json!({ "id": "board", "url": "http://127.0.0.1:5173" }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let set = common::round_trips::<dto::AgentDevUrlSet>("dev_url_set", &live);
    assert_keys(&live, &["ok", "id", "dev_url", "stopped", "message"]);
    assert_eq!(set.dev_url, "http://127.0.0.1:5173");
    assert_eq!(set.stopped.as_deref(), Some(up.container.as_str()));
    assert_eq!(
        set.message,
        format!(
            "'board' is served from http://127.0.0.1:5173; its app container was started from \
             the image, so it was stopped (stopped {})",
            up.container
        )
    );

    // Nothing running now: `stopped` is absent.
    let (_, live) = op(
        &gw,
        "agent_dev_url_set",
        json!({ "id": "board", "url": "http://127.0.0.1:5174" }),
    )
    .await;
    let again = common::round_trips::<dto::AgentDevUrlSet>("dev_url_set, idle", &live);
    assert_eq!(again.stopped, None);
    assert!(live.get("stopped").is_none(), "{live}");
    assert_eq!(
        again.message,
        "'board' is served from http://127.0.0.1:5174; no container is started for its app"
    );

    // Cleared: the key is not there either.
    let (status, live) = op(&gw, "agent_dev_url_set", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{live}");
    let cleared = common::round_trips::<dto::AgentDevUrlSet>("dev_url_set, cleared", &live);
    assert_keys(&live, &["ok", "id", "dev_url", "message"]);
    assert_eq!(
        (cleared.dev_url.as_str(), cleared.stopped.as_deref()),
        ("", None)
    );
    let port = gw.base.rsplit(':').next().unwrap();
    assert_eq!(
        cleared.message,
        format!(
            "'board' is served from its image again; the next request to \
             http://board.localhost:{port}/ starts the container"
        )
    );
}

#[tokio::test]
async fn what_stops_a_running_app_says_which_container_it_stopped() {
    let (_state, gw) = installed(podman()).await;
    let tmp = tempfile::tempdir().unwrap();
    let notes = std::fs::canonicalize(tmp.path())
        .unwrap()
        .display()
        .to_string();
    let other = tempfile::tempdir().unwrap();
    let other = std::fs::canonicalize(other.path())
        .unwrap()
        .display()
        .to_string();

    // Binding a folder with nothing running stops nothing.
    let (status, live) = op(
        &gw,
        "agent_config_set",
        json!({ "id": "board", "values": { "notes": notes } }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let bound = common::round_trips::<dto::AgentConfigSaved>("config_set, binding", &live);
    assert_eq!(bound.mounts_repointed, ["notes"]);
    assert_eq!(bound.service_stopped, None);

    // Re-pointing it while the app holds the old one stops the app.
    let up = start(&gw).await;
    let (status, live) = op(
        &gw,
        "agent_config_set",
        json!({ "id": "board", "values": { "notes": other } }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let moved = common::round_trips::<dto::AgentConfigSaved>("config_set, re-pointing", &live);
    assert_eq!(moved.mounts_repointed, ["notes"]);
    assert_eq!(
        moved.service_stopped.as_deref(),
        Some(up.container.as_str())
    );
    assert!(
        moved.message.ends_with(&format!(
            "; the app container was holding the mount for notes and was stopped (stopped {})",
            up.container
        )),
        "{live}"
    );

    // Disabling stops it in words; enabling stops nothing.
    let up = start(&gw).await;
    let (_, live) = op(
        &gw,
        "agent_enable",
        json!({ "id": "board", "enabled": false }),
    )
    .await;
    let off = common::round_trips::<dto::AgentEnabled>("enable, off", &live);
    // The container's name; the sentence is in the message.
    assert_eq!(off.service_stopped.as_deref(), Some(up.container.as_str()));
    assert_eq!(
        off.message,
        format!("agent disabled; stopped {}", up.container)
    );
    let (_, live) = op(
        &gw,
        "agent_enable",
        json!({ "id": "board", "enabled": true }),
    )
    .await;
    let on = common::round_trips::<dto::AgentEnabled>("enable, on", &live);
    assert_eq!(on.service_stopped, None);

    // Rotating the token stops it, and the message says the app was holding it.
    let up = start(&gw).await;
    let (_, live) = op(&gw, "agent_token_rotate", json!({ "id": "board" })).await;
    let rotated = common::round_trips::<dto::AgentTokenIssued>("token_rotate", &live);
    assert_eq!(
        rotated.service_stopped.as_deref(),
        Some(up.container.as_str())
    );
    assert_eq!(
        rotated.message,
        format!(
            "rotated the token for 'board' — the previous one stopped working now, and its app \
             was holding it: stopped {}",
            up.container
        )
    );
}

#[tokio::test]
async fn an_agent_with_no_app_refuses_every_app_op() {
    let (_state, gw) = gateway(Podman::default()).await;
    let (status, live) = op(&gw, "agent_set", json!({ "manifest": chat("plain", "m1") })).await;
    assert_eq!(status, 200, "{live}");
    for name in [
        "agent_service_start",
        "agent_service_stop",
        "agent_service_log",
    ] {
        let (status, refused) = op(&gw, name, json!({ "id": "plain" })).await;
        assert_eq!(status, 400, "{name}: {refused}");
        assert!(
            refused["message"]
                .as_str()
                .unwrap()
                .contains("declares no run.service"),
            "{name}: {refused}"
        );
    }
}
