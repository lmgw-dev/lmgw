//! The device listener's TLS configurations (device link design §4.2): the
//! aws-lc-rs provider, TLS 1.3 only (a client's certificate then travels
//! encrypted), ALPN `http/1.1`, no session resumption (D5: rustls's default
//! cache would restore a client's certificate without running the client
//! verifier), and rustls's stock `WebPkiClientVerifier` over lmgw's device
//! CA alone — mandatory, or optional while a pairing token is pending.

use std::sync::{Arc, OnceLock};

use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::danger::ClientCertVerifier;
use rustls::server::{NoServerSessionStorage, WebPkiClientVerifier};
use rustls::ServerConfig;

/// The ALPN protocol the device listener speaks.
pub const ALPN_HTTP1: &[u8] = b"http/1.1";

/// Whether a handshake requires a client certificate (§4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientAuth {
    /// A client without a certificate fails the handshake: while no pairing
    /// token is pending.
    Mandatory,
    /// A client may come without one, for enrolment alone: while a pairing
    /// token is pending.
    Optional,
}

/// aws-lc-rs, the provider the rest of lmgw's rustls uses; built once.
pub fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .clone()
}

/// The two client verifiers over one CA, shared by every leaf it issues.
#[derive(Debug, Clone)]
pub(super) struct ClientVerifiers {
    mandatory: Arc<dyn ClientCertVerifier>,
    optional: Arc<dyn ClientCertVerifier>,
}

impl ClientVerifiers {
    pub(super) fn new(ca: &CertificateDer<'static>) -> Result<Self, String> {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(ca.clone())
            .map_err(|e| format!("the CA certificate is no trust anchor: {e}"))?;
        let roots = Arc::new(roots);
        let mandatory = WebPkiClientVerifier::builder_with_provider(roots.clone(), provider())
            .build()
            .map_err(|e| format!("the client verifier: {e}"))?;
        let optional = WebPkiClientVerifier::builder_with_provider(roots, provider())
            .allow_unauthenticated()
            .build()
            .map_err(|e| format!("the client verifier: {e}"))?;
        Ok(Self {
            mandatory,
            optional,
        })
    }

    fn get(&self, auth: ClientAuth) -> Arc<dyn ClientCertVerifier> {
        match auth {
            ClientAuth::Mandatory => self.mandatory.clone(),
            ClientAuth::Optional => self.optional.clone(),
        }
    }
}

/// One server configuration: `chain` (the leaf, then the CA) and its key,
/// with `auth`'s verifier.
pub(super) fn build(
    chain: &[CertificateDer<'static>],
    key: &PrivateKeyDer<'static>,
    verifiers: &ClientVerifiers,
    auth: ClientAuth,
) -> Result<Arc<ServerConfig>, String> {
    let mut cfg = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("TLS 1.3 with aws-lc-rs: {e}"))?
        .with_client_cert_verifier(verifiers.get(auth))
        .with_single_cert(chain.to_vec(), key.clone_key())
        .map_err(|e| format!("the server certificate: {e}"))?;
    cfg.session_storage = Arc::new(NoServerSessionStorage {});
    cfg.send_tls13_tickets = 0;
    cfg.alpn_protocols = vec![ALPN_HTTP1.to_vec()];
    Ok(Arc::new(cfg))
}
