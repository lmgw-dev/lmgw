//! The manager's `Device` arm (§5.2, §5.3): a device row is never dialled.
//! Its entry waits for an inbound link, and follows it:
//!
//! | Link | Status |
//! |---|---|
//! | none | `Stopped`, detail "device offline (last seen …)" |
//! | `initialize` and listing | `Connecting` |
//! | linked and listed | `Ready` |
//! | `initialize` or listing failed | `Error` with the message |
//!
//! The link task owns the session (`link`); the entry holds the session's
//! peer, which the calls go through (`calls`), and the tools it listed. Each
//! link has a number, taken from the manager's claim counter: a word from a
//! link that is no longer the entry's — one a takeover replaced — changes
//! nothing.
//!
//! **On disconnect** the device's tools leave the aggregate and `/mcp`
//! subscribers get `tools/list_changed`. The device's own
//! `notifications/tools/list_changed` makes lmgw list again
//! ([`McpManager::relist_device`]).

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rmcp::model::Tool;
use rmcp::service::Peer;
use rmcp::RoleClient;

use crate::config::{McpServer, Snapshot};

use super::super::{connection_config_hash, McpConn, McpManager, McpStatus};
use super::transport::OpenCalls;

/// A device row's live link, as the manager's entry holds it.
#[derive(Clone)]
pub(crate) struct DeviceLink {
    /// The link's number: the entry follows this link and no other.
    pub(super) link: u64,
    /// The session's peer once `initialize` succeeded; `None` before.
    pub(super) peer: Option<Peer<RoleClient>>,
    /// The calls open on the link, and why lmgw closed it (`calls`).
    pub(super) calls: OpenCalls,
}

impl DeviceLink {
    /// The session's peer once `initialize` succeeded: what a resource
    /// listing goes through (`resources`).
    pub(in crate::mcp) fn peer(&self) -> Option<&Peer<RoleClient>> {
        self.peer.as_ref()
    }
}

/// A `Ready` device row's link, as a call takes it.
pub(in crate::mcp) struct Peered {
    pub(super) link: u64,
    pub(in crate::mcp) peer: Peer<RoleClient>,
    pub(super) calls: OpenCalls,
}

impl McpManager {
    /// A link for `server` opened: its entry is `Connecting`, under a link
    /// number of its own, which this returns. An older link's tools leave
    /// the aggregate now (a takeover lists again).
    pub(super) async fn link_opened(&self, server: &McpServer, calls: OpenCalls) -> u64 {
        let hash = connection_config_hash(server);
        let (had_tools, link) = {
            let mut conns = self.conns.write().await;
            let entry = conns
                .entry(server.id)
                .or_insert_with(|| McpConn::stopped(hash));
            let link = self.claims.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
            entry.status = McpStatus::Connecting;
            entry.config_hash = hash;
            entry.claim = Some(link);
            entry.device = Some(DeviceLink {
                link,
                peer: None,
                calls,
            });
            entry.idle_reaped = false;
            let had = !entry.tools.is_empty();
            entry.tools.clear();
            (had, link)
        };
        if had_tools {
            self.notify_tools_changed();
        }
        link
    }

    /// Link `link`'s handshake ended: `Ready` with its peer and tools, or
    /// `Error` with the message. `false` when the link is no longer the
    /// entry's (a takeover, the row deleted or switched off meanwhile): the
    /// caller closes it.
    pub(super) async fn link_ready(
        &self,
        server_id: i64,
        link: u64,
        result: Result<(Peer<RoleClient>, Vec<Tool>), String>,
    ) -> bool {
        let current = self
            .app()
            .map(|app| app.snapshot().mcp_servers.get(&server_id).cloned());
        let ready = {
            let mut conns = self.conns.write().await;
            let Some(entry) = conns.get_mut(&server_id) else {
                return false;
            };
            if entry.device.as_ref().map(|d| d.link) != Some(link) {
                return false;
            }
            if matches!(&current, Some(None)) || matches!(&current, Some(Some(r)) if !r.enabled) {
                entry.device = None;
                entry.claim = None;
                entry.status = McpStatus::Stopped;
                entry.tools.clear();
                return false;
            }
            entry.claim = None;
            entry.last_attempt = Some(Instant::now());
            entry.last_used = Instant::now();
            match result {
                Ok((peer, tools)) => {
                    if let Some(d) = entry.device.as_mut() {
                        d.peer = Some(peer);
                    }
                    entry.tools = tools;
                    entry.status = McpStatus::Ready;
                    entry.consecutive_failures = 0;
                    true
                }
                Err(e) => {
                    if let Some(d) = entry.device.as_mut() {
                        d.peer = None;
                    }
                    entry.tools.clear();
                    entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                    entry.status = McpStatus::Error(e);
                    false
                }
            }
        };
        if ready {
            self.notify_tools_changed();
            // Its open tasks are polled at once (MCP Tasks design §1.3).
            self.task_server_linked(server_id);
        }
        true
    }

    /// Link `link` closed: unless a newer link took its place, the entry is
    /// `Stopped` and its tools leave the aggregate.
    pub(super) async fn link_closed(&self, server_id: i64, link: u64) {
        let had_tools = {
            let mut conns = self.conns.write().await;
            let Some(entry) = conns.get_mut(&server_id) else {
                return;
            };
            if entry.device.as_ref().map(|d| d.link) != Some(link) {
                return;
            }
            entry.device = None;
            entry.claim = None;
            // An `Error` handshake stays readable until the next link.
            if !matches!(entry.status, McpStatus::Error(_)) {
                entry.status = McpStatus::Stopped;
            }
            let had = !entry.tools.is_empty();
            entry.tools.clear();
            had
        };
        if had_tools {
            self.notify_tools_changed();
        }
    }

    /// The device said its tools changed (`notifications/tools/list_changed`):
    /// list them again on the link that said it, bounded by the row's
    /// `timeout_ms`, and store them while that link is still the entry's.
    pub(crate) async fn relist_device(&self, server_id: i64, timeout: Duration) {
        let Some(Peered { link, peer, .. }) = self.device_peer(server_id).await else {
            return;
        };
        let listed = tokio::time::timeout(timeout, peer.list_all_tools()).await;
        let tools = match listed {
            Ok(Ok(tools)) => tools,
            Ok(Err(e)) => {
                tracing::warn!(
                    "device MCP server {server_id}: listing its tools again failed: {e}"
                );
                return;
            }
            Err(_) => {
                tracing::warn!(
                    "device MCP server {server_id}: listing its tools again took longer than its \
                     timeout_ms ({} ms)",
                    timeout.as_millis()
                );
                return;
            }
        };
        {
            let mut conns = self.conns.write().await;
            let Some(entry) = conns.get_mut(&server_id) else {
                return;
            };
            if entry.device.as_ref().map(|d| d.link) != Some(link) {
                return;
            }
            entry.tools = tools;
        }
        self.notify_tools_changed();
    }

    /// Whether row `server_id` is `Ready`; a device row that is not is
    /// offline, or between a link's open and its listing.
    pub async fn is_ready(&self, server_id: i64) -> bool {
        let conns = self.conns.read().await;
        conns
            .get(&server_id)
            .is_some_and(|c| c.status == McpStatus::Ready)
    }

    /// The peer of row `server_id`'s link when it is `Ready`, with the
    /// link's number and its open calls.
    pub(in crate::mcp) async fn device_peer(&self, server_id: i64) -> Option<Peered> {
        let conns = self.conns.read().await;
        let entry = conns.get(&server_id)?;
        if entry.status != McpStatus::Ready {
            return None;
        }
        let d = entry.device.as_ref()?;
        Some(Peered {
            link: d.link,
            peer: d.peer.clone()?,
            calls: d.calls.clone(),
        })
    }

    /// Close the links of device rows that are gone or switched off in
    /// `snap` (§5.2): the grant cleared, the row disabled on the MCP page —
    /// 1000. A row gone with its key, or the key disabled, is a revocation
    /// (L18): 4003 with the revocation's own reason, as the key's watch
    /// closes it, so the close is the same whichever of the two comes
    /// first (a delete reloads the snapshot before it raises the
    /// revocation).
    pub(crate) fn close_unwanted_links(&self, snap: &Snapshot) {
        use crate::devices::{close_reason, RevokeReason, CLOSE_REVOKED};
        for (id, device) in self.host.rows() {
            let revoked = |reason| (CLOSE_REVOKED, close_reason(reason, &device.who, true));
            let key = snap.api_keys.iter().find(|k| k.id == device.key_id);
            let order = match (snap.mcp_servers.get(&id), key) {
                (_, None) => revoked(RevokeReason::Deleted),
                (_, Some(k)) if !k.enabled => revoked(RevokeReason::Disabled),
                (None, _) => (
                    1000,
                    "this device no longer hosts tools on this gateway (its hosting grant was \
                     cleared)"
                        .to_string(),
                ),
                (Some(s), _) if !s.enabled => (
                    1000,
                    "this device's hosted tools were switched off on the gateway's MCP page"
                        .to_string(),
                ),
                _ => continue,
            };
            self.host.close(id, order);
        }
    }
}

/// What the MCP page says of a device row with no link: offline, and when it
/// was last seen (L15).
pub(crate) async fn offline_detail(state: &crate::state::AppState, server: &McpServer) -> String {
    match last_seen(state, server).await {
        Some(at) => format!("device offline (last seen {at})"),
        None => "device offline (never connected)".to_string(),
    }
}

/// When the device of `server` was last seen, as the key row says.
pub(crate) async fn last_seen(
    state: &crate::state::AppState,
    server: &McpServer,
) -> Option<String> {
    let id = server.device_key_id?;
    crate::store::key_last_seen(&state.db, id)
        .await
        .ok()
        .flatten()
}
