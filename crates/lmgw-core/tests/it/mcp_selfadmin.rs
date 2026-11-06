//! Built-in self-admin tools (§20) end-to-end over the real northbound
//! `/mcp/admin`.
//!
//! These drive the same wire an agent does — `initialize`, `tools/list`,
//! `tools/call` — because the parts worth protecting are the *seams*: the mode
//! gate, the reserved namespace, the audit row, and whether a mutation actually
//! reaches the database and the live snapshot.
//!
//! Since principals §3.7 the plane lives on its own route behind an **owner
//! credential** — the `owner:self-admin` key — so the harness installs one
//! with a known plaintext and presents it. That the route refuses a disabled
//! row, and that the tools are gone from `/mcp`, is covered in
//! `mcp_admin_plane.rs`.

use lmgw_core::config::{SelfAdmin, Settings};
use lmgw_core::gguf::synth;
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

mod audit;
mod catalog;
mod gate;
mod guards;
mod hf_downloads;
mod hold;
mod image;
mod local_flags;
mod local_models;
mod mutations;
mod profiles;
mod reads;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn serve_with(state: SharedState) -> String {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { crate::common::serve_on(listener, app).await });
    base
}

/// The credential these tests present: the plaintext of the enabled
/// `owner:self-admin` key the harness installs. Any value works; what matters
/// is that the route admits an owner key and nothing else.
const ADMIN_TOKEN: &str = "test-admin-token";

/// A gateway with `self_admin` persisted at `mode`. Written through the store
/// (not `set_snapshot_for_tests`) because every mutating tool calls
/// `reload_snapshot()`, which would discard a hand-swapped snapshot.
async fn state_with_mode(mode: SelfAdmin) -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    let settings = Settings {
        self_admin: mode,
        ..Settings::default()
    };
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    lmgw_core::agents::token::set_owner_key(
        &state,
        lmgw_core::agents::token::OWNER_SELF_ADMIN,
        ADMIN_TOKEN,
        true,
    )
    .await
    .unwrap();
    state
}

async fn post(base: &str, session: Option<&str>, body: Value) -> (u16, Option<String>, Value) {
    let mut req = reqwest::Client::new()
        .post(format!("{base}/mcp/admin"))
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {ADMIN_TOKEN}"))
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
    (
        status,
        sid,
        serde_json::from_str(&text).unwrap_or(Value::Null),
    )
}

async fn initialize(base: &str) -> String {
    let (status, sid, body) = post(
        base,
        None,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                        "clientInfo": { "name": "selfadmin-test", "version": "0" } }
        }),
    )
    .await;
    assert_eq!(status, 200, "initialize should 200: {body}");
    sid.expect("initialize must return a session id")
}

async fn list_tool_names(base: &str, sid: &str) -> Vec<String> {
    list_tools(base, sid)
        .await
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// [`list_tool_names`], keeping each tool's full `tools/list` entry
/// (`inputSchema` included) for a test that inspects a schema rather than
/// just checking a name is present.
async fn list_tools(base: &str, sid: &str) -> Vec<Value> {
    let (status, _, body) = post(
        base,
        Some(sid),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    assert_eq!(status, 200);
    body["result"]["tools"].as_array().unwrap().clone()
}

fn tool<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("{name} missing from tools/list"))
}

/// Call a tool and return its `result` object (JSON-RPC errors are asserted
/// away — a `-32601` here means the dispatcher, not the tool, refused).
async fn call(base: &str, sid: &str, name: &str, args: Value) -> Value {
    let (status, _, body) = post(
        base,
        Some(sid),
        json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        }),
    )
    .await;
    assert_eq!(status, 200, "tools/call should 200: {body}");
    assert!(
        body["error"].is_null(),
        "unexpected JSON-RPC error for {name}: {body}"
    );
    body["result"].clone()
}

fn is_error(result: &Value) -> bool {
    result["isError"] == Value::Bool(true)
}

fn text_of(result: &Value) -> String {
    result["content"][0]["text"].as_str().unwrap().to_string()
}

/// Parse a successful tool result's JSON payload back out of its text block.
fn payload(result: &Value) -> Value {
    assert!(!is_error(result), "tool failed: {}", text_of(result));
    serde_json::from_str(&text_of(result)).expect("tool payload should be JSON")
}
