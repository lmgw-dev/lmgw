//! The device link's TLS on the device's side (device link design §2, §6.3,
//! D1, D15): the trust pin of the whole link. Behind the non-default `tls`
//! feature (rustls 0.23 without a crypto provider of its own: the app hands
//! in its provider, Kai desktop aws-lc-rs, Kai mobile ring).
//!
//! lmgw runs a small private CA. The pairing link's `fp` is the SHA-256 of
//! that CA's SubjectPublicKeyInfo; lmgw's server certificates and every
//! device's certificate are issued by it, and lmgw sends its leaf with the
//! CA certificate after it.
//! - **Enrolment** ([`enrol_config`], [`EnrolVerifier`]): the device has
//!   only `fp`. Among the certificates lmgw sent it takes the one whose key
//!   `fp` pins and verifies the leaf with rustls's stock
//!   `WebPkiServerVerifier`, that certificate as its only root: the chain,
//!   the dates, `serverAuth`, and the URL's host against the SANs. No such
//!   certificate, or a leaf that does not chain to it, is "not the lmgw
//!   this link pairs with", and the device sends nothing. A
//!   `certificate_required` alert (what an ended token shows as) is read as
//!   [`TlsRefusal::Unproven`]: an alert is unauthenticated until the server
//!   has proved itself, so it concludes nothing about the link.
//! - **Every connection after it** ([`client_config`]): `WebPkiServerVerifier`
//!   over the stored CA certificate alone, and the device's certificate with
//!   the app's signing key (on Android a Keystore key behind a callback).
//!
//! Both are TLS 1.3 only (the device listener speaks nothing else, and the
//! device's certificate travels encrypted). A client builds every
//! lmgw-facing connection (its HTTP client, its WebSockets, the feed) from
//! the one configuration [`client_config`] returns, so none of them trusts
//! anything else. A refused handshake reads as what the device says with
//! [`TlsRefusal::of_enrolment`] and [`TlsRefusal::of`].

use std::fmt;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::sign::{CertifiedKey, SigningKey, SingleCertAndKey};
use rustls::{
    AlertDescription, CertificateError, ClientConfig, DigitallySignedStruct, Error,
    InconsistentKeys, OtherError, RootCertStore, SignatureScheme,
};

use crate::pairing::Fingerprint;
use crate::requests::Credentials;

/// The enrolment's server verifier: lmgw is whoever presents, among the
/// certificates it sends, one whose SubjectPublicKeyInfo hashes to `fp`,
/// and a leaf that chains to it for the URL's host (module doc).
#[derive(Debug)]
pub struct EnrolVerifier {
    fp: Fingerprint,
    provider: Arc<CryptoProvider>,
}

impl EnrolVerifier {
    /// `provider` verifies the signatures (the app's own).
    pub fn new(provider: Arc<CryptoProvider>, fp: Fingerprint) -> Self {
        Self { fp, provider }
    }

    /// The fingerprint it pins.
    pub fn fp(&self) -> &Fingerprint {
        &self.fp
    }
}

/// [`EnrolVerifier`]'s refusal of a server that is not the lmgw the link
/// pins: [`TlsRefusal::NotPairedLmgw`].
#[derive(Debug)]
enum NotPinned {
    /// No certificate the server sent carries the key `fp` pins.
    Absent,
    /// One does (the CA certificate is public), but the server's own
    /// certificate does not chain to it; the chain's error as webpki said it.
    NotSigned(CertificateError),
}

impl fmt::Display for NotPinned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => f.write_str(
                "no certificate the server sent carries the key the pairing link's fp pins",
            ),
            Self::NotSigned(e) => write!(
                f,
                "the server's certificate is not signed by the key the pairing link's fp pins \
                 ({e})"
            ),
        }
    }
}

impl std::error::Error for NotPinned {}

fn not_pinned(why: NotPinned) -> Error {
    Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(why))))
}

impl ServerCertVerifier for EnrolVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let pinned = std::iter::once(end_entity)
            .chain(intermediates)
            .find(|c| self.fp.pins_certificate(c))
            .ok_or_else(|| not_pinned(NotPinned::Absent))?;
        let mut root = RootCertStore::empty();
        root.add(pinned.clone().into_owned())?;
        // The pinned certificate is the only root. A leaf lmgw's CA did not
        // sign fails the chain even when the CA certificate travels beside
        // it, and the CA certificate itself is no leaf (webpki refuses a CA
        // as the end entity).
        WebPkiServerVerifier::builder_with_provider(Arc::new(root), self.provider.clone())
            .build()
            .map_err(|e| Error::General(format!("the pinned CA as a root: {e}")))?
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
            .map_err(|e| match e {
                // A chain that does not reach the pinned key is "not this
                // lmgw" at enrolment, not a changed authority: the device has
                // trusted nothing yet.
                Error::InvalidCertificate(
                    c @ (CertificateError::UnknownIssuer
                    | CertificateError::BadSignature
                    | CertificateError::UnsupportedSignatureAlgorithmForPublicKeyContext {
                        ..
                    }),
                ) => not_pinned(NotPinned::NotSigned(c)),
                e => e,
            })
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The enrolment's client configuration: TLS 1.3, [`EnrolVerifier`] for
/// `fp`, no client certificate. An error is the provider's (no TLS 1.3
/// suite).
pub fn enrol_config(provider: Arc<CryptoProvider>, fp: Fingerprint) -> Result<ClientConfig, Error> {
    let verifier = Arc::new(EnrolVerifier::new(provider.clone(), fp));
    Ok(ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

/// The configuration of every connection after enrolment: TLS 1.3,
/// `WebPkiServerVerifier` over `credentials.ca_certificate` as the only
/// root, and `credentials.certificate` as the client certificate, signed
/// with `signing_key`, the app's handle on the device's private key. No
/// ALPN: the HTTP client or WebSocket the app builds on it sets its own.
///
/// Refused when the CA certificate does not read as a root, when the
/// provider has no TLS 1.3 suite, and when `signing_key` names its public
/// key ([`SigningKey::public_key`]) and that is not the certificate's
/// (`InconsistentKeys::KeyMismatch`): a key that cannot sign for the
/// certificate would fail every handshake.
pub fn client_config(
    provider: Arc<CryptoProvider>,
    credentials: &Credentials,
    signing_key: Arc<dyn SigningKey>,
) -> Result<ClientConfig, Error> {
    let mut root = RootCertStore::empty();
    root.add(CertificateDer::from(credentials.ca_certificate.clone()))?;
    let certified = CertifiedKey::new(
        vec![CertificateDer::from(credentials.certificate.clone())],
        signing_key,
    );
    match certified.keys_match() {
        Ok(()) | Err(Error::InconsistentKeys(InconsistentKeys::Unknown)) => {}
        Err(e) => return Err(e),
    }
    Ok(ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(root)
        .with_client_cert_resolver(Arc::new(SingleCertAndKey::from(certified))))
}

/// A refused handshake as the device says it (device link §5.3, §5.5,
/// §6.3; kai-mobile §4.4). Read an enrolment's with
/// [`TlsRefusal::of_enrolment`], every later connection's with
/// [`TlsRefusal::of`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TlsRefusal {
    /// At enrolment: the server is not the lmgw the link pins. No
    /// certificate it sent carries the key `fp` pins, or its own certificate
    /// does not chain to that key (the CA certificate is public, so a
    /// stranger can send it along).
    NotPairedLmgw,
    /// At enrolment: a `certificate_required` alert. lmgw sends it when no
    /// pairing token lives (device link §4.2), but an alert can arrive
    /// before the server proved anything (it is unauthenticated), so anyone
    /// on the path could have sent it: the handshake failed, the caller may
    /// retry, and nothing follows about the link from one. (An ended token
    /// meets it on every retry, as no request of an enrolment gets through
    /// while no token lives; a caller that keeps meeting it may suggest the
    /// owner makes a new link, as a suggestion and not a finding.)
    Unproven,
    /// After enrolment: the server's certificate is not from the stored CA
    /// (an unknown issuer, a bad signature): lmgw rotated its device CA, or
    /// this is not the owner's lmgw.
    AuthorityChanged,
    /// The server's certificate does not name the URL's host; `host` as the
    /// client asked for it, when the error says.
    HostNotNamed { host: Option<String> },
    /// The server's certificate is outside its dates at the device's time:
    /// the device's clock, most likely.
    ServerCertificateDates,
    /// lmgw refused the device's certificate as lapsed (TLS's
    /// `certificate_expired` alert).
    CertificateLapsed,
    /// lmgw refused the device's certificate otherwise (an unknown CA, a bad
    /// certificate, none sent).
    CertificateRefused,
}

impl TlsRefusal {
    /// What a rustls error of an enrolment's handshake ([`enrol_config`])
    /// says: [`Self::Unproven`] for a `certificate_required` alert (which
    /// [`Self::of`] reads as a refused certificate, since a paired device
    /// that sent none is refused by it), otherwise as [`Self::of`] reads it.
    pub fn of_enrolment(e: &Error) -> Option<Self> {
        match e {
            Error::AlertReceived(AlertDescription::CertificateRequired) => Some(Self::Unproven),
            e => Self::of(e),
        }
    }

    /// What a rustls error of a connection with the paired configuration
    /// ([`client_config`]) says, when it is one of these; `None` for any
    /// other (show rustls's own words). Find the error inside an HTTP or
    /// WebSocket client's with [`find_tls_error`].
    pub fn of(e: &Error) -> Option<Self> {
        Some(match e {
            Error::InvalidCertificate(c) => match c {
                CertificateError::Other(OtherError(o))
                    if o.downcast_ref::<NotPinned>().is_some() =>
                {
                    Self::NotPairedLmgw
                }
                CertificateError::UnknownIssuer | CertificateError::BadSignature => {
                    Self::AuthorityChanged
                }
                CertificateError::NotValidForName => Self::HostNotNamed { host: None },
                CertificateError::NotValidForNameContext { expected, .. } => Self::HostNotNamed {
                    host: Some(expected.to_str().into_owned()),
                },
                CertificateError::Expired
                | CertificateError::ExpiredContext { .. }
                | CertificateError::NotValidYet
                | CertificateError::NotValidYetContext { .. } => Self::ServerCertificateDates,
                _ => return None,
            },
            Error::AlertReceived(AlertDescription::CertificateExpired) => Self::CertificateLapsed,
            Error::AlertReceived(
                AlertDescription::BadCertificate
                | AlertDescription::UnknownCA
                | AlertDescription::CertificateRequired
                | AlertDescription::CertificateUnknown
                | AlertDescription::CertificateRevoked
                | AlertDescription::UnsupportedCertificate,
            ) => Self::CertificateRefused,
            _ => return None,
        })
    }
}

impl fmt::Display for TlsRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPairedLmgw => f.write_str(
                "this is not the lmgw this link pairs with: its certificate does not chain to the \
                 key the link's fp pins; nothing was sent to it",
            ),
            Self::Unproven => f.write_str(
                "the handshake failed with a certificate_required alert before lmgw proved \
                 itself, so this says nothing about the pairing link: try again (if it keeps \
                 failing, make a new link on lmgw's Devices card)",
            ),
            Self::AuthorityChanged => {
                f.write_str("lmgw's device authority changed, or this is not your lmgw: pair again")
            }
            Self::HostNotNamed { host } => write!(
                f,
                "lmgw's certificate does not name {}: add this URL to the device's row on lmgw's \
                 Devices card",
                host.as_deref().unwrap_or("this URL's host")
            ),
            Self::ServerCertificateDates => f.write_str(
                "lmgw's certificate is not valid at this device's time: check the device's clock",
            ),
            Self::CertificateLapsed => {
                f.write_str("this device's certificate lapsed: pair it again")
            }
            Self::CertificateRefused => {
                f.write_str("lmgw refused this device's certificate: pair the device again")
            }
        }
    }
}

/// The rustls error inside `e`, an HTTP or WebSocket client's error: `e`
/// itself, or one in its `source` chain, looking into an `io::Error`'s
/// wrapped error too (whose `source` skips it).
pub fn find_tls_error<'a>(mut e: &'a (dyn std::error::Error + 'static)) -> Option<&'a Error> {
    loop {
        if let Some(tls) = e.downcast_ref::<Error>() {
            return Some(tls);
        }
        if let Some(inner) = e
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
        {
            e = inner;
            continue;
        }
        e = e.source()?;
    }
}

#[cfg(test)]
mod tests;
