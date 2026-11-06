//! The whole flow (device transfers design §1), and the app id a device
//! declares at enrolment (§2).

use serde_json::json;

use super::{call, file, offer, path_of, put, read, sha, two, APP};
use crate::support::device_pair::{mint, post_enrol_as};

/// A sends, lmgw checks, B is offered it, downloads it, says it got it:
/// delivered, the bytes deleted, both sides and the owner see the end.
#[tokio::test]
async fn a_file_goes_from_one_device_to_the_other() {
    let (w, desktop, phone) = two().await;
    let bytes = file(300_000);
    let (s, targets) = call(
        &w,
        &desktop.client,
        "GET",
        &format!("/transfers/api/targets?app_id={APP}"),
        None,
    )
    .await;
    assert_eq!(s, 200, "{targets}");
    assert_eq!(targets["targets"][0]["device_id"], phone.id);
    assert_eq!(targets["expiry_days"], 7);

    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 32, "{t}");
    assert_eq!(t["state"], "uploading");
    assert_eq!(t["received"], 0);
    assert_eq!(
        t["from"],
        json!({"device_id": desktop.id, "name": "desktop"})
    );
    assert_eq!(t["to"], json!({"device_id": phone.id, "name": "phone"}));
    assert_eq!(t["title"], "Chapter 3 — the lighthouse");
    assert!(t["expires_at"].as_str().unwrap() > t["created_at"].as_str().unwrap());
    // Not offered yet: the receiver's list does not carry it.
    let (_, l) = call(
        &w,
        &phone.client,
        "GET",
        "/transfers/api/transfers?role=incoming",
        None,
    )
    .await;
    assert_eq!(l["transfers"], json!([]), "{l}");

    let (s, v) = put(&w, &desktop.client, &id, 0, &bytes, bytes.len() as u64).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v, json!({"received": bytes.len(), "state": "ready"}));
    assert!(path_of(&w, &id).exists());

    let (_, l) = call(
        &w,
        &phone.client,
        "GET",
        "/transfers/api/transfers?role=incoming",
        None,
    )
    .await;
    assert_eq!(l["transfers"][0]["id"], id.as_str(), "{l}");
    assert_eq!(l["transfers"][0]["state"], "ready");
    let resp = phone
        .client
        .get(format!("{}/transfers/api/transfers/{id}/content", w.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["etag"].to_str().unwrap(),
        format!("\"sha256:{}\"", sha(&bytes))
    );
    assert_eq!(resp.headers()["accept-ranges"], "bytes");
    assert!(resp.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .contains("chapter-3.zip"));
    let got = resp.bytes().await.unwrap();
    assert_eq!(sha(&got), sha(&bytes));

    let (s, v) = call(
        &w,
        &phone.client,
        "POST",
        &format!("/transfers/api/transfers/{id}/received"),
        None,
    )
    .await;
    assert_eq!((s, v["state"].as_str()), (200, Some("delivered")), "{v}");
    assert!(
        v["ended_at"].is_string() && v["end_reason"].is_null(),
        "{v}"
    );
    assert!(
        !path_of(&w, &id).exists(),
        "delivered: the bytes are deleted"
    );
    let (_, v) = read(&w, &desktop.client, &id).await;
    assert_eq!(v["state"], "delivered");
    let (_, all) = call(&w, &w.gw.client(), "GET", "/transfers/api/transfers", None).await;
    assert_eq!(all["transfers"][0]["state"], "delivered", "{all}");
    assert_eq!(all["stored_bytes"], 0);
}

/// The app id is declared at enrolment and carried on the owner's key row;
/// a malformed one is refused before the token is used, which then pairs.
#[tokio::test]
async fn the_app_id_is_declared_at_enrolment_and_checked() {
    let (w, desktop, _phone) = two().await;
    let (_, keys) = call(&w, &w.gw.client(), "GET", "/api/usage/keys", None).await;
    let row = keys["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == desktop.id)
        .unwrap()
        .clone();
    assert_eq!(row["app_id"], APP);

    let link = mint(
        &w.state,
        &w.gw,
        "key_create",
        json!({"kind": "device", "name": "tablet", "local_only": true}),
    )
    .await;
    let (spki, _) = crate::support::device_pair::key();
    let (s, body) = post_enrol_as(&link, &spki, Some("Story.Player")).await;
    assert_eq!(s, 400, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["code"], "bad_request");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .ends_with(lmgw_api_types::transfer::APP_ID_RULE),
        "{v}"
    );
    // The token is unused: the same link pairs with a good one.
    let (s, body) = post_enrol_as(&link, &spki, Some("org.example.tablet")).await;
    assert_eq!(s, 201, "{body}");
}

/// A new key that waits beside a confirmed one waits with its app id: the
/// working device keeps receiving as its own app until the owner confirms
/// the new key (I3).
#[tokio::test]
async fn a_waiting_key_s_app_id_waits_for_the_confirmation() {
    let (w, _desktop, phone) = two().await;
    let app_of = |keys: &serde_json::Value| {
        keys["keys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["id"] == phone.id)
            .unwrap()["app_id"]
            .clone()
    };
    let link = mint(&w.state, &w.gw, "device_token", json!({ "id": phone.id })).await;
    let (spki, private) = crate::support::device_pair::key();
    let waiting =
        crate::support::device_pair::enrol_key_as(&link, spki, private, Some("org.example.other"))
            .await;
    let (_, keys) = call(&w, &w.gw.client(), "GET", "/api/usage/keys", None).await;
    assert_eq!(app_of(&keys), APP, "unconfirmed: the working key's app id");
    let (s, v) = crate::device_chat::op(
        &w,
        "device_confirm",
        json!({ "id": phone.id, "confirm_code": waiting.credentials.confirm_code }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (_, keys) = call(&w, &w.gw.client(), "GET", "/api/usage/keys", None).await;
    assert_eq!(app_of(&keys), "org.example.other");
}

/// A new pairing token drops a key that waited for its confirmation, and
/// the app id it declared with it: a later Confirm of the next waiting key
/// never brings back an app id nobody confirmed.
#[tokio::test]
async fn a_new_token_drops_the_waiting_key_s_app_id() {
    let (w, _desktop, phone) = two().await;
    let waiting_app = |db: sqlx::SqlitePool| async move {
        sqlx::query_scalar::<_, Option<String>>("SELECT waiting_app_id FROM api_keys WHERE id = ?1")
            .bind(phone.id)
            .fetch_one(&db)
            .await
            .unwrap()
    };
    let link = mint(&w.state, &w.gw, "device_token", json!({ "id": phone.id })).await;
    let (spki, private) = crate::support::device_pair::key();
    crate::support::device_pair::enrol_key_as(&link, spki, private, Some("org.example.other"))
        .await;
    assert_eq!(
        waiting_app(w.state.db.clone()).await.as_deref(),
        Some("org.example.other")
    );
    mint(&w.state, &w.gw, "device_token", json!({ "id": phone.id })).await;
    assert_eq!(waiting_app(w.state.db.clone()).await, None);
    let (_, keys) = call(&w, &w.gw.client(), "GET", "/api/usage/keys", None).await;
    let row = keys["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == phone.id)
        .unwrap()
        .clone();
    assert_eq!(row["app_id"], APP, "the working key's stays: {row}");
}
