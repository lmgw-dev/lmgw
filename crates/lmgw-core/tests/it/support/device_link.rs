//! A gateway with both listeners for the device link's suites (device link
//! design §13.1): the plain one on an ephemeral port, the device listener
//! armed on a free loopback port, its CA made plain in the test's own data
//! directory (no Secret Service: the gateway's default answers as a
//! headless start), and devices whose certificates the test issues and
//! whose rows it maps by hand ([`MemoryKeys`], enrolment being WP3's).

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use lmgw_core::devices::tls::{self, Ca};
use lmgw_core::devices::MemoryKeys;
use lmgw_core::state::{AppState, SharedState};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::ClientConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::common::{self, Gw};

/// A device's TLS identity: its certificate and its key.
pub type Identity = (CertificateDer<'static>, PrivateKeyDer<'static>);

/// A port free on both loopback addresses now, for TCP and for UDP (the
/// calls' device socket group binds the device listener's addresses and
/// port too): the OS's pick for TCP on `127.0.0.1`, checked free on the
/// other three. Released before it is returned, so another socket could
/// take it meanwhile; the window is a few microseconds and the suites bind
/// it at once.
pub fn free_port() -> u16 {
    for _ in 0..32 {
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = v4.local_addr().unwrap().port();
        let udp4 = match std::net::UdpSocket::bind(("127.0.0.1", port)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let v6 = (
            std::net::TcpListener::bind(("::1", port)),
            std::net::UdpSocket::bind(("::1", port)),
        );
        drop(udp4);
        match v6 {
            (Ok(_), Ok(_)) => return port,
            (Err(e), _) | (_, Err(e)) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            // No IPv6 loopback: the device listener serves 127.0.0.1 alone.
            _ => return port,
        }
    }
    panic!("no port free on both loopback addresses, TCP and UDP, in 32 tries");
}

/// A device row and its certificate.
pub struct Device {
    pub id: i64,
    pub name: String,
    pub key_hash: String,
    pub identity: Identity,
}

/// The gateway.
pub struct DeviceGw {
    pub state: SharedState,
    /// The plain listener.
    pub gw: Gw,
    /// The device listener's port.
    pub port: u16,
    pub ca: Arc<Ca>,
    pub keys: Arc<MemoryKeys>,
    /// The arm's epoch, which a disarm takes back.
    pub epoch: u64,
}

/// Turn the device listener on at `port` (loopback), with `edit` applied to
/// the settings first, and arm it as `server::serve` does; its epoch.
pub async fn arm(
    state: &SharedState,
    port: u16,
    edit: impl FnOnce(&mut lmgw_core::config::Settings),
) -> u64 {
    let mut s = state.snapshot().settings.clone();
    s.devices.listener = true;
    s.devices.port = Some(port);
    edit(&mut s);
    lmgw_core::store::save_settings(&state.db, &s)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    state.devices.listener.arm(state).await
}

/// [`arm`] on a free port: its port and epoch. A port [`free_port`] found
/// free can be taken by another suite before the listener binds it (the
/// suites run side by side): then the next free one is asked for, as the
/// owner would after reading the bind error.
pub async fn arm_free(
    state: &SharedState,
    edit: impl FnOnce(&mut lmgw_core::config::Settings),
) -> (u16, u64) {
    let mut port = free_port();
    let epoch = arm(state, port, edit).await;
    for _ in 0..16 {
        let status = state.devices.listener.status(state);
        match status.state {
            lmgw_api_types::device_link::DeviceListenerState::Listening => return (port, epoch),
            lmgw_api_types::device_link::DeviceListenerState::Error
                if status
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains("Address already in use")) => {}
            _ => panic!("the device listener does not serve: {status:?}"),
        }
        port = free_port();
        let mut s = state.snapshot().settings.clone();
        s.devices.port = Some(port);
        lmgw_core::store::save_settings(&state.db, &s)
            .await
            .unwrap();
        state.reload_snapshot().await.unwrap();
        state.devices.listener.reconcile(state).await;
    }
    panic!("no free port held for the device listener in 16 tries");
}

/// A gateway with both listeners, `edit` applied to its settings.
pub async fn gateway_with(edit: impl FnOnce(&mut lmgw_core::config::Settings)) -> DeviceGw {
    let state = AppState::init_for_tests().await.unwrap();
    let keys = Arc::new(MemoryKeys::default());
    state.devices.keys.set(keys.clone());
    let (port, epoch) = arm_free(&state, edit).await;
    let ca = state
        .devices
        .trust
        .ca()
        .expect("the listener's start made the CA");
    let gw = common::serve(state.clone()).await;
    DeviceGw {
        state,
        gw,
        port,
        ca,
        keys,
        epoch,
    }
}

/// A gateway with both listeners, settings as they come.
pub async fn gateway() -> DeviceGw {
    gateway_with(|_| {}).await
}

/// An identity from `ca` for a fresh EC P-256 key, valid `days` from `now`.
pub fn identity(ca: &Ca, days: u32, now: DateTime<Utc>) -> (Identity, String) {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let spki = rcgen::PublicKeyData::subject_public_key_info(&key);
    let issued = tls::issue_device(ca, &spki, "phone", days, now).unwrap();
    (
        (
            CertificateDer::from(issued.der),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        ),
        issued.key_hash,
    )
}

/// A rustls client that trusts `ca` alone, presenting `identity` if any,
/// speaking `versions`.
pub fn client_config(
    ca: &CertificateDer<'static>,
    identity: Option<&Identity>,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.clone()).unwrap();
    let b = ClientConfig::builder_with_provider(tls::provider())
        .with_protocol_versions(versions)
        .unwrap()
        .with_root_certificates(roots);
    let mut cfg = match identity {
        Some((cert, key)) => b
            .with_client_auth_cert(vec![cert.clone()], key.clone_key())
            .unwrap(),
        None => b.with_no_client_auth(),
    };
    cfg.alpn_protocols = vec![tls::ALPN_HTTP1.to_vec()];
    cfg
}

/// What a raw exchange read: the status, the headers (lower-case names) and
/// everything after them, up to the connection's close or the time limit.
#[derive(Debug)]
pub struct Raw {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// Whether the server closed the connection.
    pub closed: bool,
}

impl Raw {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

impl DeviceGw {
    /// A device row named `name`, enabled, holding a fresh key with a
    /// certificate from the gateway's CA, mapped `confirmed` or not.
    pub async fn device(&self, name: &str, confirmed: bool) -> Device {
        let (identity, key_hash) = identity(&self.ca, 365, Utc::now());
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO api_keys (name, key_hash, enabled, kind) VALUES (?1, ?2, 1, 'device') \
             RETURNING id",
        )
        .bind(format!("device:{name}"))
        .bind(&key_hash)
        .fetch_one(&self.state.db)
        .await
        .unwrap();
        self.state.reload_snapshot().await.unwrap();
        self.keys.set(&key_hash, id, confirmed);
        Device {
            id,
            name: name.to_string(),
            key_hash,
            identity,
        }
    }

    /// `https://127.0.0.1:<port><path>`.
    pub fn url(&self, path: &str) -> String {
        format!("https://127.0.0.1:{}{path}", self.port)
    }

    /// A reqwest client over the device listener, presenting `identity`.
    pub fn https(&self, identity: Option<&Identity>) -> reqwest::Client {
        reqwest::Client::builder()
            .tls_backend_preconfigured(client_config(
                self.ca.certificate(),
                identity,
                &[&rustls::version::TLS13],
            ))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap()
    }

    /// A TLS connection to the device listener at `host` (`127.0.0.1` or
    /// `::1`), presenting `identity`, speaking `versions`. The handshake's
    /// error when the client side refuses; a server's refusal of the client
    /// (TLS 1.3 sends it after the client's Finished) surfaces at the first
    /// read.
    pub async fn connect_with(
        &self,
        host: &str,
        identity: Option<&Identity>,
        versions: &[&'static rustls::SupportedProtocolVersion],
    ) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
        let ip: std::net::IpAddr = host.parse().unwrap();
        let tcp = TcpStream::connect((ip, self.port)).await?;
        let cfg = client_config(self.ca.certificate(), identity, versions);
        let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
        connector
            .connect(ServerName::try_from(host.to_string()).unwrap(), tcp)
            .await
    }

    /// [`Self::connect_with`] on `127.0.0.1`, TLS 1.3.
    pub async fn connect(
        &self,
        identity: Option<&Identity>,
    ) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
        self.connect_with("127.0.0.1", identity, &[&rustls::version::TLS13])
            .await
    }

    /// The `request_logs` rows written so far.
    pub async fn request_rows(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM request_logs")
            .fetch_one(&self.state.db)
            .await
            .unwrap()
    }
}

/// Send `request` (a whole HTTP/1.1 request, `\r\n` line ends) on `stream`
/// and read its answer: until the server closes, or `within` passes.
pub async fn exchange<S>(stream: &mut S, request: &str, within: Duration) -> std::io::Result<Raw>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    read_answer(stream, within).await
}

/// Read one answer from `stream`: until the server closes, or `within`
/// passes after the head arrived with nothing more.
pub async fn read_answer<S>(stream: &mut S, within: Duration) -> std::io::Result<Raw>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut got: Vec<u8> = Vec::new();
    let mut closed = false;
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let mut buf = [0u8; 8192];
        match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
            Ok(Ok(0)) => {
                closed = true;
                break;
            }
            Ok(Ok(n)) => {
                got.extend_from_slice(&buf[..n]);
                if complete(&got) {
                    break;
                }
            }
            Ok(Err(e)) if got.is_empty() => return Err(e),
            Ok(Err(_)) => {
                closed = true;
                break;
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&got).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let chunked = headers
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.eq_ignore_ascii_case("chunked"));
    Ok(Raw {
        status,
        headers,
        body: if chunked {
            dechunk(body)
        } else {
            body.to_string()
        },
        closed,
    })
}

/// A chunked body's data.
fn dechunk(mut body: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.trim(), 16) else {
            break;
        };
        if n == 0 || rest.len() < n {
            break;
        }
        out.push_str(&rest[..n]);
        body = rest[n..].trim_start_matches("\r\n");
    }
    out
}

/// Whether `got` holds a whole answer: its `Content-Length` body, or its
/// chunked body's last chunk.
fn complete(got: &[u8]) -> bool {
    let text = String::from_utf8_lossy(got);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    let header = |name: &str| {
        head.lines()
            .filter_map(|l| l.split_once(':'))
            .find(|(n, _)| n.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
    };
    if header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        return body.ends_with("0\r\n\r\n");
    }
    header("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|n| body.len() >= n)
}

/// Whether `stream` is closed by the server within `within`: a read that
/// returns nothing, or an error.
pub async fn closes_within<S>(stream: &mut S, within: Duration) -> bool
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut buf = [0u8; 1024];
    loop {
        match tokio::time::timeout(within, stream.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return true,
            Ok(Ok(_)) => continue,
            Err(_) => return false,
        }
    }
}

/// A GET request for `path`, with `extra` header lines (each ending `\r\n`).
pub fn get(path: &str, extra: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}\r\n")
}

impl DeviceGw {
    /// A TCP connection to the device listener from loopback address `from`
    /// (`127.0.0.2` stands in for a LAN host), sending nothing.
    pub async fn tcp_from(&self, from: [u8; 4]) -> TcpStream {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind((from, 0).into()).unwrap();
        socket
            .connect(([127, 0, 0, 1], self.port).into())
            .await
            .unwrap()
    }

    /// A TLS connection from loopback address `from`, presenting `identity`.
    pub async fn connect_from(
        &self,
        from: [u8; 4],
        identity: Option<&Identity>,
    ) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
        let tcp = self.tcp_from(from).await;
        let cfg = client_config(self.ca.certificate(), identity, &[&rustls::version::TLS13]);
        tokio_rustls::TlsConnector::from(Arc::new(cfg))
            .connect(ServerName::try_from("127.0.0.1").unwrap(), tcp)
            .await
    }

    /// Wait until the listener holds `n` connections without a certificate.
    pub async fn unauthenticated(&self, n: u64) {
        let reached = tokio::time::timeout(Duration::from_secs(10), async {
            while self
                .state
                .devices
                .listener
                .status(&self.state)
                .unauthenticated
                != n
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let now = self.state.devices.listener.status(&self.state);
        assert!(reached.is_ok(), "wanted {n} unauthenticated: {now:?}");
    }
}
