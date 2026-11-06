//! `GET /chat/api/tools` and `GET /chat/api/tools/{label}` (chat-plane
//! design §2.2, D4, D5): the toolsets a thread may attach, for the Chat's
//! tool picker, and one toolset's tools.
//!
//! **Under the caller's tool scope** ([`Caller::scope`]): the owner's is the
//! gateway's own, a device's its key's. The list is [`exec::labels`] — what
//! `GET /v1/mcp/servers` lists the same caller, connecting nothing — with
//! what the picker shows beside each label:
//! - **built-in toolsets** carry their tools, resolved in-process by
//!   [`exec::list_label_unstarted`] (the owner's switches and the caller's
//!   scope applied). The owner is listed every toolset, one that offers nothing
//!   now with the reason, as the inventory listed them; a device only those
//!   that offer it tools, and the self-admin toolset (`lmgw`) only at
//!   `full` — below it a device uses the threads that carry it and attaches
//!   it to none (L5's note);
//! - **servers and service agents** carry their connection's status and how
//!   many of their tools, as last listed, the caller may tick; their tools
//!   are read one label at a time by the detail, which connects that server
//!   only once a thread attaches it.
//!
//! The detail is [`exec::list_label_unstarted`] with the label alone: the
//! tools a thread attaching it gets. It connects that server, and starts no
//! service agent (D16): opening a thread's tool settings is not the ask that
//! runs an agent's container, a turn is. A label the caller may not attach is
//! the `404 not_found` of one that is not there, in the resolver's words; a
//! server that cannot be listed, an agent asleep among them, is `502
//! mcp_list_tools_failed`. `GET /v1/mcp/servers/{label}` keeps
//! [`exec::list_label`], which starts it.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::chat_reads::{ChatTool, ChatToolDetail, ChatToolList, ChatToolSource};

use super::super::chat::err_json;
use super::super::chat_caller::Caller;
use super::super::chat_extract::ChatPath;
use crate::config::{SelfAdmin, Snapshot};
use crate::mcp::exec::{self, LabelError, LabelKind, LabelTool, SELF_ADMIN_LABEL};
use crate::mcp::scope::ToolScope;
use crate::mcp::spec::{ApprovalRule, McpToolSpec};
use crate::state::SharedState;

/// `GET /chat/api/tools` (module doc).
pub(in crate::web) async fn tool_list(
    State(state): State<SharedState>,
    caller: Caller,
) -> Response {
    let scope = caller.scope(&state).await;
    let snap = state.snapshot();
    let mut sources = Vec::new();
    for (label, purpose) in exec::builtin_toolsets() {
        if label == SELF_ADMIN_LABEL && !may_attach_self_admin(&caller, &scope, &snap) {
            continue;
        }
        let (status, reason, tools) =
            match exec::list_label_unstarted(&state, &whole(label), &scope).await {
                Ok(t) => ("ready", None, t.tools.iter().map(tool).collect::<Vec<_>>()),
                // A device is listed what it may use (module doc).
                Err(_) if caller.is_device() => continue,
                Err(e) => ("unavailable", Some(e.message().to_string()), Vec::new()),
            };
        sources.push(ChatToolSource {
            server_label: label.to_string(),
            kind: "builtin".into(),
            name: label.to_string(),
            description: purpose.to_string(),
            status: status.into(),
            tool_count: count(tools.len()),
            reason,
            tools,
        });
    }
    let agg = state.mcp.aggregate(&snap).await;
    let offer = crate::mcp::media::offer(&state)
        .await
        .within(&snap, scope.key());
    let live = state.mcp.status_views(&snap).await;
    for entry in exec::labels((&snap, &offer), &agg, &scope) {
        let kind = match entry.kind {
            LabelKind::Builtin => continue,
            LabelKind::Server => "server",
            LabelKind::Agent => "agent",
        };
        let Some(server) = snap.mcp_servers.values().find(|s| s.name == entry.name) else {
            continue;
        };
        let status = live
            .iter()
            .find(|v| v.id == server.id)
            .map_or("stopped", |v| v.status);
        // Its tools as the aggregate last listed them, as the caller may
        // tick them: never one the owner switched off, nor one outside its
        // scope.
        let tickable = agg
            .reverse
            .iter()
            .filter(|(name, (id, _))| {
                *id == server.id && !agg.tool_disabled(&snap, name) && scope.admits(name)
            })
            .count();
        sources.push(ChatToolSource {
            server_label: entry.server_label,
            kind: kind.into(),
            name: entry.name,
            description: entry.description,
            status: status.into(),
            tool_count: count(tickable),
            reason: None,
            tools: Vec::new(),
        });
    }
    Json(ChatToolList { sources }).into_response()
}

/// `GET /chat/api/tools/{label}` (module doc).
pub(in crate::web) async fn tool_detail(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(label): ChatPath<String>,
) -> Response {
    let scope = caller.scope(&state).await;
    if label.trim() == SELF_ADMIN_LABEL
        && !may_attach_self_admin(&caller, &scope, &state.snapshot())
    {
        return err_json(
            StatusCode::NOT_FOUND,
            "not_found",
            format!(
                "'{SELF_ADMIN_LABEL}': attaching lmgw's admin tools to a thread needs this \
                 device's admin tools at full"
            ),
        );
    }
    match exec::list_label_unstarted(&state, &whole(&label), &scope).await {
        Ok(t) => Json(ChatToolDetail {
            server_label: label,
            tools: t.tools.iter().map(tool).collect(),
        })
        .into_response(),
        Err(LabelError::Unavailable(why)) => err_json(StatusCode::NOT_FOUND, "not_found", why),
        Err(LabelError::Unlisted(why)) => {
            err_json(StatusCode::BAD_GATEWAY, "mcp_list_tools_failed", why)
        }
    }
}

/// Whether `caller` may attach the self-admin toolset: the owner always; a
/// device only with its admin tools at `full`, its own level capped by the
/// gateway's (client-apps design L5's note). The write reads the stored
/// levels again (`chat_tool_write`); this is what the picker offers.
fn may_attach_self_admin(caller: &Caller, scope: &ToolScope, snap: &Snapshot) -> bool {
    !caller.is_device() || scope.admin_level(snap.settings.self_admin) == SelfAdmin::Full
}

/// A thread's entry naming `label` and no narrower list: every tool it has.
fn whole(label: &str) -> McpToolSpec {
    McpToolSpec {
        server_label: label.to_string(),
        allowed_tools: None,
        require_approval: ApprovalRule::Never,
        server_url: None,
        connector_id: None,
    }
}

/// One tool under its exposed name, the spelling a thread's `allowed_tools`
/// stores.
fn tool(t: &LabelTool) -> ChatTool {
    ChatTool {
        name: t.def.name.clone(),
        description: t.def.description.clone(),
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
