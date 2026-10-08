//! Listing a session's `mcp` labels, the core's half (realtime-server-tools
//! design §1.2). A `session.update` that gains a label, or changes its
//! `allowed_tools`, lists it in a task of its own, once the update is in
//! effect — a refused update lists nothing. Per label, in order:
//! 1. `conversation.item.added` with its `mcp_list_tools` item, `tools: []`.
//!    The item is in the conversation from here on: `@openai/agents`
//!    retrieves it on the next event;
//! 2. `mcp_list_tools.in_progress`;
//! 3. the task: the session key's tool scope, read now
//!    (`ToolScope::of_request`), and `exec::list_label` — the resolver
//!    `/v1/responses` and `GET /v1/mcp/servers/{label}` use, connecting only
//!    the server the label names;
//! 4. `mcp_list_tools.completed` or `.failed`, then `conversation.item.done`
//!    with the tools filled in, by their wire names (§1.3).
//!
//! A failed label also gets an `error` with code `mcp_list_tools_failed` and
//! the resolver's message: `.failed` carries only the item's id, and a client
//! that cannot say why its tools are missing is the silent failure the
//! gateway does not allow. The session goes on. So does a label whose tools
//! would share a name with a session function or another label's tool
//! (§1.1): it fails as it is listed.
//!
//! An unchanged definition is not listed again — unless its last listing
//! failed and the update names it: a client keeps its socket open for
//! hours, and the server may be up now. A label the update drops loses its
//! tools, and its item stays.
//!
//! **A listing a later update supersedes** — its label dropped or listed
//! again while it runs — closes its item at once, before any new item is
//! added: `mcp_list_tools.failed` and `conversation.item.done` with no
//! tools, and no `error` (nothing failed; the client asked for something
//! else). Its result, when it comes, is ignored: `@openai/agents` replaces a
//! label's tools on every `mcp_list_tools` item it sees done, so a stale
//! listing that finished last would win over the current one.
//!
//! **A response waits for the listings in flight** before it renders
//! (`busy_for_launch`), up to the lazy-list budget of a cold server: a
//! response that silently lacked a tool listed a moment later could not be
//! told from one whose model ignored it. The wait is the timing line's
//! `MCP listing` stage (`tools_list_ms`), and a cancel or a barge-in ends it
//! like any other wait for a launch.
//!
//! **Every listing reports back**; one that panics as failed, or a response
//! waiting for it would wait for good.

use std::collections::HashMap;
use std::time::Instant;

use futures::FutureExt;
use tokio::sync::mpsc;

use super::super::protocol::{
    ErrorObject, Item, McpListToolsItem, McpListedTool, ServerEvent, Tool,
};
use super::super::session::Core;
use super::spec_of;
use super::table::{McpTable, Plan};
use crate::mcp::exec::{list_label, LabelError, LabelTool, LabelTools, SELF_ADMIN_LABEL};
use crate::mcp::scope::ToolScope;
use crate::mcp::spec::McpToolSpec;

/// A finished listing, for the core.
pub(crate) struct Listed {
    seq: u64,
    label: String,
    item_id: String,
    started: Instant,
    result: Result<LabelTools, LabelError>,
}

pub(crate) type Tx = mpsc::UnboundedSender<Listed>;

/// The session's MCP tools: the table, and where its listings report.
#[derive(Debug, Default)]
pub(crate) struct McpSession {
    pub table: McpTable,
    /// The session loop's end (`session::run`); a core no loop runs — a
    /// unit test's — lists nothing.
    tx: Option<Tx>,
    /// The listings in flight, by label: their number and their item's id.
    in_flight: HashMap<String, (u64, String)>,
    /// A response's server-side calls ended, and no response has started
    /// since: the next one renders their results (`lifecycle::refusal`).
    pub results_in: bool,
    /// What the session's device's admin tools could do when its `lmgw`
    /// label was last listed (its level capped by the gateway's); `None`
    /// before, and for a session that is not a device's (review P-8).
    admin_listed: Option<crate::config::SelfAdmin>,
}

impl McpSession {
    pub fn listen(&mut self, tx: Tx) {
        self.tx = Some(tx);
    }
}

impl Core {
    /// A `session.update`'s `plan`, now that the update is in effect: the
    /// labels it dropped go, and each label it gains or redefined is listed.
    pub(in crate::realtime) fn mcp_apply(&mut self, plan: Plan) {
        for label in &plan.drop {
            self.mcp.table.drop_label(label);
            self.mcp_superseded(label);
        }
        for spec in plan.list {
            self.mcp_list(spec);
        }
        // A dropped label's listing no longer holds a response back.
        if !plan.drop.is_empty() {
            self.mcp_settled();
        }
    }

    /// What the session's device's admin tools may do now — its level
    /// capped by the gateway's; `None` for a session that is not a device's.
    fn device_admin_now(&self) -> Option<crate::config::SelfAdmin> {
        match &self.ctx.principal {
            crate::principal::Principal::Key {
                id,
                kind: crate::config::ApiKeyKind::Device,
                ..
            } => {
                let snap = self.state.snapshot();
                Some(crate::devices::self_admin(&snap, *id).capped(snap.settings.self_admin))
            }
            _ => None,
        }
    }

    /// A device's admin-tools level moved, or the gateway's that caps it
    /// (`devices::ReachMoves`, review P-8, P-3, 2026-10-07). When what this
    /// session's device may do moved with it, and its `lmgw` label was
    /// listed under another level, the label is listed again (module
    /// doc): its tools come or go as for a `session.update` that redefined
    /// it, and a listing refused now says why. Its calls were refused
    /// already, by the per-call checks; now its model is no longer offered
    /// them either.
    pub(in crate::realtime) fn mcp_reach_moved(&mut self) {
        let now = self.device_admin_now();
        if now.is_none() || self.mcp.admin_listed.is_none() || self.mcp.admin_listed == now {
            return;
        }
        let spec = self
            .session
            .tools
            .iter()
            .flatten()
            .filter_map(Tool::as_mcp)
            .find(|t| t.server_label == SELF_ADMIN_LABEL)
            .and_then(|t| spec_of(t).ok());
        if let Some(spec) = spec {
            tracing::info!(
                "realtime {}: the device's admin tools are at {} now; MCP server_label \
                 '{SELF_ADMIN_LABEL}' is listed again",
                self.id(),
                now.map_or("off", |l| l.as_str()),
            );
            self.mcp_list(spec);
        }
    }

    /// Steps 1 and 2 (module doc), and the task.
    fn mcp_list(&mut self, spec: McpToolSpec) {
        if spec.server_label == SELF_ADMIN_LABEL {
            self.mcp.admin_listed = self.device_admin_now();
        }
        let seq = self.mcp.table.start(&spec);
        self.mcp_superseded(&spec.server_label);
        // A response not launched yet waits for this one too.
        self.timing_tools_waiting();
        let item_id = self.conversation.fresh_item_id(&self.ids);
        self.mcp
            .in_flight
            .insert(spec.server_label.clone(), (seq, item_id.clone()));
        let item = Item::McpListTools(McpListToolsItem {
            id: Some(item_id.clone()),
            server_label: spec.server_label.clone(),
            tools: Vec::new(),
        });
        let previous_item_id = self.conversation.append(item.clone());
        self.ob.send(ServerEvent::ItemAdded {
            previous_item_id,
            item,
        });
        self.ob.send(ServerEvent::McpListToolsInProgress {
            item_id: item_id.clone(),
        });
        let Some(tx) = self.mcp.tx.clone() else {
            return;
        };
        let (state, ctx) = (self.state.clone(), self.ctx.clone());
        tokio::spawn(async move {
            let started = Instant::now();
            let label = spec.server_label.clone();
            let result = std::panic::AssertUnwindSafe(async {
                let scope = ToolScope::of_request(&state, &ctx).await;
                list_label(&state, &spec, &scope).await
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|panic| {
                let why = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "no message".into());
                Err(LabelError::Unlisted(format!(
                    "the listing failed inside lmgw: {why}"
                )))
            });
            let _ = tx.send(Listed {
                seq,
                label,
                item_id,
                started,
                result,
            });
        });
    }

    /// The listing in flight for `label`, if any, is superseded (module
    /// doc): its item closes now, with no tools and no `error`.
    fn mcp_superseded(&mut self, label: &str) {
        let Some((_, item_id)) = self.mcp.in_flight.remove(label) else {
            return;
        };
        // Gone only if the client deleted it meanwhile: nothing to close.
        let Some(item @ Item::McpListTools(_)) = self.conversation.get(&item_id).cloned() else {
            return;
        };
        self.ob.send(ServerEvent::McpListToolsFailed {
            item_id: item_id.clone(),
        });
        self.ob.send(ServerEvent::ItemDone {
            previous_item_id: self.conversation.previous_of(&item_id),
            item,
        });
    }

    /// Step 4 (module doc): a listing's result.
    pub(in crate::realtime) fn on_listed(&mut self, done: Listed) {
        let Listed {
            seq,
            label,
            item_id,
            started,
            result,
        } = done;
        let ms = started.elapsed().as_millis();
        if !self.mcp.table.current(&label, seq) {
            // Its item closed when it was superseded (module doc).
            tracing::info!(
                "realtime {}: MCP server_label '{label}' listing ended after {ms} ms, superseded; \
                 its result is ignored",
                self.id()
            );
            return;
        }
        self.mcp.in_flight.remove(&label);
        let result = result.and_then(|tools| {
            let session_tools = self.session.tools.as_deref().unwrap_or_default();
            match self.mcp.table.clash_of(&label, &tools, session_tools) {
                Some(why) => Err(LabelError::Unavailable(why)),
                None => Ok(tools),
            }
        });
        match &result {
            Ok(t) => tracing::info!(
                "realtime {}: MCP server_label '{label}' listed {} tools in {ms} ms",
                self.id(),
                t.tools.len()
            ),
            Err(e) => tracing::info!(
                "realtime {}: MCP server_label '{label}' could not be listed ({ms} ms) — {}",
                self.id(),
                e.message()
            ),
        }
        // Gone only if the client deleted it meanwhile: nothing to close.
        let item = match self.conversation.get_mut(&item_id) {
            Some(Item::McpListTools(l)) => {
                if let Ok(t) = &result {
                    l.tools = t.tools.iter().map(listed_tool).collect();
                }
                Some(Item::McpListTools(l.clone()))
            }
            _ => None,
        };
        if let Some(item) = item {
            self.ob.send(match &result {
                Ok(_) => ServerEvent::McpListToolsCompleted {
                    item_id: item_id.clone(),
                },
                Err(_) => ServerEvent::McpListToolsFailed {
                    item_id: item_id.clone(),
                },
            });
            self.ob.send(ServerEvent::ItemDone {
                previous_item_id: self.conversation.previous_of(&item_id),
                item,
            });
        }
        if let Err(e) = &result {
            self.error(listing_error(&label, e));
        }
        let result = result.map_err(|e| e.message().to_string());
        self.mcp.table.settle(&label, seq, result);
        self.mcp_settled();
    }

    /// The table changed: with nothing in flight any more, a response that
    /// waited for it stops waiting, and launches unless it waits for a
    /// transcript too.
    fn mcp_settled(&mut self) {
        if self.mcp.table.listing() {
            return;
        }
        self.timing_tools_listed();
        if !self.busy_for_launch() {
            self.launch();
        }
    }
}

/// One tool of an `mcp_list_tools` item.
fn listed_tool(t: &LabelTool) -> McpListedTool {
    McpListedTool {
        name: t.wire.clone(),
        description: t.description().to_string(),
        input_schema: t.def.parameters.clone(),
        annotations: None,
    }
}

/// The `error` beside a failed label's `mcp_list_tools.failed`: the
/// client's own when the label is not one it may use, the server's when it
/// could not be listed.
fn listing_error(label: &str, e: &LabelError) -> ErrorObject {
    let kind = match e {
        LabelError::Unavailable(_) => "invalid_request_error",
        LabelError::Unlisted(_) => "server_error",
    };
    ErrorObject {
        kind: kind.into(),
        ..ErrorObject::invalid(
            "mcp_list_tools_failed",
            format!(
                "the tools of MCP server_label '{label}' could not be listed: {}",
                e.message()
            ),
        )
    }
}
