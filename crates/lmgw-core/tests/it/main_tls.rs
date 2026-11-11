//! The main TLS listener (headless server design §4, D7–D9, D22): a test PKI
//! generated per test (never a committed key), a gateway serving its plain
//! and its TLS listener on loopback, and clients that trust only the test CA.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lmgw_core::config::Settings;
use lmgw_core::state::{AppState, SharedState};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, SerialNumber};

/// The agent proxy on the TLS listener: `X-Forwarded-Proto` (D33).
// Starts an agent (crate::agents_service's), which is not on Windows
// (Windows design §4.5).
#[cfg(unix)]
mod agent_proxy;
/// The listener: TLS versions, no client certificate, the reload (D7, D8).
/// Unix: it sets the key's mode and reads the files' inodes.
#[cfg(unix)]
mod listener;
/// What follows the scheme: the cookie, the origin rules, the URL builders.
mod scheme;
/// The TLS WebRTC group (D22).
mod webrtc;

/// A test CA and the directory its leaf pairs are written to.
pub struct Pki {
    issuer: Issuer<'static, KeyPair>,
    pub ca_der: rustls::pki_types::CertificateDer<'static>,
    pub dir: tempfile::TempDir,
}

impl Pki {
    pub fn new() -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            "lmgw test CA (generated per test)",
        );
        let ca = params.self_signed(&key).unwrap();
        Self {
            ca_der: ca.der().clone(),
            issuer: Issuer::new(params, key),
            dir: tempfile::tempdir().unwrap(),
        }
    }

    pub fn cert_path(&self) -> PathBuf {
        self.dir.path().join("tls.crt")
    }

    pub fn key_path(&self) -> PathBuf {
        self.dir.path().join("tls.key")
    }

    /// A leaf for `names` with serial `serial`, as PEM (the chain: leaf, CA)
    /// and its key.
    pub fn leaf(&self, names: &[&str], serial: u8) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let mut params =
            CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
                .unwrap();
        params.serial_number = Some(SerialNumber::from(vec![0x10, serial]));
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        let ca_pem = pem::encode(&pem::Pem::new("CERTIFICATE", self.ca_der.to_vec()));
        (format!("{}{ca_pem}", cert.pem()), key.serialize_pem())
    }

    /// Write a leaf pair for `names` with `serial` to the two files, the
    /// key readable by this account only, as a deploy hook leaves it.
    pub fn write(&self, names: &[&str], serial: u8) {
        let (cert, key) = self.leaf(names, serial);
        write_replacing(&self.cert_path(), &cert);
        write_replacing_mode(&self.key_path(), &key, 0o600);
    }

    /// A rustls client that trusts this CA only, speaking `versions`.
    pub fn client(
        &self,
        versions: &[&'static rustls::SupportedProtocolVersion],
    ) -> rustls::ClientConfig {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.ca_der.clone()).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth()
    }

    /// A reqwest client over [`Self::client`] (TLS 1.2 and 1.3).
    pub fn http(&self) -> reqwest::Client {
        self.http_resolving(None)
    }

    /// [`Self::http`] with `name` resolved to `addr`, when given.
    pub fn http_resolving(&self, resolve: Option<(&str, SocketAddr)>) -> reqwest::Client {
        let builder = match resolve {
            Some((name, addr)) => reqwest::Client::builder().resolve(name, addr),
            None => reqwest::Client::builder(),
        };
        builder
            .tls_backend_preconfigured(
                self.client(&[&rustls::version::TLS13, &rustls::version::TLS12]),
            )
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }
}

/// Write `text` to `path` as a deploy hook does: into a new file renamed
/// over the old one.
pub fn write_replacing(path: &Path, text: &str) {
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

/// [`write_replacing`] with the new file's mode set before the rename; on
/// Windows, which has no mode bits, [`write_replacing`] alone.
pub fn write_replacing_mode(path: &Path, text: &str, mode: u32) {
    #[cfg(not(unix))]
    {
        let _ = mode;
        write_replacing(path, text);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let tmp = path.with_extension("new");
        std::fs::write(&tmp, text).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }
}

/// `path`'s mode set to `mode`: only its change time moves.
#[cfg(unix)]
pub fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A gateway serving its plain listener and a TLS listener with `pki`'s
/// pair on loopback, each on a port of its own; `edit` adjusts its settings
/// before it starts.
pub struct Served {
    pub state: SharedState,
    pub plain: SocketAddr,
    pub tls: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    pub released: Option<tokio::sync::oneshot::Receiver<()>>,
    pub done: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Served {
    /// Stop it; answers once both listeners are released.
    pub async fn stop(mut self) -> SharedState {
        let _ = self.stop.take().unwrap().send(());
        let _ = self.released.take().unwrap().await;
        let _ = self.done.await;
        self.state
    }

    /// The leaf its TLS listener serves to the next connection, and the
    /// version negotiated. That connection's accept checks the pair's files
    /// itself: a connection accepted while another one's check runs is
    /// served the pair loaded then without a check of its own (D34), and
    /// nothing but the test connects ([`serve_on`] opens no connection of
    /// its own), each handshake's check over before it answers.
    #[cfg_attr(not(unix), allow(dead_code))] // used by `listener`, Unix only
    pub async fn handshake(
        &self,
        pki: &Pki,
        versions: &[&'static rustls::SupportedProtocolVersion],
    ) -> Result<(Vec<u8>, rustls::ProtocolVersion), String> {
        handshake(pki, self.tls, versions).await
    }

    /// Stop it and answer as its listeners are released, before its open
    /// connections have drained: the state, and the serve still draining.
    #[cfg_attr(not(unix), allow(dead_code))] // used by `listener`, Unix only
    pub async fn stop_released(
        mut self,
    ) -> (SharedState, tokio::task::JoinHandle<anyhow::Result<()>>) {
        let _ = self.stop.take().unwrap().send(());
        let _ = self.released.take().unwrap().await;
        (self.state, self.done)
    }
}

/// A free loopback TCP port that no other test takes before the TLS
/// listener binds it: the device link's claimed one, below the ephemeral
/// range. A port a `bind(0)` handed out and let go is in that range, where
/// any other test's listener or outgoing connection can take it first.
pub fn free_port() -> u16 {
    crate::support::device_link::free_port()
}

/// `state`'s settings changed by `edit`, written and published.
pub async fn set(state: &SharedState, edit: impl FnOnce(&mut Settings)) {
    let mut s = state.snapshot().settings.clone();
    edit(&mut s);
    lmgw_core::store::save_settings(&state.db, &s)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// Serve `state` with TLS from `pki` on fresh loopback ports, `edit`ed
/// first; WebRTC calls off unless `edit` turns them on.
pub async fn serve_on(state: SharedState, pki: &Pki, edit: impl FnOnce(&mut Settings)) -> Served {
    let (cert, key) = (pki.cert_path(), pki.key_path());
    let tls_port = free_port();
    set(&state, |s| {
        s.realtime.webrtc.webrtc_enabled = false;
        s.tls.cert_path = cert.display().to_string();
        s.tls.key_path = key.display().to_string();
        s.tls.listen = format!("127.0.0.1:{tls_port}");
        edit(s);
    })
    .await;
    let listener = lmgw_core::server::bind(&state, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let plain = listener.local_addr().unwrap();
    // A restart's state still has the start before published.
    let before = state.main_tls.current();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (released_tx, released) = tokio::sync::oneshot::channel();
    let done = tokio::spawn(lmgw_core::server::serve(
        state.clone(),
        listener,
        async {
            let _ = stopped.await;
        },
        Some(released_tx),
    ));
    let tls = state.snapshot().settings.tls.listen_addr().unwrap();
    // The start publishes its TLS listener once the socket listens, so a
    // connection from here on waits in its backlog for the accept loop. Not
    // a probe connection: its accept runs a check of the pair of its own,
    // which may still be running, or not have begun, when a test changes
    // the files and connects, and that connection is then served the pair
    // loaded before without a check (D34), the failure it should show not
    // stored yet. A start that could not bring it up says why at once.
    crate::common::patience::until("the start publishes its TLS listener", || {
        match state.main_tls.current() {
            Some(now) => before.as_ref().is_none_or(|b| !Arc::ptr_eq(b, &now)),
            None => match state.main_tls.status(&state.snapshot().settings).error {
                Some(e) => panic!("the TLS listener did not start: {e}"),
                None => false,
            },
        }
    })
    .await;
    Served {
        state,
        plain,
        tls,
        stop: Some(stop),
        released: Some(released),
        done,
    }
}

/// [`serve_on`] on a fresh in-memory gateway, its pair for `localhost` and
/// `127.0.0.1` with serial 1.
pub async fn serve(pki: &Pki, edit: impl FnOnce(&mut Settings)) -> Served {
    pki.write(&["localhost", "127.0.0.1"], 1);
    serve_on(AppState::init_for_tests().await.unwrap(), pki, edit).await
}

/// The leaf the TLS listener on `addr` serves now, and the protocol version
/// a client limited to `versions` negotiated.
#[cfg_attr(not(unix), allow(dead_code))] // used by `listener`, Unix only
pub async fn handshake(
    pki: &Pki,
    addr: SocketAddr,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Result<(Vec<u8>, rustls::ProtocolVersion), String> {
    let cfg = Arc::new(pki.client(versions));
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tls = tokio_rustls::TlsConnector::from(cfg)
        .connect(name, tcp)
        .await
        .map_err(|e| e.to_string())?;
    let (_, conn) = tls.get_ref();
    let leaf = conn.peer_certificates().unwrap()[0].to_vec();
    Ok((leaf, conn.protocol_version().unwrap()))
}

/// The serial of a DER certificate, lowercase hex.
#[cfg_attr(not(unix), allow(dead_code))] // used by `listener`, Unix only
pub fn serial_of(der: &[u8]) -> String {
    let (_, c) = x509_parser::parse_x509_certificate(der).unwrap();
    hex::encode(c.tbs_certificate.raw_serial())
}
