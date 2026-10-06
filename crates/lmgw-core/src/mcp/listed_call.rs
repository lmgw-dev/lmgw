//! A call of a tool its caller listed from one server
//! ([`McpManager::call_listed`]; realtime-server-tools design §1.2, §2.4,
//! final review #2).
//!
//! An exposed name is unique within one aggregate only: the aggregate holds
//! the servers connected now, and two servers without a tool prefix that
//! offer a tool of one name share it — the first by server name wins, the
//! other's is shadowed (§7). A name listed from `zeta` while `alpha`, a bare
//! sibling with the same tool, was reaped is `alpha`'s once `alpha` connects
//! again: routed by name alone, the call would run on a server the caller
//! was never shown. A caller that keeps a listing — a realtime session, for
//! hours; a `/v1/responses` run — calls by the name and the server it
//! listed it from:
//! - the name still routes to that server: the call goes there, as
//!   [`McpManager::call`] would send it, the owner's per-tool switch first;
//! - it routes to another: refused, visibly ([`CallError::Relisted`]), and
//!   the caller lists the label again;
//! - it routes nowhere: the listed server is not connected — reaped since,
//!   most often. It is connected (a service agent's started first) and the
//!   name looked up again; still nowhere, the name is gone (hidden, renamed,
//!   no longer offered) or the server is down, and the call says which.

use crate::config::Snapshot;

use super::{CallError, McpManager, McpStatus};

impl McpManager {
    /// [`call`](Self::call) of `exposed_name`, which its caller listed from
    /// the server `listed` (module doc).
    pub async fn call_listed(
        &self,
        snap: &Snapshot,
        exposed_name: &str,
        listed: i64,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<(rmcp::model::CallToolResult, String), CallError> {
        if snap.tool_disabled(exposed_name) {
            return Err(CallError::Disabled(exposed_name.to_string()));
        }
        let Some(server) = snap.mcp_servers.get(&listed).filter(|s| s.enabled) else {
            return Err(CallError::ToolNotFound(exposed_name.to_string()));
        };
        let mut agg = self.aggregate(snap).await;
        if !agg.reverse.contains_key(exposed_name) {
            if let (Some(app), Some(agent)) = (self.app(), server.agent_id.clone()) {
                if let Err(e) = crate::agents::service::ensure_by_id(&app, &agent).await {
                    return Err(CallError::NotConnected {
                        server: server.name.clone(),
                        detail: e.reason.clone(),
                    });
                }
            }
            self.start_one(server).await;
            agg = self.aggregate(snap).await;
        }
        match agg.reverse.get(exposed_name).cloned() {
            Some(owner) if owner.0 == listed => {
                self.call_on(snap, exposed_name, owner, arguments).await
            }
            Some(_) => Err(CallError::Relisted {
                name: exposed_name.to_string(),
                server: server.name.clone(),
            }),
            None => {
                let conns = self.conns.read().await;
                Err(match conns.get(&listed).map(|c| &c.status) {
                    Some(McpStatus::Error(e)) => CallError::NotConnected {
                        server: server.name.clone(),
                        detail: e.clone(),
                    },
                    _ => CallError::ToolNotFound(exposed_name.to_string()),
                })
            }
        }
    }
}
