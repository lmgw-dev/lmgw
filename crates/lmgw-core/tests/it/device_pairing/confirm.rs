//! The owner's confirmation (device link design §3.5, D23) and re-pairing
//! (§5.2, §5.3, §12): a re-enrolment ends no working key, so the three
//! choices of a new token each end what they say and nothing else
//! (*changed 2026-10-15*).

use futures::FutureExt;
use lmgw_api_types::device_link::code;
use lmgw_core::state::SharedState;
use serde_json::{json, Value};

use super::{world, World};
use crate::chat_feed::Feed;
use crate::common::Gw;
use crate::support::device_pair::{self, Paired};

/// The Chat feed as `client`, past its `hello`.
async fn feed(gw: &Gw, client: &reqwest::Client) -> Feed {
    let resp = client
        .get(format!("{gw}/chat/api/feed"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the feed opens");
    let mut feed = Feed::reading(resp);
    feed.until(10, |f| !f.is_empty()).await;
    assert_eq!(feed.frames[0].event, "hello", "{:?}", feed.frames);
    feed
}

/// `GET /chat/api/threads` as `client`: the status.
async fn threads(w: &World, client: &reqwest::Client) -> u16 {
    client
        .get(format!("{}/chat/api/threads", w.gw))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// A watch on device `d`'s key from now: whether a revocation of it has
/// been raised since, read without waiting (a watch resolves at once on a
/// revocation after its mark, or on a credential its row no longer holds).
struct Since {
    watch: lmgw_core::devices::RevocationWatch,
}

impl Since {
    fn now(state: &SharedState, d: &Paired) -> Self {
        let fp = state
            .snapshot()
            .api_keys
            .iter()
            .find(|k| k.id == d.id)
            .unwrap()
            .fingerprint();
        let r = &state.devices.revocations;
        Self {
            watch: r.watch_from(d.id, r.mark(), fp, None),
        }
    }

    fn ended(&mut self, state: &SharedState) -> Option<lmgw_core::devices::RevokeReason> {
        self.watch.revoked(state).now_or_never()
    }
}

/// Until the owner confirms, every request of the device is refused 403
/// naming its code — the feed too; Confirm takes the code the device shows,
/// refuses another, and serves the next request; `/api/events` says so.
#[tokio::test]
async fn an_enrolled_device_is_served_from_its_confirmation_on() {
    let w = world().await;
    let mut events = w.state.telemetry.subscribe();
    let (id, link) = w.waiting("phone").await;
    let (s, v) = w
        .op(
            "device_confirm",
            json!({ "id": id, "confirm_code": "0000-0000" }),
        )
        .await;
    assert_eq!(s, 400, "nothing enrolled yet: {v}");
    assert!(
        v["message"].as_str().unwrap().contains("has not enrolled"),
        "{v}"
    );

    let enrolled = device_pair::enrol(&link).await;
    let device = device_pair::connected(enrolled, "device:phone".into()).await;
    let code_ = device.credentials.confirm_code.clone();
    for path in ["/chat/api/threads", "/chat/api/feed", "/v1/models"] {
        let resp = device
            .client
            .get(format!("{}{path}", w.gw))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "{path}");
        let v: Value = resp.json().await.unwrap();
        let said = if path == "/v1/models" {
            &v["error"]
        } else {
            &v
        };
        assert_eq!(said["code"], code::DEVICE_UNCONFIRMED, "{path}: {v}");
        assert!(
            said["message"].as_str().unwrap().contains(&code_),
            "{path}: {v}"
        );
    }
    assert_eq!(
        w.row("phone").await["pairing"]["confirm_code"],
        json!(code_)
    );

    // No code, a code that is none, and another key's code confirm nothing.
    let (s, v) = w.op("device_confirm", json!({ "id": id })).await;
    assert_eq!(s, 400, "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("confirm_code"),
        "{v}"
    );
    let (s, v) = w
        .op(
            "device_confirm",
            json!({ "id": id, "confirm_code": "7F3A" }),
        )
        .await;
    assert_eq!(s, 400, "{v}");
    let other = if code_ == "0000-0000" {
        "0000-0001"
    } else {
        "0000-0000"
    };
    let (s, v) = w
        .op("device_confirm", json!({ "id": id, "confirm_code": other }))
        .await;
    assert_eq!(s, 409, "{v}");
    assert_eq!(v["code"], code::CONFIRM_CODE_MISMATCH, "{v}");
    assert!(
        !v["message"].as_str().unwrap().contains(&code_),
        "the right code is never said: {v}"
    );
    assert_eq!(threads(&w, &device.client).await, 403, "still unconfirmed");

    // The code as the owner types it from the device's screen.
    let typed = code_.to_lowercase().replace('-', " ");
    let (s, v) = w
        .op("device_confirm", json!({ "id": id, "confirm_code": typed }))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["confirm_code"], json!(code_), "{v}");
    assert_eq!(v["replaced"], json!(false), "{v}");
    let feed = feed(&w.gw, &device.client).await;
    assert_eq!(
        feed.hello()["principal"],
        json!({ "kind": "device", "name": "phone" })
    );
    drop(feed);
    let (s, v) = w
        .op("device_confirm", json!({ "id": id, "confirm_code": code_ }))
        .await;
    assert_eq!(s, 200, "{v}");
    assert!(v["message"].as_str().unwrap().contains("already"), "{v}");
    assert_eq!(w.row("phone").await["pairing"]["state"], json!("confirmed"));
    let said = device_pair::keys_said(&mut events, id);
    assert!(said.contains(&"confirmed".to_string()), "{said:?}");
}

/// A new token that keeps the current key, and a new key enrolling with it
/// (a move to a new phone): the kept key works on, its connections open,
/// until the owner confirms the new key's code; then it ends as a replaced
/// key does and the new key is served. Whoever photographs the new link
/// gets a key the owner never confirms, never the working device's end.
#[tokio::test]
async fn a_new_key_waits_beside_the_kept_one_until_it_is_confirmed() {
    let w = world().await;
    let old = device_pair::pair(&w.state, &w.gw, "phone", json!({})).await;
    let link = device_pair::mint(&w.state, &w.gw, "device_token", json!({ "id": old.id })).await;
    let mut feed = feed(&w.gw, &old.client).await;
    let mut since = Since::now(&w.state, &old);

    let enrolled = device_pair::enrol(&link).await;
    assert!(!enrolled.credentials.confirm_code.is_empty());
    assert_eq!(since.ended(&w.state), None, "the enrolment ended nothing");
    assert_eq!(threads(&w, &old.client).await, 200, "the kept key works");
    let pairing = &w.row("phone").await["pairing"];
    assert_eq!(pairing["state"], json!("enrolled"), "{pairing}");
    assert_eq!(
        pairing["confirm_code"],
        json!(enrolled.credentials.confirm_code)
    );
    assert_eq!(pairing["kept_code"], json!(old.credentials.confirm_code));
    assert!(pairing["confirmed_at"].is_string(), "the kept key's");

    let new = device_pair::connected(enrolled, "device:phone".into()).await;
    assert_eq!(
        threads(&w, &new.client).await,
        403,
        "waits for its confirmation"
    );
    // The kept key's code is not the waiting key's: nothing is confirmed.
    let (s, v) = w
        .op(
            "device_confirm",
            json!({ "id": old.id, "confirm_code": old.credentials.confirm_code }),
        )
        .await;
    assert_eq!(
        (s, &v["code"]),
        (409, &json!(code::CONFIRM_CODE_MISMATCH)),
        "{v}"
    );
    assert_eq!(since.ended(&w.state), None);

    let (s, v) = w
        .op(
            "device_confirm",
            json!({ "id": old.id, "confirm_code": new.credentials.confirm_code }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["replaced"], json!(true), "{v}");
    assert_eq!(
        since.ended(&w.state),
        Some(lmgw_core::devices::RevokeReason::Rotated)
    );
    feed.until(10, |_| false).await;
    let last = feed.frames.last().unwrap();
    assert_eq!(last.event, "revoked", "{:#?}", feed.frames);
    assert_eq!(last.data["kind"], "key_unknown", "{}", last.data);
    let resp = old
        .client
        .get(format!("{}/chat/api/threads", w.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["code"], "device_key_unknown", "{v}");
    assert_eq!(threads(&w, &new.client).await, 200);
    let pairing = &w.row("phone").await["pairing"];
    assert_eq!(pairing["state"], json!("confirmed"), "{pairing}");
    assert_eq!(pairing["kept_code"], Value::Null);
    assert_eq!(pairing["confirm_code"], json!(new.credentials.confirm_code));
}

/// A new token that keeps the current key, and that same key enrolling
/// with it (a renewal before the certificate lapses, §5.3): a fresh
/// certificate, the confirmation kept, nothing ended, no gap.
#[tokio::test]
async fn the_same_key_enrolling_again_renews_its_certificate_without_a_gap() {
    let w = world().await;
    let old = device_pair::pair(&w.state, &w.gw, "phone", json!({})).await;
    let serial = |w: &World| {
        let db = w.state.db.clone();
        let id = old.id;
        async move {
            sqlx::query_scalar::<_, String>("SELECT cert_serial FROM api_keys WHERE id = ?1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    let before = serial(&w).await;
    let link = device_pair::mint(&w.state, &w.gw, "device_token", json!({ "id": old.id })).await;
    let mut since = Since::now(&w.state, &old);

    let renewed = device_pair::enrol_key(&link, old.spki.clone(), old.identity.1.clone_key()).await;
    assert!(
        renewed.credentials.certificate != old.credentials.certificate,
        "a fresh certificate"
    );
    assert_eq!(
        renewed.credentials.confirm_code,
        old.credentials.confirm_code
    );
    assert_ne!(serial(&w).await, before);
    assert_eq!(since.ended(&w.state), None, "nothing ended");
    assert_eq!(
        threads(&w, &old.client).await,
        200,
        "the old certificate too"
    );
    let pairing = &w.row("phone").await["pairing"];
    assert_eq!(pairing["state"], json!("confirmed"), "{pairing}");
    assert_eq!(pairing["kept_code"], Value::Null);

    // Served at once, with no confirmation to wait for; and the enrolment
    // said so.
    let (status, body) = device_pair::post_enrol(&link, &old.spki).await;
    assert_eq!(status, 201, "{body}");
    let again: lmgw_api_types::device_link::Enrolment = serde_json::from_str(&body).unwrap();
    assert!(again.confirmed, "{body}");
    let renewed = device_pair::connected(renewed, "device:phone".into()).await;
    assert_eq!(threads(&w, &renewed.client).await, 200);
}

/// A new token that ends the current key (a lost or stolen device): it ends
/// at once, and the key the device enrols next waits for its confirmation.
#[tokio::test]
async fn ending_the_current_key_ends_it_at_once_and_the_next_key_waits() {
    let w = world().await;
    let old = device_pair::pair(&w.state, &w.gw, "phone", json!({})).await;
    let mut since = Since::now(&w.state, &old);
    let (s, v) = w
        .op("device_token", json!({ "id": old.id, "end_current": true }))
        .await;
    assert_eq!(s, 200, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("current key is ended"),
        "{v}"
    );
    assert_eq!(
        since.ended(&w.state),
        Some(lmgw_core::devices::RevokeReason::Rotated)
    );
    assert_eq!(threads(&w, &old.client).await, 401);
    assert_eq!(w.row("phone").await["pairing"]["state"], json!("waiting"));

    let enrolled = device_pair::enrol(v["link"].as_str().unwrap()).await;
    let new = device_pair::connected(enrolled, "device:phone".into()).await;
    assert_eq!(threads(&w, &new.client).await, 403);
    let pairing = &w.row("phone").await["pairing"];
    assert_eq!(pairing["kept_code"], Value::Null, "nothing was kept");
    let (s, v) = w
        .op(
            "device_confirm",
            json!({ "id": old.id, "confirm_code": new.credentials.confirm_code }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["replaced"], json!(false), "{v}");
    assert_eq!(threads(&w, &new.client).await, 200);
}

/// A row paired before TLS (`v1:`, §12) pairs again with a new token for
/// this computer, keeping its id and policy; ending "its key" says there
/// was none to end.
#[tokio::test]
async fn a_device_paired_before_tls_pairs_again_with_a_new_token() {
    let w = world().await;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO api_keys (name, key_hash, enabled, kind, rpm_limit)
         VALUES ('device:desk', 'v1:0123', 1, 'device', 30) RETURNING id",
    )
    .fetch_one(&w.state.db)
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    assert_eq!(w.row("desk").await["pairing"]["state"], json!("v1"));

    let (s, v) = w
        .op(
            "device_token",
            json!({ "id": id, "local_only": true, "end_current": true }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let said = v["message"].as_str().unwrap();
    assert!(said.contains("paired before TLS"), "{said}");
    assert!(!said.contains("connections are closed"), "{said}");
    let enrolled = device_pair::enrol(v["link"].as_str().unwrap()).await;
    device_pair::confirm(&w.gw, &enrolled).await;
    let desk = device_pair::connected(enrolled, "device:desk".into()).await;
    assert_eq!(threads(&w, &desk.client).await, 200);
    let row = w.row("desk").await;
    assert_eq!(
        (row["id"].as_i64(), &row["rpm_limit"]),
        (Some(id), &json!(30))
    );
    assert_eq!(row["pairing"]["local_only"], json!(true));
}
