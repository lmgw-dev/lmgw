//! What the agent ops answer over HTTP: `agent_install`, `agent_delete` and
//! `agent_run` through `POST /api/op/{name}`, and the `409` of
//! `POST /api/agents/{id}/runs`. Each answer reads back through the type the
//! document describes for it and keeps the keys it has always sent: which
//! values are empty text rather than `null`, which flags are `false` rather
//! than absent, and the exact sentence a person is told.
//!
//! The container runtime is a fake `podman` (`support::agent_world`) and a run
//! reads an MCP stub that holds its first call, so nothing reaches past
//! loopback and no run finishes before a test says so.

use lmgw_api_types as dto;
use lmgw_core::config::Protocol;
use serde_json::{json, Value};

use crate::common::{self, serve, Gw};
use crate::support::agent_world::{chat, gateway, held_source, pkg, register_held, Podman};

const IMAGE: &str = "localhost/board:1";

pub(crate) async fn op(gw: &Gw, name: &str, body: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The keys of `live`, to be exactly `want` (in any order).
pub(crate) fn assert_keys(live: &Value, want: &[&str]) {
    let mut got: Vec<&str> = live
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {live}"))
        .keys()
        .map(String::as_str)
        .collect();
    got.sort_unstable();
    let mut want = want.to_vec();
    want.sort_unstable();
    assert_eq!(got, want, "{live}");
}

/// An agent of the `chat` kind, installed through `agent_set`.
pub(crate) async fn keeper(gw: &Gw, id: &str) {
    let (status, live) = op(gw, "agent_set", json!({ "manifest": chat(id, "whatever") })).await;
    assert_eq!(status, 200, "{live}");
}

const INSTALLED_KEYS: [&str; 13] = [
    "ok",
    "id",
    "warnings",
    "requires",
    "replaced",
    "dropped_config",
    "config_unbound",
    "validate_only",
    "image",
    "digest",
    "pulled",
    "manifest_path",
    "message",
];

fn board(version: &str) -> String {
    pkg("board", version, IMAGE, "", "")
}

#[tokio::test]
async fn an_install_answers_the_report_and_where_the_manifest_came_from() {
    let podman = Podman::default().with(IMAGE, Some("sha256:one"), Some(&board("1.0.0")));
    let (_state, gw) = gateway(podman).await;

    let (status, live) = op(&gw, "agent_install", json!({ "image": IMAGE })).await;
    assert_eq!(status, 200, "{live}");
    let installed = common::round_trips::<dto::AgentInstalled>("install", &live);
    assert_keys(&live, &INSTALLED_KEYS);
    assert!(installed.report.ok && !installed.report.replaced && !installed.report.validate_only);
    assert_eq!(installed.report.id, "board");
    assert_eq!(installed.image, IMAGE);
    assert_eq!(installed.digest.as_deref(), Some("sha256:one"));
    // The image was here and the policy is `never`: nothing was downloaded,
    // and the answer says so rather than leaving it to be inferred.
    assert_eq!(live["pulled"], json!(false), "{live}");
    assert_eq!(installed.manifest_path, "/lmgw/agent.json");
    assert_eq!(
        installed.message,
        "installed 'board' from 'localhost/board:1'"
    );

    // Over the same id, asked to replace: the config is kept, and it says so.
    let (status, live) = op(
        &gw,
        "agent_install",
        json!({ "image": IMAGE, "replace": true }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let again = common::round_trips::<dto::AgentInstalled>("install, replacing", &live);
    assert!(again.report.replaced);
    assert_eq!(
        again.message,
        "installed 'board' from 'localhost/board:1', keeping its config"
    );

    // Validating only writes nothing and says what would have been installed.
    let (status, live) = op(
        &gw,
        "agent_install",
        json!({ "image": IMAGE, "replace": true, "validate_only": true }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let checked = common::round_trips::<dto::AgentInstalled>("install, validating", &live);
    assert!(checked.report.validate_only);
    assert_eq!(
        checked.message,
        "'board' is what 'localhost/board:1' would install — nothing written"
    );
}

#[tokio::test]
async fn an_install_says_when_it_downloaded_and_when_podman_had_no_digest() {
    // Not on the box until a pull brings it, and podman cannot report its digest.
    let podman = Podman::default().pulls(IMAGE, None, Some(&board("1.0.0")));
    let (_state, gw) = gateway(podman).await;

    let (status, live) = op(
        &gw,
        "agent_install",
        json!({ "image": format!("  {IMAGE}  "), "pull": "missing" }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let installed = common::round_trips::<dto::AgentInstalled>("install, pulling", &live);
    // No digest is an absent key, not the empty text or `null`.
    let keys: Vec<&str> = INSTALLED_KEYS
        .iter()
        .copied()
        .filter(|k| *k != "digest")
        .collect();
    assert_keys(&live, &keys);
    assert_eq!(live["pulled"], json!(true), "{live}");
    // The reference is the trimmed one.
    assert_eq!(live["image"], json!(IMAGE), "{live}");
    assert!(installed.digest.is_none(), "{live}");
    assert!(
        installed
            .report
            .warnings
            .iter()
            .any(|w| w.contains("podman could not report a digest")),
        "{live}"
    );
}

#[tokio::test]
async fn a_delete_answers_the_threads_it_kept_and_how_to_bring_the_agent_back() {
    let (_state, gw) = gateway(Podman::default()).await;
    // No thread opened through it: nothing to report but the hint for a
    // manifest that was not shipped.
    keeper(&gw, "plain").await;
    let (status, live) = op(&gw, "agent_delete", json!({ "id": "plain" })).await;
    assert_eq!(status, 200, "{live}");
    let deleted = common::round_trips::<dto::AgentDeleted>("delete", &live);
    assert_keys(&live, &["ok", "id", "threads_kept", "message", "hint"]);
    assert!(deleted.ok);
    assert_eq!(deleted.id, "plain");
    assert_eq!(live["threads_kept"], json!(0), "{live}");
    assert_eq!(deleted.message, "deleted agent 'plain'");
    assert_eq!(
        deleted.hint,
        "gone; import the exported file to bring it back"
    );

    // One and two threads opened through it stay, and the message counts them.
    for (id, threads, message) in [
        (
            "one",
            1,
            "deleted agent 'one'; its 1 chat thread was kept, on the Chat page",
        ),
        (
            "two",
            2,
            "deleted agent 'two'; its 2 chat threads were kept, on the Chat page",
        ),
    ] {
        keeper(&gw, id).await;
        for _ in 0..threads {
            let (status, opened) = op(&gw, "agent_open_chat", json!({ "id": id })).await;
            assert_eq!(status, 200, "{opened}");
        }
        let (status, live) = op(&gw, "agent_delete", json!({ "id": id })).await;
        assert_eq!(status, 200, "{live}");
        let deleted = common::round_trips::<dto::AgentDeleted>("delete, with threads", &live);
        assert_eq!(live["threads_kept"], json!(threads), "{live}");
        assert_eq!(deleted.message, message);
    }

    // A shipped agent says how to get it back.
    let shipped = lmgw_core::agents::seed::shipped_all()
        .first()
        .expect("an agent ships")
        .id
        .clone();
    let (status, live) = op(&gw, "agent_delete", json!({ "id": shipped })).await;
    assert_eq!(status, 200, "{live}");
    let deleted = common::round_trips::<dto::AgentDeleted>("delete, shipped", &live);
    assert_eq!(
        deleted.hint,
        "this was a shipped agent; 'Restore shipped agents' (agents_restore) brings it back"
    );
}

#[tokio::test]
async fn a_run_start_answers_the_job_and_reports_one_already_in_flight() {
    let state = crate::agents_runs::gateway("http://127.0.0.1:1", Protocol::Openai).await;
    let (source, release) = held_source().await;
    register_held(&state, &source).await;
    let gw = serve(state.clone()).await;
    crate::agents_runs::install(&gw, &crate::agents_runs::doc("m1")).await;

    let (status, live) = op(
        &gw,
        "agent_run",
        json!({ "id": "labeler", "phase": "list" }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let started = common::round_trips::<dto::AgentRunStarted>("run", &live);
    assert_keys(
        &live,
        &["ok", "id", "job_id", "phase", "already_running", "message"],
    );
    assert!(started.ok && !started.already_running);
    assert!(live["job_id"].is_i64(), "{live}");
    assert_eq!(started.id, "labeler");
    assert_eq!(started.phase, "list");
    assert_eq!(started.message, "started the list run of 'labeler'");

    // The run is reading the source, so it is in flight: another start is told
    // so, and handed that run rather than a new one.
    source.wait_calls(1).await;
    let (status, live) = op(
        &gw,
        "agent_run",
        json!({ "id": "labeler", "phase": "classify" }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let again = common::round_trips::<dto::AgentRunStarted>("run, in flight", &live);
    assert!(again.already_running);
    assert_eq!(again.job_id, started.job_id);
    // The in-flight run's phase, not the one asked for.
    assert_eq!(again.phase, "list");
    assert_eq!(
        again.message,
        "a run of 'labeler' is already in flight; showing that one"
    );

    release.now();
    crate::agents_runs::settle(&state, started.job_id).await;
}

#[tokio::test]
async fn a_second_open_run_is_refused_with_the_run_it_met() {
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let gw = serve(state).await;
    crate::agents_ledger::install(&gw, &crate::agents_ledger::doc("labeler", 0)).await;
    let (_, token) =
        crate::agents_ledger::op(&gw, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = token["token"].as_str().unwrap().to_string();
    let open = || {
        crate::agents_ledger::post_as(
            &gw,
            "/api/agents/labeler/runs",
            Some(&token),
            json!({ "phase": "run" }),
        )
    };

    let (status, opened) = open().await;
    assert_eq!(status, 200, "{opened}");
    let opened = common::round_trips::<dto::AgentRunOpened>("open a run", &opened);

    let (status, refused) = open().await;
    assert_eq!(status, 409, "{refused}");
    assert_keys(&refused, &["code", "message", "run"]);
    // Held to the document too: the 409 is a documented response of the route.
    let conflict = common::round_trips::<dto::AgentRunConflict>("the 409", &refused);
    assert_eq!(conflict.code, "already_running");
    assert_eq!(conflict.run, opened.run);
    assert_eq!(
        conflict.message,
        format!(
            "'labeler' already has a run in flight (#{}); one live run per agent",
            opened.run
        )
    );
}
