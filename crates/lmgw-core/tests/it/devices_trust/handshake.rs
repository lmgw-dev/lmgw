//! Real rustls handshakes, in memory, over what WP1 issues (§4.2, D1, D5):
//! a client that trusts lmgw's CA alone verifies each leaf by the name it
//! dialled, the mandatory configuration refuses a client without a device
//! certificate and the optional one lets it through, a certificate of
//! another CA or a lapsed one is refused, TLS 1.2 is refused, the protocol
//! is `http/1.1` and nothing resumes.

use std::sync::Arc;

use chrono::{Duration, Utc};
use lmgw_core::devices::tls::{self, Ca, ClientAuth, Leaf, LeafCache};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, ServerConfig, ServerConnection};

use super::{device_key, plain_ca, spki};

/// A device's TLS identity: its certificate and its key.
type Identity = (CertificateDer<'static>, PrivateKeyDer<'static>);

/// A client that trusts `ca` alone, presenting `identity` if any, speaking
/// `versions`.
fn client(
    ca: &CertificateDer<'static>,
    identity: Option<&Identity>,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Arc<ClientConfig> {
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
    Arc::new(cfg)
}

/// A device identity issued by `ca`, valid `days` from `now`.
fn device(ca: &Ca, days: u32, now: chrono::DateTime<Utc>) -> Identity {
    let key = device_key();
    let issued = tls::issue_device(ca, &spki(&key), "phone", days, now).unwrap();
    (
        CertificateDer::from(issued.der),
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
    )
}

/// Run a handshake between `client` dialling `name` and `server`, moving
/// the bytes in memory until both are done or one refuses.
fn handshake(
    client: &Arc<ClientConfig>,
    server: Arc<ServerConfig>,
    name: &str,
) -> Result<(ClientConnection, ServerConnection), String> {
    let name = ServerName::try_from(name.to_string()).unwrap();
    let mut c = ClientConnection::new(client.clone(), name).map_err(|e| e.to_string())?;
    let mut s = ServerConnection::new(server).map_err(|e| e.to_string())?;
    for _ in 0..16 {
        let mut to_s = Vec::new();
        while c.wants_write() {
            c.write_tls(&mut to_s).unwrap();
        }
        let mut rd = to_s.as_slice();
        while !rd.is_empty() {
            s.read_tls(&mut rd).unwrap();
            s.process_new_packets()
                .map_err(|e| format!("server: {e}"))?;
        }
        let mut to_c = Vec::new();
        while s.wants_write() {
            s.write_tls(&mut to_c).unwrap();
        }
        let mut rd = to_c.as_slice();
        while !rd.is_empty() {
            c.read_tls(&mut rd).unwrap();
            c.process_new_packets()
                .map_err(|e| format!("client: {e}"))?;
        }
        if !c.is_handshaking() && !s.is_handshaking() && !c.wants_write() && !s.wants_write() {
            return Ok((c, s));
        }
    }
    Err("the handshake did not finish".into())
}

struct World {
    _dir: tempfile::TempDir,
    ca: Arc<Ca>,
    cache: LeafCache,
}

async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let (_, ca) = plain_ca(dir.path()).await;
    let cache = LeafCache::with_interfaces(|| vec!["203.0.113.5".parse().unwrap()]);
    World {
        _dir: dir,
        ca,
        cache,
    }
}

impl World {
    fn leaf(&self, local: &str) -> Arc<Leaf> {
        self.cache
            .leaf(&self.ca, local.parse().unwrap(), 30, Utc::now())
            .unwrap()
    }
}

#[tokio::test]
async fn a_device_certificate_passes_the_mandatory_configuration() {
    let w = world().await;
    let id = device(&w.ca, 365, Utc::now());
    let leaf = w.leaf("127.0.0.1");
    let c = client(w.ca.certificate(), Some(&id), &[&rustls::version::TLS13]);
    let (client_conn, server_conn) =
        handshake(&c, leaf.config(ClientAuth::Mandatory), "127.0.0.1").unwrap();

    assert_eq!(
        server_conn.protocol_version(),
        Some(rustls::ProtocolVersion::TLSv1_3)
    );
    assert_eq!(server_conn.alpn_protocol(), Some(tls::ALPN_HTTP1));
    // The server holds the device's certificate: what the row is found by.
    let peer = server_conn.peer_certificates().unwrap();
    assert_eq!(peer, std::slice::from_ref(&id.0));
    // The chain sent is the leaf and the CA, so enrolment finds the CA by fp.
    let chain = client_conn.peer_certificates().unwrap();
    assert_eq!(
        chain,
        [leaf.certificate().clone(), w.ca.certificate().clone()]
    );
}

#[tokio::test]
async fn without_a_certificate_only_the_optional_configuration_lets_a_client_in() {
    let w = world().await;
    let leaf = w.leaf("127.0.0.1");
    let anon = client(w.ca.certificate(), None, &[&rustls::version::TLS13]);
    let e = handshake(&anon, leaf.config(ClientAuth::Mandatory), "localhost").expect_err("refused");
    assert!(e.starts_with("server:"), "{e}");
    let (_, s) = handshake(&anon, leaf.config(ClientAuth::Optional), "localhost").unwrap();
    assert!(s.peer_certificates().is_none());
    // The optional one still checks a certificate that is presented.
    let id = device(&w.ca, 365, Utc::now());
    let c = client(w.ca.certificate(), Some(&id), &[&rustls::version::TLS13]);
    let (_, s) = handshake(&c, leaf.config(ClientAuth::Optional), "localhost").unwrap();
    assert_eq!(s.peer_certificates().unwrap(), [id.0]);
}

#[tokio::test]
async fn another_cas_certificate_or_a_lapsed_one_is_refused() {
    let w = world().await;
    let other = world().await;
    let leaf = w.leaf("::1");
    for auth in [ClientAuth::Mandatory, ClientAuth::Optional] {
        let foreign = device(&other.ca, 365, Utc::now());
        let c = client(
            w.ca.certificate(),
            Some(&foreign),
            &[&rustls::version::TLS13],
        );
        let e = handshake(&c, leaf.config(auth), "::1").expect_err("refused");
        assert!(e.starts_with("server:"), "{auth:?}: {e}");

        let lapsed = device(&w.ca, 1, Utc::now() - Duration::days(3));
        let c = client(
            w.ca.certificate(),
            Some(&lapsed),
            &[&rustls::version::TLS13],
        );
        let e = handshake(&c, leaf.config(auth), "::1").expect_err("refused");
        assert!(e.contains("xpired"), "{auth:?}: {e}");
    }
}

#[tokio::test]
async fn a_client_verifies_lmgw_by_its_ca_and_the_name_it_dialled() {
    let w = world().await;
    w.cache
        .set_url_hosts(["https://phone-gw.example:8788", "https://192.0.2.1:8788"]);
    let c = client(w.ca.certificate(), None, &[&rustls::version::TLS13]);
    let lo = w.leaf("127.0.0.1");
    for name in [
        "127.0.0.1",
        "::1",
        "localhost",
        "phone-gw.example",
        "192.0.2.1",
    ] {
        handshake(&c, lo.config(ClientAuth::Optional), name)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    let lan = w.leaf("203.0.113.5");
    handshake(&c, lan.config(ClientAuth::Optional), "203.0.113.5").unwrap();
    handshake(&c, lan.config(ClientAuth::Optional), "192.0.2.1").unwrap();
    for wrong in ["203.0.113.6", "127.0.0.1", "elsewhere.example"] {
        let e = handshake(&c, lan.config(ClientAuth::Optional), wrong).expect_err("refused");
        assert!(e.starts_with("client:"), "{wrong}: {e}");
    }
    // A client that trusts another CA refuses lmgw before it sends anything
    // of its own (D13).
    let other = world().await;
    let stranger = client(other.ca.certificate(), None, &[&rustls::version::TLS13]);
    let e =
        handshake(&stranger, lo.config(ClientAuth::Optional), "127.0.0.1").expect_err("refused");
    assert!(e.starts_with("client:"), "{e}");
}

#[tokio::test]
async fn tls_1_2_is_refused_and_nothing_resumes() {
    let w = world().await;
    let leaf = w.leaf("127.0.0.1");
    let id = device(&w.ca, 365, Utc::now());
    let old = client(w.ca.certificate(), Some(&id), &[&rustls::version::TLS12]);
    assert!(handshake(&old, leaf.config(ClientAuth::Mandatory), "127.0.0.1").is_err());

    // The same client config (with its session cache) connects twice: the
    // second handshake is a full one too (D5).
    let c = client(w.ca.certificate(), Some(&id), &[&rustls::version::TLS13]);
    for round in 0..2 {
        let (cc, sc) = handshake(&c, leaf.config(ClientAuth::Mandatory), "127.0.0.1").unwrap();
        assert_eq!(
            sc.handshake_kind(),
            Some(rustls::HandshakeKind::Full),
            "round {round}"
        );
        assert_eq!(cc.handshake_kind(), Some(rustls::HandshakeKind::Full));
        // And the server ran its verifier: it holds the certificate.
        assert_eq!(sc.peer_certificates().unwrap(), std::slice::from_ref(&id.0));
    }
}

/// A paired device's certificate, presented by whoever holds the device's
/// key, never passes as lmgw's server certificate: it carries `clientAuth`
/// alone and no subjectAltName, so a client that checks lmgw's leaf against
/// the CA refuses it by usage and by name. lmgw-client's pin relies on
/// this; every server leaf carries `serverAuth`.
#[tokio::test]
async fn a_device_certificate_never_passes_as_lmgw() {
    let w = world().await;
    let key = device_key();
    let issued = tls::issue_device(&w.ca, &spki(&key), "127.0.0.1", 365, Utc::now()).unwrap();
    let chain = vec![CertificateDer::from(issued.der), w.ca.certificate().clone()];
    let posing = ServerConfig::builder_with_provider(tls::provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            chain,
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap();
    let posing = Arc::new(posing);
    let c = client(w.ca.certificate(), None, &[&rustls::version::TLS13]);
    // Even dialled by the name it carries as its common name.
    for name in ["127.0.0.1", "localhost", "phone"] {
        let e = handshake(&c, posing.clone(), name).expect_err("refused");
        assert!(e.starts_with("client:"), "{name}: {e}");
    }
    // The real leaf for the same address passes, and says serverAuth.
    for local in ["127.0.0.1", "203.0.113.5"] {
        let leaf = w.leaf(local);
        let (_, cert) = x509_parser::parse_x509_certificate(leaf.certificate()).unwrap();
        let eku = cert.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.server_auth && !eku.client_auth, "{local}");
        handshake(&c, leaf.config(ClientAuth::Optional), local).unwrap();
    }
}
