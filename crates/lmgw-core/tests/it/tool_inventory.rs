//! The gateway's tool inventory and its per-tool owner switch, plus the
//! built-in toolsets as attachable labels.
//!
//! Two behaviours are pinned here, both easy to regress by touching one plane
//! and forgetting the others:
//!
//! * **A disabled tool is off everywhere.** Not listed on `/mcp`, `/mcp/admin`,
//!   the thread picker or a `/v1/responses` run — *and* refused by name when a
//!   caller names it anyway. Hiding alone would leave any client that kept a
//!   name from an earlier `tools/list` still able to call it.
//! * **The built-in toolsets are opt-in labels.** `lmgw` and `docs` resolve
//!   like a registered server, with the same `allowed_tools` narrowing, and
//!   `lmgw` cannot escalate past the `self_admin` Setting.
//! * **A client key's tool scope binds on every plane it can reach.** `/mcp`
//!   lists and routes through it, a `/v1/responses` run resolves through it,
//!   and the `lmgw` toolset needs an owner credential there whatever a key's
//!   list says.
//!
//! No containers and no real network: the model is a wiremock upstream and the
//! southbound MCP server is an in-test Streamable-HTTP stub.

use lmgw_core::config::{McpTransport, Protocol, SelfAdmin, Settings, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewMcpServer, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common;
use common::{serve, Gw};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A gateway with `my-model` routed at `upstream_base`, self-admin at `mode`
/// and `admin-tok` on the `/mcp/admin` route.
async fn setup(upstream_base: &str, mode: SelfAdmin) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: upstream_base.trim_end_matches('/').to_string(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "my-model".into(),
            upstream_id: up_id,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    store::save_settings(
        &state.db,
        &Settings {
            self_admin: mode,
            ..Settings::default()
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    lmgw_core::agents::token::set_owner_key(
        &state,
        lmgw_core::agents::token::OWNER_SELF_ADMIN,
        "admin-tok",
        true,
    )
    .await
    .unwrap();
    let base = serve(state.clone()).await;
    (state, base)
}

/// A minimal southbound MCP server with two tools, same shape as the other
/// suites' stub.
async fn mcp_stub() -> String {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let handler = move |body: String| async move {
        let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let Some(id) = req.get("id").cloned() else {
            return StatusCode::ACCEPTED.into_response();
        };
        let tool = |name: &str| {
            json!({
                "name": name,
                "description": format!("{name} the input"),
                "inputSchema": {"type": "object",
                                "properties": {"text": {"type": "string"}}},
            })
        };
        let result = match req.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => json!({
                "protocolVersion": req.pointer("/params/protocolVersion")
                    .cloned().unwrap_or(json!("2025-06-18")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "stub", "version": "0.1.0"},
            }),
            "tools/list" => json!({"tools": [tool("echo"), tool("ping")]}),
            "tools/call" => json!({
                "content": [{"type": "text", "text": "echoed"}],
                "isError": false,
            }),
            _ => json!({}),
        };
        (
            StatusCode::OK,
            [
                ("content-type", "application/json"),
                ("mcp-session-id", "stub-session"),
            ],
            json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
        )
            .into_response()
    };

    let app = axum::Router::new().route("/mcp", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/mcp")
}

async fn register_mcp(state: &SharedState, url: &str) -> i64 {
    let id = store::insert_mcp_server(
        &state.db,
        &NewMcpServer {
            name: "stub-server".into(),
            enabled: true,
            transport: McpTransport::Http,
            command: None,
            args: vec![],
            env: vec![],
            cwd: None,
            container_image: None,
            extra_run_args: vec![],
            url: Some(url.into()),
            headers: vec![],
            tool_prefix: "stub".into(),
            timeout_ms: 5_000,
            autostart: false,
            idle_seconds: 300,
            allow_sampling: false,
            sampling_alias: None,
            agent_id: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    state.mcp.reconcile(&state.snapshot()).await;
    id
}

// ---- the dashboard plane ----

async fn inventory(base: &Gw) -> lmgw_api_types::ToolInventory {
    let text = base
        .client()
        .get(format!("{base}/api/tools"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("/api/tools does not decode as the UI's DTO ({e}): {text}"))
}

/// `POST /api/op/{name}` → `(http status, body)`.
async fn op(base: &Gw, name: &str, body: Value) -> (u16, Value) {
    let r = base
        .client()
        .post(format!("{base}/api/op/{name}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

async fn set_tool(base: &Gw, name: &str, enabled: bool) -> (u16, Value) {
    op(base, "tool_set", json!({"name": name, "enabled": enabled})).await
}

// ---- the northbound MCP planes ----

/// Open a session on `route` and return its id.
async fn mcp_session(base: &Gw, route: &str, token: Option<&str>) -> String {
    let mut req = base
        .client()
        .post(format!("{base}{route}"))
        .header("accept", "application/json")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                       "clientInfo": {"name": "inventory-test", "version": "0"}}
        }));
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200, "initialize on {route}");
    resp.headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .expect("session id")
        .to_string()
}

async fn mcp_rpc(base: &Gw, route: &str, token: Option<&str>, sid: &str, body: Value) -> Value {
    let mut req = base
        .client()
        .post(format!("{base}{route}"))
        .header("accept", "application/json")
        .header("mcp-session-id", sid)
        .json(&body);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    req.send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap_or(Value::Null)
}

async fn tool_names(base: &Gw, route: &str, token: Option<&str>) -> Vec<String> {
    let sid = mcp_session(base, route, token).await;
    let body = mcp_rpc(
        base,
        route,
        token,
        &sid,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    body["result"]["tools"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect()
}

/// The full `tools/list` entries, schemas included — what a test about a
/// tool's *arguments* needs, where [`tool_names`] only answers "is it there".
async fn tool_entries(base: &Gw, route: &str, token: Option<&str>) -> Vec<Value> {
    let sid = mcp_session(base, route, token).await;
    let body = mcp_rpc(
        base,
        route,
        token,
        &sid,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    body["result"]["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

async fn call_tool(base: &Gw, route: &str, token: Option<&str>, name: &str) -> Value {
    let sid = mcp_session(base, route, token).await;
    mcp_rpc(
        base,
        route,
        token,
        &sid,
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
               "params": {"name": name, "arguments": {}}}),
    )
    .await
}

fn entry<'a>(
    inv: &'a lmgw_api_types::ToolInventory,
    name: &str,
) -> &'a lmgw_api_types::ToolEntryView {
    inv.tools
        .iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("'{name}' is missing from the inventory"))
}

// ---------------------------------------------------------------------------
// The inventory itself
// ---------------------------------------------------------------------------

/// The point of the endpoint: one list that names *every* tool the gateway can
/// serve, including the two built-in toolsets that appeared in no UI before.
#[tokio::test]
async fn the_inventory_names_every_tool_and_where_it_comes_from() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    let stub = mcp_stub().await;
    let sid = register_mcp(&state, &stub).await;

    let inv = inventory(&base).await;

    let labels: Vec<&str> = inv.sources.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(
        labels,
        vec!["lmgw", "docs", "kb", "stub"],
        "the built-ins are sources too: {labels:?}"
    );

    let admin = entry(&inv, "lmgw__status");
    assert_eq!(admin.source_label, "lmgw");
    assert_eq!(admin.source_kind, "builtin");
    assert_eq!(admin.plane, "/mcp/admin");
    assert!(admin.available && admin.enabled);

    let docs = entry(&inv, "docs__resolve");
    assert_eq!(docs.source_label, "docs");
    assert_eq!(docs.plane, "/mcp", "docs rides the aggregate plane");
    assert!(docs.available);

    let echo = entry(&inv, "stub__echo");
    assert_eq!(echo.source_label, "stub");
    assert_eq!(echo.source_kind, "server");
    assert_eq!(echo.server_id, Some(sid));
    assert_eq!(echo.upstream_name.as_deref(), Some("echo"));
    assert!(echo.available);
    assert!(inv.tools.iter().all(|t| !t.stale), "nothing is stale yet");
}

/// A registered server that offers nothing still has a row — that row is where
/// "it is disabled" gets said, and a per-tool list cannot say it.
#[tokio::test]
async fn a_source_that_offers_nothing_still_carries_its_reason() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri(), SelfAdmin::Off).await;
    let stub = mcp_stub().await;
    let id = register_mcp(&state, &stub).await;
    op(
        &base,
        "mcp_server_set",
        json!({"action": "disable", "id": id}),
    )
    .await;

    let inv = inventory(&base).await;
    let server = inv.sources.iter().find(|s| s.label == "stub").unwrap();
    assert!(!server.available);
    assert!(
        server
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("disabled"),
        "{server:?}"
    );

    // Self-admin off is a *source* condition, distinct from the owner switch.
    let admin = inv.sources.iter().find(|s| s.label == "lmgw").unwrap();
    assert!(!admin.available);
    assert!(
        admin
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("switched off"),
        "{admin:?}"
    );
    let status = entry(&inv, "lmgw__status");
    assert!(status.enabled, "the owner did not switch this one off");
    assert!(!status.available, "but the mode gate hides it");
}

/// The five owner-credential ops are **not** tools, on any plane (principals
/// §3.7, §3.12).
///
/// `key_reveal` and `key_rotate` hand an owner key back in the clear;
/// `key_create`, `key_set` and `key_delete` govern which credentials exist and
/// what they may spend. They live on `/api`, behind a session, and that is the
/// whole of where they live. A model driving `/mcp/admin` at `self_admin:
/// full` configures this gateway — it does not read out, mint or switch off
/// the credentials that reach it, and a self-admin plane that could hand
/// itself a fresh owner key would make the mode gate a formality.
///
/// The absence is a decision, so it is asserted here rather than left to
/// whoever next extends the catalog.
#[tokio::test]
async fn the_owner_key_ops_are_on_no_tool_plane() {
    const NEVER: [&str; 5] = [
        "key_reveal",
        "key_rotate",
        "key_create",
        "key_set",
        "key_delete",
    ];

    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri(), SelfAdmin::Full).await;

    let served = tool_names(&base, "/mcp/admin", Some("admin-tok")).await;
    assert!(
        served.iter().any(|n| n == "lmgw__settings_set"),
        "the write half of the plane really is open here: {served:?}"
    );
    for op in NEVER {
        assert!(
            !served.contains(&format!("lmgw__{op}")),
            "'{op}' is served on /mcp/admin: {served:?}"
        );
    }

    // And the inventory is every plane at once — the list a new tool appears
    // in first, whichever plane it was added to.
    let inv = inventory(&base).await;
    let names: Vec<&str> = inv.tools.iter().map(|t| t.name.as_str()).collect();
    for op in NEVER {
        let name = format!("lmgw__{op}");
        assert!(
            !names.contains(&name.as_str()),
            "'{name}' appears in the tool inventory: {names:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Disable: hidden from every list, refused by name on every plane
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disabling_a_docs_tool_hides_it_and_refuses_the_call_on_mcp() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri(), SelfAdmin::Full).await;

    assert!(tool_names(&base, "/mcp", None)
        .await
        .contains(&"docs__resolve".to_string()));

    let (status, body) = set_tool(&base, "docs__resolve", false).await;
    assert_eq!(status, 200, "{body}");

    let names = tool_names(&base, "/mcp", None).await;
    assert!(!names.contains(&"docs__resolve".to_string()), "{names:?}");
    assert!(
        names.contains(&"docs__query".to_string()),
        "its siblings are untouched: {names:?}"
    );

    // Hiding is not the boundary — a client that kept the name is refused.
    let resp = call_tool(&base, "/mcp", None, "docs__resolve").await;
    assert_eq!(resp["error"]["code"], -32601, "{resp}");
    let msg = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("disabled"), "the reason must be named: {msg}");

    // And back on again.
    set_tool(&base, "docs__resolve", true).await;
    assert!(tool_names(&base, "/mcp", None)
        .await
        .contains(&"docs__resolve".to_string()));
}

#[tokio::test]
async fn disabling_a_self_admin_tool_hides_it_and_refuses_it_on_the_admin_plane() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    let tok = Some("admin-tok");

    let (status, body) = set_tool(&base, "lmgw__status", false).await;
    assert_eq!(status, 200, "{body}");

    let names = tool_names(&base, "/mcp/admin", tok).await;
    assert!(!names.contains(&"lmgw__status".to_string()), "{names:?}");
    assert!(names.contains(&"lmgw__models".to_string()), "{names:?}");

    let resp = call_tool(&base, "/mcp/admin", tok, "lmgw__status").await;
    assert_eq!(resp["error"]["code"], -32601, "{resp}");
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("disabled"));
}

#[tokio::test]
async fn disabling_a_server_tool_hides_it_from_mcp_and_from_the_thread_picker() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    let stub = mcp_stub().await;
    let id = register_mcp(&state, &stub).await;

    let (status, body) = set_tool(&base, "stub__echo", false).await;
    assert_eq!(status, 200, "{body}");

    let names = tool_names(&base, "/mcp", None).await;
    assert!(!names.contains(&"stub__echo".to_string()), "{names:?}");
    assert!(names.contains(&"stub__ping".to_string()), "{names:?}");

    let resp = call_tool(&base, "/mcp", None, "stub__echo").await;
    assert_eq!(resp["error"]["code"], -32601, "{resp}");
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("disabled"));

    // The Chat picker reads this endpoint; a tick the send would refuse would
    // be a lie, so the tool is gone from here too.
    let picker: Value = base
        .client()
        .get(format!("{base}/api/mcp-servers/{id}/tools"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let offered: Vec<&str> = picker["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(offered, vec!["stub__ping"], "{picker}");
}

/// The switch is owner state, so it must never become invisible: a record whose
/// tool has gone away is shown as stale, and re-enabling clears it.
#[tokio::test]
async fn a_switch_whose_tool_is_gone_shows_as_stale_and_can_be_cleared() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    let stub = mcp_stub().await;
    let id = register_mcp(&state, &stub).await;
    set_tool(&base, "stub__echo", false).await;

    op(
        &base,
        "mcp_server_set",
        json!({"action": "delete", "id": id}),
    )
    .await;

    let inv = inventory(&base).await;
    let stale = entry(&inv, "stub__echo");
    assert!(stale.stale, "{stale:?}");
    assert!(!stale.enabled);
    assert_eq!(
        stale.source_label, "stub",
        "it remembers where it came from"
    );
    assert!(
        stale
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("no tool by this name is offered"),
        "{stale:?}"
    );

    let (status, body) = set_tool(&base, "stub__echo", true).await;
    assert_eq!(status, 200, "{body}");
    let inv = inventory(&base).await;
    assert!(
        !inv.tools.iter().any(|t| t.name == "stub__echo"),
        "clearing the record removes the row entirely"
    );
}

/// Disabling a name nothing offers would create exactly the invisible state the
/// stale rows exist to prevent, so it is refused with the name in the message.
#[tokio::test]
async fn disabling_a_name_the_gateway_does_not_offer_is_refused() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    let (status, body) = set_tool(&base, "nope__nothing", false).await;
    assert_eq!(status, 400, "{body}");
    assert!(body["message"]
        .as_str()
        .unwrap_or_default()
        .contains("nope__nothing"));
}

/// Both halves of the composition are reported, because fixing one of them
/// would otherwise look like it did nothing.
#[tokio::test]
async fn the_mode_gate_and_the_owner_switch_compose() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri(), SelfAdmin::ReadOnly).await;

    // A write tool at read-only: hidden by the Setting, owner switch untouched.
    let inv = inventory(&base).await;
    let write = entry(&inv, "lmgw__settings_set");
    assert!(write.enabled && !write.available);
    assert!(
        write
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("read only"),
        "{write:?}"
    );
    assert!(entry(&inv, "lmgw__status").available);

    // Now switch the read tool off too: the row names both conditions.
    set_tool(&base, "lmgw__settings_set", false).await;
    let inv = inventory(&base).await;
    let write = entry(&inv, "lmgw__settings_set");
    assert!(!write.enabled && !write.available);
    let reason = write.reason.clone().unwrap_or_default();
    assert!(
        reason.contains("owner") && reason.contains("read only"),
        "{reason}"
    );
}

// ---------------------------------------------------------------------------
// Built-in toolsets as attachable labels
// ---------------------------------------------------------------------------

async fn new_thread(base: &Gw, kind: &str) -> i64 {
    base.client()
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({"model_alias": "my-model", "kind": kind}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap()
}

async fn attach(base: &Gw, tid: i64, mcp: Value) {
    base.client()
        .post(format!("{base}/chat/api/threads/{tid}/settings"))
        .json(&json!({"mcp_tools": mcp}))
        .send()
        .await
        .unwrap();
}

async fn send(base: &Gw, tid: i64, content: &str) -> String {
    base.client()
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({"content": content}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// One streamed upstream reply asking for `name`, then one plain answer.
async fn mount_call_then_answer(mock: &MockServer, name: &str, answer: &str) {
    let call = format!(
        concat!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c1\",",
            "\"type\":\"function\",\"function\":{{\"name\":\"{}\",\"arguments\":\"\"}}}}]}}}}]}}\n\n",
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,",
            "\"function\":{{\"arguments\":\"{{\\\"library\\\":\\\"axum\\\"}}\"}}}}]}}}}]}}\n\n",
            "data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n",
            "data: [DONE]\n\n"
        ),
        name
    );
    let done = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{answer}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    );
    for (i, body) in [call, done].into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .up_to_n_times(1)
            .with_priority((i + 1) as u8)
            .mount(mock)
            .await;
    }
}

/// The gap this closes: `docs__*` was served to every external agent but was
/// unreachable from the gateway's own chat and from `/v1/responses`. A thread
/// attaches the toolset by its label, exactly like a registered server.
#[tokio::test]
async fn a_thread_can_attach_the_docs_toolset() {
    let mock = MockServer::start().await;
    mount_call_then_answer(&mock, "docs__resolve", "nothing ingested yet").await;
    let (_state, base) = setup(&mock.uri(), SelfAdmin::Off).await;
    let tid = new_thread(&base, "chat").await;
    attach(&base, tid, json!([{"server_label": "docs"}])).await;

    let body = send(&base, tid, "what does the gateway know about axum?").await;
    assert!(body.contains("docs__resolve"), "no tool call: {body}");
    assert!(
        body.contains("docs__request"),
        "the resolve miss points at the request tool: {body}"
    );
    assert!(body.contains("nothing ingested yet"), "{body}");

    // The whole toolset, because the thread narrowed nothing.
    let reqs = mock.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let mut names: Vec<&str> = sent["tools"]
        .as_array()
        .expect("tools must be sent")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["docs__query", "docs__request", "docs__resolve"],
        "{sent:#}"
    );
}

/// `allowed_tools` narrows a built-in toolset exactly as it narrows a server.
#[tokio::test]
async fn a_thread_can_narrow_a_builtin_toolset() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                    "text/event-stream",
                ),
        )
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri(), SelfAdmin::Off).await;
    let tid = new_thread(&base, "chat").await;
    attach(
        &base,
        tid,
        json!([{"server_label": "docs", "allowed_tools": ["docs__query"]}]),
    )
    .await;
    send(&base, tid, "hello").await;

    let reqs = mock.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let names: Vec<&str> = sent["tools"]
        .as_array()
        .expect("tools must be sent")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert_eq!(names, vec!["docs__query"], "{sent:#}");
}

/// Attaching `lmgw` cannot widen what the `self_admin` Setting allows: at
/// read-only the mutating tools are simply not there.
#[tokio::test]
async fn attaching_the_lmgw_toolset_cannot_escalate_past_read_only() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                    "text/event-stream",
                ),
        )
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri(), SelfAdmin::ReadOnly).await;
    let tid = new_thread(&base, "chat").await;
    attach(&base, tid, json!([{"server_label": "lmgw"}])).await;
    send(&base, tid, "how are you set up?").await;

    let reqs = mock.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let names: Vec<&str> = sent["tools"]
        .as_array()
        .expect("tools must be sent")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(names.contains(&"lmgw__status"), "{names:?}");
    assert!(
        !names.iter().any(|n| n.ends_with("_set")),
        "read-only must not carry a mutating tool: {names:?}"
    );
}

/// And at `off` the refusal says which Setting is in the way, rather than the
/// toolset silently not being there.
#[tokio::test]
async fn attaching_the_lmgw_toolset_while_self_admin_is_off_says_so() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri(), SelfAdmin::Off).await;
    let tid = new_thread(&base, "chat").await;
    attach(&base, tid, json!([{"server_label": "lmgw"}])).await;

    let body = send(&base, tid, "reconfigure yourself").await;
    assert!(body.contains("event: error"), "{body}");
    assert!(body.contains("Self-admin"), "{body}");
    assert_eq!(
        mock.received_requests().await.unwrap().len(),
        0,
        "the model must not be called at all"
    );
}

/// An admin thread keeps its automatic wiring — the built-in label is additive,
/// not a replacement for it.
#[tokio::test]
async fn an_admin_thread_still_gets_its_tools_without_attaching_anything() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                    "text/event-stream",
                ),
        )
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    let tid = new_thread(&base, "admin").await;
    send(&base, tid, "status?").await;

    let reqs = mock.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let names: Vec<&str> = sent["tools"]
        .as_array()
        .expect("tools must be sent")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(names.contains(&"lmgw__status"), "{names:?}");
}

/// The owner switch reaches the in-process executors too: a disabled tool is
/// not offered to an admin thread's model.
#[tokio::test]
async fn a_disabled_self_admin_tool_is_not_offered_to_an_admin_thread() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                    "text/event-stream",
                ),
        )
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    set_tool(&base, "lmgw__status", false).await;
    let tid = new_thread(&base, "admin").await;
    send(&base, tid, "status?").await;

    let reqs = mock.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let names: Vec<&str> = sent["tools"]
        .as_array()
        .expect("tools must be sent")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(!names.contains(&"lmgw__status"), "{names:?}");
    assert!(names.contains(&"lmgw__models"), "{names:?}");
}

// ---------------------------------------------------------------------------
// /v1/responses
// ---------------------------------------------------------------------------

async fn responses(base: &Gw, body: Value) -> Value {
    base.client()
        .post(format!("{base}/v1/responses"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap_or(Value::Null)
}

fn json_reply(call: Option<(&str, Value)>, text: &str) -> Value {
    let calling = call.is_some();
    let message = match call {
        Some((n, args)) => json!({
            "role": "assistant", "content": null,
            "tool_calls": [{"id": "c1", "type": "function",
                            "function": {"name": n, "arguments": args.to_string()}}],
        }),
        None => json!({"role": "assistant", "content": text}),
    };
    json!({
        "id": "chatcmpl-1", "object": "chat.completion", "model": "tgt-model",
        "choices": [{"index": 0, "message": message,
                     "finish_reason": if calling { "tool_calls" } else { "stop" }}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 4},
    })
}

async fn mount_json_sequence(mock: &MockServer, replies: Vec<Value>) {
    for (i, reply) in replies.into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .up_to_n_times(1)
            .with_priority((i + 1) as u8)
            .mount(mock)
            .await;
    }
}

/// quickdoc design §13 open question 1, resolved: a `/v1/responses` client can
/// name the `docs` toolset in its request and the tools compose with the rest
/// of the loop. Opt-in — a request that does not name it gets nothing.
#[tokio::test]
async fn a_responses_client_can_name_the_docs_toolset() {
    let mock = MockServer::start().await;
    mount_json_sequence(
        &mock,
        vec![
            json_reply(Some(("docs__resolve", json!({"library": "axum"}))), ""),
            json_reply(None, "no corpus yet"),
        ],
    )
    .await;
    let (_state, base) = setup(&mock.uri(), SelfAdmin::Off).await;

    let resp = responses(
        &base,
        json!({
            "model": "my-model",
            "input": "what axum docs do you have?",
            "tools": [{"type": "mcp", "server_label": "docs", "require_approval": "never"}],
        }),
    )
    .await;
    assert_eq!(resp["status"], "completed", "{resp}");
    let types: Vec<&str> = resp["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["type"].as_str())
        .collect();
    assert_eq!(
        types,
        vec!["mcp_list_tools", "mcp_call", "message"],
        "{resp}"
    );
    assert_eq!(resp["output"][0]["server_label"], "docs");
    assert_eq!(resp["output"][1]["name"], "docs__resolve");
    assert_eq!(resp["output"][1]["error"], Value::Null, "{resp}");
    assert!(
        resp["output"][1]["output"]
            .as_str()
            .unwrap_or_default()
            .contains("docs__request"),
        "the tool really ran: {resp}"
    );
}

/// Nothing is attached implicitly: a request that names no toolset gets no
/// gateway tools, which is what keeps `lmgw__*` out of ordinary traffic.
#[tokio::test]
async fn a_responses_request_that_names_nothing_gets_no_builtin_tools() {
    let mock = MockServer::start().await;
    mount_json_sequence(&mock, vec![json_reply(None, "hi")]).await;
    let (_state, base) = setup(&mock.uri(), SelfAdmin::Full).await;

    let resp = responses(&base, json!({"model": "my-model", "input": "hi"})).await;
    assert_eq!(resp["status"], "completed", "{resp}");
    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert!(sent.get("tools").is_none(), "{sent:#}");
}

/// A run that resolved a tool and *then* had it switched off — an approval
/// resume, or simply a slow loop — reaches the executors with a name the list
/// no longer contains. That is the hole a hide-only switch would leave, so the
/// three in-process executors are checked directly, by name.
#[tokio::test]
async fn every_executor_refuses_a_disabled_tool_by_name() {
    use lmgw_core::agent::ToolExecutor;
    use lmgw_core::mcp::exec::{DocsExecutor, McpExecutor, SelfAdminExecutor};
    use lmgw_core::proxy::RequestCtx;

    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    let stub = mcp_stub().await;
    register_mcp(&state, &stub).await;
    for name in ["stub__echo", "lmgw__status", "docs__resolve"] {
        let (status, body) = set_tool(&base, name, false).await;
        assert_eq!(status, 200, "{body}");
    }

    let args = json!({});
    let cases: Vec<(&str, Box<dyn ToolExecutor>)> = vec![
        (
            "stub__echo",
            Box::new(McpExecutor::new(state.clone(), RequestCtx::default())),
        ),
        (
            "lmgw__status",
            Box::new(SelfAdminExecutor::new(state.clone(), RequestCtx::default())),
        ),
        (
            "docs__resolve",
            Box::new(DocsExecutor::new(state.clone(), RequestCtx::default())),
        ),
    ];
    for (name, exec) in cases {
        let outcome = exec.call(name, &args).await;
        assert!(outcome.is_error, "'{name}' must be refused, not run");
        let (text, _) = lmgw_core::ir::flatten_tool_result(&outcome.blocks);
        assert!(
            text.contains("disabled") && text.contains(name),
            "the refusal must name the tool and the reason: {text}"
        );
    }
}

/// The list side of the same story, on the `/v1/responses` plane: a disabled
/// tool is not in the `mcp_list_tools` item the client receives.
#[tokio::test]
async fn a_disabled_tool_is_not_listed_to_a_responses_client() {
    let mock = MockServer::start().await;
    mount_json_sequence(&mock, vec![json_reply(None, "nothing to do")]).await;
    let (state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    let stub = mcp_stub().await;
    register_mcp(&state, &stub).await;
    set_tool(&base, "stub__echo", false).await;

    let resp = responses(
        &base,
        json!({
            "model": "my-model",
            "input": "echo hi",
            "tools": [{"type": "mcp", "server_label": "stub", "require_approval": "never"}],
        }),
    )
    .await;
    let listed: Vec<&str> = resp["output"][0]["tools"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(listed, vec!["stub__ping"], "{resp}");
}

// ---------------------------------------------------------------------------
// The image class's place in the inventory (image-generation design §8)
// ---------------------------------------------------------------------------

/// `lmgw__image_model_set` is a write tool, and the class it configures has to
/// be reachable from the tools that already existed — otherwise an agent can
/// create an image row and then has no way to read it, check it, test it or
/// start its container.
///
/// The `image` spellings are asserted per tool rather than in bulk, because
/// which tools carry them is a decision, not a sweep: WP3 taught the
/// downloader the stable-diffusion.cpp file kinds, so `lmgw__gguf_files`,
/// `lmgw__hf_repo`, `lmgw__hf_add` and `lmgw__hf_set` now offer `image` too —
/// before that they deliberately did not, because listing it would have
/// advertised a path that writes `.safetensors` into the chat tree.
#[tokio::test]
async fn the_image_class_is_reachable_from_the_tools_that_already_existed() {
    let mock = MockServer::start().await;
    let (_state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    let tok = Some("admin-tok");

    let entries = tool_entries(&base, "/mcp/admin", tok).await;
    let tool = |name: &str| {
        entries
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name} is missing from tools/list"))
            .clone()
    };
    let target_enum = |name: &str, prop: &str| -> Vec<String> {
        tool(name)["inputSchema"]["properties"][prop]["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("{name}.{prop} has no enum"))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };

    // The new tool exists, takes `action` and carries the class's two maps.
    let set = tool("lmgw__image_model_set");
    for prop in ["action", "model_id", "files", "args", "modes", "edit"] {
        assert!(
            set["inputSchema"]["properties"].get(prop).is_some(),
            "image_model_set.{prop} missing: {set}"
        );
    }
    assert_eq!(set["inputSchema"]["required"], json!(["action"]));
    assert_eq!(set["inputSchema"]["additionalProperties"], json!(false));
    // The description is the documentation: an agent has no other way to learn
    // that exactly one of the two loaders is required, or that per-request
    // parameters are not row fields.
    let desc = set["description"].as_str().unwrap();
    for teach in [
        "diffusion_model",
        "EXACTLY ONE",
        "RELATIVE",
        "sd_cpp_extra_args",
        "lmgw__local_model_test",
    ] {
        assert!(
            desc.contains(teach),
            "the description never mentions {teach}"
        );
    }

    for (name, prop) in [
        ("lmgw__local_model_get", "target"),
        ("lmgw__local_model_check", "target"),
        ("lmgw__local_model_plan", "target"),
        ("lmgw__local_model_test", "target"),
        ("lmgw__container", "target"),
        ("lmgw__models", "kind"),
        // `modelinfo::class_of` has resolved `image` since WP1; the tool that
        // reads a header out of a models dir has to offer the dir it reads.
        ("lmgw__model_inspect", "target"),
    ] {
        assert!(
            target_enum(name, prop).contains(&"image".to_string()),
            "{name}.{prop} does not offer 'image': {:?}",
            target_enum(name, prop)
        );
    }

    // …including the downloader, since WP3.
    for name in [
        "lmgw__gguf_files",
        "lmgw__hf_repo",
        "lmgw__hf_add",
        "lmgw__hf_set",
    ] {
        assert!(
            target_enum(name, "target").contains(&"image".to_string()),
            "{name} does not offer target=image: {:?}",
            target_enum(name, "target")
        );
    }
    // The tool kept its name, so its description has to say what the image
    // target actually returns — an agent has no other way to learn that a
    // `.safetensors` shows up in a listing called "gguf_files".
    let gguf = tool("lmgw__gguf_files");
    let desc = gguf["description"].as_str().unwrap();
    assert!(desc.contains(".safetensors"), "{desc}");
    assert!(desc.contains("target=image"), "{desc}");

    // The two recipe tools, and the chain their descriptions teach.
    let recipes = tool("lmgw__image_recipes");
    assert_eq!(recipes["inputSchema"]["properties"], json!({}));
    assert_eq!(recipes["inputSchema"]["required"], json!([]));
    let add = tool("lmgw__image_recipe_add");
    assert_eq!(add["inputSchema"]["required"], json!(["key"]));
    for prop in ["key", "diffusion_file"] {
        assert!(
            add["inputSchema"]["properties"].get(prop).is_some(),
            "image_recipe_add.{prop} missing: {add}"
        );
    }
    let add_desc = add["description"].as_str().unwrap();
    for teach in [
        "lmgw__image_recipes",
        "lmgw__hf_downloads",
        "lmgw__image_model_set",
        "lmgw__local_model_test",
        "does NOT create the row",
    ] {
        assert!(add_desc.contains(teach), "the chain never mentions {teach}");
    }

    // A write tool, so it is gone at read_only — and so is the add verb,
    // which queues transfers. Reading the recipe list is not a write.
    let (_state, ro_base) = setup(&mock.uri(), SelfAdmin::ReadOnly).await;
    let ro = tool_names(&ro_base, "/mcp/admin", tok).await;
    assert!(!ro.contains(&"lmgw__image_model_set".to_string()), "{ro:?}");
    assert!(
        !ro.contains(&"lmgw__image_recipe_add".to_string()),
        "queueing downloads is not a read: {ro:?}"
    );
    assert!(
        ro.contains(&"lmgw__image_recipes".to_string()),
        "listing the shipped recipes is: {ro:?}"
    );
    assert!(
        ro.contains(&"lmgw__local_model_get".to_string()),
        "the read tools must stay: {ro:?}"
    );
}

// ---------------------------------------------------------------------------
// A key's tool scope (key tool scope design)
// ---------------------------------------------------------------------------

/// A client key named `name` whose plaintext is `lmgw-<name>`, with its tool
/// scope set through the dashboard's own op — the path the Keys dialog takes.
async fn scoped_key(state: &SharedState, base: &Gw, name: &str, mode: &str, patterns: &str) {
    let id = store::insert_api_key(
        &state.db,
        name,
        &lmgw_core::config::hash_api_key(&format!("lmgw-{name}")),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let (status, body) = op(
        base,
        "key_set",
        json!({"id": id, "tool_scope_mode": mode, "tool_scope_patterns": patterns}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

/// One `/mcp` request as exactly one credential: `Some(token)` presents it,
/// `None` presents nothing at all — not the dashboard key [`Gw::client`]
/// carries.
async fn mcp_as(base: &Gw, token: Option<&str>, method_: &str, params: Value) -> Value {
    let client = base.anon();
    let bearer = |r: reqwest::RequestBuilder| match token {
        Some(t) => r.header("authorization", format!("Bearer {t}")),
        None => r,
    };
    let init = bearer(
        client
            .post(format!("{base}/mcp"))
            .header("accept", "application/json")
            .json(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                           "clientInfo": {"name": "scope-test", "version": "0"}}
            })),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(init.status().as_u16(), 200, "initialize");
    let sid = init.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    bearer(
        client
            .post(format!("{base}/mcp"))
            .header("accept", "application/json")
            .header("mcp-session-id", sid)
            .json(&json!({"jsonrpc": "2.0", "id": 2, "method": method_, "params": params})),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap_or(Value::Null)
}

async fn names_as(base: &Gw, token: Option<&str>) -> Vec<String> {
    let body = mcp_as(base, token, "tools/list", json!({})).await;
    let mut names: Vec<String> = body["result"]["tools"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect();
    names.sort();
    names
}

/// `/v1/responses` as exactly one credential, like [`mcp_as`].
async fn responses_as(base: &Gw, token: Option<&str>, body: Value) -> Value {
    let mut req = base.anon().post(format!("{base}/v1/responses")).json(&body);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    req.send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap_or(Value::Null)
}

/// The `mcp_list_tools` item a run reported for `label`.
fn list_item<'a>(resp: &'a Value, label: &str) -> &'a Value {
    resp["output"]
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|i| i["type"] == "mcp_list_tools" && i["server_label"] == label)
        })
        .unwrap_or_else(|| panic!("no mcp_list_tools item for '{label}': {resp}"))
}

fn listed_names(item: &Value) -> Vec<&str> {
    item["tools"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default()
}

/// The feature itself: a key sees exactly what its list admits, and a name
/// it kept from somewhere else is refused when called — while a caller with
/// no credential, and the owner, still see the whole aggregate.
#[tokio::test]
async fn a_client_key_sees_and_calls_only_what_its_tool_scope_admits_on_mcp() {
    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    let stub = mcp_stub().await;
    register_mcp(&state, &stub).await;
    scoped_key(&state, &base, "ci", "allow", "stub__echo\ndocs__resolve").await;

    assert_eq!(
        names_as(&base, Some("lmgw-ci")).await,
        vec!["docs__resolve", "stub__echo"]
    );
    let everything = names_as(&base, None).await;
    assert!(
        everything.contains(&"stub__ping".to_string()),
        "{everything:?}"
    );
    assert!(
        everything.contains(&"docs__query".to_string()),
        "{everything:?}"
    );
    assert_eq!(
        names_as(&base, Some(&base.key)).await,
        everything,
        "an owner key is not scoped"
    );

    let refused = mcp_as(
        &base,
        Some("lmgw-ci"),
        "tools/call",
        json!({"name": "stub__ping", "arguments": {}}),
    )
    .await;
    let msg = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("stub__ping") && msg.contains("tool scope of key 'ci'"),
        "the refusal names the tool and the key: {refused}"
    );
    let ran = mcp_as(
        &base,
        Some("lmgw-ci"),
        "tools/call",
        json!({"name": "stub__echo", "arguments": {}}),
    )
    .await;
    assert_eq!(ran["result"]["content"][0]["text"], "echoed", "{ran}");

    // Deny is the other reading of the same list.
    let id = state
        .snapshot()
        .api_keys
        .iter()
        .find(|k| k.name == "ci")
        .unwrap()
        .id;
    op(
        &base,
        "key_set",
        json!({"id": id, "tool_scope_mode": "deny", "tool_scope_patterns": "docs__*"}),
    )
    .await;
    assert_eq!(
        names_as(&base, Some("lmgw-ci")).await,
        vec![
            "kb__list",
            "kb__read",
            "kb__search",
            "stub__echo",
            "stub__ping"
        ]
    );
}

/// The same list binds a `/v1/responses` run, or the scope would be one
/// request shape away from decorative: the server's tools are narrowed, and a
/// toolset the list admits nothing of says why.
#[tokio::test]
async fn a_client_keys_tool_scope_binds_a_responses_run_too() {
    let mock = MockServer::start().await;
    mount_json_sequence(&mock, vec![json_reply(None, "nothing to do")]).await;
    let (state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    let stub = mcp_stub().await;
    register_mcp(&state, &stub).await;
    scoped_key(&state, &base, "ci", "allow", "stub__echo").await;

    let resp = responses_as(
        &base,
        Some("lmgw-ci"),
        json!({
            "model": "my-model",
            "input": "echo hi",
            "tools": [
                {"type": "mcp", "server_label": "stub", "require_approval": "never"},
                {"type": "mcp", "server_label": "docs", "require_approval": "never"},
            ],
        }),
    )
    .await;
    assert_eq!(listed_names(list_item(&resp, "stub")), vec!["stub__echo"]);
    let docs = list_item(&resp, "docs");
    assert!(
        docs["error"]
            .as_str()
            .is_some_and(|e| e.contains("tool scope of key 'ci'")),
        "{docs}"
    );
    // And the model was offered exactly that one tool.
    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    let offered: Vec<&str> = sent["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t["function"]["name"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(offered, vec!["stub__echo"], "{sent:#}");
}

/// `/mcp/admin` is behind an owner credential; attaching the `lmgw` label to
/// a `/v1/responses` run must not be the side door around it. Not for a key
/// whose list is `all`, not for a key whose list names `lmgw__*`, and not for
/// a caller with no credential — only for an owner.
#[tokio::test]
async fn the_self_admin_toolset_needs_an_owner_credential_on_responses() {
    let mock = MockServer::start().await;
    for _ in 0..4 {
        mount_json_sequence(&mock, vec![json_reply(None, "ok")]).await;
    }
    let (state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    scoped_key(&state, &base, "open", "all", "").await;
    scoped_key(&state, &base, "greedy", "allow", "lmgw__*").await;
    let body = json!({
        "model": "my-model",
        "input": "status?",
        "tools": [{"type": "mcp", "server_label": "lmgw", "require_approval": "never"}],
    });

    for token in [Some("lmgw-open"), Some("lmgw-greedy"), None] {
        let resp = responses_as(&base, token, body.clone()).await;
        let item = list_item(&resp, "lmgw");
        assert!(
            item["error"]
                .as_str()
                .is_some_and(|e| e.contains("owner credential")),
            "{token:?}: {item}"
        );
        assert!(listed_names(item).is_empty(), "{token:?}: {item}");
    }

    let resp = responses_as(&base, Some(&base.key), body).await;
    let item = list_item(&resp, "lmgw");
    assert!(item["error"].is_null(), "{item}");
    assert!(
        listed_names(item).contains(&"lmgw__status"),
        "the owner still attaches it: {item}"
    );
}

/// A server the key can never use is answered like a label that does not
/// exist — not with its tool count, its disabled flag or its connection
/// error — and the list of what *is* available leaves it out.
#[tokio::test]
async fn a_scoped_key_cannot_map_the_servers_it_may_not_use() {
    let mock = MockServer::start().await;
    mount_json_sequence(&mock, vec![json_reply(None, "nothing to do")]).await;
    let (state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    let stub = mcp_stub().await;
    register_mcp(&state, &stub).await;
    scoped_key(&state, &base, "narrow", "allow", "docs__*").await;

    let resp = responses_as(
        &base,
        Some("lmgw-narrow"),
        json!({
            "model": "my-model",
            "input": "hi",
            "tools": [{"type": "mcp", "server_label": "stub", "require_approval": "never"}],
        }),
    )
    .await;
    let err = list_item(&resp, "stub")["error"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        err.contains("no MCP server with label 'stub'") && err.contains("(available: docs)"),
        "{err}"
    );
}

/// The run-time half of the scope. A resumed approval executes a name an
/// earlier request stored, and a long run outlives an owner's edit, so the
/// run's executor reads the key's scope at each call — and a key deleted
/// mid-run reaches nothing.
#[tokio::test]
async fn the_run_executor_reads_the_keys_scope_at_each_call() {
    use lmgw_core::agent::ToolExecutor;
    use lmgw_core::config::ApiKeyKind;
    use lmgw_core::mcp::exec::McpExecutor;
    use lmgw_core::mcp::scope::ScopedExecutor;
    use lmgw_core::principal::Principal;
    use lmgw_core::proxy::RequestCtx;

    let mock = MockServer::start().await;
    let (state, base) = setup(&mock.uri(), SelfAdmin::Full).await;
    let stub = mcp_stub().await;
    register_mcp(&state, &stub).await;
    scoped_key(&state, &base, "ci", "allow", "docs__*").await;
    // What a run's resolve step does first: connect and list the server, so
    // the executor has a route for the name.
    state.mcp.list_tools(&state.snapshot()).await;
    let id = state
        .snapshot()
        .api_keys
        .iter()
        .find(|k| k.name == "ci")
        .unwrap()
        .id;
    let ctx = RequestCtx {
        principal: Principal::Key {
            id,
            name: "ci".into(),
            kind: ApiKeyKind::Key,
            agent_id: None,
            fingerprint: String::new(),
        },
        client_key: Some("ci".into()),
        ..RequestCtx::default()
    };
    let exec = ScopedExecutor::new(
        McpExecutor::new(state.clone(), ctx.clone()),
        state.clone(),
        ctx,
    );
    let call = || async {
        let out = exec.call("stub__echo", &json!({})).await;
        let (text, _) = lmgw_core::ir::flatten_tool_result(&out.blocks);
        (out.is_error, text)
    };

    let (refused, text) = call().await;
    assert!(refused && text.contains("tool scope of key 'ci'"), "{text}");

    op(
        &base,
        "key_set",
        json!({"id": id, "tool_scope_patterns": "docs__*\nstub__*"}),
    )
    .await;
    let (refused, text) = call().await;
    assert!(
        !refused && text.contains("echoed"),
        "widened mid-run: {text}"
    );

    let (status, body) = op(&base, "key_delete", json!({"id": id})).await;
    assert_eq!(status, 200, "{body}");
    let (refused, text) = call().await;
    assert!(
        refused && text.contains("no longer exists"),
        "a deleted key reaches nothing: {text}"
    );
}
