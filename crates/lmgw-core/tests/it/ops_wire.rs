//! The dashboard reads that `ops::reads` answers, over HTTP: each one reads
//! back through the type the document describes, and keeps the bytes it always
//! had where a key is absent, `null` or a number.
//!
//! `GET /api/tools` and `POST /api/op/tool_set` are pinned in
//! `tool_inventory.rs`, next to the states that make them interesting.

use lmgw_api_types as dto;
use lmgw_core::config::{HoldFallbackMode, McpTransport};
use lmgw_core::ops::{self, CandidateAliasPatch};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewCandidateAlias, NewLocalModel, NewMcpServer};
use serde_json::{json, Value};

use crate::common::{self, serve, Gw};
use crate::support::mcp_stub;

async fn get(gw: &Gw, path: &str) -> Value {
    let resp = gw.client().get(format!("{gw}{path}")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200, "GET {path}");
    resp.json().await.unwrap()
}

/// A header-only GGUF, enough for a local chat row to be read.
fn write_gguf(path: &std::path::Path) {
    fn put_str(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
        buf.extend_from_slice(s.as_bytes());
    }
    let mut kv = Vec::new();
    put_str(&mut kv, "general.architecture");
    kv.extend_from_slice(&8u32.to_le_bytes());
    put_str(&mut kv, "qwen3");
    let mut out = Vec::new();
    out.extend_from_slice(b"GGUF");
    out.extend_from_slice(&3u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&1u64.to_le_bytes());
    out.extend_from_slice(&kv);
    std::fs::write(path, out).unwrap();
}

/// A local chat row whose facets are pinned by `capabilities_override`.
fn local_row(id: &str, vision: bool) -> NewLocalModel {
    NewLocalModel {
        model_id: id.into(),
        gguf_path: "m.gguf".into(),
        params: Default::default(),
        args: vec![],
        idle_seconds: 300,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: Some(json!({
            "capabilities": {
                "vision": vision,
                "input_modalities": if vision { json!(["text", "image"]) } else { json!(["text"]) },
                "tool_calls": {"kind": "native"},
                "reasoning": {"kind": "fixed", "enabled": false},
                "structured_output": {"json_schema": true, "json_object": true},
            }
        })),
        ladder: vec![],
    }
}

async fn log_deferral(state: &SharedState, alias: &str) {
    sqlx::query(
        "INSERT INTO request_logs (ts, ingress_proto, requested_alias, status, error_kind)
         VALUES (strftime('%Y-%m-%d %H:%M:%S', 'now'), 'openai', ?, 503, 'gpu_deferred')",
    )
    .bind(alias)
    .execute(&state.db)
    .await
    .unwrap();
}

fn server(name: &str, transport: McpTransport) -> NewMcpServer {
    NewMcpServer {
        name: name.into(),
        enabled: true,
        transport,
        command: None,
        args: vec![],
        env: vec![],
        cwd: None,
        container_image: None,
        extra_run_args: vec![],
        url: None,
        headers: vec![],
        tool_prefix: name.into(),
        timeout_ms: 5_000,
        autostart: false,
        idle_seconds: 300,
        allow_sampling: false,
        sampling_alias: None,
        agent_id: None,
    }
}

/// `candidate_aliases` is a key of its own: absent while none is configured
/// (no empty array, no `null`), then one entry per alias, by name.
#[tokio::test]
async fn the_status_names_candidate_aliases_only_once_one_is_configured() {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    write_gguf(&dir.path().join("m.gguf"));
    let mut settings = state.snapshot().settings.clone();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    let gw = serve(state.clone()).await;

    let live = get(&gw, "/api/status").await;
    let none = common::round_trips::<dto::GatewayStatus>("GET /api/status", &live);
    assert_eq!(none.candidate_aliases, None);
    assert!(live.get("candidate_aliases").is_none(), "{live}");
    // The ledger is always reported, with the reason it is inactive.
    assert_eq!(live["vram"]["active"], false, "{live}");
    assert!(live["vram"]["inactive_reason"].is_string(), "{live}");

    store::insert_local_model(&state.db, &local_row("m1", true))
        .await
        .unwrap();
    store::insert_local_model(&state.db, &local_row("m2", false))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    for (alias, candidates, background) in [("zeta", "m1, m2", false), ("Alpha", "m2", true)] {
        ops::candidate_alias_set(
            &state,
            CandidateAliasPatch {
                action: "create".into(),
                alias: Some(alias.into()),
                candidates: Some(candidates.into()),
                background: Some(background),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    // A row that names a model that is gone has a problem, and one saved with
    // an empty list has no primary: `null`, not absent.
    for (alias, candidates) in [("broken", vec!["m1", "gone"]), ("empty-list", vec![])] {
        store::insert_candidate_alias(
            &state.db,
            &NewCandidateAlias {
                alias: alias.into(),
                candidates: candidates.into_iter().map(String::from).collect(),
                background: false,
                fallback_mode: HoldFallbackMode::Inherit,
                fallback: None,
                capabilities_disabled: vec![],
                capabilities_enabled: vec![],
                enabled: true,
                notes: String::new(),
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    log_deferral(&state, "zeta").await;
    log_deferral(&state, "ZETA").await;

    let live = get(&gw, "/api/status").await;
    let typed = common::round_trips::<dto::GatewayStatus>("GET /api/status", &live);
    let aliases = typed.candidate_aliases.expect("candidate aliases");
    let names: Vec<&str> = aliases.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["Alpha", "broken", "empty-list", "zeta"]);
    let by = |name: &str| aliases.iter().find(|a| a.name == name).unwrap();
    assert_eq!(by("Alpha").mode, "background");
    assert_eq!(by("zeta").mode, "owner");
    assert_eq!(by("zeta").primary.as_deref(), Some("m1"));
    assert_eq!(by("zeta").alternates, ["m2"]);
    assert_eq!(by("zeta").deferrals_24h, 2, "both spellings count");
    assert_eq!(by("Alpha").deferrals_24h, 0, "a real zero, a number");
    assert_eq!(by("broken").problems, ["candidate 'gone' does not exist"]);
    // The wire: a missing primary is `null`, the counts are integers.
    let row = live["candidate_aliases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "empty-list")
        .unwrap();
    assert!(row["primary"].is_null(), "{row}");
    assert_eq!(row["deferrals_24h"], json!(0), "{row}");
    assert!(row["deferrals_24h"].is_u64(), "an integer, not 0.0: {row}");
}

/// One server list holding every kind of row the page tells apart: a ready
/// server, an owner's container-isolated one (secrets redacted), an agent's
/// registration asleep, one overridden by a dev server, a paired device's, and
/// one that cannot connect.
#[tokio::test]
async fn the_server_list_and_the_status_read_back_for_every_kind_of_row() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;

    // Nothing registered: an empty array, not absent.
    let live = get(&gw, "/api/mcp-servers").await;
    let empty = common::round_trips::<dto::McpServersResponse>("GET /api/mcp-servers", &live);
    assert!(empty.mcp_servers.is_empty());
    assert_eq!(live, json!({"mcp_servers": []}));

    let stub = mcp_stub::stub(
        json!([
            {"name": "echo", "description": "echo the input",
             "inputSchema": {"type": "object", "properties": {}}},
            {"name": "nodesc", "inputSchema": {"type": "object", "properties": {}}},
        ]),
        false,
    )
    .await;
    let mut ready = server("ready", McpTransport::Http);
    ready.url = Some(stub.url.clone());
    ready.headers = vec![("Authorization".into(), "Bearer secret".into())];
    ready.allow_sampling = true;
    ready.sampling_alias = Some("sampler".into());
    let ready_id = store::insert_mcp_server(&state.db, &ready).await.unwrap();

    let mut isolated = server("isolated", McpTransport::Stdio);
    isolated.command = Some("mcp-server-x".into());
    isolated.args = vec!["--verbose".into(), "two words".into()];
    isolated.env = vec![("TOKEN".into(), "abc".into()), ("MODE".into(), "x".into())];
    isolated.cwd = Some("/srv".into());
    isolated.container_image = Some("img:1".into());
    isolated.extra_run_args = vec!["-v".into(), "./d:/d:Z".into()];
    isolated.enabled = false;
    store::insert_mcp_server(&state.db, &isolated)
        .await
        .unwrap();

    let (dead, _held) = mcp_stub::dead_url();
    let mut broken = server("broken", McpTransport::Http);
    broken.url = Some(dead);
    broken.autostart = true;
    store::insert_mcp_server(&state.db, &broken).await.unwrap();

    for agent in ["sleepy", "devvy"] {
        store::insert_agent(&state.db, agent, "{}", "authored")
            .await
            .unwrap();
        let mut s = server(&format!("agent:{agent}"), McpTransport::Http);
        s.url = Some("http://127.0.0.1:9/mcp".into());
        s.agent_id = Some(agent.into());
        store::insert_mcp_server(&state.db, &s).await.unwrap();
    }
    store::set_agent_dev_url(&state.db, "devvy", Some("http://127.0.0.1:5173"))
        .await
        .unwrap();

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
    for n in 0..2 {
        sqlx::query(
            "INSERT INTO mcp_tasks (server_id, server_label, task_id, tool, call_id, state, status)
             VALUES (?1, 'ready', ?2, 'build', 'c', 'open', 'working')",
        )
        .bind(ready_id)
        .bind(format!("task-{n}"))
        .execute(&state.db)
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    state.mcp.reconcile(&state.snapshot()).await;
    // Listing connects what can be connected, as the page's own reads do.
    let _ = state.mcp.list_tools(&state.snapshot()).await;

    let live = get(&gw, "/api/mcp-servers").await;
    let typed = common::round_trips::<dto::McpServersResponse>("GET /api/mcp-servers", &live);
    let row = |name: &str| {
        typed
            .mcp_servers
            .iter()
            .find(|s| s.name.ends_with(name))
            .unwrap_or_else(|| panic!("no {name} row in {live}"))
    };
    assert_eq!(row("ready").status, "ready");
    assert_eq!(row("ready").tool_count, 2);
    assert_eq!(row("ready").open_tasks, 2);
    assert_eq!(row("ready").headers, "Authorization: <set>");
    assert_eq!(row("ready").sampling_alias.as_deref(), Some("sampler"));
    assert_eq!(row("isolated").env, "TOKEN=<set>\nMODE=<set>");
    assert_eq!(row("isolated").args, "--verbose\ntwo words");
    assert_eq!(row("isolated").extra_run_args, ["-v", "./d:/d:Z"]);
    assert_eq!(row("isolated").status, "stopped");
    assert_eq!(row("broken").status, "error");
    assert_eq!(row("sleepy").agent_id.as_deref(), Some("sleepy"));
    assert!(row("sleepy")
        .status_detail
        .as_deref()
        .is_some_and(|d| d.starts_with("sleeping")));
    assert!(row("devvy")
        .status_detail
        .as_deref()
        .is_some_and(|d| d.starts_with("served from a dev server at http://127.0.0.1:5173")));
    assert_eq!(row("desktop").device.as_deref(), Some("desktop"));
    assert_eq!(row("desktop").device_key_id, Some(key));
    assert_eq!(row("desktop").transport, "device");
    // The wire: the keys an owner's row has no value for are `null`, never
    // absent, and the counts are integers.
    let owner = live["mcp_servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "isolated")
        .unwrap();
    for key in [
        "agent_id",
        "device",
        "device_key_id",
        "status_detail",
        "url",
    ] {
        assert!(owner[key].is_null(), "{key} in {owner}");
    }
    for key in ["tool_count", "open_tasks", "timeout_ms", "idle_seconds"] {
        assert!(
            owner[key].is_i64() || owner[key].is_u64(),
            "{key} in {owner}"
        );
    }

    // The status carries the same connections, as the page's badges.
    let live = get(&gw, "/api/status").await;
    let status = common::round_trips::<dto::GatewayStatus>("GET /api/status", &live);
    assert_eq!(status.counts.mcp_servers, 6);
    let ready = status
        .mcp_servers
        .iter()
        .find(|s| s.id == ready_id)
        .unwrap_or_else(|| panic!("no ready connection in {live}"));
    assert_eq!((ready.status.as_str(), ready.tool_count), ("ready", 2));

    // The picker: the exposed names, the server's own, and a missing
    // description as `null`.
    let live = get(&gw, &format!("/api/mcp-servers/{ready_id}/tools")).await;
    let tools = common::round_trips::<dto::McpServerToolsResponse>(
        "GET /api/mcp-servers/{id}/tools",
        &live,
    );
    let names: Vec<(&str, &str)> = tools
        .tools
        .iter()
        .map(|t| (t.name.as_str(), t.upstream_name.as_str()))
        .collect();
    assert_eq!(
        names,
        [("ready__echo", "echo"), ("ready__nodesc", "nodesc")]
    );
    assert_eq!(
        tools.tools[0].description.as_deref(),
        Some("echo the input")
    );
    assert!(live["tools"][1]["description"].is_null(), "{live}");
    // A server nothing is known about offers nothing: an empty list.
    let live = get(&gw, "/api/mcp-servers/9999/tools").await;
    let none = common::round_trips::<dto::McpServerToolsResponse>(
        "GET /api/mcp-servers/{id}/tools",
        &live,
    );
    assert!(none.tools.is_empty());
    assert_eq!(live, json!({"tools": []}));
}
