//! `/mcp` and `/mcp/admin` (no per-method expansion of MCP's JSON-RPC
//! is documented): the Streamable HTTP transport
//! `mcp/ingress.rs` hand-rolls — single request/response per `POST`, no
//! batching, session lifecycle via `Mcp-Session-Id`, `GET` for the
//! server→client notification stream, `DELETE` to end a session.
//!
//! What is documented is the *envelope* — JSON-RPC 2.0's own shape — not each
//! method's own `params`/`result`: `initialize`, `tools/list`, `tools/call`,
//! `resources/list`, `resources/templates/list`, `resources/read` and `ping`
//! all go through the one request schema, same as a hand client reads the
//! wire. `POST /mcp/admin` carries exactly the same envelope for
//! the `lmgw__*` self-admin tools (`mcp/selfadmin/catalog.rs`).

use schemars::generate::SchemaGenerator;
use schemars::Schema;
use serde_json::{json, Value};

use super::super::schemas;

/// Registers and refs `JsonRpcRequest` — what [`super::super::build`]'s
/// `Req::JsonRpc` arm resolves to (see `error_ref`'s doc comment): `{jsonrpc, id?, method, params?}`. `id` is absent on
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
                    tools/list, tools/call, resources/list, resources/templates/list, \
                    resources/read, notifications/initialized."},
                "params": {"type": "object", "additionalProperties": true}
            }
        }),
    )
}

/// The success envelope: `{jsonrpc, id, result}`. `result`'s shape is the
/// method's own — `tools/list` answers `{tools: [...]}`, `tools/call`
/// answers a `CallToolResult` (`content`, `isError`) — left untyped rather
/// than modeled per method.
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

/// `GET /mcp`'s push stream: `notifications/tools/list_changed` and
/// `notifications/resources/list_changed` (`mcp/ingress.rs`), SSE-framed as a bare
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

// ---------------------------------------------------------------------------
// GET /v1/mcp/servers[/{label}]
// ---------------------------------------------------------------------------

/// The properties every `mcp.server` object has (`mcp/discovery.rs`).
fn server_properties() -> Value {
    json!({
        "object": {"type": "string", "enum": ["mcp.server"]},
        "server_label": {"type": "string", "description": "What goes into a \
            {\"type\": \"mcp\", \"server_label\"} tool on /v1/realtime or /v1/responses: a \
            built-in toolset's label, a registered server's tool prefix, else its name."},
        "kind": {"type": "string", "enum": ["builtin", "server", "agent"],
            "description": "builtin: lmgw's own toolsets (lmgw, for owner credentials only; \
                docs; kb); server: a registered MCP server; agent: a service agent's tools."},
        "name": {"type": "string", "description": "A server's configured name; a toolset's \
            label."},
        "description": {"type": "string", "description": "A toolset's purpose; \"\" for a \
            server."}
    })
}

const SERVER_REQUIRED: [&str; 5] = ["object", "server_label", "kind", "name", "description"];

/// `GET /v1/mcp/servers`: the labels this caller may attach, connecting
/// nothing.
pub(crate) fn servers_list(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "McpServerList",
        schemars::json_schema!({
            "type": "object",
            "required": ["object", "data"],
            "properties": {
                "object": {"type": "string", "enum": ["list"]},
                "data": {"type": "array", "items": {
                    "type": "object",
                    "required": SERVER_REQUIRED,
                    "properties": server_properties()
                }}
            }
        }),
    )
}

/// `GET /v1/mcp/servers/{label}`: the same object plus the label's tools,
/// exactly as a `/v1/realtime` session's `mcp_list_tools` item lists them.
pub(crate) fn server_detail(g: &mut SchemaGenerator) -> Schema {
    let mut properties = server_properties();
    properties["tools"] = json!({"type": "array", "items": {
        "type": "object",
        "required": ["name", "description", "input_schema"],
        "properties": {
            "name": {"type": "string", "description": "The server's own name for the tool: \
                its exposed name minus the <prefix>__ namespace."},
            "description": {"type": "string", "description": "\"\" when the server gave none."},
            "input_schema": {"type": "object", "additionalProperties": true}
        }
    }});
    let mut required: Vec<&str> = SERVER_REQUIRED.to_vec();
    required.push("tools");
    schemas::named(
        g,
        "McpServerDetail",
        schemars::json_schema!({
            "type": "object",
            "required": required,
            "properties": properties
        }),
    )
}
