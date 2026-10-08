//! A device at `full` and the agents (review G-1, 2026-10-07). An agent's
//! run calls lmgw's admin tools as whoever started it — a device's run as
//! that device, at what its admin tools may do as each call is made — so
//! the access settings stay refused there too, and a level lowered mid-run
//! refuses the run's next write. The run's other tools are the agent's own
//! (its manifest's labels), not narrowed by the device's tool scope. A
//! device replaces and deletes only an agent it created, so it cannot turn
//! the owner's agent into one that runs what it chose, and the owner's
//! write adopts an agent a device created.

use std::sync::Arc;
use std::time::Duration;

use lmgw_core::config::SelfAdmin;
use lmgw_core::state::SharedState;
use lmgw_core::store;
use serde_json::{json, Value};
use tokio::sync::Notify;

use super::admin_guards::{tool_in_turn, toolset_thread};
use super::{op, pair, rows_of};
use crate::realtime_chat_thread::world;

/// The job `job_id` once it ended.
async fn settle(state: &SharedState, job_id: i64) -> store::JobRow {
    for _ in 0..400 {
        let row = store::get_job(&state.db, job_id).await.unwrap().unwrap();
        if !matches!(row.status.as_str(), "queued" | "running") {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {job_id} never finished");
}

/// A list-only batch agent `id` whose source is `tool` with `args`.
fn source_agent(id: &str, tool: &str, args: Value) -> Value {
    json!({
        "schema_version": 1,
        "id": id,
        "name": id,
        "model": { "alias": "chatty" },
        "tools": [ { "label": "lmgw", "allowed": [tool] } ],
        "run": { "kind": "batch",
            "source": { "tool": tool, "args": args },
            "items_path": "/changed",
            "item": { "id": "{{item}}", "columns": {} }
        }
    })
}

/// The device writes an agent whose `list` step calls
/// `lmgw__settings_set {auth_enabled: false}` and runs it: the call is
/// refused, as every tool call of an access setting is, auth stays on, and
/// the run's call is the device's own row.
#[tokio::test]
async fn a_device_cannot_launder_an_access_setting_through_an_agent() {
    let w = world(|s| {
        s.self_admin = SelfAdmin::Full;
        s.auth_enabled = true;
    })
    .await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "full" })).await;
    let tid = toolset_thread(&w, &desk.client).await;
    let said = tool_in_turn(
        &w,
        &desk.client,
        tid,
        "lmgw__agent_set",
        r#"{"manifest": {"schema_version": 1, "id": "opener", "name": "Opener",
            "model": {"alias": "chatty"},
            "tools": [{"label": "lmgw", "allowed": ["lmgw__settings_set"]}],
            "run": {"kind": "batch",
              "source": {"tool": "lmgw__settings_set", "args": {"auth_enabled": false}},
              "items_path": "/changed", "item": {"id": "{{item}}", "columns": {}}}},
            "replace": false}"#,
    )
    .await;
    assert!(said.contains("\"id\": \"opener\""), "installed: {said}");
    let row = store::get_agent(&w.state.db, "opener")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.created_by_key,
        Some(desk.id),
        "recorded as the device's"
    );

    let said = tool_in_turn(
        &w,
        &desk.client,
        tid,
        "lmgw__agent_run",
        r#"{"id": "opener", "phase": "list"}"#,
    )
    .await;
    let started: Value = serde_json::from_str(&said).unwrap_or_else(|_| panic!("{said}"));
    let job = settle(&w.state, started["job_id"].as_i64().unwrap()).await;
    let told = format!("{:?} {:?}", job.error, job.result);
    assert!(
        told.contains("auth_enabled cannot be changed through lmgw's admin tools"),
        "{told}"
    );
    assert!(w.state.snapshot().settings.auth_enabled, "auth stays on");
    let rows = rows_of(&w, desk.id).await;
    assert!(
        rows.iter()
            .any(|r| r.0 == "agent-tool" && r.1 == "lmgw__settings_set"),
        "the run's call is the device's: {rows:?}"
    );
}

/// The run-caller cap through the router, where it changes the outcome:
/// a device at `full` starts the owner's agent, whose list step waits on a
/// server and then engages the GPU hold for each item. While it waits, the
/// device is lowered to `read_only` — by the owner's key_set, and by the
/// stored row alone, before any snapshot reload (the branch review's
/// verification V-7: the run reads the stored level, as the device's turns
/// do). The run's write is refused and the hold stays off. The same agent
/// run by the owner engages it, so the device as the run's caller is what
/// refused it.
#[tokio::test]
async fn a_device_s_run_is_capped_when_its_level_drops_mid_run() {
    for lowered_by in ["key_set", "the stored row alone"] {
        run_lowered_mid_run(lowered_by).await;
    }
}

async fn run_lowered_mid_run(lowered_by: &str) {
    use crate::support::mcp_stub::{answer, answering, register};
    let w = world(|s| s.self_admin = SelfAdmin::Full).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "full" })).await;
    let tid = toolset_thread(&w, &desk.client).await;
    let gate = Arc::new(Notify::new());
    let held = gate.clone();
    let feed = answering(
        json!([{ "name": "items", "description": "the items",
                 "inputSchema": { "type": "object", "properties": {} } }]),
        false,
        answer(move |_name, _args| {
            let held = held.clone();
            async move {
                held.notified().await;
                json!({ "content": [], "structuredContent": { "items": ["a"] }, "isError": false })
            }
        }),
    )
    .await;
    register(&w.state, "feed", "feed", &feed.url, true, None).await;
    let manifest = json!({
        "schema_version": 1, "id": "gated", "name": "Gated",
        "model": { "alias": "chatty" },
        "tools": [ { "label": "feed" }, { "label": "lmgw", "allowed": ["lmgw__hold_set"] } ],
        "run": { "kind": "batch",
            "source": { "tool": "feed__items", "args": {} },
            "items_path": "/items",
            "item": { "id": "{{item}}",
                "fetch": { "tool": "lmgw__hold_set", "args": { "active": true } },
                "columns": {} }
        }
    });
    let (s, v) = op(&w, "agent_set", json!({ "manifest": manifest.to_string() })).await;
    assert_eq!(s, 200, "{v}");

    let said = tool_in_turn(
        &w,
        &desk.client,
        tid,
        "lmgw__agent_run",
        r#"{"id": "gated", "phase": "list"}"#,
    )
    .await;
    let started: Value = serde_json::from_str(&said).unwrap_or_else(|_| panic!("{said}"));
    feed.wait_calls(1).await;
    if lowered_by == "key_set" {
        let (s, v) = op(
            &w,
            "key_set",
            json!({ "id": desk.id, "self_admin": "read_only" }),
        )
        .await;
        assert_eq!(s, 200, "{v}");
    } else {
        // The commit a key write makes, without its snapshot reload.
        sqlx::query("UPDATE api_keys SET self_admin = 1 WHERE id = ?1")
            .bind(desk.id)
            .execute(&w.state.db)
            .await
            .unwrap();
    }
    gate.notify_one();
    let job = settle(&w.state, started["job_id"].as_i64().unwrap()).await;
    let told = format!("{:?} {:?}", job.error, job.result);
    assert!(told.contains("read only"), "{lowered_by}: {told}");
    assert!(
        !w.state.snapshot().settings.hold.active,
        "{lowered_by}: the hold stays off"
    );

    let (s, v) = op(&w, "agent_run", json!({ "id": "gated", "phase": "list" })).await;
    assert_eq!(s, 200, "{v}");
    feed.wait_calls(2).await;
    gate.notify_one();
    let job = settle(&w.state, v["job_id"].as_i64().unwrap()).await;
    assert!(
        w.state.snapshot().settings.hold.active,
        "{lowered_by}: the owner's run engages it: {:?} {:?}",
        job.error,
        job.result
    );
}

/// A run a device starts calls the admin tools at what the device's may
/// do: the owner's agent whose `list` step engages the GPU hold, started for
/// a device at `read_only`, is refused that write; the hold stays off.
#[tokio::test]
async fn a_device_s_run_is_capped_at_its_level() {
    let w = world(|s| s.self_admin = SelfAdmin::Full).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let (s, v) = op(
        &w,
        "agent_set",
        json!({ "manifest": source_agent("holder", "lmgw__hold_set", json!({"active": true}))
                    .to_string() }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let started = lmgw_core::ops::agent_run(&w.state, "holder", "list", Some(desk.id))
        .await
        .unwrap();
    let job = settle(&w.state, started["job_id"].as_i64().unwrap()).await;
    let told = format!("{:?} {:?}", job.error, job.result);
    assert!(told.contains("read only"), "{told}");
    assert!(
        !w.state.snapshot().settings.hold.active,
        "the hold stays off"
    );
}

/// The owner's agent is not a device's to replace, `replace` or not; its
/// own agent is.
#[tokio::test]
async fn a_device_replaces_only_an_agent_it_created() {
    let w = world(|s| s.self_admin = SelfAdmin::Full).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "full" })).await;
    let tid = toolset_thread(&w, &desk.client).await;
    let (s, v) = op(
        &w,
        "agent_set",
        json!({ "manifest": source_agent("owned", "lmgw__models", json!({})).to_string() }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let said = tool_in_turn(
        &w,
        &desk.client,
        tid,
        "lmgw__agent_set",
        r#"{"manifest": {"schema_version": 1, "id": "owned", "name": "Taken over",
            "model": {"alias": "chatty"},
            "tools": [{"label": "lmgw", "allowed": ["lmgw__models"]}],
            "run": {"kind": "batch", "source": {"tool": "lmgw__models", "args": {}},
              "items_path": "/models", "item": {"id": "{{item.name}}", "columns": {}}}},
            "replace": true}"#,
    )
    .await;
    assert!(
        said.contains("this device did not create it, so it may not replace it"),
        "{said}"
    );
    let row = store::get_agent(&w.state.db, "owned")
        .await
        .unwrap()
        .unwrap();
    assert!(
        row.manifest.contains("\"name\": \"owned\""),
        "{}",
        row.manifest
    );
    assert_eq!(row.created_by_key, None);

    for name in ["Mine", "Mine again"] {
        let args: &'static str = if name == "Mine" {
            r#"{"manifest": {"schema_version": 1, "id": "mine", "name": "Mine",
                "model": {"alias": "chatty"},
                "tools": [{"label": "lmgw", "allowed": ["lmgw__models"]}],
                "run": {"kind": "batch", "source": {"tool": "lmgw__models", "args": {}},
                  "items_path": "/models", "item": {"id": "{{item.name}}", "columns": {}}}}}"#
        } else {
            r#"{"manifest": {"schema_version": 1, "id": "mine", "name": "Mine again",
                "model": {"alias": "chatty"},
                "tools": [{"label": "lmgw", "allowed": ["lmgw__models"]}],
                "run": {"kind": "batch", "source": {"tool": "lmgw__models", "args": {}},
                  "items_path": "/models", "item": {"id": "{{item.name}}", "columns": {}}}},
                "replace": true}"#
        };
        let said = tool_in_turn(&w, &desk.client, tid, "lmgw__agent_set", args).await;
        assert!(said.contains("\"id\": \"mine\""), "{name}: {said}");
    }
    let row = store::get_agent(&w.state.db, "mine")
        .await
        .unwrap()
        .unwrap();
    assert!(row.manifest.contains("Mine again"), "{}", row.manifest);
    assert_eq!(row.created_by_key, Some(desk.id));
}

/// `lmgw__agent_set` arguments for a list-only agent `mine` named `name`.
const MINE: &str = r#"{"manifest": {"schema_version": 1, "id": "mine", "name": "Mine",
    "model": {"alias": "chatty"},
    "tools": [{"label": "lmgw", "allowed": ["lmgw__models"]}],
    "run": {"kind": "batch", "source": {"tool": "lmgw__models", "args": {}},
      "items_path": "/models", "item": {"id": "{{item.name}}", "columns": {}}}},
    "replace": true}"#;

/// [`MINE`] under the name "Changed".
const MINE_CHANGED: &str = r#"{"manifest": {"schema_version": 1, "id": "mine", "name": "Changed",
    "model": {"alias": "chatty"},
    "tools": [{"label": "lmgw", "allowed": ["lmgw__models"]}],
    "run": {"kind": "batch", "source": {"tool": "lmgw__models", "args": {}},
      "items_path": "/models", "item": {"id": "{{item.name}}", "columns": {}}}},
    "replace": true}"#;

/// A device deletes only an agent it created (the branch review's
/// verification V-5): deleting the owner's agent, then creating one under
/// its id, would replace it with what the device chose.
#[tokio::test]
async fn a_device_deletes_only_an_agent_it_created() {
    let w = world(|s| s.self_admin = SelfAdmin::Full).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "full" })).await;
    let tid = toolset_thread(&w, &desk.client).await;
    let (s, v) = op(
        &w,
        "agent_set",
        json!({ "manifest": source_agent("owned", "lmgw__models", json!({})).to_string() }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let before = store::get_agent(&w.state.db, "owned").await.unwrap();

    let said = tool_in_turn(
        &w,
        &desk.client,
        tid,
        "lmgw__agent_delete",
        r#"{"id": "owned"}"#,
    )
    .await;
    assert!(
        said.contains("was not created by this device, so it may not delete it"),
        "{said}"
    );
    assert_eq!(
        store::get_agent(&w.state.db, "owned").await.unwrap(),
        before,
        "nothing changed"
    );

    let said = tool_in_turn(&w, &desk.client, tid, "lmgw__agent_set", MINE).await;
    assert!(said.contains("\"id\": \"mine\""), "{said}");
    let said = tool_in_turn(
        &w,
        &desk.client,
        tid,
        "lmgw__agent_delete",
        r#"{"id": "mine"}"#,
    )
    .await;
    assert!(said.contains("deleted agent 'mine'"), "{said}");
    assert!(store::get_agent(&w.state.db, "mine")
        .await
        .unwrap()
        .is_none());
}

/// The owner's write to an agent a device created adopts it (V-5): from
/// then on the device may neither replace nor delete it. A dashboard op
/// adopts it, and so does the owner's own tool call.
#[tokio::test]
async fn an_owner_s_write_adopts_a_device_s_agent() {
    let w = world(|s| s.self_admin = SelfAdmin::Full).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "full" })).await;
    let tid = toolset_thread(&w, &desk.client).await;
    let owners = toolset_thread(&w, &w.gw.client()).await;
    let mine = || async { store::get_agent(&w.state.db, "mine").await.unwrap() };

    for adopt in ["the dashboard", "the owner's tool call"] {
        let said = tool_in_turn(&w, &desk.client, tid, "lmgw__agent_set", MINE).await;
        assert!(said.contains("\"id\": \"mine\""), "{adopt}: {said}");
        assert_eq!(
            mine().await.unwrap().created_by_key,
            Some(desk.id),
            "{adopt}"
        );
        match adopt {
            "the dashboard" => {
                let (s, v) = op(
                    &w,
                    "agent_enable",
                    json!({ "id": "mine", "enabled": false }),
                )
                .await;
                assert_eq!(s, 200, "{v}");
            }
            _ => {
                let said = tool_in_turn(&w, &w.gw.client(), owners, "lmgw__agent_set", MINE).await;
                assert!(said.contains("\"id\": \"mine\""), "{adopt}: {said}");
            }
        }
        let adopted = mine().await.unwrap();
        assert_eq!(adopted.created_by_key, None, "{adopt}: the owner's now");

        let said = tool_in_turn(&w, &desk.client, tid, "lmgw__agent_set", MINE_CHANGED).await;
        assert!(
            said.contains("this device did not create it, so it may not replace it"),
            "{adopt}: {said}"
        );
        let said = tool_in_turn(
            &w,
            &desk.client,
            tid,
            "lmgw__agent_delete",
            r#"{"id": "mine"}"#,
        )
        .await;
        assert!(
            said.contains("was not created by this device, so it may not delete it"),
            "{adopt}: {said}"
        );
        assert_eq!(mine().await, Some(adopted), "{adopt}: nothing changed");
        // The owner clears it for the next round.
        let (s, v) = op(&w, "agent_delete", json!({ "id": "mine" })).await;
        assert_eq!(s, 200, "{v}");
    }
}
