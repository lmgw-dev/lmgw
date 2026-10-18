//! Certificates for the tests, made with rcgen as lmgw makes them (device
//! link design §2): a device CA (P-256, `pathlen 0`, `keyCertSign` and
//! `cRLSign`, valid to 9999-12-31), server leaves (`serverAuth`, the
//! addresses as SANs) and device certificates (`clientAuth`), each for a
//! fresh P-256 key.

use rcgen::{
    date_time_ymd, BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, PublicKeyData,
};

use crate::pairing::Fingerprint;

pub(crate) type Ca = CertifiedIssuer<'static, KeyPair>;

/// A device CA named as lmgw names its own; `cn` tells them apart.
pub(crate) fn ca(cn: &str) -> Ca {
    ca_naming(cn, &[])
}

/// A device CA with `names` as SANs as well, which lmgw's never has: for a
/// test that must not fail on the host name.
pub(crate) fn ca_naming(cn: &str, names: &[&str]) -> Ca {
    let mut p =
        CertificateParams::new(names.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    dn.push(DnType::OrganizationName, "lmgw");
    p.distinguished_name = dn;
    p.not_before = date_time_ymd(2026, 1, 1);
    p.not_after = date_time_ymd(9999, 12, 31);
    CertifiedIssuer::self_signed(p, KeyPair::generate().unwrap()).unwrap()
}

/// `ca` as the issuer of what it signs.
fn issuer(ca: &Ca) -> &Issuer<'static, KeyPair> {
    ca
}

/// The `fp` that pins `ca`.
pub(crate) fn fp(ca: &Ca) -> Fingerprint {
    Fingerprint::of_spki(&ca.key().subject_public_key_info())
}

/// A certificate and its key.
pub(crate) struct Leaf {
    pub der: Vec<u8>,
    pub key: KeyPair,
}

/// Validity: the years `from` to `to` (1 January each).
pub(crate) type Years = (i32, i32);
/// Valid now and for decades.
pub(crate) const CURRENT: Years = (2020, 2100);
/// Lapsed years ago.
pub(crate) const LAPSED: Years = (2020, 2021);

fn params(names: &[&str], eku: ExtendedKeyUsagePurpose, years: Years) -> CertificateParams {
    let mut p =
        CertificateParams::new(names.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = vec![eku];
    p.use_authority_key_identifier_extension = true;
    p.not_before = date_time_ymd(years.0, 1, 1);
    p.not_after = date_time_ymd(years.1, 1, 1);
    p
}

/// A server leaf for `names` (IP addresses become IP SANs), signed by `ca`.
pub(crate) fn server_leaf(ca: &Ca, names: &[&str], years: Years) -> Leaf {
    leaf(ca, names, ExtendedKeyUsagePurpose::ServerAuth, years)
}

/// A device certificate for a fresh key, signed by `ca`.
pub(crate) fn device_cert(ca: &Ca, years: Years) -> Leaf {
    leaf(ca, &[], ExtendedKeyUsagePurpose::ClientAuth, years)
}

/// A certificate for a fresh key with extended key usage `eku`.
pub(crate) fn leaf(ca: &Ca, names: &[&str], eku: ExtendedKeyUsagePurpose, years: Years) -> Leaf {
    let key = KeyPair::generate().unwrap();
    let der = params(names, eku, years)
        .signed_by(&key, issuer(ca))
        .unwrap()
        .der()
        .to_vec();
    Leaf { der, key }
}

/// A device certificate for `key`, as enrolment issues it.
pub(crate) fn device_cert_for(ca: &Ca, key: &KeyPair) -> Vec<u8> {
    params(&[], ExtendedKeyUsagePurpose::ClientAuth, CURRENT)
        .signed_by(key, issuer(ca))
        .unwrap()
        .der()
        .to_vec()
}
