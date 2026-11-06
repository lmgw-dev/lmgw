//! A client key's reach over the media tools on `/mcp` (design §3.7,
//! review finding 8): its tool scope lists them or not, and its alias scope
//! narrows each tool's `model` enum to the models it may use.

use serde_json::{json, Value};
use wiremock::MockServer;

use lmgw_core::state::SharedState;

use super::{gateway, image_row, submitted, Z_IMAGE};
use crate::common::Gw;

/// A client key `name` with `policy` (`key_set`'s fields): its token.
pub(super) async fn key(state: &SharedState, gw: &Gw, name: &str, policy: Value) -> String {
    let id = lmgw_core::store::insert_api_key(
        &state.db,
        name,
        &lmgw_core::config::hash_api_key(&format!("lmgw-{name}")),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let mut body = policy;
    body["id"] = json!(id);
    let r = gw
        .client()
        .post(format!("{gw}/api/op/key_set"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    format!("lmgw-{name}")
}

/// One `/mcp` request as `token`.
pub(super) async fn mcp_as(gw: &Gw, token: &str, method: &str, params: Value) -> Value {
    let client = gw.anon();
    let init = client
        .post(format!("{gw}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "scope-test", "version": "0"}}
        }))
        .send()
        .await
        .unwrap();
    let sid = init.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    let _ = init.text().await;
    let text = client
        .post(format!("{gw}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {token}"))
        .header("mcp-session-id", sid)
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": method, "params": params}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("/mcp is not JSON ({e}): {text}"))
}

fn names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("{list}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_kb_scoped_key_reaches_no_media_tool() {
    let sd = MockServer::start().await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let token = key(
        &state,
        &gw,
        "kb-only",
        json!({"tool_scope_mode": "allow", "tool_scope_patterns": "kb__*"}),
    )
    .await;
    let listed = names(&mcp_as(&gw, &token, "tools/list", json!({})).await);
    let call = mcp_as(
        &gw,
        &token,
        "tools/call",
        json!({"name": "image__generate",
               "arguments": {"model": "image/z", "prompt": "a red square"}}),
    )
    .await;
    assert!(
        !listed
            .iter()
            .any(|n| n.starts_with("image__") || n.starts_with("audio__"))
            && call
                .get("result")
                .is_none_or(|r| r["isError"] == json!(true))
            && submitted(&sd).await.is_empty(),
        "listed {listed:?}, called {call}"
    );
}

#[tokio::test]
async fn a_key_s_alias_scope_narrows_the_models_offered() {
    let sd = MockServer::start().await;
    let rows = [
        image_row("z", Z_IMAGE, false),
        image_row("y", Z_IMAGE, false),
    ];
    let (state, gw) = gateway(&sd, &rows).await;
    let token = key(
        &state,
        &gw,
        "z-only",
        json!({"scope_mode": "allow", "scope_patterns": "image/z"}),
    )
    .await;
    let list = mcp_as(&gw, &token, "tools/list", json!({})).await;
    let tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "image__generate")
        .cloned()
        .unwrap_or_else(|| panic!("{list}"));
    assert_eq!(
        tool["inputSchema"]["properties"]["model"]["enum"],
        json!(["image/z"])
    );
    assert!(!tool["description"].as_str().unwrap().contains("image/y"));

    let call = mcp_as(
        &gw,
        &token,
        "tools/call",
        json!({"name": "image__generate",
               "arguments": {"model": "image/y", "prompt": "a red square"}}),
    )
    .await;
    assert_eq!(call["result"]["isError"], json!(true), "{call}");
    assert!(submitted(&sd).await.is_empty());
}
