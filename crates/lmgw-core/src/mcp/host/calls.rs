//! One call forwarded to a device (§5.4, §5.5).
//!
//! - **`_meta`.** Every `tools/call` carries `lmgw/caller`, `lmgw/approval`
//!   and `lmgw/timeout_ms` (the row's `timeout_ms`, when lmgw stops waiting).
//! - **lmgw cancels every call it stops waiting on** (R14). At the timeout,
//!   and when the caller stops waiting — a turn's cancel, a barge-in, a
//!   client that hung up: the call's future is dropped — it sends
//!   `notifications/cancelled {requestId, reason}` to the device before the
//!   call is reported. A link lmgw closes cancels the calls open on it
//!   itself (`link`).
//! - **A link that drops mid-call** reports the Chat's abandoned wording, so
//!   the model does not retry blindly; a link lmgw itself closed says so,
//!   with the close's reason (a takeover, a revocation, a stop, the row
//!   switched off), not that the device disconnected.
//! - **A `resources/read`** goes the same way ([`McpManager::device_request`]):
//!   `_meta`, the timeout, the cancel, a closed link's reason.
//! - **An offline device** answers at once, without the lazy-list budget's
//!   wait: "device '<name>' is not connected (last seen …)".
//! - **A task-augmented call** (MCP Tasks design §1.1, §1.2) carries
//!   `task: {}` and `_meta["lmgw/task"]`; `lmgw/timeout_ms` bounds the wait
//!   for its `CreateTaskResult` only, and a timeout there is a normal
//!   call's. The task it answers with goes to `mcp::tasks`.

use std::time::{Duration, Instant};

use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotification,
    CancelledNotificationParam, ClientNotification, ClientRequest, ErrorData, Meta, RequestId,
    ServerResult, TaskMetadata,
};
use rmcp::service::{Peer, PeerRequestOptions, ServiceError};
use rmcp::RoleClient;

use lmgw_api_types::mcp_host::{CallMeta, TaskMeta};

use crate::config::McpServer;

use super::super::tasks::{self, Answered, Augment};
use super::super::{CallError, InFlightGuard, McpManager};
use super::CallFrom;

/// The words of a call whose link went while it ran (server-tools decision
/// 5's, for a link).
pub(crate) fn abandoned(device: &str) -> String {
    format!(
        "device '{device}' disconnected while the call ran: the call was abandoned; it had \
         already been sent, so it may or may not have run"
    )
}

/// The words of a call cut off because lmgw itself closed the link — a
/// takeover, a revocation, a stop, the row switched off — naming why: the
/// device was told to cancel it, and it had been sent.
pub(crate) fn cut_off(device: &str, why: &str) -> String {
    format!(
        "lmgw closed device '{device}''s host link while the call ran ({why}): the call was \
         cancelled on the device, but it had already been sent, so it may or may not have run"
    )
}

/// A device row's device, by its short name.
pub(crate) fn device_name(server: &McpServer) -> &str {
    crate::devices::short_name(&server.name)
}

impl McpManager {
    /// Call `upstream_tool` on device row `server` as `from`; `exposed` is
    /// the name the caller called.
    pub(in crate::mcp) async fn call_device(
        &self,
        server: &McpServer,
        exposed: &str,
        upstream_tool: String,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
        from: &CallFrom,
    ) -> Result<(CallToolResult, String), CallError> {
        let started = Instant::now();
        // Offline, it is a normal call: `device_send` answers so at once.
        let augment = match self.device_peer(server.id).await {
            Some(p) => self.augment(server, &upstream_tool, &p.peer).await,
            None => Augment::Normal,
        };
        let augmented = match augment {
            Augment::Normal => false,
            Augment::Task => true,
            Augment::Refused(why) => {
                return Err(CallError::Upstream {
                    server: server.name.clone(),
                    detail: why,
                })
            }
        };
        let mut params = CallToolRequestParams::new(upstream_tool);
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }
        if augmented {
            params = params.with_task(TaskMetadata::new());
        }
        let task = augmented.then(|| tasks::meta::device_task(from));
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        match self.device_send(server, request, from, task).await? {
            Ok(answer) => match tasks::answered(answer, augmented) {
                Ok(Answered::Result(result)) => Ok((result, server.name.clone())),
                Ok(Answered::Task(created)) => {
                    self.task_created(server, exposed, created, from, started)
                        .await
                }
                Err(why) => Err(CallError::Upstream {
                    server: server.name.clone(),
                    detail: format!("the device {why}"),
                }),
            },
            Err(e) => Err(CallError::Upstream {
                server: server.name.clone(),
                detail: ServiceError::McpError(e).to_string(),
            }),
        }
    }

    /// Send `request` to device row `server` as `from`, as a call goes
    /// (module doc): with `_meta`, bounded by the row's `timeout_ms`,
    /// cancelled on the device when lmgw stops waiting, and a link that goes
    /// under it reported with why. `Ok(Err(_))` is the device's own JSON-RPC
    /// error, for the caller to word; a `resources/read` passes its code on.
    pub(in crate::mcp) async fn device_request(
        &self,
        server: &McpServer,
        request: ClientRequest,
        from: &CallFrom,
    ) -> Result<Result<ServerResult, ErrorData>, CallError> {
        self.device_send(server, request, from, None).await
    }

    /// [`device_request`](Self::device_request), with `_meta["lmgw/task"]`
    /// when `task` is set (a task-augmented `tools/call`).
    async fn device_send(
        &self,
        server: &McpServer,
        request: ClientRequest,
        from: &CallFrom,
        task: Option<TaskMeta>,
    ) -> Result<Result<ServerResult, ErrorData>, CallError> {
        let device = device_name(server).to_string();
        let Some(super::conn::Peered { peer, calls, .. }) = self.device_peer(server.id).await
        else {
            return Err(self.offline(server).await);
        };
        let in_flight = {
            let conns = self.conns.read().await;
            conns.get(&server.id).map(|c| c.in_flight.clone())
        };
        let _in_flight = in_flight.map(InFlightGuard::new);

        let meta = CallMeta {
            caller: Some(from.caller.clone()),
            approval: from.approval.clone(),
            timeout_ms: Some(server.timeout_ms),
            task,
        };
        let mut options = PeerRequestOptions::no_options();
        options.meta = Some(Meta(meta.to_meta()));
        let handle = match peer.send_cancellable_request(request, options).await {
            Ok(h) => h,
            Err(_) => {
                return Err(CallError::NotConnected {
                    server: server.name.clone(),
                    detail: format!("device '{device}''s host link closed before the call"),
                })
            }
        };
        let id = handle.id.clone();
        let rx = handle.rx;
        // Armed until the call is answered: a caller that stops waiting —
        // its future dropped — cancels it on the device.
        let mut guard = CancelOnDrop {
            peer: Some(peer.clone()),
            id: id.clone(),
        };
        let timeout = Duration::from_millis(server.timeout_ms);
        let outcome = tokio::time::timeout(timeout, rx).await;
        if let Some(c) = self.conns.write().await.get_mut(&server.id) {
            c.last_used = Instant::now();
        }
        guard.disarm();
        let Ok(answer) = outcome else {
            cancel(
                &peer,
                id,
                format!(
                    "lmgw stopped waiting after {} ms (the server's timeout_ms)",
                    server.timeout_ms
                ),
            )
            .await;
            return Err(CallError::Timeout {
                server: server.name.clone(),
                timeout_ms: server.timeout_ms,
            });
        };
        match answer {
            // The session ended under the call: the link went — closed by
            // lmgw for a reason it names, or dropped by the device.
            Err(_) | Ok(Err(ServiceError::TransportClosed)) => Err(CallError::Upstream {
                server: server.name.clone(),
                detail: match calls.closed_by() {
                    Some(why) => cut_off(&device, &why),
                    None => abandoned(&device),
                },
            }),
            Ok(Err(ServiceError::McpError(e))) => Ok(Err(e)),
            Ok(Err(e)) => Err(CallError::Upstream {
                server: server.name.clone(),
                detail: e.to_string(),
            }),
            Ok(Ok(result)) => Ok(Ok(result)),
        }
    }

    /// The refusal of a call on an offline device row: at once (§5.3).
    pub(in crate::mcp) async fn offline(&self, server: &McpServer) -> CallError {
        CallError::NotConnected {
            server: server.name.clone(),
            detail: self.offline_words(server).await,
        }
    }

    /// "device '<name>' is not connected (last seen …)".
    pub(crate) async fn offline_words(&self, server: &McpServer) -> String {
        let device = device_name(server);
        let seen = match self.app() {
            Some(app) => super::conn::last_seen(&app, server).await,
            None => None,
        };
        match seen {
            Some(at) => format!("device '{device}' is not connected (last seen {at})"),
            None => format!("device '{device}' is not connected (never seen)"),
        }
    }
}

/// Send `notifications/cancelled` for `id`, best effort: a link already
/// gone has nobody to tell.
async fn cancel(peer: &Peer<RoleClient>, id: RequestId, reason: String) {
    let note = CancelledNotification::new(CancelledNotificationParam::new(Some(id), Some(reason)));
    let _ = peer
        .send_notification(ClientNotification::CancelledNotification(note))
        .await;
}

/// Cancels the call on the device when dropped armed: its caller stopped
/// waiting.
struct CancelOnDrop {
    peer: Option<Peer<RoleClient>>,
    id: RequestId,
}

impl CancelOnDrop {
    fn disarm(&mut self) {
        self.peer = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(peer) = self.peer.take() else {
            return;
        };
        let id = self.id.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                cancel(
                    &peer,
                    id,
                    "the caller stopped waiting (its turn was cancelled, or it hung up)".into(),
                )
                .await;
            });
        }
    }
}
