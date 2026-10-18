//! What the routing, MCP server, key, settings and hold saves answer over HTTP
//! (`POST /api/op/<name>`), for every action: each answer reads back through
//! the type the document describes for that op, and keeps the keys its action
//! has always sent: which are there, which are absent, which are `null`, which
//! are whole numbers and which are lists.

use lmgw_api_types as dto;
use lmgw_core::config::{hash_api_key, DeviceAdmin, KeyPolicy};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

use crate::common::{self, serve, Gw};
use crate::support::mcp_stub;

async fn gateway() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

/// `POST /api/op/{name}`, which has to answer 200.
async fn op(gw: &Gw, name: &str, args: Value) -> Value {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "{name} {args}: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{name} is not JSON ({e}): {body}"))
}

/// The keys of `live`, to be exactly `want` (in any order).
fn assert_keys(live: &Value, want: &[&str]) {
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

/// The schema the document gives `op`'s answer (inline, titled with the type).
fn answer_schema(op: &str) -> Value {
    let doc = lmgw_core::openapi::admin_doc();
    doc["paths"][format!("/api/op/{op}")]["post"]["responses"]["200"]["content"]["application/json"]
        ["schema"]
        .clone()
}

#[tokio::test]
async fn the_document_names_the_type_each_save_answers_with() {
    // What the op sends is required, what an action may leave out is not.
    let required = |schema: &Value| -> Vec<String> {
        let mut r: Vec<String> = schema["required"]
            .as_array()
            .unwrap_or_else(|| panic!("no required list in {schema}"))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        r.sort();
        r
    };
    let row = ["id", "message", "ok"];
    for (op, ty, want) in [
        ("upstream_set", "RowSaved", &row[..]),
        ("model_set", "RowSaved", &row[..]),
        ("mcp_server_set", "McpServerSaved", &row[..]),
        ("audio_model_set", "AudioModelSaved", &row[..]),
        (
            "key_set",
            "KeySaved",
            &["changed", "id", "message", "name", "ok"][..],
        ),
        (
            "settings_set",
            "SettingsSaved",
            &["changed", "message", "ok"][..],
        ),
        (
            "hold_set",
            "HoldSet",
            &[
                "active",
                "draining",
                "failed",
                "fallback_alias",
                "kept_on_cpu",
                "message",
                "ok",
                "stopped",
            ][..],
        ),
    ] {
        let schema = answer_schema(op);
        assert_eq!(schema["title"], ty, "{op}: {schema}");
        assert_eq!(required(&schema), want, "{op}");
    }
    // `tool_count` and `benchmark_aborted` are left out by some actions, so
    // the document must not promise them; neither is ever `null`.
    let tool_count = &answer_schema("mcp_server_set")["properties"]["tool_count"];
    assert_eq!(tool_count["type"], "integer", "{tool_count}");
    let aborted = &answer_schema("hold_set")["properties"]["benchmark_aborted"];
    assert!(!aborted.to_string().contains("null"), "{aborted}");
}

#[tokio::test]
async fn an_upstream_save_and_an_alias_save_answer_the_row_they_wrote() {
    let (_state, gw) = gateway().await;
    let keys = ["ok", "id", "message"];

    // Upstreams: every action answers the same three keys, the id a whole
    // number.
    let what = "POST /api/op/upstream_set";
    let live = op(
        &gw,
        "upstream_set",
        json!({"action": "create", "name": "o1", "base_url": "https://api.example.test/v1",
               "protocol": "openai", "api_key": "sk-test"}),
    )
    .await;
    let created = common::round_trips::<dto::RowSaved>(what, &live);
    assert_keys(&live, &keys);
    assert!(live["id"].is_i64(), "{live}");
    assert_eq!(created.message, "upstream 'o1' created");
    let up = created.id;
    for (action, extra) in [
        ("update", json!({"name": "o1b", "timeout_ms": 5000})),
        ("disable", json!({})),
        ("enable", json!({})),
    ] {
        let mut args = json!({"action": action, "id": up});
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        let live = op(&gw, "upstream_set", args).await;
        let saved = common::round_trips::<dto::RowSaved>(what, &live);
        assert_keys(&live, &keys);
        assert_eq!((saved.ok, saved.id), (true, up), "{action}: {live}");
        let verb = match action {
            "disable" => "disabled",
            "enable" => "enabled",
            _ => "updated",
        };
        assert_eq!(saved.message, format!("upstream 'o1b' {verb}"), "{action}");
    }

    // Aliases.
    let what = "POST /api/op/model_set";
    let live = op(
        &gw,
        "model_set",
        json!({"action": "create", "alias": "gpt", "upstream": "o1b", "upstream_model": "m1"}),
    )
    .await;
    let created = common::round_trips::<dto::RowSaved>(what, &live);
    assert_keys(&live, &keys);
    assert!(live["id"].is_i64(), "{live}");
    assert_eq!(created.message, "alias 'gpt' created");
    let alias = created.id;
    for (action, extra) in [
        ("update", json!({"alias": "gpt2", "upstream_model": "m2"})),
        ("disable", json!({})),
        ("enable", json!({})),
    ] {
        let mut args = json!({"action": action, "id": alias});
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        let live = op(&gw, "model_set", args).await;
        let saved = common::round_trips::<dto::RowSaved>(what, &live);
        assert_keys(&live, &keys);
        assert_eq!((saved.ok, saved.id), (true, alias), "{action}: {live}");
        let verb = match action {
            "disable" => "disabled",
            "enable" => "enabled",
            _ => "updated",
        };
        assert_eq!(saved.message, format!("alias 'gpt2' {verb}"), "{action}");
    }
    let live = op(&gw, "model_set", json!({"action": "delete", "id": alias})).await;
    let deleted = common::round_trips::<dto::RowSaved>(what, &live);
    assert_keys(&live, &keys);
    assert_eq!(deleted.message, "alias 'gpt2' deleted");

    let live = op(&gw, "upstream_set", json!({"action": "delete", "id": up})).await;
    let deleted = common::round_trips::<dto::RowSaved>("POST /api/op/upstream_set", &live);
    assert_keys(&live, &keys);
    assert_eq!(
        (deleted.id, deleted.message.as_str()),
        (up, "upstream 'o1b' deleted")
    );
}

/// Only a test says how many tools the server lists; the rest answer the id
/// and a sentence, a paired device's hosted-tools row included.
#[tokio::test]
async fn an_mcp_server_test_counts_its_tools_and_no_other_action_does() {
    let (state, gw) = gateway().await;
    let what = "POST /api/op/mcp_server_set";
    let plain = ["ok", "id", "message"];
    let stub = mcp_stub::stub(
        json!([
            {"name": "echo", "description": "echo", "inputSchema": {"type": "object", "properties": {}}},
            {"name": "two", "inputSchema": {"type": "object", "properties": {}}},
        ]),
        false,
    )
    .await;

    let live = op(
        &gw,
        "mcp_server_set",
        json!({"action": "create", "name": "srv", "transport": "http", "url": stub.url,
               "autostart": false, "tool_prefix": "srv"}),
    )
    .await;
    let created = common::round_trips::<dto::McpServerSaved>(what, &live);
    assert_keys(&live, &plain);
    assert_eq!(created.tool_count, None);
    assert_eq!(created.message, "MCP server 'srv' created");
    let id = created.id;

    let live = op(&gw, "mcp_server_set", json!({"action": "test", "id": id})).await;
    let tested = common::round_trips::<dto::McpServerSaved>(what, &live);
    assert_keys(&live, &["ok", "id", "tool_count", "message"]);
    assert_eq!(tested.tool_count, Some(2));
    assert!(live["tool_count"].is_u64(), "a whole number: {live}");
    assert_eq!(tested.message, "connected to 'srv' — 2 tools");

    for (action, extra) in [
        ("update", json!({"name": "srv2", "timeout_ms": 7000})),
        ("disable", json!({})),
        ("enable", json!({})),
    ] {
        let mut args = json!({"action": action, "id": id});
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        let live = op(&gw, "mcp_server_set", args).await;
        let saved = common::round_trips::<dto::McpServerSaved>(what, &live);
        assert_keys(&live, &plain);
        assert_eq!((saved.id, saved.tool_count), (id, None), "{action}");
        let verb = match action {
            "disable" => "disabled",
            "enable" => "enabled",
            _ => "updated",
        };
        assert_eq!(
            saved.message,
            format!("MCP server 'srv2' {verb}"),
            "{action}"
        );
    }

    // A paired device's hosted-tools row has an update path of its own.
    let key = store::insert_api_key(&state.db, "desk", "hash")
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO mcp_servers
                (name, enabled, transport, tool_prefix, timeout_ms, autostart,
                 idle_seconds, allow_sampling, device_key_id)
         VALUES (?1, 1, 'device', 'desk', 30000, 0, 0, 0, ?2)",
    )
    .bind(format!("{}desktop", lmgw_core::devices::NAME_PREFIX))
    .bind(key)
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let device = store::list_mcp_servers(&state.db)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.is_device())
        .expect("the device row")
        .id;
    for (action, extra) in [
        ("update", json!({"timeout_ms": 45000})),
        ("disable", json!({})),
    ] {
        let mut args = json!({"action": action, "id": device});
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        let live = op(&gw, "mcp_server_set", args).await;
        let saved = common::round_trips::<dto::McpServerSaved>(what, &live);
        assert_keys(&live, &plain);
        assert_eq!((saved.id, saved.tool_count), (device, None), "{action}");
        let verb = if action == "disable" {
            "disabled"
        } else {
            "updated"
        };
        assert_eq!(saved.message, format!("MCP server 'device:desktop' {verb}"));
    }

    let live = op(&gw, "mcp_server_set", json!({"action": "delete", "id": id})).await;
    let deleted = common::round_trips::<dto::McpServerSaved>(what, &live);
    assert_keys(&live, &plain);
    assert_eq!(deleted.message, "MCP server 'srv2' deleted");
}

/// `changed` is a list of field names in the order the op checks them, and an
/// empty list (not absent, not `null`) when every field read as sent.
#[tokio::test]
async fn a_key_save_lists_the_fields_it_changed_and_an_empty_list_for_none() {
    let (state, gw) = gateway().await;
    let what = "POST /api/op/key_set";
    let keys = ["ok", "id", "name", "changed", "message"];
    let id = store::insert_api_key(&state.db, "kilo", &hash_api_key("lmgw-test"))
        .await
        .unwrap();
    let dev = store::insert_device_key(
        &state.db,
        "device:desktop",
        &hash_api_key("lmgw-dev"),
        &KeyPolicy::default(),
        (None, DeviceAdmin::Off),
        "",
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let live = op(
        &gw,
        "key_set",
        json!({"id": id, "enabled": true, "note": ""}),
    )
    .await;
    let none = common::round_trips::<dto::KeySaved>(what, &live);
    assert_keys(&live, &keys);
    assert_eq!(live["changed"], json!([]), "{live}");
    assert!(live["id"].is_i64(), "{live}");
    assert_eq!((none.id, none.name.as_str()), (id, "kilo"));
    assert_eq!(
        none.message,
        "'kilo' already reads exactly that — nothing written"
    );

    // The order is the op's, not the body's.
    let live = op(
        &gw,
        "key_set",
        json!({"id": id, "note": "laptop", "rpm_limit": 60, "scope_mode": "allow",
               "expires_at": "2099-12-31", "enabled": false}),
    )
    .await;
    let saved = common::round_trips::<dto::KeySaved>(what, &live);
    assert_keys(&live, &keys);
    assert_eq!(
        saved.changed,
        ["enabled", "scope_mode", "rpm_limit", "expires_at", "note"]
    );
    assert_eq!(
        saved.message,
        "'kilo': updated enabled, scope_mode, rpm_limit, expires_at, note"
    );

    // Said again, it reads the same and writes nothing.
    let live = op(
        &gw,
        "key_set",
        json!({"id": id, "rpm_limit": 60, "note": "laptop"}),
    )
    .await;
    let again = common::round_trips::<dto::KeySaved>(what, &live);
    assert!(again.changed.is_empty(), "{live}");

    let live = op(
        &gw,
        "key_set",
        json!({"id": dev, "hosts_label": "desk", "self_admin": "read_only"}),
    )
    .await;
    let device = common::round_trips::<dto::KeySaved>(what, &live);
    assert_keys(&live, &keys);
    assert_eq!(device.name, "device:desktop");
    assert_eq!(device.changed, ["hosts_label", "self_admin"]);
}

#[tokio::test]
async fn a_settings_save_lists_the_settings_it_changed_in_the_order_it_applied_them() {
    let (_state, gw) = gateway().await;
    let what = "POST /api/op/settings_set";

    let live = op(&gw, "settings_set", json!({"retention_days": 30})).await;
    let one = common::round_trips::<dto::SettingsSaved>(what, &live);
    assert_keys(&live, &["ok", "changed", "message"]);
    assert_eq!(one.changed, ["retention_days"]);
    assert_eq!(one.message, "updated retention_days");

    // A section's fields are named under it, after the plain ones.
    let live = op(
        &gw,
        "settings_set",
        json!({"realtime": {"asr_alias": "", "default_voice": "alba"},
               "max_body_mb": 64, "retention_days": 7}),
    )
    .await;
    let many = common::round_trips::<dto::SettingsSaved>(what, &live);
    assert_keys(&live, &["ok", "changed", "message"]);
    assert_eq!(
        many.changed,
        [
            "retention_days",
            "max_body_mb",
            "realtime.asr_alias",
            "realtime.default_voice"
        ]
    );
    assert_eq!(
        many.message,
        "updated retention_days, max_body_mb, realtime.asr_alias, realtime.default_voice"
    );
}

/// `benchmark_aborted` is there only when a run was aborted: not on a no-op,
/// not on a change that aborted none.
#[tokio::test]
async fn the_hold_leaves_benchmark_aborted_out_unless_a_run_was_aborted() {
    let (state, gw) = gateway().await;
    let what = "POST /api/op/hold_set";
    let without_run = [
        "ok",
        "active",
        "fallback_alias",
        "stopped",
        "draining",
        "failed",
        "kept_on_cpu",
        "message",
    ];

    let live = op(&gw, "hold_set", json!({"active": false})).await;
    let noop = common::round_trips::<dto::HoldSet>(what, &live);
    assert_keys(&live, &without_run);
    assert_eq!(noop.benchmark_aborted, None);
    assert!(live["fallback_alias"].is_null(), "present, `null`: {live}");
    assert_eq!(live["stopped"], json!([]));
    assert_eq!(noop.message, "the GPU hold was already off — nothing to do");

    // A fallback is a string; nothing is running, so nothing is stopped.
    let mut s = state.snapshot().settings.clone();
    s.hold.fallback_alias = Some("cloud-chat".into());
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let live = op(&gw, "hold_set", json!({"active": true})).await;
    let engaged = common::round_trips::<dto::HoldSet>(what, &live);
    assert_keys(&live, &without_run);
    assert_eq!(engaged.benchmark_aborted, None);
    assert!(engaged.active);
    assert_eq!(engaged.fallback_alias.as_deref(), Some("cloud-chat"));
    for list in ["stopped", "draining", "failed", "kept_on_cpu"] {
        assert_eq!(live[list], json!([]), "{list}: {live}");
    }

    let live = op(&gw, "hold_set", json!({"active": true})).await;
    let noop = common::round_trips::<dto::HoldSet>(what, &live);
    assert_keys(&live, &without_run);
    assert_eq!(noop.benchmark_aborted, None);
    assert_eq!(noop.message, "the GPU hold was already on — nothing to do");

    let live = op(&gw, "hold_set", json!({"active": false})).await;
    let released = common::round_trips::<dto::HoldSet>(what, &live);
    assert_keys(&live, &without_run);
    assert_eq!(released.benchmark_aborted, None);
    assert!(!released.active);
    assert_eq!(
        released.message,
        "GPU hold released — local models start on demand again"
    );
}
