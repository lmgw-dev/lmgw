//! `GET /v1/mcp/servers` and `GET /v1/mcp/servers/{label}`
//! (realtime-server-tools design §1.4): the labels a caller may put into a
//! `{type: "mcp", server_label}` tool, so a client can offer a picker rather
//! than a free-text list. lmgw extensions under `/v1`, like
//! `/v1/audio/voices`; OpenAI has no such route.
//!
//! The list **connects nothing**: it is read from the config, the built-in
//! toolsets, the service agents and the tools already listed, under the
//! caller's tool scope (`exec::labels`) — a bare server is shown to a scoped
//! key only once a tool of it the key may use is listed. The detail lists one label's tools through the **same**
//! function `/v1/realtime` fills an `mcp_list_tools` item with
//! (`exec::list_label`) — the caller's scope, the owner's disables, the
//! targeted connect of that one server — so the two cannot disagree.
//!
//! An unknown or out-of-scope label is a 404, a server that cannot be listed
//! a 502, each with the resolver's own words. Neither route calls a model or
//! a tool, so neither writes a `request_logs` row.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Serialize;
use serde_json::{json, Value};

use crate::proxy::RequestCtx;
use crate::state::SharedState;

use super::exec::{self, LabelEntry, LabelError, LabelKind, LabelTools};
use super::scope::ToolScope;
use super::spec::{ApprovalRule, McpToolSpec};

/// One label, as both routes write it (the detail adds `tools`).
#[derive(Serialize)]
struct ServerObject<'a> {
    object: &'static str,
    server_label: &'a str,
    kind: LabelKind,
    name: &'a str,
    description: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ServerTool<'a>>>,
}

/// One tool of the detail: exactly an `mcp_list_tools` item's, without its
/// `annotations` (always `null` there).
#[derive(Serialize)]
struct ServerTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

fn object<'a>(e: &'a LabelEntry, tools: Option<&'a LabelTools>) -> ServerObject<'a> {
    ServerObject {
        object: "mcp.server",
        server_label: &e.server_label,
        kind: e.kind,
        name: &e.name,
        description: &e.description,
        tools: tools.map(|t| {
            t.tools
                .iter()
                .map(|t| ServerTool {
                    name: &t.wire,
                    description: t.description(),
                    input_schema: &t.def.parameters,
                })
                .collect()
        }),
    }
}

/// `GET /v1/mcp/servers`.
pub(crate) async fn list(
    State(state): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
) -> Response {
    let scope = ToolScope::of_request(&state, &ctx).await;
    let snap = state.snapshot();
    let agg = state.mcp.aggregate(&snap).await;
    let entries = exec::labels(&snap, &agg, &scope);
    let data: Vec<ServerObject<'_>> = entries.iter().map(|e| object(e, None)).collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

/// `GET /v1/mcp/servers/{label}`.
pub(crate) async fn detail(
    State(state): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(label): Path<String>,
) -> Response {
    let scope = ToolScope::of_request(&state, &ctx).await;
    // A label alone, as a client attaching it would write it: every tool.
    let spec = McpToolSpec {
        server_label: label.clone(),
        allowed_tools: None,
        require_approval: ApprovalRule::Never,
    };
    let tools = match exec::list_label(&state, &spec, &scope).await {
        Ok(t) => t,
        Err(LabelError::Unavailable(why)) => {
            return error(
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "mcp_server_not_found",
                why,
            )
        }
        Err(LabelError::Unlisted(why)) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "server_error",
                "mcp_list_tools_failed",
                why,
            )
        }
    };
    // Listed a moment ago: only a config change in between leaves no entry.
    let Some(entry) = exec::label_entry(&state.snapshot(), &label, &scope) else {
        return error(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "mcp_server_not_found",
            format!("MCP server_label '{label}' went away while it was being listed"),
        );
    };
    Json(object(&entry, Some(&tools))).into_response()
}

/// An OpenAI-shaped error with a code of these routes' own.
fn error(status: StatusCode, kind: &str, code: &str, message: String) -> Response {
    let body = json!({"error": {"message": message, "type": kind, "param": null, "code": code}});
    (status, Json(body)).into_response()
}
