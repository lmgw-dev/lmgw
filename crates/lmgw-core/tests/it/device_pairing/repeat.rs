//! A lost answer asked for again (device link design §3.3, D8, the notes of
//! 2026-10-15): the key that used a token gets the same answer while the
//! token lives, enrolled and confirmed alike, and only that key; the token
//! ends when the owner ends it or when the confirmed device makes its first
//! request with the certificate it was answered, and then nothing answers
//! without a certificate.

use lmgw_api_types::device_link::Enrolment;
use serde_json::json;

use super::world;
use crate::support::device_pair;

/// A certificate's serial (standard base64 DER), as the row keeps it.
fn serial_of(certificate_b64: &str) -> String {
    use base64::Engine;
    let der = base64::engine::general_purpose::STANDARD
        .decode(certificate_b64)
        .unwrap();
    lmgw_core::server::DevicePeer::read(&der).unwrap().serial
}

/// The handshake without a certificate is refused once no token lives: an
/// enrolment with `link`'s token and the key `spki` meets the TLS alert.
async fn meets_the_ended_link(link: &str, spki: &[u8]) {
    let parsed = lmgw_client::pairing::PairingLink::parse(link).unwrap();
    let cfg =
        lmgw_client::tls::enrol_config(lmgw_core::devices::tls::provider(), parsed.fp).unwrap();
    let e = reqwest::Client::builder()
        .tls_backend_preconfigured(cfg)
        .build()
        .unwrap()
        .post(format!("{}/devices/enrol", parsed.urls.as_slice()[0]))
        .body(super::enrolment(&super::token_of(link), spki))
        .send()
        .await
        .unwrap_err();
    assert!(lmgw_client::tls::find_tls_error(&e).is_some(), "{e}");
}

#[tokio::test]
async fn the_same_key_gets_the_same_answer_while_the_token_lives() {
    let w = world().await;
    let (id, link) = w.waiting("phone").await;
    let (spki, _) = device_pair::key();
    let (s, first) = device_pair::post_enrol(&link, &spki).await;
    assert_eq!(s, 201, "{first}");
    let first: Enrolment = serde_json::from_str(&first).unwrap();
    assert!(!first.confirmed);

    // The answer got lost: the same key asks again, on a new connection.
    let (s, again) = device_pair::post_enrol(&link, &spki).await;
    assert_eq!(s, 201, "{again}");
    let again: Enrolment = serde_json::from_str(&again).unwrap();
    assert_eq!(again, first, "the same certificate, dates, URLs and code");

    // Confirmed meanwhile (the owner compared the card's code with the one
    // the device derives from its own key): the same certificate, saying so.
    let (s, v) = w
        .op(
            "device_confirm",
            json!({ "id": id, "confirm_code": first.confirm_code }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let (s, third) = device_pair::post_enrol(&link, &spki).await;
    assert_eq!(s, 201, "{third}");
    let third: Enrolment = serde_json::from_str(&third).unwrap();
    assert!(third.confirmed);
    assert_eq!(third.certificate, first.certificate);
    assert_eq!(third.confirm_code, first.confirm_code);

    // Nothing was counted as a failure, and the row holds the certificate
    // issued first: no repeat issued another.
    let status = lmgw_core::devices::status(&w.state).pairing;
    assert_eq!(status.failures_last_hour, 0);
    let serial: String = sqlx::query_scalar("SELECT cert_serial FROM api_keys WHERE id = ?1")
        .bind(id)
        .fetch_one(&w.state.db)
        .await
        .unwrap();
    assert_eq!(serial, serial_of(&first.certificate));

    // Ended (the QR card's close): nothing answers without a certificate
    // any more, the same key included.
    w.op("device_token_end", json!({ "id": id })).await;
    assert_eq!(lmgw_core::devices::status(&w.state).pairing.tokens_alive, 0);
    meets_the_ended_link(&link, &spki).await;
}

/// D8's note of 2026-10-15: the confirmed device's first request with the
/// certificate its token answered ends the token, so the handshake without
/// a certificate does not stay open for the token's whole life. Confirm
/// alone does not end it (the device may not hold its answer), and a
/// request the device makes with another certificate of its key does not.
#[tokio::test]
async fn the_device_s_first_request_with_its_certificate_ends_the_token() {
    let w = world().await;
    let (id, link) = w.waiting("phone").await;
    let enrolled = device_pair::enrol(&link).await;
    device_pair::confirm(&w.gw, &enrolled).await;
    let alive = || lmgw_core::devices::status(&w.state).pairing.tokens_alive;
    assert_eq!(alive(), 1, "Confirm alone does not end it");
    let spki = enrolled.spki.clone();
    let phone = device_pair::connected(enrolled, "device:phone".into()).await;

    // Another certificate for the same key: not the answer the token gave.
    let (other, _) = device_pair::lapsing(&w.state, &phone, chrono::Duration::hours(1)).await;
    let resp = other
        .client
        .get(format!("{}/chat/api/threads", w.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(alive(), 1, "another certificate leaves it");

    let resp = phone
        .client
        .get(format!("{}/chat/api/threads", w.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(alive(), 0, "the handshake needs a certificate again");
    crate::common::patience::until_async("the token's row says it ended", || async {
        w.row("phone").await["pairing"]["token"]["ended_at"].is_string()
    })
    .await;
    meets_the_ended_link(&link, &spki).await;
    let said = w.row("phone").await;
    assert_eq!(said["pairing"]["state"], json!("confirmed"), "{said}");
    assert_eq!(said["id"], json!(id));
}
