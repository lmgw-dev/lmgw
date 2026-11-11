//! The audit log (headless server design §5.8, §12's audit group): one
//! entry per non-`GET` `Admin` request, per writing `lmgw__*` call, per
//! sign-in event, per offline command-line write and per switch; none on a
//! desktop with accounts off; a retention change written before it prunes;
//! every `Admin` write route reaching the gate's writer; no value of a
//! redacted field in any entry.

use serde_json::{json, Value};

pub use crate::accounts::{op, op_as, sign_in, turn_on, PASSWORD};
pub use crate::common::{serve, Gw};
use lmgw_core::state::SharedState;

mod activity;
mod events;
mod flood;
mod off;
mod redaction;
mod retention;
mod route_walk;
mod writes;

/// The newest `count` entries, newest first (`audit_list`).
pub async fn entries(gw: &Gw, count: u32) -> Vec<Value> {
    let (status, page) = op(gw, "audit_list", json!({ "count": count })).await;
    assert_eq!(status, 200, "{page}");
    page["entries"].as_array().unwrap().clone()
}

/// Every entry whose action is `action`, newest first.
pub async fn with_action(gw: &Gw, action: &str) -> Vec<Value> {
    let (_, page) = op(gw, "audit_list", json!({ "count": 100000 })).await;
    page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == action)
        .cloned()
        .collect()
}

/// The rows in `audit_log`.
pub async fn count(state: &SharedState) -> i64 {
    crate::accounts::rows(state, "audit_log").await
}

/// Every column of every row in `audit_log`, as one text.
pub async fn whole_log(state: &SharedState) -> String {
    let rows: Vec<(String, String, String, String, String)> =
        sqlx::query_as("SELECT principal, via, peer, action, target FROM audit_log ORDER BY id")
            .fetch_all(&state.db)
            .await
            .unwrap();
    format!("{rows:?}")
}

/// Settings saved straight to the store and published, as a test sets up
/// what it needs.
pub async fn with_settings(state: &SharedState, f: impl FnOnce(&mut lmgw_core::config::Settings)) {
    let mut s = state.snapshot().settings.clone();
    f(&mut s);
    lmgw_core::store::save_settings(&state.db, &s)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// `tools/call` of `name` on `/mcp/admin` with the dashboard's bearer:
/// whether it answered an error.
pub async fn mcp_call(gw: &Gw, name: &str, args: Value) -> bool {
    let rpc = |body: Value, sid: Option<String>| {
        let mut req = gw
            .client()
            .post(format!("{gw}/mcp/admin"))
            .header("accept", "application/json, text/event-stream")
            .json(&body);
        if let Some(sid) = sid {
            req = req.header("mcp-session-id", sid);
        }
        req.send()
    };
    let resp = rpc(
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                        "clientInfo": { "name": "audit-test", "version": "0" } }
        }),
        None,
    )
    .await
    .unwrap();
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let resp = rpc(
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": name, "arguments": args}}),
        sid,
    )
    .await
    .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"].is_null(), "{name}: {body}");
    body["result"]["isError"] == true
}
