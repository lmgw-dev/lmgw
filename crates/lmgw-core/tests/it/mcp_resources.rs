//! `/mcp` passes resources through, and the MCP Apps metadata (client-apps
//! design §7, §9's resources list), against Streamable-HTTP stubs that serve
//! resources and a fake device over its host link.
//!
//! - `routing`: the capabilities both ways, `resources/list`,
//!   `resources/templates/list` and `resources/read` routed by namespace and
//!   by who claims a URI, errors, the stream's `resources/list_changed`;
//! - `apps`: one namespaced URI from `tools/list` through a tool's result to
//!   a read, app-only tools listed on `/mcp` and never offered to a model,
//!   the Chat `tool` frame's fields;
//! - `reach`: resources follow the caller's reach, a device's (L16)
//!   included; a server a narrowed caller reaches is connected to judge
//!   it, one it cannot reach is never started, and a listing asks no
//!   device the caller does not reach;
//! - `device`: a device's own `resources/list_changed`, and a read over its
//!   link cancelled on a timeout, when lmgw stops waiting and when lmgw
//!   closes the link.

use serde_json::{json, Value};

use crate::mcp_host::{mcp_rpc, mcp_session};
use crate::realtime_chat_thread::World;
use crate::support::mcp_apps_stub::{apps_stub, Apps, AppsStub};
use crate::support::mcp_stub::register;

mod apps;
mod device;
mod reach;
mod routing;

/// The UI resource the weather server's `show` tool links to, as the server
/// spells it.
pub(crate) const CARD: &str = "ui://weather/card";

/// A weather server: `show` (linking [`CARD`]), `refresh` (app-only), the
/// card listed, a per-city template; its calls answer with a link to the
/// card, the card embedded, and structured content.
pub(crate) fn weather() -> Apps {
    Apps {
        tools: json!([
            {"name": "show", "description": "show the weather",
             "inputSchema": {"type": "object"},
             "_meta": {"ui": {"resourceUri": CARD}, "ui/resourceUri": CARD}},
            {"name": "refresh", "description": "refresh the card",
             "inputSchema": {"type": "object"},
             "_meta": {"ui": {"resourceUri": CARD, "visibility": ["app"]}}}
        ]),
        resources: json!([{"uri": CARD, "name": "card",
                           "mimeType": "text/html;profile=mcp-app"}]),
        templates: json!([{"uriTemplate": "ui://weather/{city}/card", "name": "city card"}]),
        call_result: json!({
            "content": [
                {"type": "text", "text": "sunny"},
                {"type": "resource_link", "uri": CARD, "name": "card"},
                {"type": "resource", "resource": {"uri": CARD, "text": "<p>sunny</p>",
                                                  "mimeType": "text/html;profile=mcp-app"}}
            ],
            "structuredContent": {"temp_c": 21, "sky": "sunny"},
            "isError": false
        }),
    }
}

/// A server `name` with tool prefix `prefix` offering `apps`, registered.
pub(crate) async fn server(w: &World, name: &str, prefix: &str, apps: Apps) -> AppsStub {
    let stub = apps_stub(apps).await;
    register(&w.state, name, prefix, &stub.url, true, None).await;
    stub
}

/// Connect every registered server, as a first `tools/list` does.
pub(crate) async fn connect_all(w: &World) {
    w.state.mcp.list_tools(&w.state.snapshot()).await;
}

/// One `/mcp` request as `client`, on a session of its own: the answer.
pub(crate) async fn rpc(w: &World, client: &reqwest::Client, method: &str, params: Value) -> Value {
    let sid = mcp_session(w, client).await;
    mcp_rpc(w, client, &sid, method, params).await
}

/// An `/mcp` session as `client` for an MCP Apps host: one whose
/// `initialize` declares the extension. Its id.
pub(crate) async fn apps_session(w: &World, client: &reqwest::Client) -> String {
    let resp = client
        .post(format!("{}/mcp", w.gw))
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25",
                        "capabilities": {"extensions": {"io.modelcontextprotocol/ui":
                                            {"mimeTypes": ["text/html;profile=mcp-app"]}}},
                        "clientInfo": { "name": "apps-host", "version": "0" } }
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

/// The URIs `/mcp`'s `resources/list` gives `client`.
pub(crate) async fn listed(w: &World, client: &reqwest::Client) -> Vec<String> {
    let v = rpc(w, client, "resources/list", json!({})).await;
    v["result"]["resources"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .map(|r| r["uri"].as_str().unwrap().to_string())
        .collect()
}
