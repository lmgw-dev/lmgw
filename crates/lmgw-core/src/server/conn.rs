//! What a request knows about the connection it came in on (device link
//! design §4.5): one connect-info type for both listeners, [`Conn`].
//!
//! - **`via`**: the plain listener (`bind_addr`, http), the main TLS
//!   listener (`tls.listen`, https, headless server design §4) or the device
//!   listener (`devices.listen`, https with client certificates);
//! - **`peer`**: the client: the TCP peer, or, behind a trusted proxy
//!   (headless server design §4.5), the client address the proxy forwarded
//!   — what the agent proxy forwards as `X-Forwarded-For` (origins §4.6),
//!   and what every reader of a client's address reads;
//! - **`proxy`** and **`forwarded_scheme`**: behind a trusted proxy, its own
//!   TCP address and the scheme it said the client used
//!   ([`super::forwarded`]), so [`Conn::scheme`] is the client's;
//! - **`local`**: the local address the connection arrived on, made
//!   canonical (an IPv4 client of a `[::]` listener reads as IPv4): a
//!   WebRTC call's candidate (calls §3.2) and the device listener's choice of
//!   server certificate (§2.2);
//! - **`device`**: the client certificate the device listener's handshake
//!   verified, read once per connection ([`DevicePeer`]);
//! - **`socket`**: a handle to the connection's socket options, so the Chat
//!   feed and a streamed chat answer can set their keep-alive and
//!   `TCP_USER_TIMEOUT` for as long as they run (§4.3, §7.1). The
//!   device listener's authenticated connections (a certificate that opens
//!   a working device) carry one; the plain listener's do not (no device
//!   connects there, D12), nor any other: a handle costs a descriptor.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

/// The scheme a listener speaks and a request came in with (§4.3).
mod scheme;
pub use scheme::scheme_of;
/// Whether a request came straight from this machine's loopback (§4.5).
mod loopback;

/// Which listener a connection came in on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Via {
    /// `bind_addr`, plain HTTP: local OpenAI clients, keys and the
    /// dashboard's cookie.
    #[default]
    Plain,
    /// The main TLS listener (`tls.listen`, headless server design §4.3):
    /// the same router as the plain one, over TLS 1.2 or 1.3 with the
    /// administrator's certificate.
    Tls,
    /// The device listener: TLS 1.3, a device's client certificate.
    Devices,
}

impl Via {
    /// How a log line names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Tls => "tls",
            Self::Devices => "devices",
        }
    }
}

impl std::fmt::Display for Via {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The connection a request came in on. `ConnectInfo<Conn>` on every request
/// of both listeners.
#[derive(Debug, Clone)]
pub struct Conn {
    pub via: Via,
    /// The client, canonical: the TCP peer, or behind a trusted proxy the
    /// client address it forwarded (`proxy` then holds the TCP peer).
    pub peer: SocketAddr,
    /// The trusted proxy's TCP address, when the request came through one
    /// (headless server design §4.5); `None` for a direct connection.
    pub proxy: Option<SocketAddr>,
    /// The scheme a trusted proxy said the client used; `None` when no
    /// proxy said one, and the listener's own stands ([`Conn::scheme`]).
    pub forwarded_scheme: Option<crate::net::Scheme>,
    /// The local address the connection arrived on, canonical.
    pub local: SocketAddr,
    /// The device listener's verified client certificate, if the client
    /// sent one; `None` on the plain listener.
    pub device: Option<DevicePeer>,
    /// The connection's socket options, while it lives; `None` on the plain
    /// listener and for a connection that has not authenticated.
    pub socket: Option<SocketHandle>,
}

impl Conn {
    /// A plain-listener connection between `peer` and `local`.
    pub fn plain(peer: SocketAddr, local: SocketAddr) -> Self {
        Self {
            via: Via::Plain,
            peer: canonical(peer),
            proxy: None,
            forwarded_scheme: None,
            local: canonical(local),
            device: None,
            socket: None,
        }
    }

    /// The plain listener's connect info for a stream it accepted. A stream
    /// whose local address cannot be read (it closed between the accept and
    /// here) reads as `0.0.0.0:0`, which no candidate or certificate rule
    /// takes for anything.
    fn of_plain_stream(io: &TcpStream, peer: SocketAddr) -> Self {
        let local = io
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
        Self::plain(peer, local)
    }
}

/// `addr` with an IPv4-mapped address unmapped.
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

impl
    axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>>
    for Conn
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        Self::of_plain_stream(stream.io(), *stream.remote_addr())
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, PlainListener>>
    for Conn
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, PlainListener>) -> Self {
        Self::of_plain_stream(stream.io(), *stream.remote_addr())
    }
}

/// The plain listener's socket as `server::serve` serves it: `released`
/// fires as it closes, which axum does when the stop ends its accept loop —
/// before the open connections have drained. A restart that binds the same
/// port again waits for it instead of racing the old listener.
pub struct PlainListener {
    inner: tokio::net::TcpListener,
    released: Option<tokio::sync::oneshot::Sender<()>>,
}

impl PlainListener {
    pub fn new(
        inner: tokio::net::TcpListener,
        released: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Self {
        Self { inner, released }
    }
}

impl Drop for PlainListener {
    fn drop(&mut self) {
        if let Some(tx) = self.released.take() {
            let _ = tx.send(());
        }
    }
}

impl axum::serve::Listener for PlainListener {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (TcpStream, SocketAddr) {
        axum::serve::Listener::accept(&mut self.inner).await
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// The client certificate a device listener's handshake verified (§4.5,
/// D5): what the principal is resolved from on every request of the
/// connection.
#[derive(Clone, PartialEq, Eq)]
pub struct DevicePeer {
    /// The SHA-256 of the certificate's DER SubjectPublicKeyInfo: the
    /// device's key, which a device row's `key_hash` names
    /// ([`Self::key_hash`]).
    pub spki_sha256: [u8; 32],
    /// The certificate's serial, lowercase hex of the serial's bytes without a DER sign byte,
    /// as `devices::tls::DeviceCertificate::serial` writes it.
    pub serial: String,
    /// The certificate's `notAfter`, checked on every request (D5).
    pub not_after: DateTime<Utc>,
}

impl std::fmt::Debug for DevicePeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevicePeer")
            .field("key", &self.confirm_code())
            .field("serial", &self.serial)
            .field("not_after", &self.not_after)
            .finish()
    }
}

impl DevicePeer {
    /// The end-entity certificate `der`, read: its key's hash, its serial
    /// and its `notAfter`. Read with x509-parser, which reads all three
    /// (rustls-webpki keeps the validity `pub(crate)`).
    pub fn read(der: &[u8]) -> Result<Self, String> {
        let (_, cert) = x509_parser::parse_x509_certificate(der)
            .map_err(|e| format!("the client certificate does not parse: {e}"))?;
        let tbs = &cert.tbs_certificate;
        let spki_sha256: [u8; 32] = Sha256::digest(tbs.subject_pki.raw).into();
        let raw = tbs.raw_serial();
        // Only DER's sign byte (a 00 before a byte >= 0x80) is not part of the
        // serial; `devices::tls::ca::serial_from` writes serials that never
        // start with 00, so this is exactly the hex it stores.
        let start = usize::from(raw.len() > 1 && raw[0] == 0 && raw[1] >= 0x80);
        let serial = hex::encode(&raw[start..]);
        let not_after = DateTime::from_timestamp(tbs.validity.not_after.timestamp(), 0)
            .ok_or_else(|| "the client certificate's notAfter is out of range".to_string())?;
        Ok(Self {
            spki_sha256,
            serial,
            not_after,
        })
    }

    /// The device row's `key_hash` for this key (D5): `spki:` and the hex
    /// SHA-256 of its SubjectPublicKeyInfo, as enrolment writes it
    /// (`devices::tls::key_hash`).
    pub fn key_hash(&self) -> String {
        format!("spki:{}", hex::encode(self.spki_sha256))
    }

    /// The key's confirmation code (§3.5), `7F3A-91C2`.
    pub fn confirm_code(&self) -> String {
        lmgw_api_types::device_link::confirm_code(&self.spki_sha256)
    }

    /// Whether the certificate has lapsed at `now`.
    pub fn lapsed(&self, now: DateTime<Utc>) -> bool {
        now > self.not_after
    }
}

/// A handle to one connection's socket options, valid while the connection
/// lives (§4.5, §7.1).
///
/// It holds a duplicate of the socket's descriptor, closed by the stream's
/// own drop just before the stream's descriptor ([`TrackedStream`]), under
/// the lock a setter holds: so a setter never reaches a descriptor number
/// the process has since given to another file, and the duplicate never
/// keeps a closed connection's socket open.
///
/// Its timers are set for as long as a stream runs, never for good
/// ([`Self::override_timers`]): a stream's own values hold while its
/// [`TimersOverride`] lives, and the connection's accept-time values are
/// back once the last one on it is gone (2026-10-24).
#[derive(Clone)]
pub struct SocketHandle(Arc<Mutex<Shared>>);

/// What a [`SocketHandle`] shares with its stream: the descriptor, and the
/// timer overrides in force on it.
#[derive(Default)]
struct Shared {
    fd: Option<SocketFd>,
    /// The connection's timers before the first override, read then: what
    /// the last one gone restores.
    base: Option<Applied>,
    /// The overrides in force, oldest first, each by its id.
    overrides: Vec<(u64, Override)>,
    next: u64,
}

/// The timers a socket carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Applied {
    idle: Duration,
    interval: Duration,
    user_timeout: Option<Duration>,
}

/// One stream's timers: each `None` leaves what the connection has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Override {
    keepalive: Option<Duration>,
    user_timeout: Option<Duration>,
}

impl Shared {
    /// The base with every override in force laid over it, oldest first, so
    /// the newest one's fields win.
    fn wanted(&self) -> Option<Applied> {
        let mut t = self.base?;
        for (_, o) in &self.overrides {
            if let Some(every) = o.keepalive {
                t.idle = every;
                t.interval = every;
            }
            if let Some(u) = o.user_timeout {
                t.user_timeout = Some(u);
            }
        }
        Some(t)
    }

    /// Put [`Self::wanted`] on the socket; nothing once it has closed.
    fn apply(&self) -> std::io::Result<()> {
        let (Some(fd), Some(t)) = (self.fd.as_ref(), self.wanted()) else {
            return Ok(());
        };
        write_timers(fd, t)
    }
}

/// The handle's own copy of the connection's socket.
#[cfg(unix)]
type SocketFd = std::os::fd::OwnedFd;
#[cfg(windows)]
type SocketFd = std::os::windows::io::OwnedSocket;

#[cfg(unix)]
fn dup_socket(s: &TcpStream) -> std::io::Result<SocketFd> {
    use std::os::fd::AsFd;
    s.as_fd().try_clone_to_owned()
}

#[cfg(windows)]
fn dup_socket(s: &TcpStream) -> std::io::Result<SocketFd> {
    use std::os::windows::io::AsSocket;
    s.as_socket().try_clone_to_owned()
}

#[cfg(target_os = "linux")]
fn read_timers(fd: &SocketFd) -> std::io::Result<Applied> {
    let s = socket2::SockRef::from(fd);
    Ok(Applied {
        idle: s.tcp_keepalive_time()?,
        interval: s.tcp_keepalive_interval()?,
        user_timeout: s.tcp_user_timeout()?,
    })
}

#[cfg(target_os = "linux")]
fn write_timers(fd: &SocketFd, t: Applied) -> std::io::Result<()> {
    let s = socket2::SockRef::from(fd);
    s.set_tcp_keepalive(
        &socket2::TcpKeepalive::new()
            .with_time(t.idle)
            .with_interval(t.interval),
    )?;
    s.set_tcp_user_timeout(t.user_timeout)
}

#[cfg(target_os = "linux")]
fn read_user_timeout(fd: &SocketFd) -> std::io::Result<Option<Duration>> {
    socket2::SockRef::from(fd).tcp_user_timeout()
}

/// Windows (Windows design §4.4, WP6): the keep-alive times are read back
/// with `TCP_KEEPIDLE` and `TCP_KEEPINTVL` (Windows 10 1709 and later) and set
/// through `socket2` (`SIO_KEEPALIVE_VALS`). Windows has no
/// `TCP_USER_TIMEOUT`: a socket reads as having none, and a stream that asks
/// for one gets its keep-alive alone, which the log says once per process.
#[cfg(windows)]
fn read_timers(fd: &SocketFd) -> std::io::Result<Applied> {
    use windows_sys::Win32::Networking::WinSock::{TCP_KEEPIDLE, TCP_KEEPINTVL};
    Ok(Applied {
        idle: Duration::from_secs(tcp_seconds(fd, TCP_KEEPIDLE)?),
        interval: Duration::from_secs(tcp_seconds(fd, TCP_KEEPINTVL)?),
        user_timeout: None,
    })
}

/// An `IPPROTO_TCP` option that holds seconds as a `DWORD`.
#[cfg(windows)]
fn tcp_seconds(fd: &SocketFd, option: i32) -> std::io::Result<u64> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{getsockopt, IPPROTO_TCP, SOCKET_ERROR};
    let mut value: u32 = 0;
    let mut len = std::mem::size_of::<u32>() as i32;
    // SAFETY: the socket is open while `fd` is; `value` and `len` describe
    // a buffer of the option's size.
    let rc = unsafe {
        getsockopt(
            fd.as_raw_socket() as usize,
            IPPROTO_TCP,
            option,
            (&mut value as *mut u32).cast(),
            &mut len,
        )
    };
    if rc == SOCKET_ERROR {
        return Err(std::io::Error::last_os_error());
    }
    Ok(u64::from(value))
}

#[cfg(windows)]
fn write_timers(fd: &SocketFd, t: Applied) -> std::io::Result<()> {
    let s = socket2::SockRef::from(fd);
    s.set_tcp_keepalive(
        &socket2::TcpKeepalive::new()
            .with_time(t.idle)
            .with_interval(t.interval),
    )?;
    if let Some(wanted) = t.user_timeout {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            tracing::info!(
                "a stream asked for TCP_USER_TIMEOUT ({wanted:?}), which Windows does not have: \
                 its connection gets the keep-alive alone, so a peer that vanished while data \
                 was in flight is found by the keep-alive probes and the system's retransmission \
                 limit (said once per process)"
            )
        });
    }
    Ok(())
}

#[cfg(windows)]
fn read_user_timeout(_fd: &SocketFd) -> std::io::Result<Option<Duration>> {
    Ok(None)
}

/// Neither Linux nor Windows (macOS is not built yet): a stream's timers are
/// refused, which its caller logs and serves on without.
#[cfg(not(any(target_os = "linux", windows)))]
fn timers_unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "a stream's TCP keep-alive and TCP_USER_TIMEOUT are not supported on this system yet",
    )
}

#[cfg(not(any(target_os = "linux", windows)))]
fn read_timers(_fd: &SocketFd) -> std::io::Result<Applied> {
    Err(timers_unsupported())
}

#[cfg(not(any(target_os = "linux", windows)))]
fn write_timers(_fd: &SocketFd, _t: Applied) -> std::io::Result<()> {
    Err(timers_unsupported())
}

#[cfg(not(any(target_os = "linux", windows)))]
fn read_user_timeout(_fd: &SocketFd) -> std::io::Result<Option<Duration>> {
    Err(timers_unsupported())
}

impl std::fmt::Debug for SocketHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let open = self.0.lock().map(|g| g.fd.is_some()).unwrap_or(false);
        f.debug_struct("SocketHandle").field("open", &open).finish()
    }
}

fn closed() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotConnected,
        "the connection has closed",
    )
}

impl SocketHandle {
    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Set the connection's TCP keep-alive (the first probe after
    /// `keepalive` idle, then one each `keepalive`) and its
    /// `TCP_USER_TIMEOUT` (how long sent data or probes may stay
    /// unacknowledged before the kernel ends the connection) for as long as
    /// the returned guard lives; `None` leaves the one the connection has.
    /// Dropped, the guard puts back what the connection had before it —
    /// the other overrides still in force laid over its accept-time values.
    /// An error once the connection has closed, or when the kernel refuses
    /// an option (nothing is left changed then).
    pub fn override_timers(
        &self,
        keepalive: Option<Duration>,
        user_timeout: Option<Duration>,
    ) -> std::io::Result<TimersOverride> {
        let mut g = self.lock();
        let fd = g.fd.as_ref().ok_or_else(closed)?;
        if g.base.is_none() {
            g.base = Some(read_timers(fd)?);
        }
        let id = g.next;
        g.next += 1;
        g.overrides.push((
            id,
            Override {
                keepalive,
                user_timeout,
            },
        ));
        if let Err(e) = g.apply() {
            g.overrides.pop();
            let _ = g.apply();
            return Err(e);
        }
        Ok(TimersOverride {
            shared: self.0.clone(),
            id,
        })
    }

    /// The connection's `TCP_USER_TIMEOUT` now.
    pub fn tcp_user_timeout(&self) -> std::io::Result<Option<Duration>> {
        let g = self.lock();
        let fd = g.fd.as_ref().ok_or_else(closed)?;
        read_user_timeout(fd)
    }

    /// Whether the connection is still open.
    pub fn is_open(&self) -> bool {
        self.0.lock().map(|g| g.fd.is_some()).unwrap_or(false)
    }
}

/// One stream's timers on its connection ([`SocketHandle::override_timers`]),
/// in force until it is dropped.
pub struct TimersOverride {
    shared: Arc<Mutex<Shared>>,
    id: u64,
}

impl std::fmt::Debug for TimersOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimersOverride")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for TimersOverride {
    fn drop(&mut self) {
        let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        g.overrides.retain(|(id, _)| *id != self.id);
        if let Err(e) = g.apply() {
            tracing::warn!("a stream's TCP timers were not put back on its connection: {e}");
        }
    }
}

/// A TCP stream that hands out a [`SocketHandle`] and closes it as it goes.
pub(crate) struct TrackedStream {
    inner: TcpStream,
    handle: Arc<Mutex<Shared>>,
}

impl TrackedStream {
    /// `inner`, with no handle yet: a connection holds one descriptor until
    /// [`Self::handle`] is asked (after a handshake that presented a
    /// certificate), so one that has not authenticated costs no second one.
    pub(crate) fn new(inner: TcpStream) -> Self {
        Self {
            inner,
            handle: Arc::new(Mutex::new(Shared::default())),
        }
    }

    /// A handle to its socket, duplicating the descriptor the first time. A
    /// descriptor that cannot be duplicated (the process is at its limit)
    /// leaves the handle closed, and the connection serves on without one.
    pub(crate) fn handle(&self) -> SocketHandle {
        let mut shared = self.handle.lock().unwrap_or_else(|e| e.into_inner());
        if shared.fd.is_none() {
            shared.fd = dup_socket(&self.inner).ok();
        }
        SocketHandle(self.handle.clone())
    }
}

impl Drop for TrackedStream {
    fn drop(&mut self) {
        // Before `inner` closes: under the lock, so a setter in progress
        // finishes first and none starts after.
        self.handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fd
            .take();
    }
}

impl AsyncRead for TrackedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for TrackedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows: a stream's keep-alive is set and read back, the asked
    /// TCP_USER_TIMEOUT is not applied (Windows has none), and the
    /// accept-time times are back once the stream's override is gone.
    #[cfg(windows)]
    #[tokio::test]
    async fn the_keep_alive_is_set_and_restored_on_windows() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server, _) = listener.accept().await.unwrap();
        let _client = client.await.unwrap();
        let s = Duration::from_secs;
        socket2::SockRef::from(&server)
            .set_tcp_keepalive(
                &socket2::TcpKeepalive::new()
                    .with_time(s(23))
                    .with_interval(s(23)),
            )
            .unwrap();
        let tracked = TrackedStream::new(server);
        let handle = tracked.handle();
        let now = || read_timers(handle.lock().fd.as_ref().unwrap()).unwrap();
        assert_eq!(
            now(),
            Applied {
                idle: s(23),
                interval: s(23),
                user_timeout: None
            }
        );
        let over = handle.override_timers(Some(s(41)), Some(s(41))).unwrap();
        assert_eq!((now().idle, now().interval), (s(41), s(41)));
        assert_eq!(handle.tcp_user_timeout().unwrap(), None);
        drop(over);
        assert_eq!((now().idle, now().interval), (s(23), s(23)));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_socket_handle_closes_with_its_stream() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server, _) = listener.accept().await.unwrap();
        let _client = client.await.unwrap();
        let tracked = TrackedStream::new(server);
        let handle = tracked.handle();
        assert!(handle.is_open());
        // Asked again: the same descriptor, no second one.
        let again = tracked.handle();
        assert!(Arc::ptr_eq(&handle.0, &again.0));
        let over = handle
            .override_timers(None, Some(Duration::from_secs(7)))
            .unwrap();
        assert_eq!(
            handle.tcp_user_timeout().unwrap(),
            Some(Duration::from_secs(7))
        );
        assert!(handle.is_open());
        drop(tracked);
        assert!(!handle.is_open());
        // Dropped after its connection: nothing to put back, no panic.
        drop(over);
        let e = handle
            .override_timers(None, Some(Duration::from_secs(7)))
            .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotConnected);
    }

    /// Two streams on one connection: each one's timers hold while it
    /// runs, the newest's fields winning, and once both are gone the
    /// connection has its accept-time values again, whichever ended first.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn overrides_stack_and_the_last_gone_restores_the_accept_time_timers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server, _) = listener.accept().await.unwrap();
        let _client = client.await.unwrap();
        let s = Duration::from_secs;
        socket2::SockRef::from(&server)
            .set_tcp_keepalive(
                &socket2::TcpKeepalive::new()
                    .with_time(s(23))
                    .with_interval(s(23)),
            )
            .unwrap();
        let tracked = TrackedStream::new(server);
        let handle = tracked.handle();
        let now = || {
            let s = socket2::SockRef::from(&tracked.inner);
            (
                s.tcp_keepalive_time().unwrap(),
                s.tcp_keepalive_interval().unwrap(),
                s.tcp_user_timeout().unwrap(),
            )
        };
        assert_eq!(now(), (s(23), s(23), None));

        let feed = handle.override_timers(Some(s(37)), Some(s(37))).unwrap();
        assert_eq!(now(), (s(37), s(37), Some(s(37))));
        let chat = handle.override_timers(None, Some(s(46))).unwrap();
        assert_eq!(now(), (s(37), s(37), Some(s(46))), "the newest wins");
        drop(feed);
        assert_eq!(
            now(),
            (s(23), s(23), Some(s(46))),
            "the chat stream's alone"
        );
        drop(chat);
        assert_eq!(now(), (s(23), s(23), None), "the accept-time timers");
    }

    #[test]
    fn an_ipv4_mapped_address_reads_as_ipv4() {
        let mapped: SocketAddr = "[::ffff:203.0.113.5]:443".parse().unwrap();
        assert_eq!(canonical(mapped), "203.0.113.5:443".parse().unwrap());
    }
}
