//! The trust pin, by real handshakes in memory: certificates rcgen makes as
//! lmgw makes them (`crate::test_pki`), a server configured as lmgw's
//! device listener is (device link §4.2: TLS 1.3, the leaf with the CA after
//! it, client certificates verified against the CA, no resumption), and
//! the device's configurations from this module. The server runs on
//! aws-lc-rs, and so does the device's side (Kai desktop's provider), except
//! where a test says ring (Kai mobile's).

use std::sync::Arc;

use rcgen::{ExtendedKeyUsagePurpose, KeyPair};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::{NoServerSessionStorage, WebPkiClientVerifier};
use rustls::sign::{CertifiedKey, SigningKey, SingleCertAndKey};
use rustls::{
    ClientConfig, ClientConnection, Connection, Error, InconsistentKeys, RootCertStore,
    ServerConfig, ServerConnection, SupportedProtocolVersion,
};

use super::*;
use crate::pairing::Urls;
use crate::requests::Credentials;
use crate::test_pki::{self, Ca, Leaf, CURRENT, LAPSED};

/// aws-lc-rs: the server's provider, and Kai desktop's.
fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// ring: Kai mobile's provider.
fn ring() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn signing_key(key: &KeyPair) -> Arc<dyn SigningKey> {
    signing_key_with(&provider(), key)
}

fn signing_key_with(provider: &CryptoProvider, key: &KeyPair) -> Arc<dyn SigningKey> {
    provider
        .key_provider
        .load_private_key(PrivateKeyDer::Pkcs8(key.serialize_der().into()))
        .unwrap()
}

/// Whether the server asks for a client certificate as lmgw does while no
/// pairing token is pending (`Mandatory`) or during a token's life
/// (`Optional`, for enrolment).
#[derive(Clone, Copy)]
enum ClientAuth {
    Mandatory,
    Optional,
}

/// A server sending `chain` (the leaf first) signed with `key`, verifying
/// client certificates against `client_ca`.
fn server_with(
    chain: &[&[u8]],
    key: &KeyPair,
    client_ca: &Ca,
    auth: ClientAuth,
    versions: &[&'static SupportedProtocolVersion],
) -> ServerConfig {
    let mut roots = RootCertStore::empty();
    roots.add(client_ca.der().clone()).unwrap();
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider());
    let verifier = match auth {
        ClientAuth::Mandatory => verifier.build(),
        ClientAuth::Optional => verifier.allow_unauthenticated().build(),
    }
    .unwrap();
    let chain = chain
        .iter()
        .map(|c| CertificateDer::from(c.to_vec()))
        .collect();
    let mut cfg = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(versions)
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(Arc::new(SingleCertAndKey::from(CertifiedKey::new(
            chain,
            signing_key(key),
        ))));
    cfg.session_storage = Arc::new(NoServerSessionStorage {});
    cfg.send_tls13_tickets = 0;
    cfg
}

/// lmgw's device listener under `ca`, serving `leaf` with the CA after it.
fn lmgw(ca: &Ca, leaf: &Leaf, auth: ClientAuth) -> ServerConfig {
    server_with(
        &[&leaf.der, ca.der()],
        &leaf.key,
        ca,
        auth,
        &[&rustls::version::TLS13],
    )
}

/// What a handshake left: each side's error, and the client certificate the
/// server saw.
#[derive(Debug, Default)]
struct Outcome {
    client: Option<Error>,
    server: Option<Error>,
    client_cert: Option<Vec<u8>>,
}

impl Outcome {
    fn ok(&self) -> bool {
        self.client.is_none() && self.server.is_none()
    }

    /// What a paired device says about it.
    fn refusal(&self) -> Option<TlsRefusal> {
        self.client.as_ref().and_then(TlsRefusal::of)
    }

    /// What an enrolling device says about it.
    fn enrol_refusal(&self) -> Option<TlsRefusal> {
        self.client.as_ref().and_then(TlsRefusal::of_enrolment)
    }
}

/// Moves what `from` has to send into `to`; whether any bytes moved. An
/// error of `to` is kept in `err` (its alert goes back on the next move).
fn pump(from: &mut Connection, to: &mut Connection, err: &mut Option<Error>) -> bool {
    let mut buf = Vec::new();
    while from.wants_write() {
        from.write_tls(&mut buf).unwrap();
    }
    if buf.is_empty() {
        return false;
    }
    let mut rd = buf.as_slice();
    while !rd.is_empty() && err.is_none() {
        if to.read_tls(&mut rd).is_err() {
            break;
        }
        if let Err(e) = to.process_new_packets() {
            *err = Some(e);
        }
    }
    true
}

/// A handshake between `client` and `server`, the client dialing `host`.
fn handshake(client: ClientConfig, server: ServerConfig, host: &str) -> Outcome {
    let name = ServerName::try_from(host.to_string()).unwrap();
    let mut c = Connection::from(ClientConnection::new(Arc::new(client), name).unwrap());
    let mut s = Connection::from(ServerConnection::new(Arc::new(server)).unwrap());
    let mut out = Outcome::default();
    let mut rounds = 0;
    while pump(&mut c, &mut s, &mut out.server) | pump(&mut s, &mut c, &mut out.client) {
        rounds += 1;
        assert!(rounds < 16, "the handshake does not settle");
    }
    if out.ok() {
        assert!(!c.is_handshaking() && !s.is_handshaking());
        out.client_cert = s
            .peer_certificates()
            .and_then(|certs| certs.first())
            .map(|c| c.to_vec());
    }
    out
}

fn enrol(fp: Fingerprint) -> ClientConfig {
    enrol_config(provider(), fp).unwrap()
}

fn assert_invalid_certificate(out: &Outcome) {
    assert!(
        matches!(out.client, Some(Error::InvalidCertificate(_))),
        "{out:?}"
    );
}

fn credentials(ca: &Ca, certificate: Vec<u8>) -> Credentials {
    Credentials {
        device_id: 12,
        certificate,
        ca_certificate: ca.der().to_vec(),
        not_after_ms: 0,
        notice_after_ms: 0,
        urls: Urls::try_from(vec!["https://127.0.0.1:8788".to_string()]).unwrap(),
        confirm_code: String::new(),
    }
}

/// The client configuration of a device enrolled under `ca`.
fn paired(ca: &Ca) -> (ClientConfig, Vec<u8>) {
    paired_with(provider(), ca)
}

fn paired_with(provider: Arc<CryptoProvider>, ca: &Ca) -> (ClientConfig, Vec<u8>) {
    let device = test_pki::device_cert(ca, CURRENT);
    let key = signing_key_with(&provider, &device.key);
    let cfg = client_config(provider, &credentials(ca, device.der.clone()), key).unwrap();
    (cfg, device.der)
}

const NAMES: &[&str] = &["127.0.0.1", "::1", "localhost", "lmgw.test"];

// ------------------------------------------------------------------ enrolment

#[test]
fn enrolment_trusts_the_server_whose_chain_holds_the_pinned_ca() {
    let ca = test_pki::ca("lmgw devices 9f2c0123");
    let leaf = test_pki::server_leaf(&ca, NAMES, CURRENT);
    for host in ["127.0.0.1", "::1", "lmgw.test"] {
        let out = handshake(
            enrol(test_pki::fp(&ca)),
            lmgw(&ca, &leaf, ClientAuth::Optional),
            host,
        );
        assert!(out.ok(), "{host}: {out:?}");
        assert_eq!(out.client_cert, None, "enrolment sends no certificate");
    }
}

#[test]
fn enrolment_refuses_a_server_under_another_ca() {
    let ca = test_pki::ca("lmgw devices 9f2c0123");
    // Another lmgw, or a stranger on the same address: a CA of the same
    // name, a valid chain of its own.
    let other = test_pki::ca("lmgw devices 9f2c0123");
    let leaf = test_pki::server_leaf(&other, NAMES, CURRENT);
    let out = handshake(
        enrol(test_pki::fp(&ca)),
        lmgw(&other, &leaf, ClientAuth::Optional),
        "127.0.0.1",
    );
    assert_eq!(
        out.enrol_refusal(),
        Some(TlsRefusal::NotPairedLmgw),
        "{out:?}"
    );
    assert!(out.server.is_some(), "the server hears the refusal");
    // So does a server that sends its leaf without the CA.
    let leaf = test_pki::server_leaf(&ca, NAMES, CURRENT);
    let alone = server_with(
        &[&leaf.der],
        &leaf.key,
        &ca,
        ClientAuth::Optional,
        &[&rustls::version::TLS13],
    );
    let out = handshake(enrol(test_pki::fp(&ca)), alone, "127.0.0.1");
    assert_eq!(
        out.enrol_refusal(),
        Some(TlsRefusal::NotPairedLmgw),
        "{out:?}"
    );
}

/// While no pairing token is pending lmgw asks every handshake for a client
/// certificate (device link §4.2): an enrolment with an ended link meets
/// `certificate_required` before it can send its request. An alert is
/// unauthenticated, so the client reads it as a failed handshake to retry
/// and concludes nothing about the link.
#[test]
fn certificate_required_at_enrolment_concludes_nothing_about_the_link() {
    let ca = test_pki::ca("lmgw devices 9f2c0123");
    let leaf = test_pki::server_leaf(&ca, NAMES, CURRENT);
    let out = handshake(
        enrol(test_pki::fp(&ca)),
        lmgw(&ca, &leaf, ClientAuth::Mandatory),
        "127.0.0.1",
    );
    assert!(
        matches!(
            out.client,
            Some(Error::AlertReceived(AlertDescription::CertificateRequired))
        ),
        "{out:?}"
    );
    assert_eq!(out.enrol_refusal(), Some(TlsRefusal::Unproven));
    assert!(out
        .enrol_refusal()
        .unwrap()
        .to_string()
        .contains("try again"));
    // A paired device that sent none reads it as its certificate refused.
    assert_eq!(out.refusal(), Some(TlsRefusal::CertificateRefused));
}

/// The CA certificate is public: it travels in every handshake. A server
/// that sends it beside a leaf of its own (from a CA that even shares lmgw's
/// CA's name) proves nothing: the leaf must chain to the pinned key.
#[test]
fn enrolment_refuses_a_leaf_the_pinned_ca_did_not_sign() {
    let ca = test_pki::ca("lmgw devices 9f2c0123");
    let forger = test_pki::ca("lmgw devices 9f2c0123");
    let leaf = test_pki::server_leaf(&forger, NAMES, CURRENT);
    for chain in [
        vec![leaf.der.as_slice(), ca.der()],
        vec![leaf.der.as_slice(), ca.der(), forger.der()],
        vec![leaf.der.as_slice(), forger.der(), ca.der()],
    ] {
        let server = server_with(
            &chain,
            &leaf.key,
            &ca,
            ClientAuth::Optional,
            &[&rustls::version::TLS13],
        );
        let out = handshake(enrol(test_pki::fp(&ca)), server, "127.0.0.1");
        assert_invalid_certificate(&out);
        assert_eq!(
            out.enrol_refusal(),
            Some(TlsRefusal::NotPairedLmgw),
            "the device has trusted nothing yet: not a changed authority"
        );
    }
}

/// The CA's own certificate, even one naming the host, is no server
/// certificate: webpki refuses a CA as the end entity.
#[test]
fn enrolment_refuses_the_ca_itself_as_the_server_s_certificate() {
    let ca = test_pki::ca_naming("lmgw devices 9f2c0123", &["127.0.0.1"]);
    let server = server_with(
        &[ca.der()],
        ca.key(),
        &ca,
        ClientAuth::Optional,
        &[&rustls::version::TLS13],
    );
    let out = handshake(enrol(test_pki::fp(&ca)), server, "127.0.0.1");
    assert_invalid_certificate(&out);
    assert!(
        format!("{:?}", out.client).contains("CaUsedAsEndEntity"),
        "{out:?}"
    );
}

#[test]
fn enrolment_checks_the_leaf_s_dates_host_and_purpose() {
    let ca = test_pki::ca("lmgw devices 9f2c0123");
    let fp = test_pki::fp(&ca);
    let lapsed = test_pki::server_leaf(&ca, NAMES, LAPSED);
    let out = handshake(
        enrol(fp),
        lmgw(&ca, &lapsed, ClientAuth::Optional),
        "127.0.0.1",
    );
    assert_eq!(
        out.enrol_refusal(),
        Some(TlsRefusal::ServerCertificateDates),
        "{out:?}"
    );

    let leaf = test_pki::server_leaf(&ca, &["127.0.0.1"], CURRENT);
    for host in ["198.51.100.20", "lmgw.test"] {
        let out = handshake(enrol(fp), lmgw(&ca, &leaf, ClientAuth::Optional), host);
        assert_eq!(
            out.enrol_refusal(),
            Some(TlsRefusal::HostNotNamed {
                host: Some(host.into())
            }),
            "{out:?}"
        );
        assert!(out.enrol_refusal().unwrap().to_string().contains(host));
    }

    // A device certificate (clientAuth only) is no server's.
    let device = test_pki::leaf(&ca, NAMES, ExtendedKeyUsagePurpose::ClientAuth, CURRENT);
    let out = handshake(
        enrol(fp),
        lmgw(&ca, &device, ClientAuth::Optional),
        "127.0.0.1",
    );
    assert!(
        matches!(
            out.client,
            Some(Error::InvalidCertificate(
                CertificateError::InvalidPurpose | CertificateError::InvalidPurposeContext { .. }
            ))
        ),
        "{out:?}"
    );
}

// ------------------------------------------------------------------ after it

#[test]
fn a_paired_device_connects_with_its_certificate() {
    let ca = test_pki::ca("lmgw devices 9f2c0123");
    let leaf = test_pki::server_leaf(&ca, NAMES, CURRENT);
    let (cfg, device) = paired(&ca);
    for auth in [ClientAuth::Mandatory, ClientAuth::Optional] {
        let out = handshake(cfg.clone(), lmgw(&ca, &leaf, auth), "127.0.0.1");
        assert!(out.ok(), "{out:?}");
        assert_eq!(out.client_cert.as_ref(), Some(&device));
    }
    // A leaf re-issued with a new key and new names needs nothing of the
    // device: it trusts the CA.
    let reissued = test_pki::server_leaf(&ca, &["192.0.2.1"], CURRENT);
    let out = handshake(
        cfg,
        lmgw(&ca, &reissued, ClientAuth::Mandatory),
        "192.0.2.1",
    );
    assert!(out.ok(), "{out:?}");
}

#[test]
fn a_paired_device_trusts_its_stored_ca_alone() {
    let ca = test_pki::ca("lmgw devices 9f2c0123");
    let (cfg, _) = paired(&ca);
    // A rotated CA, or a stranger with a CA of the same name.
    let rotated = test_pki::ca("lmgw devices 9f2c0123");
    let leaf = test_pki::server_leaf(&rotated, NAMES, CURRENT);
    let out = handshake(
        cfg.clone(),
        lmgw(&rotated, &leaf, ClientAuth::Mandatory),
        "127.0.0.1",
    );
    assert_eq!(out.refusal(), Some(TlsRefusal::AuthorityChanged), "{out:?}");
    assert_eq!(out.client_cert, None);
    // The stored CA certificate sent beside a foreign leaf.
    let server = server_with(
        &[&leaf.der, ca.der()],
        &leaf.key,
        &ca,
        ClientAuth::Mandatory,
        &[&rustls::version::TLS13],
    );
    let out = handshake(cfg.clone(), server, "127.0.0.1");
    assert_eq!(out.refusal(), Some(TlsRefusal::AuthorityChanged), "{out:?}");
    // lmgw's own leaf, but lapsed, or for another host.
    let lapsed = test_pki::server_leaf(&ca, NAMES, LAPSED);
    let out = handshake(
        cfg.clone(),
        lmgw(&ca, &lapsed, ClientAuth::Mandatory),
        "127.0.0.1",
    );
    assert_eq!(
        out.refusal(),
        Some(TlsRefusal::ServerCertificateDates),
        "{out:?}"
    );
    let leaf = test_pki::server_leaf(&ca, &["127.0.0.1"], CURRENT);
    let out = handshake(cfg, lmgw(&ca, &leaf, ClientAuth::Mandatory), "192.0.2.1");
    assert!(
        matches!(out.refusal(), Some(TlsRefusal::HostNotNamed { .. })),
        "{out:?}"
    );
}

#[test]
fn lmgw_refuses_a_lapsed_or_foreign_device_certificate() {
    let ca = test_pki::ca("lmgw devices 9f2c0123");
    let leaf = test_pki::server_leaf(&ca, NAMES, CURRENT);
    let lapsed = test_pki::device_cert(&ca, LAPSED);
    let cfg = client_config(
        provider(),
        &credentials(&ca, lapsed.der),
        signing_key(&lapsed.key),
    )
    .unwrap();
    let out = handshake(cfg, lmgw(&ca, &leaf, ClientAuth::Mandatory), "127.0.0.1");
    assert!(out.server.is_some(), "{out:?}");
    assert_eq!(
        out.refusal(),
        Some(TlsRefusal::CertificateLapsed),
        "{out:?}"
    );

    // A certificate from another CA (a rotated one: its name carries its
    // own fingerprint) is an unknown CA to lmgw.
    let other = test_pki::ca("lmgw devices 41ab9f2c");
    let foreign = test_pki::device_cert(&other, CURRENT);
    let cfg = client_config(
        provider(),
        &credentials(&ca, foreign.der),
        signing_key(&foreign.key),
    )
    .unwrap();
    let out = handshake(cfg, lmgw(&ca, &leaf, ClientAuth::Mandatory), "127.0.0.1");
    assert_eq!(
        out.refusal(),
        Some(TlsRefusal::CertificateRefused),
        "{out:?}"
    );

    // One from a CA forged with lmgw's CA's very name fails its signature.
    // rustls answers that with the generic `decrypt_error` alert, which the
    // device does not read as a verdict on its certificate.
    let forged = test_pki::ca("lmgw devices 9f2c0123");
    let foreign = test_pki::device_cert(&forged, CURRENT);
    let cfg = client_config(
        provider(),
        &credentials(&ca, foreign.der),
        signing_key(&foreign.key),
    )
    .unwrap();
    let out = handshake(cfg, lmgw(&ca, &leaf, ClientAuth::Mandatory), "127.0.0.1");
    assert!(
        matches!(out.server, Some(Error::InvalidCertificate(_))) && out.client.is_some(),
        "{out:?}"
    );
}

#[test]
fn tls_1_2_is_refused_both_ways() {
    let ca = test_pki::ca("lmgw devices 9f2c0123");
    let leaf = test_pki::server_leaf(&ca, NAMES, CURRENT);
    let tls12 = || {
        server_with(
            &[&leaf.der, ca.der()],
            &leaf.key,
            &ca,
            ClientAuth::Optional,
            &[&rustls::version::TLS12],
        )
    };
    let (cfg, _) = paired(&ca);
    for client in [enrol(test_pki::fp(&ca)), cfg] {
        let out = handshake(client, tls12(), "127.0.0.1");
        assert!(out.client.is_some() && !out.ok(), "{out:?}");
    }
}

/// The app's signing key may not say its public key (an Android Keystore
/// key behind a callback need not); when it does, it must be the
/// certificate's.
#[test]
fn the_signing_key_must_be_the_certificate_s() {
    let ca = test_pki::ca("lmgw devices 9f2c0123");
    let device = test_pki::device_cert(&ca, CURRENT);
    let stranger = KeyPair::generate().unwrap();
    let e = client_config(
        provider(),
        &credentials(&ca, device.der.clone()),
        signing_key(&stranger),
    )
    .unwrap_err();
    assert_eq!(e, Error::InconsistentKeys(InconsistentKeys::KeyMismatch));

    #[derive(Debug)]
    struct Callback(Arc<dyn SigningKey>);
    impl SigningKey for Callback {
        fn choose_scheme(
            &self,
            offered: &[rustls::SignatureScheme],
        ) -> Option<Box<dyn rustls::sign::Signer>> {
            self.0.choose_scheme(offered)
        }
        fn algorithm(&self) -> rustls::SignatureAlgorithm {
            self.0.algorithm()
        }
    }
    let cfg = client_config(
        provider(),
        &credentials(&ca, device.der.clone()),
        Arc::new(Callback(signing_key(&device.key))),
    )
    .unwrap();
    let leaf = test_pki::server_leaf(&ca, NAMES, CURRENT);
    let out = handshake(cfg, lmgw(&ca, &leaf, ClientAuth::Mandatory), "127.0.0.1");
    assert!(out.ok(), "{out:?}");
    // Not a CA certificate: refused before any handshake.
    let mut broken = credentials(&ca, device.der);
    broken.ca_certificate = b"not a certificate".to_vec();
    assert!(client_config(provider(), &broken, signing_key(&device.key)).is_err());
}

/// Kai mobile's provider against lmgw's: the enrolment, its refusal of
/// another lmgw, and a paired connection, from a ring client to the
/// aws-lc-rs server. (The Keystore signer is kai-mobile's to test.)
#[test]
fn a_ring_client_enrols_and_connects_too() {
    let ca = test_pki::ca("lmgw devices 9f2c0123");
    let leaf = test_pki::server_leaf(&ca, NAMES, CURRENT);
    let enrol_ring = |fp| enrol_config(ring(), fp).unwrap();
    let out = handshake(
        enrol_ring(test_pki::fp(&ca)),
        lmgw(&ca, &leaf, ClientAuth::Optional),
        "127.0.0.1",
    );
    assert!(out.ok(), "{out:?}");

    let other = test_pki::ca("lmgw devices 41ab9f2c");
    let foreign = test_pki::server_leaf(&other, NAMES, CURRENT);
    let out = handshake(
        enrol_ring(test_pki::fp(&ca)),
        lmgw(&other, &foreign, ClientAuth::Optional),
        "127.0.0.1",
    );
    assert_eq!(
        out.enrol_refusal(),
        Some(TlsRefusal::NotPairedLmgw),
        "{out:?}"
    );

    let (cfg, device) = paired_with(ring(), &ca);
    let out = handshake(cfg, lmgw(&ca, &leaf, ClientAuth::Mandatory), "127.0.0.1");
    assert!(out.ok(), "{out:?}");
    assert_eq!(out.client_cert, Some(device));
}

#[test]
fn a_tls_error_is_found_inside_a_client_s_error() {
    #[derive(Debug)]
    struct Wrapper(std::io::Error);
    impl std::fmt::Display for Wrapper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("request failed")
        }
    }
    impl std::error::Error for Wrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }
    let tls = Error::InvalidCertificate(CertificateError::UnknownIssuer);
    let e = Wrapper(std::io::Error::other(tls.clone()));
    let found = find_tls_error(&e).unwrap();
    assert_eq!(found, &tls);
    assert_eq!(TlsRefusal::of(found), Some(TlsRefusal::AuthorityChanged));
    let plain = Wrapper(std::io::Error::other("connection refused"));
    assert!(find_tls_error(&plain).is_none());
}
