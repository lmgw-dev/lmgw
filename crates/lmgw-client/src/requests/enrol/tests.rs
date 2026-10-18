//! The enrolment's request and reader, against certificates rcgen makes.

use rcgen::{KeyPair, PublicKeyData};

use super::*;
use crate::requests::{key_refused, KeyRefused};
use crate::test_pki::{self, Ca};

const TOKEN: &str = "lmgw-enrol-4b1d0123456789abcdef0123456789ab";

fn link_for(ca: &Ca, urls: &[&str]) -> PairingLink {
    let urls: String = urls
        .iter()
        .map(|u| format!("&url={}", u.replace(':', "%3A").replace('/', "%2F")))
        .collect();
    PairingLink::parse(&format!(
        "lmgw-pair:?v=2&name=phone{urls}&fp={}&token={TOKEN}",
        test_pki::fp(ca)
    ))
    .unwrap()
}

/// The answer lmgw gives for `key`, as §3.4 writes it.
fn answer(ca: &Ca, key: &KeyPair, urls: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "device": {"id": 12, "name": "phone"},
        "certificate": crate::base64::encode(&test_pki::device_cert_for(ca, key)),
        "ca_certificate": crate::base64::encode(ca.der()),
        "not_after": "2027-10-14T14:03:11Z",
        "notice_after": "2027-06-14T14:03:11Z",
        "urls": urls,
        "confirm_code": confirm_code(&key.subject_public_key_info()),
        "confirmed": false,
    })
}

#[test]
fn the_enrolment_posts_the_token_and_the_key() {
    let token = PairingToken::parse(TOKEN).unwrap();
    let r = enrol(
        &token,
        &[0x30, 0x59, 0x30],
        Some("Kai mobile 0.1.0 (Android 16)"),
    );
    assert_eq!(
        (r.method, r.path.as_str()),
        (Method::Post, "/devices/enrol")
    );
    let body: serde_json::Value = serde_json::from_str(r.body.as_deref().unwrap()).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"token": TOKEN, "public_key": "MFkw",
                           "client": "Kai mobile 0.1.0 (Android 16)"})
    );
    assert!(r.headers.iter().all(|h| h.name != "Authorization"));
    let r = enrol(&token, &[1], None);
    assert!(!r.body.unwrap().contains("client"));
}

#[test]
fn an_answer_reads_as_credentials_with_its_dates_and_https_urls() {
    let ca = test_pki::ca("lmgw devices 9f2c0123");
    let key = KeyPair::generate().unwrap();
    let spki = key.subject_public_key_info();
    let link = link_for(&ca, &["https://192.0.2.1:8788"]);
    let body = answer(
        &ca,
        &key,
        &[
            "https://192.0.2.1:8788/",
            "http://198.51.100.20:8787",
            "https://198.51.100.20:8788",
        ],
    );
    let c = read_enrolment(201, &body.to_string(), &link, &spki).unwrap();
    assert_eq!(c.device_id, 12);
    assert_eq!(c.ca_certificate, ca.der().to_vec());
    assert_eq!(
        crate::der::certificate_spki(&c.certificate),
        Some(spki.as_slice())
    );
    assert_eq!(
        c.urls.as_slice(),
        ["https://192.0.2.1:8788", "https://198.51.100.20:8788"],
        "the http URL is dropped"
    );
    assert_eq!(c.confirm_code, confirm_code(&spki));
    assert_eq!(c.confirm_code.len(), 9);
    // The dates decide the state.
    let (notice, lapse) = (1_812_981_791_000, 1_823_522_591_000);
    assert_eq!((c.notice_after_ms, c.not_after_ms), (notice, lapse));
    assert_eq!(c.state(notice - 1), CredentialState::Valid);
    assert_eq!(c.state(notice), CredentialState::Notice);
    assert_eq!(c.state(lapse - 1), CredentialState::Notice);
    assert_eq!(c.state(lapse), CredentialState::Lapsed);
    // Kept as JSON and read back as it was; the debug form shows no bytes.
    let stored = serde_json::to_string(&c).unwrap();
    assert_eq!(serde_json::from_str::<Credentials>(&stored).unwrap(), c);
    assert!(format!("{c:?}").contains("bytes>"));
    // No https URL in the answer: the link's stay.
    let body = answer(&ca, &key, &["http://198.51.100.20:8787"]);
    let c = read_enrolment(201, &body.to_string(), &link, &spki).unwrap();
    assert_eq!(c.urls, link.urls);
}

#[test]
fn an_answer_that_does_not_hold_together_is_refused() {
    let ca = test_pki::ca("lmgw devices 9f2c0123");
    let key = KeyPair::generate().unwrap();
    let spki = key.subject_public_key_info();
    let link = link_for(&ca, &["https://192.0.2.1:8788"]);
    let urls = ["https://192.0.2.1:8788"];
    let read = |body: serde_json::Value| read_enrolment(201, &body.to_string(), &link, &spki);

    // Another CA than the link pins, even one with lmgw's name.
    let impostor = test_pki::ca("lmgw devices 9f2c0123");
    let e = read(answer(&impostor, &key, &urls)).unwrap_err();
    assert_eq!(e.code, code::CA_NOT_PINNED);
    assert!(
        e.message.contains("not the lmgw this link pairs with"),
        "{}",
        e.message
    );
    // A certificate for another key.
    let mut body = answer(&ca, &key, &urls);
    let other = KeyPair::generate().unwrap();
    body["certificate"] = crate::base64::encode(&test_pki::device_cert_for(&ca, &other)).into();
    assert_eq!(read(body).unwrap_err().code, code::CERTIFICATE_NOT_FOR_KEY);
    // A code that is not the key's.
    let mut body = answer(&ca, &key, &urls);
    body["confirm_code"] = confirm_code(&other.subject_public_key_info()).into();
    assert_eq!(read(body).unwrap_err().code, code::CONFIRM_CODE_MISMATCH);
    // Not base64, not a date, not the shape.
    for (field, value) in [
        ("ca_certificate", "*"),
        ("certificate", "*"),
        ("not_after", "2027-10-14"),
        ("notice_after", "soon"),
    ] {
        let mut body = answer(&ca, &key, &urls);
        body[field] = value.into();
        let e = read(body).unwrap_err();
        assert!(
            e.code == "unreadable" || e.code == code::CA_NOT_PINNED,
            "{field}: {e:?}"
        );
    }
    let mut body = answer(&ca, &key, &urls);
    body.as_object_mut().unwrap().remove("certificate");
    assert_eq!(read(body).unwrap_err().code, "unreadable");
}

#[test]
fn a_refusal_reads_as_lmgw_s_and_a_used_token_says_so() {
    let ca = test_pki::ca("lmgw devices 9f2c0123");
    let link = link_for(&ca, &["https://h"]);
    let e = read_enrolment(
        410,
        r#"{"code": "pairing_token_used", "message": "this pairing token was used by another key"}"#,
        &link,
        &[],
    )
    .unwrap_err();
    assert_eq!(e.code, code::PAIRING_TOKEN_USED);
    assert_eq!(key_refused(&e), Some(KeyRefused::TokenUsedElsewhere));
    let e = read_enrolment(
        410,
        r#"{"code": "pairing_token_expired", "message": "the token expired at 14:32"}"#,
        &link,
        &[],
    )
    .unwrap_err();
    assert_eq!(e.code, code::PAIRING_TOKEN_EXPIRED);
    assert_eq!(key_refused(&e), None, "a new link, not a dead key");
}
