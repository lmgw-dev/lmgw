//! `/mcp` and `/mcp/admin` (api-docs design §4.8 tail, §1 non-goal "no
//! per-method expansion of MCP's JSON-RPC"): the Streamable HTTP transport
//! `mcp/ingress.rs` hand-rolls — single request/response per `POST`, no
//! batching, session lifecycle via `Mcp-Session-Id`, `GET` for the
//! server→client notification stream, `DELETE` to end a session.
//!
//! What is documented is the *envelope* — JSON-RPC 2.0's own shape — not each
//! method's own `params`/`result`: `initialize`, `tools/list`, `tools/call`
//! and `ping` all go through the one request schema, same as a hand client
//! reads the wire. `POST /mcp/admin` carries exactly the same envelope for
//! the `lmgw__*` self-admin tools (`mcp/selfadmin/catalog.rs`).

use schemars::generate::SchemaGenerator;
use schemars::Schema;
use serde_json::{json, Value};

use super::super::schemas;

/// Registers and refs `JsonRpcRequest` — what [`super::super::build`]'s
/// `Req::JsonRpc` arm resolves to (fixed alongside `error_ref`'s same bug,
/// see its doc comment): `{jsonrpc, id?, method, params?}`. `id` is absent on
/// a notification (`notifications/*`); JSON-RPC leaves its type to the
/// caller, so it is left unconstrained here too.
pub(crate) fn jsonrpc_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "JsonRpcRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["jsonrpc", "method"],
            "properties": {
                "jsonrpc": {"type": "string", "enum": ["2.0"]},
                "id": {"description": "Absent on a notification (e.g. \
                    notifications/initialized); string or number otherwise."},
                "method": {"type": "string", "description": "e.g. initialize, ping, \
                    tools/list, tools/call, notifications/initialized."},
                "params": {"type": "object", "additionalProperties": true}
            }
        }),
    )
}

/// The success envelope: `{jsonrpc, id, result}`. `result`'s shape is the
/// method's own — `tools/list` answers `{tools: [...]}`, `tools/call`
/// answers a `CallToolResult` (`content`, `isError`) — left untyped rather
/// than modeled per method (§1 non-goal).
pub(crate) fn jsonrpc_response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "JsonRpcResponse",
        schemars::json_schema!({
            "type": "object",
            "required": ["jsonrpc", "id", "result"],
            "properties": {
                "jsonrpc": {"type": "string", "enum": ["2.0"]},
                "id": {},
                "result": {"type": "object", "additionalProperties": true}
            }
        }),
    )
}

/// `GET /mcp`'s push stream: `notifications/tools/list_changed` today
/// (`mcp/ingress.rs`'s aggregate composition change), SSE-framed as a bare
/// JSON-RPC notification — the same envelope as [`jsonrpc_request`] with no
/// `id`.
pub(crate) fn notification_event(g: &mut SchemaGenerator) -> Schema {
    jsonrpc_request(g)
}

/// `initialize`, the one call every session's first request must be.
pub(crate) fn initialize_example() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "example-client", "version": "0.1.0" }
        }
    })
}

/// `tools/call`, the self-admin plane's own shape (`lmgw__status` takes no
/// arguments).
pub(crate) fn admin_call_example() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "lmgw__status", "arguments": {} }
    })
}
