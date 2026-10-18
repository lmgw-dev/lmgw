//! Device certificates (device link design §2.3, D4, D5): issued by the CA
//! for the public key a device enrols with — its DER SubjectPublicKeyInfo,
//! EC P-256 only — as `CN=<device name>, O=lmgw device`, `clientAuth` only,
//! `KeyUsage: digitalSignature`, a random 127-bit serial, `notBefore` a day
//! back and `notAfter` `devices.client_cert_days` on. `notice_after` is two
//! thirds of the way: from then on the card and the device say it lapses.
//!
//! The CA does not decide who a device is; the row does: a certificate gets
//! a client through the handshake, and its key, matched to a confirmed row
//! by [`key_hash`], is the authentication.

use chrono::{DateTime, Utc};
use lmgw_api_types::device_link as wire;
use rcgen::{CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyUsagePurpose, PublicKeyData};
use sha2::{Digest, Sha256};

use super::ca::{random_serial, set_validity, Ca};

/// Why an enrolling key is refused, 400
/// [`wire::code::PUBLIC_KEY_UNSUPPORTED`]. The message names the accepted
/// kind and never repeats the key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublicKeyRefused {
    /// Not a DER SubjectPublicKeyInfo, or not in canonical form.
    #[error(
        "public_key is not a DER SubjectPublicKeyInfo ({0}); send an EC P-256 key as the DER \
         SubjectPublicKeyInfo, standard base64"
    )]
    NotSpki(String),
    /// Another kind of key.
    #[error("public_key is {0}; lmgw takes EC P-256 keys only (id-ecPublicKey, prime256v1)")]
    Unsupported(String),
}

/// Why a device certificate was not issued.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IssueError {
    #[error(transparent)]
    PublicKey(#[from] PublicKeyRefused),
    #[error("{0}")]
    Sign(String),
}

/// A device certificate as enrolment answers it and the device row keeps it
/// (§3.4, §3.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCertificate {
    /// The certificate, DER.
    pub der: Vec<u8>,
    /// Its serial, lowercase hex as a parser reads it back.
    pub serial: String,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    /// Two thirds of the validity from the issue.
    pub notice_after: DateTime<Utc>,
    /// The device row's `key_hash`: `spki:` and the hex SHA-256 of the key.
    pub key_hash: String,
    /// The confirmation code both ends show (§3.5), in the form
    /// [`wire::confirm_code`] shares with `lmgw-client`.
    pub confirm_code: String,
}

/// The device row's `key_hash` for a key (D5): `spki:` and the lowercase
/// hex SHA-256 of its DER SubjectPublicKeyInfo.
pub fn key_hash(spki_der: &[u8]) -> String {
    format!("spki:{}", hex::encode(Sha256::digest(spki_der)))
}

/// What a SubjectPublicKeyInfo's algorithm is, in words, for a refusal.
fn algorithm_name(spki_der: &[u8]) -> String {
    use x509_parser::prelude::FromDer;
    let Ok((_, spki)) = x509_parser::x509::SubjectPublicKeyInfo::from_der(spki_der) else {
        return "an unknown key".into();
    };
    let alg = spki.algorithm.algorithm.to_id_string();
    let curve = spki
        .algorithm
        .parameters
        .as_ref()
        .and_then(|p| p.as_oid().ok())
        .map(|o| o.to_id_string());
    match (alg.as_str(), curve.as_deref()) {
        ("1.2.840.113549.1.1.1", _) => "an RSA key".into(),
        ("1.3.101.112", _) => "an Ed25519 key".into(),
        ("1.2.840.10045.2.1", Some("1.3.132.0.34")) => "an EC P-384 key".into(),
        ("1.2.840.10045.2.1", Some("1.3.132.0.35")) => "an EC P-521 key".into(),
        ("1.2.840.10045.2.1", Some(c)) => format!("an EC key on curve {c}"),
        (other, _) => format!("a key of algorithm {other}"),
    }
}

/// `spki_der` as the key a device certificate is issued for: a canonical
/// DER SubjectPublicKeyInfo of an uncompressed EC P-256 point on the curve.
/// Canonical, so that the SHA-256 lmgw keeps at enrolment is the one it
/// computes from every certificate the device presents.
pub fn check_public_key(spki_der: &[u8]) -> Result<rcgen::SubjectPublicKeyInfo, PublicKeyRefused> {
    let spki = rcgen::SubjectPublicKeyInfo::from_der(spki_der).map_err(|e| match e {
        rcgen::Error::UnsupportedSignatureAlgorithm => {
            PublicKeyRefused::Unsupported(algorithm_name(spki_der))
        }
        other => PublicKeyRefused::NotSpki(other.to_string()),
    })?;
    if spki.algorithm() != &rcgen::PKCS_ECDSA_P256_SHA256 {
        return Err(PublicKeyRefused::Unsupported(algorithm_name(spki_der)));
    }
    let point = spki.der_bytes();
    if point.len() != 65 || point[0] != 0x04 {
        return Err(PublicKeyRefused::Unsupported(
            "an EC P-256 key in compressed form".into(),
        ));
    }
    aws_lc_rs::signature::ParsedPublicKey::new(
        &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1,
        point,
    )
    .map_err(|_| PublicKeyRefused::NotSpki("its point is not on P-256".into()))?;
    if spki.subject_public_key_info() != spki_der {
        return Err(PublicKeyRefused::NotSpki(
            "not in canonical DER form".into(),
        ));
    }
    Ok(spki)
}

/// A device certificate from `ca` for the device key `spki_der`, named
/// `name` (the device's name without `device:`), valid `days` from `now`
/// (`devices.client_cert_days`).
pub fn issue_device(
    ca: &Ca,
    spki_der: &[u8],
    name: &str,
    days: u32,
    now: DateTime<Utc>,
) -> Result<DeviceCertificate, IssueError> {
    let validity = chrono::Duration::days(i64::from(days.max(1)));
    issue_device_for(ca, spki_der, name, now, validity)
}

/// [`issue_device`] valid `validity` from `now`, to the second: what a
/// suite issues to watch a certificate lapse while its links are open
/// (§5.1, §13.1).
pub fn issue_device_for(
    ca: &Ca,
    spki_der: &[u8],
    name: &str,
    now: DateTime<Utc>,
    validity: chrono::Duration,
) -> Result<DeviceCertificate, IssueError> {
    let key = check_public_key(spki_der)?;
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, name);
    params
        .distinguished_name
        .push(DnType::OrganizationName, "lmgw device");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.use_authority_key_identifier_extension = true;
    let (serial, serial_hex) = random_serial().map_err(IssueError::Sign)?;
    params.serial_number = Some(serial);
    let now = DateTime::from_timestamp(now.timestamp(), 0).unwrap_or(now);
    let not_before = now - chrono::Duration::days(1);
    let not_after = now + validity;
    set_validity(&mut params, not_before, not_after);
    let cert = params
        .signed_by(&key, ca.issuer())
        .map_err(|e| IssueError::Sign(format!("signing the device certificate: {e}")))?;
    Ok(DeviceCertificate {
        der: cert.der().to_vec(),
        serial: serial_hex,
        not_before,
        not_after,
        notice_after: now + validity * 2 / 3,
        key_hash: key_hash(spki_der),
        confirm_code: wire::confirm_code(&Sha256::digest(spki_der).into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_hash_is_the_spki_hash_with_its_prefix() {
        let spki = b"any bytes";
        let d = Sha256::digest(spki);
        assert_eq!(key_hash(spki), format!("spki:{}", hex::encode(d)));
    }
}
