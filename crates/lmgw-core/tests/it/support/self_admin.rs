//! The self-admin tools' test harness: a gateway with self-admin at `full`,
//! an MCP session on `/mcp/admin`, and the read-back every tool-text test
//! ends with. Shared by the suites that read a tool's text into its type
//! (`self_admin_wire`, `self_admin_docs`).

use lmgw_core::config::{SelfAdmin, Settings};
use lmgw_core::state::SharedState;
use lmgw_core::store;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use crate::common::{serve, Gw};

pub const TOKEN: &str = "admin-tok";

/// A gateway with self-admin at `full`, and an MCP session on `/mcp/admin`.
pub struct Admin {
    gw: Gw,
    sid: String,
}

impl Admin {
    pub async fn new(state: &SharedState) -> Self {
        let settings = Settings {
            self_admin: SelfAdmin::Full,
            ..state.snapshot().settings.clone()
        };
        store::save_settings(&state.db, &settings).await.unwrap();
        state.reload_snapshot().await.unwrap();
        lmgw_core::agents::token::set_owner_key(
            state,
            lmgw_core::agents::token::OWNER_SELF_ADMIN,
            TOKEN,
            true,
        )
        .await
        .unwrap();
        let gw = serve(state.clone()).await;
        let init = rpc(
            &gw,
            None,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                            "clientInfo": { "name": "self-admin-wire", "version": "0" } }
            }),
        )
        .await;
        Self {
            gw,
            sid: init.0.expect("initialize returns a session id"),
        }
    }

    /// A tool's result: whether it failed, and its text.
    pub async fn call(&self, tool: &str, args: Value) -> (bool, String) {
        let (_, body) = rpc(
            &self.gw,
            Some(&self.sid),
            json!({
                "jsonrpc": "2.0", "id": 9, "method": "tools/call",
                "params": { "name": tool, "arguments": args }
            }),
        )
        .await;
        assert!(body["error"].is_null(), "{tool}: {body}");
        let result = &body["result"];
        (
            result["isError"] == Value::Bool(true),
            result["content"][0]["text"].as_str().unwrap().to_string(),
        )
    }

    /// A tool's text, which has to be a success.
    pub async fn text(&self, tool: &str, args: Value) -> String {
        let (failed, text) = self.call(tool, args).await;
        assert!(!failed, "{tool} failed: {text}");
        text
    }
}

pub async fn rpc(gw: &Gw, session: Option<&str>, body: Value) -> (Option<String>, Value) {
    let mut req = reqwest::Client::new()
        .post(format!("{gw}/mcp/admin"))
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {TOKEN}"))
        .json(&body);
    if let Some(sid) = session {
        req = req.header("mcp-session-id", sid);
    }
    let resp = req.send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let text = resp.text().await.unwrap();
    (sid, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// Read a tool's text into `T` and write it out again: the same text.
pub fn read_back<T: DeserializeOwned + Serialize>(text: &str) -> T {
    let typed: T = serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"));
    let again = serde_json::to_string_pretty(&serde_json::to_value(&typed).unwrap()).unwrap();
    assert_eq!(again, text, "writing the type back changed the tool's text");
    typed
}

pub fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}
