//! lmgw's device CA (device link design §2.1, D1, D2): one ECDSA P-256 key
//! that signs lmgw's server certificates and every device's client
//! certificate. Self-signed, `CN=lmgw devices <8 hex of fp>, O=lmgw`,
//! `BasicConstraints: CA, pathlen 0`, `KeyUsage: keyCertSign, cRLSign`, a
//! random 127-bit serial, `notBefore` its creation day and `notAfter`
//! 9999-12-31T23:59:59Z, RFC 5280's "no well-defined expiration": a rustls
//! trust anchor is the subject, the key and the name constraints alone, so
//! the pin is the key.
//!
//! `fp` is `sha256:` and the lowercase hex SHA-256 of the certificate's DER
//! SubjectPublicKeyInfo, RFC 7469's pin input:
//! `openssl x509 -in ca.crt -pubkey -noout | openssl pkey -pubin -outform DER | sha256sum`.

use chrono::{DateTime, Utc};
use lmgw_api_types::device_link::{self as wire, FP_PREFIX};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
    PublicKeyData, SerialNumber,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use secrecy::zeroize::Zeroizing;
use sha2::{Digest, Sha256};

use super::files::{CERT_NAME, KEY_NAME};
use super::server_config::ClientVerifiers;
use super::trust::TrustError;

/// How the key file keeps the key (D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMode {
    /// Encrypted under a key in the session's Secret Service.
    Wrapped,
    /// Plain PKCS#8, `0600`: a start without a Secret Service.
    Plain,
}

impl KeyMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Wrapped => "wrapped",
            Self::Plain => "plain",
        }
    }
}

impl From<KeyMode> for wire::DeviceCaKeyMode {
    fn from(m: KeyMode) -> Self {
        match m {
            KeyMode::Wrapped => Self::Wrapped,
            KeyMode::Plain => Self::Plain,
        }
    }
}

/// [`FP_PREFIX`] and the lowercase hex SHA-256 of `spki_der`: the pairing
/// link's `fp` for a CA certificate's SubjectPublicKeyInfo.
pub fn fingerprint(spki_der: &[u8]) -> String {
    format!("{FP_PREFIX}{}", hex::encode(Sha256::digest(spki_der)))
}

/// The public facts of the CA's certificate: what the status shows without
/// opening the key, and what the key must match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaFacts {
    /// The pin (§2.1).
    pub fp: String,
    /// The creation day: the certificate's `notBefore`.
    pub created: DateTime<Utc>,
    spki: Vec<u8>,
}

impl CaFacts {
    /// The facts of the certificate `der`.
    pub fn read(der: &[u8]) -> Result<Self, String> {
        let (rest, cert) = x509_parser::parse_x509_certificate(der)
            .map_err(|e| format!("not an X.509 certificate ({e})"))?;
        if !rest.is_empty() {
            return Err("trailing bytes after the certificate".into());
        }
        let spki = cert.public_key().raw.to_vec();
        let created = DateTime::from_timestamp(cert.validity().not_before.timestamp(), 0)
            .ok_or_else(|| "its notBefore is out of range".to_string())?;
        Ok(Self {
            fp: fingerprint(&spki),
            created,
            spki,
        })
    }

    /// `sha256:9f2c…41ab`: the pin shortened for a log line.
    pub fn fp_short(&self) -> String {
        let hex = self.fp.trim_start_matches(FP_PREFIX);
        format!("{FP_PREFIX}{}…{}", &hex[..4], &hex[hex.len() - 4..])
    }
}

/// The CA loaded to sign: its certificate, facts and key mode, the issuer
/// holding its key, and the client verifiers over it. Its `Debug` shows no
/// key.
pub struct Ca {
    der: CertificateDer<'static>,
    facts: CaFacts,
    mode: KeyMode,
    issuer: rcgen::Issuer<'static, KeyPair>,
    verifiers: ClientVerifiers,
}

impl std::fmt::Debug for Ca {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ca")
            .field("fp", &self.facts.fp)
            .field("created", &self.facts.created)
            .field("mode", &self.mode)
            .field("key", &"[redacted]")
            .finish()
    }
}

impl Ca {
    /// The certificate, DER.
    pub fn certificate(&self) -> &CertificateDer<'static> {
        &self.der
    }

    /// The certificate as `ca.crt` holds it.
    pub fn certificate_pem(&self) -> String {
        pem::encode(&pem::Pem::new("CERTIFICATE", self.der.to_vec()))
    }

    pub fn facts(&self) -> &CaFacts {
        &self.facts
    }

    /// The pin (§2.1).
    pub fn fp(&self) -> &str {
        &self.facts.fp
    }

    pub fn key_mode(&self) -> KeyMode {
        self.mode
    }

    pub(super) fn issuer(&self) -> &rcgen::Issuer<'static, KeyPair> {
        &self.issuer
    }

    pub(super) fn verifiers(&self) -> &ClientVerifiers {
        &self.verifiers
    }
}

/// A new CA: its certificate and its PKCS#8 key, not yet written.
pub(super) struct Generated {
    pub der: CertificateDer<'static>,
    pub pkcs8: Zeroizing<Vec<u8>>,
}

/// Make a CA on `now`'s day.
pub(super) fn generate(now: DateTime<Utc>) -> Result<Generated, String> {
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| format!("generating the CA key: {e}"))?;
    let fp = fingerprint(&key.subject_public_key_info());
    let hex = fp.trim_start_matches(FP_PREFIX);
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, format!("lmgw devices {}", &hex[..8]));
    dn.push(DnType::OrganizationName, "lmgw");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.serial_number = Some(random_serial()?.0);
    let day = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|t| t.and_utc())
        .unwrap_or(now);
    let never = DateTime::from_timestamp(NO_EXPIRY, 0).unwrap_or(now);
    set_validity(&mut params, day, never);
    let cert = params
        .self_signed(&key)
        .map_err(|e| format!("signing the CA certificate: {e}"))?;
    Ok(Generated {
        der: cert.der().clone(),
        pkcs8: Zeroizing::new(key.serialize_der()),
    })
}

/// 9999-12-31T23:59:59Z (RFC 5280 §4.1.2.5's "no well-defined expiration").
pub const NO_EXPIRY: i64 = 253_402_300_799;

/// The CA of certificate `der` and PKCS#8 key `pkcs8`, as read from its
/// files: the key must be P-256 and belong to the certificate.
pub(super) fn load(
    der: CertificateDer<'static>,
    pkcs8: &[u8],
    mode: KeyMode,
) -> Result<Ca, TrustError> {
    let cert_err = |why: String| TrustError::Unreadable(format!("{CERT_NAME}: {why}"));
    let key_err = |why: &str| TrustError::Unreadable(format!("{KEY_NAME}: {why}"));
    let facts = CaFacts::read(&der).map_err(cert_err)?;
    let key = KeyPair::from_pkcs8_der_and_sign_algo(
        &PrivatePkcs8KeyDer::from(pkcs8),
        &rcgen::PKCS_ECDSA_P256_SHA256,
    )
    .map_err(|_| key_err("not an ECDSA P-256 PKCS#8 key"))?;
    if key.subject_public_key_info() != facts.spki {
        return Err(key_err(
            "the key does not belong to ca.crt (a CA made or rotated half-way); rotate the \
             device CA",
        ));
    }
    let issuer = rcgen::Issuer::from_ca_cert_der(&der, key)
        .map_err(|e| cert_err(format!("cannot sign with it: {e}")))?;
    let verifiers = ClientVerifiers::new(&der).map_err(cert_err)?;
    Ok(Ca {
        der,
        facts,
        mode,
        issuer,
        verifiers,
    })
}

/// A random 127-bit serial number (§2.1, §2.3) and its hex as a parser
/// reads it back.
pub(super) fn random_serial() -> Result<(SerialNumber, String), String> {
    let mut b = [0u8; 16];
    aws_lc_rs::rand::fill(&mut b).map_err(|_| "the random source failed".to_string())?;
    Ok(serial_from(b))
}

/// The serial for `b`, and its hex. The first byte is forced into
/// `0x01..=0x7f`: the top bit clear keeps the INTEGER positive without a DER
/// sign byte, and non-zero means rcgen writes exactly these 16 bytes, so the
/// hex of all of them is what any parser reads back (a leading `00` is only
/// ever a sign byte, never part of our serial). Stored and compared serials
/// are this string; `DevicePeer::read` strips only a sign byte to match.
fn serial_from(mut b: [u8; 16]) -> (SerialNumber, String) {
    b[0] &= 0x7f;
    if b[0] == 0 {
        b[0] = 1;
    }
    (SerialNumber::from_slice(&b), hex::encode(b))
}

/// `from` and `until` as a certificate's `notBefore` and `notAfter`, to the
/// second.
pub(super) fn set_validity(
    params: &mut CertificateParams,
    from: DateTime<Utc>,
    until: DateTime<Utc>,
) {
    let at = |t: DateTime<Utc>| {
        rcgen::date_time_ymd(1970, 1, 1)
            + std::time::Duration::from_secs(u64::try_from(t.timestamp()).unwrap_or(0))
    };
    params.not_before = at(from);
    params.not_after = at(until);
}

#[cfg(test)]
mod serial_tests {
    use super::*;

    /// The serial the gate's reader gets from a certificate issued with `serial`.
    fn parsed(serial: SerialNumber) -> String {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["x".to_string()]).unwrap();
        params.serial_number = Some(serial);
        let cert = params.self_signed(&key).unwrap();
        crate::server::DevicePeer::read(cert.der()).unwrap().serial
    }

    #[test]
    fn a_serial_whose_random_bytes_start_with_zero_matches_its_certificate() {
        let mut b = [0x5au8; 16];
        b[0] = 0x00;
        b[1] = 0x95;
        let (serial, hex) = serial_from(b);
        assert!(!hex.starts_with("00"));
        assert_eq!(hex, parsed(serial));
    }

    #[test]
    fn a_serial_with_no_leading_zero_matches_its_certificate() {
        for first in [0x01u8, 0x7f, 0x95, 0xff] {
            let mut b = [0x5au8; 16];
            b[0] = first;
            let (serial, hex) = serial_from(b);
            assert_eq!(hex.len(), 32);
            assert_eq!(hex, parsed(serial));
        }
    }

    #[test]
    fn random_serials_match_their_certificates() {
        for _ in 0..64 {
            let (serial, hex) = random_serial().unwrap();
            assert_eq!(hex, parsed(serial));
        }
    }
}
