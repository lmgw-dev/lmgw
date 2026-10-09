//! The link as an rmcp transport (§5.1): what rmcp's client session sends
//! goes to the link task as one serialized JSON-RPC message per frame, and
//! what the link task reads comes back parsed.
//!
//! **The calls open on the link** are kept here ([`OpenCalls`]): a
//! `tools/call` or `resources/read` lmgw sent and the device has not
//! answered yet. The link task
//! reads them when lmgw closes the link (a takeover, a revocation, a stop,
//! the row switched off), and sends `notifications/cancelled` for each
//! before the close (R14): lmgw cancels every forwarded call it stops
//! waiting on.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rmcp::model::{ClientNotification, ClientRequest, JsonRpcMessage, RequestId};
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::RoleClient;
use tokio::sync::mpsc;

/// The `tools/call` and `resources/read` requests open on one link, by
/// request id, and why lmgw closed the link, once it did.
#[derive(Debug, Default, Clone)]
pub(crate) struct OpenCalls(Arc<Mutex<Open>>);

#[derive(Debug, Default)]
struct Open {
    calls: HashMap<String, RequestId>,
    /// The reason lmgw closed the link with (a takeover, a revocation, a
    /// stop, the row switched off): what a call cut off by it reports,
    /// rather than "the device disconnected".
    closed_by: Option<String>,
}

impl OpenCalls {
    fn key(id: &RequestId) -> String {
        id.to_string()
    }

    fn opened(&self, id: &RequestId) {
        self.lock().calls.insert(Self::key(id), id.clone());
    }

    pub(super) fn closed(&self, id: &RequestId) {
        self.lock().calls.remove(&Self::key(id));
    }

    /// lmgw closes the link for `reason`: every call still open, taken —
    /// what the link cancels before it closes — and the reason kept for
    /// the calls to report.
    pub(super) fn close_with(&self, reason: &str) -> Vec<RequestId> {
        let mut open = self.lock();
        open.closed_by = Some(reason.to_string());
        open.calls.drain().map(|(_, id)| id).collect()
    }

    /// Why lmgw closed the link, if it did.
    pub(super) fn closed_by(&self) -> Option<String> {
        self.lock().closed_by.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Open> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The link task's ends of the transport.
pub(super) struct LinkSide {
    /// Serialized messages rmcp sends, one per frame.
    pub(super) outgoing: mpsc::UnboundedReceiver<String>,
    /// Parsed messages the device sent, for rmcp to receive.
    pub(super) incoming: mpsc::UnboundedSender<RxJsonRpcMessage<RoleClient>>,
    pub(super) open: OpenCalls,
}

/// rmcp's end.
pub(super) struct HostTransport {
    outgoing: mpsc::UnboundedSender<String>,
    incoming: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleClient>>,
    open: OpenCalls,
}

/// A transport and the link task's ends of it.
pub(super) fn pair() -> (HostTransport, LinkSide) {
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let open = OpenCalls::default();
    (
        HostTransport {
            outgoing: out_tx,
            incoming: in_rx,
            open: open.clone(),
        },
        LinkSide {
            outgoing: out_rx,
            incoming: in_tx,
            open,
        },
    )
}

/// The link is gone: nothing more can be sent on it.
#[derive(Debug)]
pub(super) struct LinkClosed;

impl std::fmt::Display for LinkClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the device's host link is closed")
    }
}

impl std::error::Error for LinkClosed {}

impl rmcp::transport::Transport<RoleClient> for HostTransport {
    type Error = LinkClosed;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        // Which calls are open, read off what goes out: a `tools/call` or a
        // `resources/read` opens one, a `notifications/cancelled` lmgw sends
        // closes it.
        match &item {
            JsonRpcMessage::Request(r) => {
                if matches!(
                    r.request,
                    ClientRequest::CallToolRequest(_) | ClientRequest::ReadResourceRequest(_)
                ) {
                    self.open.opened(&r.id);
                }
            }
            JsonRpcMessage::Notification(n) => {
                if let ClientNotification::CancelledNotification(c) = &n.notification {
                    if let Some(id) = &c.params.request_id {
                        self.open.closed(id);
                    }
                }
            }
            _ => {}
        }
        let sent = serde_json::to_string(&item)
            .map_err(|_| LinkClosed)
            .and_then(|text| self.outgoing.send(text).map_err(|_| LinkClosed));
        std::future::ready(sent)
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.incoming.recv()
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        // The session ended: the link task sees its outgoing channel close.
        self.incoming.close();
        std::future::ready(Ok(()))
    }
}
