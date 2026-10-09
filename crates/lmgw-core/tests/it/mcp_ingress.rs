//! Northbound MCP spike (§17.0): the hand-rolled `/mcp` Streamable HTTP server
//! completes `initialize → tools/list → tools/call` and honours the §6 MUST-list.
//!
//! This stands in for an interactive Claude Code / Cursor session (which must be
//! the manual sign-off): it asserts the wire behaviours a real client depends on.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use lmgw_core::config::{McpServer, McpTransport, Snapshot};
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use serde_json::{json, Value};

/// Spin the full app router on a real port (auth is off by default in tests).
async fn serve() -> String {
    let state = AppState::init_for_tests().await.unwrap();
    serve_with(state).await
}

/// Same, but with a caller-provided state (so a test can pre-seed an aggregate).
async fn serve_with(state: SharedState) -> String {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// A minimal stdio `McpServer` definition for tests (prefix-only matters here).
fn test_server(id: i64, name: &str, prefix: &str) -> McpServer {
    McpServer {
        id,
        name: name.into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("true".into()),
        args: vec![],
        env: vec![],
        cwd: None,
        container_image: None,
        extra_run_args: vec![],
        url: None,
        headers: vec![],
        tool_prefix: prefix.into(),
        timeout_ms: 1000,
        autostart: false,
        idle_seconds: 0,
        allow_sampling: false,
        sampling_alias: None,
        agent_id: None,
        device_key_id: None,
    }
}

/// Seed `state` with one Ready server `id` (prefix `prefix`) exposing the named
/// tools, so the northbound aggregate is populated without a live MCP server.
async fn seed_aggregate(state: &SharedState, id: i64, name: &str, prefix: &str, tools: &[&str]) {
    let mut snap = Snapshot::default();
    let mut servers = HashMap::new();
    servers.insert(id, test_server(id, name, prefix));
    snap.mcp_servers = servers;
    state.set_snapshot_for_tests(snap);
    let seed: Vec<(String, Value)> = tools
        .iter()
        .map(|t| {
            (
                (*t).to_string(),
                json!({ "type": "object", "properties": {} }),
            )
        })
        .collect();
    state.mcp.seed_ready_conn_for_tests(id, seed).await;
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// POST a JSON-RPC body with the standard Accept header and an optional session.
async fn post(base: &str, session: Option<&str>, body: Value) -> (u16, Option<String>, Value) {
    let mut req = client()
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(sid) = session {
        req = req.header("mcp-session-id", sid);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let text = resp.text().await.unwrap_or_default();
    let val = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, sid, val)
}

async fn initialize(base: &str) -> String {
    let (status, sid, body) = post(
        base,
        None,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "spike-test", "version": "0" }
            }
        }),
    )
    .await;
    assert_eq!(status, 200, "initialize should 200: {body}");
    assert!(
        body["result"]["protocolVersion"].is_string(),
        "missing protocolVersion: {body}"
    );
    assert!(
        body["result"]["capabilities"]["tools"].is_object(),
        "missing tools capability: {body}"
    );
    sid.expect("initialize must return an MCP-Session-Id header")
}

#[tokio::test]
async fn full_handshake_initialize_list_aggregate() {
    // Seed a Ready server `time` (prefix `time`) exposing one `now` tool, so the
    // northbound aggregate is populated through the real McpManager path.
    let state = AppState::init_for_tests().await.unwrap();
    seed_aggregate(&state, 1, "time", "time", &["now"]).await;
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    // The client's `notifications/initialized` (no id) → 202, no body.
    let resp = client()
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &sid)
        .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    // tools/list surfaces the aggregate: the upstream `now` tool exposed under
    // the server's `time__` prefix, schema forwarded verbatim.
    let (status, _, body) = post(
        &base,
        Some(&sid),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    assert_eq!(status, 200);
    let tools = body["result"]["tools"].as_array().unwrap();
    let now = tools
        .iter()
        .find(|t| t["name"] == "time__now")
        .unwrap_or_else(|| panic!("expected time__now in aggregate: {body}"));
    assert_eq!(now["inputSchema"]["type"], "object");

    // ping → empty result.
    let (status, _, body) = post(
        &base,
        Some(&sid),
        json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" }),
    )
    .await;
    assert_eq!(status, 200);
    assert!(body["result"].is_object());
}

#[tokio::test]
async fn empty_aggregate_when_no_servers() {
    // No MCP servers configured ⇒ the aggregate contributes nothing, and
    // tools/call for any tool is a JSON-RPC method-not-found (-32601, §14).
    // lmgw's own built-in toolsets are filtered out here so this test keeps
    // asserting exactly what it always did — the *southbound* aggregate: the
    // `lmgw__*` self-admin tools (§20) are a separate plane, and the `docs__*`
    // quickdoc tools are served on this one but are not southbound.
    let base = serve().await;
    let sid = initialize(&base).await;

    let (status, _, body) = post(
        &base,
        Some(&sid),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    assert_eq!(status, 200);
    let southbound: Vec<&Value> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| {
            let n = t["name"].as_str().unwrap_or("");
            !n.starts_with("lmgw__") && !n.starts_with("docs__") && !n.starts_with("kb__")
        })
        .collect();
    assert_eq!(southbound.len(), 0, "unexpected southbound tools: {body}");

    let (status, _, body) = post(
        &base,
        Some(&sid),
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "anything", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        body["error"]["code"], -32601,
        "unknown tool → -32601: {body}"
    );
}

#[tokio::test]
async fn must_rules_session_and_errors() {
    let base = serve().await;
    let sid = initialize(&base).await;

    // Missing session id on a non-initialize request → 400.
    let (status, _, _) = post(
        &base,
        None,
        json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/list" }),
    )
    .await;
    assert_eq!(status, 400, "missing session must be 400");

    // Unknown session id → 404.
    let (status, _, _) = post(
        &base,
        Some("deadbeef"),
        json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/list" }),
    )
    .await;
    assert_eq!(status, 404, "unknown session must be 404");

    // Unknown method → JSON-RPC method-not-found (-32601), HTTP 200.
    let (status, _, body) = post(
        &base,
        Some(&sid),
        json!({ "jsonrpc": "2.0", "id": 10, "method": "does/not/exist" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["error"]["code"], -32601);

    // Unknown tool → JSON-RPC tool/method-not-found (-32601, §14).
    let (status, _, body) = post(
        &base,
        Some(&sid),
        json!({
            "jsonrpc": "2.0", "id": 11, "method": "tools/call",
            "params": { "name": "nope", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["error"]["code"], -32601);
}

#[tokio::test]
async fn must_rules_transport() {
    let base = serve().await;

    // GET /mcp without a session → 400 (the §6 missing-session error). M5 turned
    // GET into the server→client SSE channel, so a session-less GET is a clean
    // 4xx now, no longer the M0 `405` placeholder. (A *valid*-session GET opens a
    // stream — see `get_sse_streams_tools_list_changed`.)
    let resp = client().get(format!("{base}/mcp")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 400, "session-less GET must be 400");

    // GET /mcp with an unknown session → 404 (matches POST's session error).
    let resp = client()
        .get(format!("{base}/mcp"))
        .header("mcp-session-id", "deadbeef")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "unknown-session GET must be 404"
    );

    // JSON-RPC batching was removed in 2025-06-18 → reject arrays with 400.
    let resp = client()
        .post(format!("{base}/mcp"))
        .header("accept", "application/json")
        .json(&json!([
            { "jsonrpc": "2.0", "id": 1, "method": "ping" }
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400, "batch must be rejected");

    // Non-localhost Origin → 403 (DNS-rebinding defense), even with auth off.
    let resp = client()
        .post(format!("{base}/mcp"))
        .header("accept", "application/json")
        .header("origin", "http://evil.example")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403, "cross-origin must be 403");

    // A localhost Origin **on the gateway's own port** is allowed through to
    // dispatch (origins §4.2): the port is part of an origin, and
    // `http://127.0.0.1:9999` is another process on this box. The port here is
    // `bind_addr`'s default, which is what the guard compares against — this
    // suite serves on an ephemeral one.
    let resp = client()
        .post(format!("{base}/mcp"))
        .header("accept", "application/json")
        .header("origin", "http://127.0.0.1:8787")
        .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
}

#[tokio::test]
async fn delete_terminates_session() {
    let base = serve().await;
    let sid = initialize(&base).await;

    // tools/list works while the session lives.
    let (status, _, _) = post(
        &base,
        Some(&sid),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    assert_eq!(status, 200);

    // DELETE terminates it → 204.
    let resp = client()
        .delete(format!("{base}/mcp"))
        .header("mcp-session-id", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    // Reusing the terminated session → 404.
    let (status, _, _) = post(
        &base,
        Some(&sid),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
    )
    .await;
    assert_eq!(status, 404);
}

/// M5 §6/§8: a valid-session `GET /mcp` opens the server→client SSE stream and
/// pushes a `notifications/tools/list_changed` JSON-RPC notification (no `id`)
/// when the aggregate's composition changes. Here the change is triggered via the
/// manager's test hook (the real triggers are connect/disconnect/reap +
/// `on_tool_list_changed`).
#[tokio::test]
async fn get_sse_streams_tools_list_changed() {
    let state = AppState::init_for_tests().await.unwrap();
    let mcp = Arc::clone(&state);
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    // Open the GET SSE stream with the valid session; it must be a 200
    // text/event-stream.
    let resp = client()
        .get(format!("{base}/mcp"))
        .header("accept", "text/event-stream")
        .header("mcp-session-id", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "valid-session GET opens a stream"
    );
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ctype.contains("text/event-stream"),
        "GET stream must be SSE, got {ctype}"
    );

    // Fire a tools-changed nudge shortly after we start reading the body, so the
    // frame lands on the open subscriber.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        mcp.mcp.notify_tools_changed_for_tests();
    });

    // Read SSE bytes until we see the notification or time out. The frame's
    // `data:` carries the hand-built JSON-RPC notification.
    let mut body = resp.bytes_stream();
    let saw = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut buf = String::new();
        while let Some(chunk) = body.next().await {
            let bytes = chunk.unwrap();
            buf.push_str(&String::from_utf8_lossy(&bytes));
            if buf.contains("notifications/tools/list_changed") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(
        saw,
        "GET SSE must push a notifications/tools/list_changed frame on change"
    );
}

/// §10 fixes: an MCP `tools/call` logs a `request_logs` row (so it shows in the
/// unified feed) but is mapped non-ok on failure (fix 1) and does **not** move
/// the token / request / error-rate aggregates (fix 2).
#[tokio::test]
async fn mcp_tool_call_logs_but_is_excluded_from_stats() {
    let state = AppState::init_for_tests().await.unwrap();
    // A Ready-but-peerless seeded server: routing succeeds (the tool is in the
    // reverse map) but the call fails `NotConnected` → a *non-ok* logged row,
    // exercising fix 1 without needing a live MCP server.
    seed_aggregate(&state, 1, "time", "time", &["now"]).await;
    let db = state.db.clone();
    let tele = Arc::clone(&state);
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    // Baseline: only LLM traffic counts, and there's none yet.
    let before = tele.telemetry.stats();
    assert_eq!(before.total_requests, 0);

    // Call the exposed tool — routes to the seeded server, fails NotConnected.
    let (status, _, body) = post(
        &base,
        Some(&sid),
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "time__now", "arguments": {} }
        }),
    )
    .await;
    // HTTP 200 with a JSON-RPC error object (§14 — the surface is JSON-RPC).
    assert_eq!(status, 200);
    assert!(
        body["error"].is_object(),
        "expected a JSON-RPC error: {body}"
    );

    // A request_logs row was written for the feed, ingress_proto = 'mcp'…
    let rows = lmgw_core::store::query_logs(
        &db,
        &lmgw_core::store::LogFilter {
            limit: 50,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mcp_row = rows
        .iter()
        .find(|r| r.ingress_proto == "mcp")
        .expect("an mcp tool-call row should be logged");
    // …mapped NON-ok (fix 1): status >= 400 and error_kind set, so the dashboard
    // renders it red instead of green-on-HTTP-200.
    assert!(mcp_row.status >= 400, "mcp failure must log non-200 status");
    assert!(
        mcp_row.error_kind.is_some(),
        "mcp failure must set error_kind"
    );
    assert_eq!(mcp_row.mcp_tool.as_deref(), Some("time__now"));
    assert!(mcp_row.prompt_tokens.is_none() && mcp_row.completion_tokens.is_none());

    // …but the stats aggregate is unchanged (fix 2): the 'mcp' row didn't bump
    // total_requests / total_errors / the trailing-minute window.
    let after = tele.telemetry.stats();
    assert_eq!(after.total_requests, 0, "mcp row must not count in stats");
    assert_eq!(after.total_errors, 0);
    assert_eq!(after.req_last_minute, 0);
    assert_eq!(after.err_last_minute, 0);
}
