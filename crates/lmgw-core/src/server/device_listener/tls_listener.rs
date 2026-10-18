//! [`TlsListener`]: the device listener's sockets and their accept tasks
//! (device link design §4.3).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::connection::Serving;

/// How long an accept task waits after an accept that failed for a reason
/// of the listener's own (the process out of descriptors), so it does not
/// spin: axum's serve loop's pause for the same case.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);

/// The device listener's listening sockets (two for `loopback`) and an
/// accept task per socket, which owns it. Dropping the listener aborts the
/// tasks, which closes the sockets; [`Self::close`] also waits until they
/// are closed, so the same port can be bound again at once.
///
/// An accept hands each connection to a task of its own at once: the TLS
/// handshake and everything after it run off the accept loop, so one slow
/// peer never holds another's accept.
pub struct TlsListener {
    addrs: Vec<SocketAddr>,
    accepts: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for TlsListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsListener")
            .field("addrs", &self.addrs)
            .finish()
    }
}

impl TlsListener {
    /// Accept on `sockets`, each connection served by `serving`.
    pub(super) fn start(sockets: Vec<TcpListener>, serving: Arc<Serving>) -> Self {
        let addrs = sockets.iter().filter_map(|s| s.local_addr().ok()).collect();
        let accepts = sockets
            .into_iter()
            .map(|socket| tokio::spawn(accept(socket, serving.clone())))
            .collect();
        Self { addrs, accepts }
    }

    /// The addresses it is bound to.
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// Stop accepting, and wait until every socket is closed.
    pub async fn close(mut self) {
        let accepts = std::mem::take(&mut self.accepts);
        for task in &accepts {
            task.abort();
        }
        for task in accepts {
            // Cancelled: the task, and the socket it owned, are gone.
            let _ = task.await;
        }
    }
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        for task in &self.accepts {
            task.abort();
        }
    }
}

/// One socket's accept loop.
async fn accept(socket: TcpListener, serving: Arc<Serving>) {
    loop {
        match socket.accept().await {
            Ok((tcp, peer)) => serving.spawn(tcp, peer),
            // The peer gave up between its SYN and this accept: its own
            // business, and the next accept is unaffected.
            Err(e) if is_peer_error(&e) => {
                tracing::debug!("device listener: an accept failed ({e})");
            }
            Err(e) => {
                tracing::warn!(
                    "device listener: an accept on {} failed ({e}); trying again in {} s",
                    socket
                        .local_addr()
                        .map_or_else(|_| "its socket".into(), |a| a.to_string()),
                    ACCEPT_RETRY.as_secs()
                );
                tokio::time::sleep(ACCEPT_RETRY).await;
            }
        }
    }
}

/// An accept error that is the connection's, not the listener's.
fn is_peer_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        e.kind(),
        ConnectionRefused | ConnectionAborted | ConnectionReset
    )
}
