//! A call of a tool its caller listed from one server
//! ([`McpManager::call_listed`]; realtime-server-tools design §1.2, §2.4,
//! final review #2).
//!
//! An exposed name can change under a listing: two servers that offer a
//! tool of one name both take their server's prefix ([`super::names`]), so
//! `echo` listed from `zeta` before `alpha`, a bare sibling with the same
//! tool, was ever listed is `zeta__echo` once `alpha` is; and a name can come
//! to route to another server than the one it was listed from (a name that
//! collides no more). Routed by name alone, the call could run on a server
//! the caller was never shown. A caller that keeps a listing — a realtime
//! session, for hours; a `/v1/responses` run — calls by the name and the
//! server it listed it from:
//! - the name still routes to that server: the call goes there, as
//!   [`McpManager::call`] would send it, the owner's per-tool switch first;
//! - it routes to another: refused, visibly ([`CallError::Relisted`]), and
//!   the caller lists the label again;
//! - it routes nowhere, and the listed server offers the tool under its
//!   server's prefix now — another source came to offer a tool of that name
//!   since, and the colliding tools took their servers' prefixes
//!   ([`super::names`]): the call goes to the listed server's tool, under
//!   the name it has now. That is the tool the caller was shown;
//! - it routes nowhere else: the listed server is not connected — reaped
//!   since, most often. It is connected (a service agent's started first)
//!   and the name looked up again; still nowhere, the name is gone (hidden,
//!   renamed, no longer offered) or the server is down, and the call says
//!   which.

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
        from: &super::host::CallFrom,
    ) -> Result<(rmcp::model::CallToolResult, String), CallError> {
        if snap.tool_disabled(exposed_name) {
            return Err(CallError::Disabled(exposed_name.to_string()));
        }
        let Some(server) = snap.mcp_servers.get(&listed).filter(|s| s.enabled) else {
            return Err(CallError::ToolNotFound(exposed_name.to_string()));
        };
        let mut agg = self.aggregate(snap).await;
        if !agg.reverse.contains_key(exposed_name) && agg.moved_on(exposed_name, listed).is_none() {
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
        // The name it has now: a collision since the listing may have given
        // it its server's prefix (module doc).
        let now = match agg.reverse.contains_key(exposed_name) {
            true => exposed_name,
            false => agg.moved_on(exposed_name, listed).unwrap_or(exposed_name),
        };
        if agg.tool_disabled(snap, now) {
            return Err(CallError::Disabled(now.to_string()));
        }
        match agg.reverse.get(now).cloned() {
            Some(owner) if owner.0 == listed => {
                self.call_on(snap, now, owner, arguments, from).await
            }
            Some(_) => Err(CallError::Relisted {
                name: exposed_name.to_string(),
                server: server.name.clone(),
            }),
            // An offline device answers that it is offline (§5.3).
            None if server.is_device() => Err(self.offline(server).await),
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
