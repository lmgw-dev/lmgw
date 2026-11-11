//! [`MainTlsListener`]: the main TLS listener's socket as `axum::serve`
//! serves it (headless server design §4.4). Each accepted connection's
//! handshake runs on a task of its own, so one slow peer never holds
//! another's accept, bounded by `tls.handshake_timeout_s`; the finished
//! ones are handed to axum as they finish. Dropping the listener (the stop
//! ends axum's accept loop) closes the socket at once, aborts every
//! handshake still running and fires `released`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_rustls::server::TlsStream;

use super::MainTls;
use crate::server::conn::{canonical, Conn, Via};

/// How long the accept loop waits after an accept that failed for a reason
/// of the listener's own (the process out of descriptors), so it does not
/// spin: the device listener's and axum's pause for the same case.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);

/// A finished handshake: the stream and its peer.
type Done = Option<(TlsStream<TcpStream>, SocketAddr)>;

/// The main TLS listener (module doc).
pub struct MainTlsListener {
    tcp: TcpListener,
    tls: Arc<MainTls>,
    handshake: Duration,
    pending: JoinSet<Done>,
    released: Option<tokio::sync::oneshot::Sender<()>>,
}

impl MainTlsListener {
    /// Serve `tcp` with `tls`'s pair, each handshake within `handshake`;
    /// `released` fires as it closes.
    pub fn new(
        tcp: TcpListener,
        tls: Arc<MainTls>,
        handshake: Duration,
        released: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Self {
        Self {
            tcp,
            tls,
            handshake,
            pending: JoinSet::new(),
            released,
        }
    }

    fn spawn(&mut self, tcp: TcpStream, peer: SocketAddr) {
        let (tls, within) = (self.tls.clone(), self.handshake);
        self.pending.spawn(async move {
            let _ = tcp.set_nodelay(true);
            // The pair as it is on disk now (D8): a renewed one is served
            // from this connection on. Its check is inside the bound, so a
            // stalled mount costs this connection its handshake time and no
            // more.
            let handshake = async {
                let acceptor = tokio_rustls::TlsAcceptor::from(tls.config().await);
                acceptor.accept(tcp).await
            };
            match tokio::time::timeout(within, handshake).await {
                Ok(Ok(stream)) => Some((stream, peer)),
                Ok(Err(e)) => {
                    tracing::debug!("TLS listener: the handshake with {peer} failed: {e}");
                    None
                }
                Err(_) => {
                    tracing::debug!(
                        "TLS listener: the handshake with {peer} did not finish within {} s \
                         (tls.handshake_timeout_s)",
                        within.as_secs()
                    );
                    None
                }
            }
        });
    }
}

impl Drop for MainTlsListener {
    fn drop(&mut self) {
        self.pending.abort_all();
        if let Some(tx) = self.released.take() {
            let _ = tx.send(());
        }
    }
}

impl axum::serve::Listener for MainTlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, SocketAddr) {
        loop {
            tokio::select! {
                accepted = self.tcp.accept() => match accepted {
                    Ok((tcp, peer)) => {
                        if self.tls.accept_worked() {
                            tracing::info!("TLS listener: accepting connections again");
                        }
                        self.spawn(tcp, peer)
                    }
                    // The peer gave up between its SYN and this accept.
                    Err(e) if is_peer_error(&e) => {
                        tracing::debug!("TLS listener: an accept failed ({e})");
                    }
                    // The listener's own (D36: no cap on handshakes but the
                    // process's descriptors, so running out of them is said
                    // here, in the log once per episode and on Settings
                    // until an accept works again).
                    Err(e) => {
                        if self.tls.accept_failed(&e) {
                            tracing::warn!(
                                "TLS listener: an accept failed ({e}); trying again every {} s \
                                 until one works (Settings → Network & access → TLS shows it \
                                 meanwhile)",
                                ACCEPT_RETRY.as_secs()
                            );
                        } else {
                            tracing::debug!("TLS listener: an accept failed again ({e})");
                        }
                        tokio::time::sleep(ACCEPT_RETRY).await;
                    }
                },
                Some(done) = self.pending.join_next(), if !self.pending.is_empty() => {
                    if let Ok(Some(ready)) = done {
                        return ready;
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.tcp.local_addr()
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

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, MainTlsListener>>
    for Conn
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, MainTlsListener>) -> Self {
        let local = stream
            .io()
            .get_ref()
            .0
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
        Conn {
            via: Via::Tls,
            peer: canonical(*stream.remote_addr()),
            proxy: None,
            forwarded_scheme: None,
            local: canonical(local),
            device: None,
            socket: None,
        }
    }
}
