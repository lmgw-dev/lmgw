//! One device listener connection, from its accept to its close (device
//! link design §4.2, §4.3), and the connections of one listener start:
//! [`Serving`].

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ConnectInfo;
use axum::http::{header, HeaderValue};
use axum::Router;
use chrono::Utc;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use super::preauth::{PreAuth, Slot};
use crate::devices::tls::ClientAuth;
use crate::server::conn::TrackedStream;
use crate::server::{canonical, Conn, DevicePeer, ServedAt, Via};
use crate::state::SharedState;

/// What one start of the device listener serves with: the device router
/// stamped with its generation, and its connections, which its stop drains
/// and, past the bound, closes.
pub(super) struct Serving {
    state: SharedState,
    app: Router,
    pub(super) served_at: ServedAt,
    /// `true` from the stop on: each connection finishes the request in
    /// progress and closes; an idle one closes at once.
    drain: watch::Sender<bool>,
    next: AtomicU64,
    live: Mutex<HashMap<u64, AbortHandle>>,
    /// How many connections are open, for the drain to wait on.
    open: watch::Sender<usize>,
    /// The connections without a certificate yet, bounded (§4.3).
    pub(super) pre: Arc<PreAuth>,
}

impl Serving {
    pub(super) fn new(state: SharedState, app: Router, served_at: ServedAt) -> Arc<Self> {
        Arc::new(Self {
            state,
            app,
            served_at,
            drain: watch::Sender::new(false),
            next: AtomicU64::new(0),
            live: Mutex::default(),
            open: watch::Sender::new(0),
            pre: Arc::default(),
        })
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<u64, AbortHandle>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Serve the connection `tcp` from `peer` on a task of its own, with a
    /// place among the connections that have not authenticated: at a bound
    /// the oldest of them is closed to make room (§4.3, `preauth`).
    pub(super) fn spawn(self: &Arc<Self>, tcp: TcpStream, peer: SocketAddr) {
        let (max, per_peer) = {
            let snap = self.state.snapshot();
            let d = &snap.settings.devices;
            (d.unauthenticated_max, d.unauthenticated_per_peer)
        };
        let slot = self.pre.admit(peer.ip(), max, per_peer);
        let evicted = slot.evicted();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut live = self.live();
        let this = self.clone();
        let task = tokio::spawn(async move {
            let _gone = Gone {
                serving: this.clone(),
                id,
            };
            tokio::select! {
                () = this.connection(tcp, peer, slot) => {}
                () = evicted => tracing::debug!(
                    "device listener: the connection from {peer}, not authenticated, was closed \
                     to make room for a newer one"
                ),
            }
        });
        live.insert(id, task.abort_handle());
        self.open.send_replace(live.len());
    }

    /// Its open connections now, handshakes included.
    pub(super) fn connections(&self) -> usize {
        *self.open.borrow()
    }

    /// Let every connection finish the request it serves and close.
    pub(super) fn drain(&self) {
        self.drain.send_replace(true);
    }

    /// Wait, at most `within`, until every connection has closed; whether
    /// they all did.
    pub(super) async fn drained(&self, within: Duration) -> bool {
        let mut open = self.open.subscribe();
        let done = tokio::time::timeout(within, async {
            let _ = open.wait_for(|n| *n == 0).await;
        })
        .await;
        done.is_ok()
    }

    /// Close every connection still open now; how many there were.
    pub(super) fn close_all(&self) -> usize {
        let live = std::mem::take(&mut *self.live());
        for task in live.values() {
            task.abort();
        }
        self.open.send_replace(0);
        live.len()
    }

    /// One connection: the handshake, then the requests. `slot` is its
    /// place among the connections that have not authenticated, given back
    /// when its certificate opens a working device, else held until it ends.
    async fn connection(&self, tcp: TcpStream, peer: SocketAddr, mut slot: Slot) {
        let accepted = Instant::now();
        let peer = canonical(peer);
        let st = &self.state;
        let d = st.snapshot().settings.devices.clone();
        // An answer is never held back for the next one (§4.3).
        let _ = tcp.set_nodelay(true);
        let local = match tcp.local_addr() {
            Ok(a) => canonical(a),
            Err(e) => {
                tracing::debug!("device listener: a connection from {peer} went at once ({e})");
                return;
            }
        };
        let Some(ca) = st.devices.trust.ca() else {
            tracing::warn!(
                "device listener: the connection from {peer} is closed: the device CA is not \
                 loaded"
            );
            return;
        };
        let now = Utc::now();
        // Optional only while a pairing token is pending (§4.2, D11).
        let auth = if st.devices.pending.any(now) {
            ClientAuth::Optional
        } else {
            ClientAuth::Mandatory
        };
        let leaf = match st
            .devices
            .trust
            .leaves
            .leaf(&ca, local.ip(), d.server_cert_days, now)
        {
            Ok(leaf) => leaf,
            Err(e) => {
                tracing::warn!(
                    "device listener: no server certificate for {local} ({e}); the connection \
                     from {peer} is closed"
                );
                return;
            }
        };
        let acceptor = tokio_rustls::TlsAcceptor::from(leaf.config(auth));
        // A connection that may come without a certificate is bounded as a
        // whole from its accept (§4.3): its handshake runs inside that bound
        // too, so the bound is the whole connection's, never handshake plus
        // bound.
        let lapses = accepted + Duration::from_secs(u64::from(d.unauthenticated_timeout_s));
        let mut handshake_ends = accepted + Duration::from_secs(u64::from(d.handshake_timeout_s));
        let mut bound = "devices.handshake_timeout_s";
        if auth == ClientAuth::Optional && lapses < handshake_ends {
            handshake_ends = lapses;
            bound = "devices.unauthenticated_timeout_s";
        }
        let accept = acceptor.accept(TrackedStream::new(tcp));
        // Raced against the listener's stop: a handshake has no request in
        // progress, so a stop ends it at once instead of waiting it out.
        let mut drain = self.drain.subscribe();
        let handshake = tokio::select! {
            done = tokio::time::timeout_at(handshake_ends, accept) => done,
            _ = drain.wait_for(|stopping| *stopping) => {
                tracing::debug!(
                    "device listener: the handshake with {peer} ended at the listener's stop"
                );
                return;
            }
        };
        let tls = match handshake {
            Ok(Ok(tls)) => tls,
            Ok(Err(e)) => {
                tracing::debug!("device listener: the TLS handshake with {peer} failed: {e}");
                return;
            }
            Err(_) => {
                tracing::debug!(
                    "device listener: the TLS handshake with {peer} did not finish within its \
                     bound ({bound})"
                );
                return;
            }
        };
        // Read once per connection (§4.5); the verifier has checked its
        // chain, purpose and dates already.
        let device = match tls.get_ref().1.peer_certificates().and_then(|c| c.first()) {
            None => None,
            Some(der) => match DevicePeer::read(der) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!("device listener: {e}; the connection from {peer} is closed");
                    return;
                }
            },
        };
        // Authenticated when the certificate opens a working device: then
        // its place is free for the next connection, and it gets the handle
        // to its socket (the feed's `TCP_USER_TIMEOUT`, §7.1), a second
        // descriptor only for such a connection. A certificate whose key no
        // row holds, or whose row is disabled, unconfirmed or expired, keeps
        // its place and the lifetime of a connection without one; the gate
        // still answers each request with the reason.
        let authenticated = device
            .as_ref()
            .is_some_and(|p| opens_a_working_device(st, p));
        let socket = authenticated.then(|| {
            slot.release();
            tls.get_ref().0.handle()
        });
        let conn = Conn {
            via: Via::Devices,
            peer,
            local,
            device,
            socket,
        };
        // Not authenticated: the whole connection is bounded (§4.3).
        let lapses = (!authenticated).then_some(lapses);
        let header = Duration::from_secs(u64::from(d.header_timeout_s));
        self.serve(TokioIo::new(tls), conn, authenticated, header, lapses)
            .await;
    }

    /// Serve the requests of `io` until it closes, the drain closes it, or
    /// it lapses.
    async fn serve<I>(
        &self,
        io: I,
        conn: Conn,
        authenticated: bool,
        header: Duration,
        lapses: Option<Instant>,
    ) where
        I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let peer = conn.peer;
        let close = !authenticated;
        let app = self.app.clone();
        let service = hyper::service::service_fn(move |mut req: hyper::Request<_>| {
            let (app, conn) = (app.clone(), conn.clone());
            async move {
                req.extensions_mut().insert(ConnectInfo(conn));
                if close {
                    // Never upgraded: a WebSocket would outlive both the
                    // place and the lapse that bound this connection. Its
                    // route then answers as a request that cannot upgrade.
                    req.extensions_mut().remove::<hyper::upgrade::OnUpgrade>();
                }
                let resp = tower::ServiceExt::oneshot(app, req.map(axum::body::Body::new)).await;
                let Ok(mut resp) = resp;
                if close {
                    // Every answer to a connection that has not
                    // authenticated ends it (§4.7): its next request, if
                    // any, comes on a new connection.
                    resp.headers_mut()
                        .insert(header::CONNECTION, HeaderValue::from_static("close"));
                }
                Ok::<_, Infallible>(resp)
            }
        });
        let mut builder = hyper::server::conn::http1::Builder::new();
        // hyper starts the timer whenever it waits for a request's head:
        // a slow head and a kept-alive connection's wait for the next one.
        builder.timer(TokioTimer::new()).header_read_timeout(header);
        let served = builder.serve_connection(io, service).with_upgrades();
        tokio::pin!(served);
        let mut drain = self.drain.subscribe();
        let mut draining = *drain.borrow_and_update();
        if draining {
            served.as_mut().graceful_shutdown();
        }
        let lapse = async move {
            match lapses {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(lapse);
        loop {
            tokio::select! {
                done = served.as_mut() => {
                    if let Err(e) = done {
                        tracing::debug!("device listener: the connection from {peer} ended: {e}");
                    }
                    return;
                }
                changed = drain.changed(), if !draining => {
                    if changed.is_err() || *drain.borrow_and_update() {
                        draining = true;
                        served.as_mut().graceful_shutdown();
                    }
                }
                () = &mut lapse => {
                    tracing::debug!(
                        "device listener: the connection from {peer}, not authenticated, is \
                         closed after devices.unauthenticated_timeout_s"
                    );
                    return;
                }
            }
        }
    }
}

/// Whether `peer`'s certificate opens a working device as the snapshot says
/// now: a row holds its key, enabled, confirmed and not past its
/// `expires_at`.
fn opens_a_working_device(state: &SharedState, peer: &DevicePeer) -> bool {
    let snap = state.snapshot();
    match state.devices.keys.find(&snap, &peer.key_hash()) {
        crate::devices::KeyMatch::Found {
            key,
            confirmed: true,
        } => key.enabled && crate::policy::check_expiry(&key, Utc::now()).is_ok(),
        _ => false,
    }
}

/// Takes a connection off the open ones as its task ends, however it ends.
struct Gone {
    serving: Arc<Serving>,
    id: u64,
}

impl Drop for Gone {
    fn drop(&mut self) {
        let mut live = self.serving.live();
        if live.remove(&self.id).is_some() {
            self.serving.open.send_replace(live.len());
        }
    }
}
