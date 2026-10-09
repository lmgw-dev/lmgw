//! `/mcp`'s `resources/*` and what `initialize` says of them (client-apps
//! design §7.2, L14; [`crate::mcp::resources`] for the aggregate and the
//! routing).
//!
//! - **Capabilities**, on every revision `/mcp` speaks (each defines
//!   resources): `resources {listChanged: true}`, and the MCP Apps extension
//!   under `extensions` with its `mimeTypes`. No revision is added for it.
//! - **`resources/list`, `resources/templates/list`**: the aggregate, in one
//!   page. lmgw hands out no cursor, so a request that carries one is
//!   answered `-32602`, as MCP asks for an invalid cursor.
//! - **`resources/read`**: routed to the server that has the URI, for a
//!   caller that reaches it. `-32002` for a URI no server has, or one whose
//!   server the caller does not reach (with why); a server's own error
//!   passes on with its code.
//! - Subscriptions (`resources/subscribe`) are not offered (§Later): the
//!   capability says `subscribe` is absent, and the method answers `-32601`.
//!
//! `/mcp/admin` serves no resources and says so in its capabilities.

use serde_json::{json, Value};

use crate::mcp::resources::{apps::ui_extension_settings, UI_EXTENSION};
use crate::mcp::scope::ToolScope;
use crate::proxy::RequestCtx;
use crate::state::SharedState;

/// `initialize`'s `capabilities` for the aggregate plane.
pub(super) fn aggregate_capabilities() -> Value {
    json!({
        "tools": { "listChanged": true },
        "resources": { "listChanged": true },
        "extensions": { UI_EXTENSION: ui_extension_settings() },
    })
}

/// One `resources/*` request on the aggregate plane, as `ctx`'s caller:
/// `None` for a method this plane does not serve.
pub(super) async fn serve(
    state: &SharedState,
    ctx: &RequestCtx,
    method: &str,
    params: &Value,
) -> Option<Result<Value, (i64, String)>> {
    let listing = matches!(method, "resources/list" | "resources/templates/list");
    if !listing && method != "resources/read" {
        return None;
    }
    if listing && params.get("cursor").is_some_and(|c| !c.is_null()) {
        return Some(Err((
            -32602,
            "invalid params: lmgw answers this list in one page and hands out no cursor".into(),
        )));
    }
    let snap = state.snapshot();
    let scope = ToolScope::of_request(state, ctx).await;
    Some(match method {
        "resources/list" => {
            let resources = state.mcp.list_resources(&snap, &scope).await;
            Ok(json!({ "resources": resources }))
        }
        "resources/templates/list" => {
            let templates = state.mcp.list_resource_templates(&snap, &scope).await;
            Ok(json!({ "resourceTemplates": templates }))
        }
        _ => {
            let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                return Some(Err((
                    -32602,
                    "invalid params: resources/read requires a string `uri`".into(),
                )));
            };
            let from = crate::mcp::host::CallFrom::of(ctx);
            state
                .mcp
                .read_resource(&snap, uri, &scope, &from)
                .await
                .map_err(|e| (e.rpc_code(), e.to_string()))
        }
    })
}

/// The notification `GET /mcp` sends when the resources may have changed.
pub(super) fn resources_list_changed_notification() -> String {
    json!({ "jsonrpc": "2.0", "method": "notifications/resources/list_changed" }).to_string()
}
