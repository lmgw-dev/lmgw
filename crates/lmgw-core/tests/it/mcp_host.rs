//! A paired device's MCP host link, `GET /mcp/host` (client-apps design §5,
//! §9's host-link and reach lists), driven by a fake device over a real
//! WebSocket through the router and the gate.
//!
//! - `link`: `initialize` and listing, the row's status, a call with
//!   `_meta`, a takeover, the refusals before the 101, a size overrun, a
//!   revocation's 4003, sampling refused, the device's own `list_changed`;
//! - `cancel`: `notifications/cancelled` on a timeout, a turn's cancel and a
//!   link lmgw closes, and a link dropped mid-call reported as abandoned;
//! - `offline`: an offline device's label fails at once, and the turn still
//!   answers;
//! - `reach`: who reaches a device-hosted label (L16) on `/mcp`, discovery,
//!   `/v1/responses`, realtime and the Chat, and an `all` device writing
//!   another device's label (review W3-9);
//! - `close`: how lmgw closes a link and says why — a key deleted 4003, the
//!   grant cleared or the row off 1000, a frame overrun, a missed pong —
//!   and two links opening at once;
//! - `offered`: a run calls only the tools it offered the model, and an
//!   agent reaches a device label only through its manifest;
//! - `names`: a name is a device's by the server that serves it, labels and
//!   prefixes whose namespaces run into each other refused, and an `all`
//!   scope narrowed for a foreign device row only.

use serde_json::{json, Value};

use crate::device_chat::{op, pair, Device};
use crate::realtime_chat_thread::{world, World};

mod cancel;
mod close;
mod device;
mod link;
mod names;
mod offered;
mod offline;
mod reach;

pub(crate) use device::{tool, FakeDevice};

pub(crate) type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A world with device `desktop` paired, hosting tools under `desktop`.
pub(crate) async fn host_world() -> (World, Device) {
    let w = world(|_| {}).await;
    let d = pair(&w, "desktop", json!({ "hosts_label": "desktop" })).await;
    (w, d)
}

/// The device row of key `key_id`.
pub(crate) fn device_row(w: &World, key_id: i64) -> lmgw_core::config::McpServer {
    w.state
        .snapshot()
        .mcp_servers
        .values()
        .find(|s| s.device_key_id == Some(key_id))
        .cloned()
        .expect("the grant made the device's row")
}

/// `desktop` connected with `tools`, its row `Ready`.
pub(crate) async fn linked(w: &World, d: &Device, tools: &[&str]) -> FakeDevice {
    let dev = FakeDevice::connect(&w.addr(), &d.key, tools.iter().map(|t| tool(t)).collect()).await;
    let id = device_row(w, d.id).id;
    crate::common::patience::until_async("the device row is ready", || async {
        w.state.mcp.is_ready(id).await
    })
    .await;
    dev
}

/// The row's status view, as the MCP page reads it.
pub(crate) async fn status(w: &World, key_id: i64) -> Value {
    let (s, v) = crate::device_chat::get(w, &w.gw.client(), "/api/mcp-servers").await;
    assert_eq!(s, 200, "{v}");
    v["mcp_servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["device_key_id"] == key_id)
        .cloned()
        .unwrap_or_else(|| panic!("no device row in {v}"))
}

/// An `/mcp` session as `client`: its id.
pub(crate) async fn mcp_session(w: &World, client: &reqwest::Client) -> String {
    let resp = client
        .post(format!("{}/mcp", w.gw))
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                        "clientInfo": { "name": "test", "version": "0" } }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sid = resp.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    let _ = resp.text().await;
    sid
}

/// One `/mcp` JSON-RPC call as `client` on session `sid`: its answer.
pub(crate) async fn mcp_rpc(
    w: &World,
    client: &reqwest::Client,
    sid: &str,
    method: &str,
    params: Value,
) -> Value {
    let resp = client
        .post(format!("{}/mcp", w.gw))
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", sid)
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": method, "params": params}))
        .send()
        .await
        .unwrap();
    let text = resp.text().await.unwrap();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("/mcp is not JSON ({e}): {text}"))
}

/// The names `/mcp`'s `tools/list` gives `client`.
pub(crate) async fn mcp_tools(w: &World, client: &reqwest::Client) -> Vec<String> {
    let sid = mcp_session(w, client).await;
    let v = mcp_rpc(w, client, &sid, "tools/list", json!({})).await;
    v["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

/// Set the device row's `timeout_ms`.
pub(crate) async fn set_timeout(w: &World, key_id: i64, ms: u64) {
    let id = device_row(w, key_id).id;
    let (s, v) = op(
        w,
        "mcp_server_set",
        json!({"action": "update", "id": id, "timeout_ms": ms}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
}
