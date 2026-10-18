//! `POST /devices/enrol` (device link design §3.3, §3.4).

use lmgw_api_types::device_link::code;
use lmgw_client::requests::{key_refused, read_refusal, KeyRefused};
use serde_json::{json, Value};

use super::{enrolment, post_raw, token_of, world};
use crate::support::device_pair;

/// A fresh token enrols a key: its certificate, for that key, is the one
/// the device listener then accepts; the dates, the URLs and the code are
/// the key's; the row says where it came from and waits for its
/// confirmation, and `/api/events` says it enrolled.
#[tokio::test]
async fn a_fresh_token_enrols_a_key_and_the_device_waits_for_its_confirmation() {
    let w = world().await;
    let mut events = w.state.telemetry.subscribe();
    let (id, link) = w.waiting("phone").await;
    let enrolled = device_pair::enrol(&link).await;
    assert!(
        !w.state.devices.minted.take(&link),
        "a used link is handed to no desktop client"
    );
    let creds = &enrolled.credentials;
    assert_eq!(creds.device_id, id);
    assert_eq!(creds.urls.as_slice(), enrolled.link.urls.as_slice());
    assert_eq!(
        creds.confirm_code,
        lmgw_client::requests::confirm_code(&enrolled.spki)
    );
    // `devices.client_cert_days` from now, the notice two thirds of it on.
    let days = w.state.snapshot().settings.devices.client_cert_days;
    let now = chrono::Utc::now().timestamp_millis();
    let span = creds.not_after_ms - now;
    let want = i64::from(days) * 86_400_000;
    assert!((want - 60_000..=want).contains(&span), "{span} vs {want}");
    let notice = creds.notice_after_ms - now;
    assert!(
        (want * 2 / 3 - 60_000..=want * 2 / 3).contains(&notice),
        "{notice}"
    );

    let row = w.row("phone").await;
    let pairing = &row["pairing"];
    assert_eq!(pairing["state"], json!("enrolled"), "{row}");
    assert_eq!(pairing["confirm_code"], json!(creds.confirm_code), "{row}");
    assert_eq!(pairing["enrolled_from"], json!("127.0.0.1"), "{row}");
    assert_eq!(pairing["client"], json!("lmgw tests"), "{row}");
    assert_eq!(pairing["token"]["used_from"], json!("127.0.0.1"), "{row}");
    assert!(pairing["cert_not_after"].is_string(), "{row}");
    let key_hash: String = sqlx::query_scalar("SELECT key_hash FROM api_keys WHERE id = ?1")
        .bind(id)
        .fetch_one(&w.state.db)
        .await
        .unwrap();
    assert_eq!(key_hash, enrolled.key_hash);

    let said = device_pair::keys_said(&mut events, id);
    assert!(said.contains(&"token".to_string()), "{said:?}");
    assert!(said.contains(&"enrolled".to_string()), "{said:?}");

    // The handshake takes the certificate (the CA signed it); the row
    // answers with the confirmation it waits for.
    let device = device_pair::connected(enrolled, "device:phone".into()).await;
    let resp = device
        .client
        .get(format!("{}/chat/api/threads", w.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["code"], code::DEVICE_UNCONFIRMED, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains(&device.credentials.confirm_code),
        "{v}"
    );
}

/// Two devices racing with one token: one gets the certificate, the other
/// `pairing_token_used`, and the row is contested (§3.3).
#[tokio::test]
async fn two_enrolments_racing_one_token_get_one_certificate() {
    let w = world().await;
    let (_, link) = w.waiting("phone").await;
    let (a, _) = device_pair::key();
    let (b, _) = device_pair::key();
    let ((sa, _), (sb, _)) = tokio::join!(
        device_pair::post_enrol(&link, &a),
        device_pair::post_enrol(&link, &b)
    );
    let mut got = [sa, sb];
    got.sort_unstable();
    assert_eq!(got, [201, 410]);
    assert!(w.row("phone").await["pairing"]["contested_at"].is_string());
}

/// A used token another key presents is refused 410 and marks the row
/// contested, with the place it came from and the `keys` frame; the device
/// reads it as "someone else used this code".
#[tokio::test]
async fn a_used_token_another_key_presents_marks_the_device_contested() {
    let w = world().await;
    let mut events = w.state.telemetry.subscribe();
    let (id, link) = w.waiting("phone").await;
    device_pair::enrol(&link).await;
    let (stranger, _) = device_pair::key();
    let (status, body) = device_pair::post_enrol(&link, &stranger).await;
    assert_eq!(status, 410, "{body}");
    let e = read_refusal(status, &body);
    assert_eq!(e.code, code::PAIRING_TOKEN_USED);
    assert_eq!(key_refused(&e), Some(KeyRefused::TokenUsedElsewhere));
    let pairing = &w.row("phone").await["pairing"];
    assert!(pairing["contested_at"].is_string(), "{pairing}");
    assert_eq!(pairing["contested_from"], json!("127.0.0.1"));
    // The first key's enrolment stands.
    assert_eq!(pairing["state"], json!("enrolled"));
    let said = device_pair::keys_said(&mut events, id);
    assert!(said.contains(&"contested".to_string()), "{said:?}");

    // Presented again: refused and counted, the mark already said.
    let first = pairing["contested_at"].clone();
    let (status, body) = device_pair::post_enrol(&link, &stranger).await;
    assert_eq!(status, 410, "{body}");
    assert_eq!(w.row("phone").await["pairing"]["contested_at"], first);
    let said = device_pair::keys_said(&mut events, id);
    assert!(!said.contains(&"contested".to_string()), "{said:?}");
    assert_eq!(
        lmgw_core::devices::status(&w.state)
            .pairing
            .failures_last_hour,
        2
    );

    // A new token starts over: the mark was the old token's.
    device_pair::mint(&w.state, &w.gw, "device_token", json!({ "id": id })).await;
    let pairing = &w.row("phone").await["pairing"];
    assert_eq!(pairing["contested_at"], Value::Null, "{pairing}");
    assert_eq!(pairing["contested_from"], Value::Null, "{pairing}");
}

/// Every refusal of §3.4's table that a request can reach, each in the
/// shape a device reads.
#[tokio::test]
async fn an_enrolment_is_refused_by_its_token_its_key_and_its_body() {
    let w = world().await;
    let (_, link) = w.waiting("phone").await;
    let token = token_of(&link);
    let (spki, _) = device_pair::key();
    let refused = |status: u16, body: &str| (status, read_refusal(status, body).code);

    // An unknown token, of the right shape or not.
    let other = format!("lmgw-enrol-{}", "0".repeat(32));
    for t in [other.as_str(), "not-a-token"] {
        let (s, _, b) = post_raw(&link, enrolment(t, &spki)).await;
        assert_eq!(
            refused(s, &b),
            (401, code::PAIRING_TOKEN_UNKNOWN.into()),
            "{b}"
        );
    }
    // Not this shape; not a key; not P-256; not canonical.
    let (s, _, b) = post_raw(&link, json!({ "token": token }).to_string()).await;
    assert_eq!(refused(s, &b), (400, "bad_request".into()), "{b}");
    let (s, _, b) = post_raw(&link, enrolment(&token, b"not a key")).await;
    assert_eq!(
        refused(s, &b),
        (400, code::PUBLIC_KEY_UNSUPPORTED.into()),
        "{b}"
    );
    let p384 = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let (s, _, b) = post_raw(
        &link,
        enrolment(
            &token,
            &rcgen::PublicKeyData::subject_public_key_info(&p384),
        ),
    )
    .await;
    assert_eq!(
        refused(s, &b),
        (400, code::PUBLIC_KEY_UNSUPPORTED.into()),
        "{b}"
    );
    assert!(b.contains("P-384") && b.contains("P-256"), "{b}");
    // Over 16 KiB.
    let big = "x".repeat(lmgw_api_types::device_link::BODY_LIMIT_BYTES + 1);
    let (s, _, b) = post_raw(&link, big).await;
    assert_eq!(refused(s, &b), (413, code::BODY_TOO_LARGE.into()), "{b}");
    assert!(b.contains("16384"), "{b}");

    // None of it used the token.
    let (s, _, b) = post_raw(&link, enrolment(&token, &spki)).await;
    assert_eq!(s, 201, "{b}");

    // A key another device enrolled: 409, and that token stays unused.
    let (_, second) = w.waiting("tablet").await;
    let (s, _, b) = post_raw(&second, enrolment(&token_of(&second), &spki)).await;
    assert_eq!(refused(s, &b), (409, code::PUBLIC_KEY_IN_USE.into()), "{b}");
    assert_eq!(w.row("tablet").await["pairing"]["state"], json!("waiting"));
}

/// A disabled device's token is refused and stays unused: enabled again,
/// it enrolls.
#[tokio::test]
async fn a_disabled_device_s_token_stays_unused() {
    let w = world().await;
    let (id, link) = w.waiting("phone").await;
    w.op("key_set", json!({ "id": id, "enabled": false })).await;
    let (spki, _) = device_pair::key();
    let (s, _, b) = post_raw(&link, enrolment(&token_of(&link), &spki)).await;
    assert_eq!(
        (s, read_refusal(s, &b).code),
        (401, "device_disabled".into())
    );
    assert!(w.row("phone").await["pairing"]["token"]["used_at"].is_null());
    w.op("key_set", json!({ "id": id, "enabled": true })).await;
    let (s, _, b) = post_raw(&link, enrolment(&token_of(&link), &spki)).await;
    assert_eq!(s, 201, "{b}");
}

/// A lapsed token and one the owner ended are 410 `pairing_token_expired`,
/// each saying which (a device reaches them while another token lives).
#[tokio::test]
async fn a_lapsed_or_ended_token_is_refused_saying_which() {
    let w = world().await;
    let (lapsed_id, lapsed) = w.waiting("phone").await;
    let (ended_id, ended) = w.waiting("tablet").await;
    let (_, _alive) = w.waiting("watch").await;
    sqlx::query(
        "UPDATE device_pairing_tokens SET expires_at = '2020-01-01T00:00:00Z' WHERE key_id = ?1",
    )
    .bind(lapsed_id)
    .execute(&w.state.db)
    .await
    .unwrap();
    let (s, v) = w.op("device_token_end", json!({ "id": ended_id })).await;
    assert_eq!((s, &v["ended"]), (200, &json!(true)), "{v}");
    let (s, v) = w.op("device_token_end", json!({ "id": ended_id })).await;
    assert_eq!((s, &v["ended"]), (200, &json!(false)), "nothing alive: {v}");

    let (spki, _) = device_pair::key();
    let (s, _, b) = post_raw(&lapsed, enrolment(&token_of(&lapsed), &spki)).await;
    assert_eq!(
        (s, read_refusal(s, &b).code),
        (410, code::PAIRING_TOKEN_EXPIRED.into()),
        "{b}"
    );
    assert!(b.contains("devices.token_minutes"), "{b}");
    let (s, _, b) = post_raw(&ended, enrolment(&token_of(&ended), &spki)).await;
    assert_eq!(
        (s, read_refusal(s, &b).code),
        (410, code::PAIRING_TOKEN_EXPIRED.into()),
        "{b}"
    );
    assert!(b.contains("ended"), "{b}");
}

/// Failed enrolments count per address; past
/// `devices.enrol_refuse_per_min` the address is answered 429 with
/// `Retry-After`, and the status shows the failures.
#[tokio::test]
async fn failures_are_counted_and_past_the_setting_answered_429() {
    let w = world().await;
    let mut s = w.state.snapshot().settings.clone();
    s.devices.enrol_refuse_per_min = 2;
    lmgw_core::store::save_settings(&w.state.db, &s)
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let (_, link) = w.waiting("phone").await;
    let (spki, _) = device_pair::key();
    let unknown = format!("lmgw-enrol-{}", "1".repeat(32));
    for _ in 0..2 {
        let (s, _, b) = post_raw(&link, enrolment(&unknown, &spki)).await;
        assert_eq!(s, 401, "{b}");
    }
    let (s, headers, b) = post_raw(&link, enrolment(&token_of(&link), &spki)).await;
    assert_eq!(
        (s, read_refusal(s, &b).code),
        (429, code::PAIRING_RATE_LIMITED.into()),
        "{b}"
    );
    assert!(b.contains("devices.enrol_refuse_per_min"), "{b}");
    let retry: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry), "{retry}");
    let status = lmgw_core::devices::status(&w.state).pairing;
    assert_eq!(status.failures_last_hour, 2);
    assert_eq!(status.last_failure_from.as_deref(), Some("127.0.0.1"));
    assert_eq!(status.tokens_alive, 1);
}

/// With no token alive, the device listener's handshake requires a client
/// certificate: an enrolment meets the alert `lmgw-client` reads as an
/// ended link.
#[tokio::test]
async fn with_no_token_alive_an_enrolment_meets_the_ended_link() {
    let w = world().await;
    let (id, link) = w.waiting("phone").await;
    w.op("device_token_end", json!({ "id": id })).await;
    assert_eq!(lmgw_core::devices::status(&w.state).pairing.tokens_alive, 0);
    let parsed = lmgw_client::pairing::PairingLink::parse(&link).unwrap();
    let cfg =
        lmgw_client::tls::enrol_config(lmgw_core::devices::tls::provider(), parsed.fp).unwrap();
    let client = reqwest::Client::builder()
        .tls_backend_preconfigured(cfg)
        .build()
        .unwrap();
    let (spki, _) = device_pair::key();
    let e = client
        .post(format!("{}/devices/enrol", parsed.urls.as_slice()[0]))
        .body(enrolment(&token_of(&link), &spki))
        .send()
        .await
        .unwrap_err();
    let tls = lmgw_client::tls::find_tls_error(&e).expect("a TLS error");
    assert_eq!(
        lmgw_client::tls::TlsRefusal::of_enrolment(tls),
        Some(lmgw_client::tls::TlsRefusal::Unproven)
    );
}
