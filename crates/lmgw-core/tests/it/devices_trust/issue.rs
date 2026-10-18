//! Device certificates (§2.3, D4, D5): issued for the sent EC P-256 key,
//! `clientAuth` only, with the stated dates, the row's `key_hash` and the
//! confirmation code; every other key refused, naming the accepted kind.

use chrono::Utc;
use lmgw_api_types::device_link::{code::PUBLIC_KEY_UNSUPPORTED, confirm_code};
use lmgw_core::devices::tls::{
    check_public_key, issue_device, key_hash, IssueError, PublicKeyRefused,
};
use rcgen::{KeyPair, PublicKeyData};
use sha2::{Digest, Sha256};

use super::{device_key, plain_ca, spki};

#[tokio::test]
async fn a_device_certificate_is_for_the_sent_key_and_the_ca_signed_it() {
    let dir = tempfile::tempdir().unwrap();
    let (_, ca) = plain_ca(dir.path()).await;
    let key = device_key();
    let spki = spki(&key);
    let now = Utc::now();
    let issued = issue_device(&ca, &spki, "phone", 365, now).unwrap();

    let (_, cert) = x509_parser::parse_x509_certificate(&issued.der).unwrap();
    assert_eq!(
        cert.public_key().raw,
        spki.as_slice(),
        "the sent key, as sent"
    );
    let names = |n: &x509_parser::x509::X509Name<'_>| -> Vec<String> {
        n.iter_common_name()
            .chain(n.iter_organization())
            .map(|a| a.as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(names(cert.subject()), ["phone", "lmgw device"]);
    let (_, ca_cert) = x509_parser::parse_x509_certificate(ca.certificate()).unwrap();
    assert_eq!(cert.issuer(), ca_cert.subject());
    cert.verify_signature(Some(ca_cert.public_key()))
        .expect("the CA signed it");
    // clientAuth alone and no subjectAltName: with both, a device's
    // certificate never passes a client's check of lmgw's server leaf
    // (lmgw-client's pin relies on it;
    // `handshake::a_device_certificate_never_passes_as_lmgw`).
    let eku = cert.extended_key_usage().unwrap().unwrap().value;
    assert!(eku.client_auth && !eku.server_auth && !eku.any && eku.other.is_empty());
    assert!(
        cert.subject_alternative_name().unwrap().is_none(),
        "no SANs"
    );
    let ku = cert.key_usage().unwrap().unwrap().value;
    assert!(ku.digital_signature() && !ku.key_cert_sign() && !ku.key_encipherment());
    assert!(cert
        .basic_constraints()
        .unwrap()
        .is_none_or(|b| !b.value.ca));

    let secs = now.timestamp();
    assert_eq!(cert.validity().not_before.timestamp(), secs - 86_400);
    assert_eq!(cert.validity().not_after.timestamp(), secs + 365 * 86_400);
    assert_eq!(issued.not_after.timestamp(), secs + 365 * 86_400);
    assert_eq!(issued.not_before.timestamp(), secs - 86_400);
    // The notice from two thirds of the validity on.
    assert_eq!(issued.notice_after.timestamp(), secs + 365 * 86_400 * 2 / 3);
    // A random positive serial, as a parser reads it back.
    assert_eq!(issued.serial, hex::encode(cert.raw_serial()));
    assert!(cert.raw_serial().len() <= 16 && cert.raw_serial()[0] < 0x80);
    // The row's key hash and the confirmation code, from the key alone.
    let d = Sha256::digest(&spki);
    assert_eq!(issued.key_hash, format!("spki:{}", hex::encode(d)));
    assert_eq!(issued.key_hash, key_hash(&spki));
    assert_eq!(
        issued.confirm_code,
        format!("{:02X}{:02X}-{:02X}{:02X}", d[0], d[1], d[2], d[3])
    );
    assert_eq!(issued.confirm_code, confirm_code(&d.into()));

    // Re-pairing the same key gives another certificate (a new serial) for
    // the same key hash and code; another key gives another code.
    let again = issue_device(&ca, &spki, "phone", 365, now).unwrap();
    assert_ne!(again.serial, issued.serial);
    assert_eq!(again.key_hash, issued.key_hash);
    let other = issue_device(&ca, &super::spki(&device_key()), "phone", 365, now).unwrap();
    assert_ne!(other.confirm_code, issued.confirm_code);
    // The validity is the setting's.
    let short = issue_device(&ca, &spki, "phone", 1, now).unwrap();
    assert_eq!(short.not_after.timestamp(), secs + 86_400);
}

#[tokio::test]
async fn every_other_key_is_refused_naming_the_accepted_kind() {
    let dir = tempfile::tempdir().unwrap();
    let (_, ca) = plain_ca(dir.path()).await;
    let now = Utc::now();
    let refused = |der: &[u8]| -> PublicKeyRefused {
        match issue_device(&ca, der, "x", 365, now).unwrap_err() {
            IssueError::PublicKey(r) => {
                assert_eq!(check_public_key(der).unwrap_err(), r);
                let text = r.to_string();
                assert!(text.contains("P-256"), "{text}");
                // The key itself is never repeated.
                assert!(!text.contains(&hex::encode(der)), "{text}");
                r
            }
            other => panic!("{other}"),
        }
    };
    assert_eq!(PUBLIC_KEY_UNSUPPORTED, "public_key_unsupported");

    let p384 = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    assert_eq!(
        refused(&p384.subject_public_key_info()),
        PublicKeyRefused::Unsupported("an EC P-384 key".into())
    );
    let ed = KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    assert_eq!(
        refused(&ed.subject_public_key_info()),
        PublicKeyRefused::Unsupported("an Ed25519 key".into())
    );
    let rsa = KeyPair::generate_rsa_for(&rcgen::PKCS_RSA_SHA256, rcgen::RsaKeySize::_2048).unwrap();
    assert_eq!(
        refused(&rsa.subject_public_key_info()),
        PublicKeyRefused::Unsupported("an RSA key".into())
    );
    assert!(matches!(
        refused(b"not a key"),
        PublicKeyRefused::NotSpki(_)
    ));

    let good = spki(&device_key());
    // Trailing bytes after the structure.
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(matches!(refused(&trailing), PublicKeyRefused::NotSpki(_)));
    // The same key's point off the curve.
    let mut off = good.clone();
    *off.last_mut().unwrap() ^= 0x01;
    assert_eq!(
        refused(&off),
        PublicKeyRefused::NotSpki("its point is not on P-256".into())
    );
    // A compressed point: the algorithm identifier of a P-256 key, then a
    // 33-byte point.
    let alg = &good[2..2 + 0x15];
    let x = &good[good.len() - 64..good.len() - 32];
    let mut bits = vec![0x03, 0x22, 0x00, 0x02];
    bits.extend_from_slice(x);
    let mut compressed = vec![0x30, (alg.len() + bits.len()) as u8];
    compressed.extend_from_slice(alg);
    compressed.extend_from_slice(&bits);
    assert_eq!(
        refused(&compressed),
        PublicKeyRefused::Unsupported("an EC P-256 key in compressed form".into())
    );
    // The same key, not in canonical DER: the outer length in long form
    // (`81 59` for `59`), and a BIT STRING that claims unused bits. Either
    // would hash otherwise than the certificate's own encoding.
    let mut long_length = vec![0x30, 0x81, 0x59];
    long_length.extend_from_slice(&good[2..]);
    assert!(
        matches!(refused(&long_length), PublicKeyRefused::NotSpki(_)),
        "a non-minimal length"
    );
    let mut unused = good.clone();
    let bit_string = 2 + 0x15;
    assert_eq!(&unused[bit_string..bit_string + 3], &[0x03, 0x42, 0x00]);
    unused[bit_string + 2] = 0x01;
    assert!(
        matches!(refused(&unused), PublicKeyRefused::NotSpki(_)),
        "unused bits"
    );
    // The good key passes.
    check_public_key(&good).unwrap();
}

#[test]
fn a_canonical_p256_key_hashes_as_sent() {
    // Android's `PublicKey.getEncoded()` and rcgen both write the canonical
    // form: the SubjectPublicKeyInfo of a P-256 key is 91 bytes.
    let k = device_key();
    let s = spki(&k);
    assert_eq!(s.len(), 91);
    assert_eq!(&s[..4], &[0x30, 0x59, 0x30, 0x13]);
}
