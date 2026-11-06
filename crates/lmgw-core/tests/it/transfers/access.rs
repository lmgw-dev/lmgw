//! Who reaches what (device transfers design §1, §9.1, §10): targets by app
//! id and never the caller; a third device never sees another's transfer;
//! a plain API key, an agent token, and the owner on a device's route, are
//! refused 403 `transfer_device_only`, nobody at all 401
//! `session_required`; an offer to no target is 422, one from a device
//! that just ended the gate's refusal.

use serde_json::{json, Value};

use super::{call, device, file, offer, read, ready, sha, two, APP};

fn target_ids(v: &Value) -> Vec<i64> {
    v["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["device_id"].as_i64().unwrap())
        .collect()
}

/// Only the caller's other devices that declared the app id and can
/// receive now; never the caller; `online` while its feed is open.
#[tokio::test]
async fn targets_are_the_other_devices_of_the_app() {
    let (w, desktop, phone) = two().await;
    let other = device(&w, "watch", Some("org.example.other")).await;
    let _none = device(&w, "kai", None).await;
    let tablet = device(&w, "tablet", Some(APP)).await;
    let path = format!("/transfers/api/targets?app_id={APP}");
    let (s, v) = call(&w, &desktop.client, "GET", &path, None).await;
    assert_eq!(s, 200, "{v}");
    let mut ids = target_ids(&v);
    ids.sort();
    assert_eq!(ids, [phone.id, tablet.id]);
    assert!(v["free_bytes"].as_u64().unwrap() > 0, "{v}");
    let (_, v) = call(&w, &phone.client, "GET", &path, None).await;
    let mut ids = target_ids(&v);
    ids.sort();
    assert_eq!(ids, [desktop.id, tablet.id], "never the caller");
    let (_, v) = call(
        &w,
        &desktop.client,
        "GET",
        "/transfers/api/targets?app_id=org.example.other",
        None,
    )
    .await;
    assert_eq!(target_ids(&v), [other.id]);

    // Online while it holds its feed.
    let feed = crate::chat_feed::Feed::open(&w, &phone.client, "", None).await;
    let (_, v) = call(&w, &desktop.client, "GET", &path, None).await;
    let phone_row = v["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["device_id"] == phone.id)
        .unwrap()
        .clone();
    assert_eq!(phone_row["online"], true, "{v}");
    assert!(phone_row["last_seen_at"].is_string(), "{v}");
    drop(feed);

    // Disabled: no target, and an offer to it is 422.
    let (s, v) =
        crate::device_chat::op(&w, "key_set", json!({"id": tablet.id, "enabled": false})).await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = call(&w, &desktop.client, "GET", &path, None).await;
    assert_eq!(target_ids(&v), [phone.id]);
    let bytes = file(10);
    let (s, v) = call(
        &w,
        &desktop.client,
        "POST",
        "/transfers/api/transfers",
        Some(
            json!({"to_device": tablet.id, "app_id": APP, "kind": "save",
                    "filename": "a.bin", "size": 10, "sha256": sha(&bytes)}),
        ),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (422, Some("transfer_target_invalid")),
        "{v}"
    );
    assert!(v["message"].as_str().unwrap().contains("disabled"), "{v}");
    let (_, v) = call(
        &w,
        &desktop.client,
        "GET",
        "/transfers/api/targets?app_id=Bad",
        None,
    )
    .await;
    assert_eq!(v["code"], "bad_request", "{v}");
}

/// An offer to itself, to another app's device, to one without an app id
/// or to no device is 422 with what failed; malformed fields are 400.
#[tokio::test]
async fn an_offer_names_a_target_and_well_formed_fields() {
    let (w, desktop, phone) = two().await;
    let none = device(&w, "kai", None).await;
    let bytes = file(10);
    let body = |to: i64| {
        json!({"to_device": to, "app_id": APP, "kind": "save", "filename": "a.bin",
               "size": 10, "sha256": sha(&bytes)})
    };
    for (to, words) in [
        (desktop.id, "itself"),
        (none.id, "declared no app id"),
        (987_654, "no device of this lmgw"),
    ] {
        let (s, v) = call(
            &w,
            &desktop.client,
            "POST",
            "/transfers/api/transfers",
            Some(body(to)),
        )
        .await;
        assert_eq!(
            (s, v["code"].as_str()),
            (422, Some("transfer_target_invalid")),
            "{v}"
        );
        assert!(
            v["message"].as_str().unwrap().contains(words),
            "{words}: {v}"
        );
    }
    for (field, value) in [
        ("filename", json!("../x")),
        ("filename", json!("")),
        ("kind", json!("")),
        ("size", json!(0)),
        ("sha256", json!("ABC")),
        ("app_id", json!("nodots")),
    ] {
        let mut b = body(phone.id);
        b[field] = value;
        let (s, v) = call(
            &w,
            &desktop.client,
            "POST",
            "/transfers/api/transfers",
            Some(b),
        )
        .await;
        assert_eq!(
            (s, v["code"].as_str()),
            (400, Some("bad_request")),
            "{field}: {v}"
        );
    }
    let big = "x".repeat(lmgw_api_types::transfer::BODY_LIMIT_BYTES);
    let mut b = body(phone.id);
    b["title"] = json!(big);
    let (s, v) = call(
        &w,
        &desktop.client,
        "POST",
        "/transfers/api/transfers",
        Some(b),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (413, Some("body_too_large")),
        "{v}"
    );
}

/// A third device of the same app reaches neither the offer nor its bytes;
/// the receiver reads it by id before `ready` and is told "not yet" on its
/// download (I21).
#[tokio::test]
async fn a_third_device_never_reaches_a_transfer() {
    let (w, desktop, phone) = two().await;
    let tablet = device(&w, "tablet", Some(APP)).await;
    let bytes = file(64);
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap();
    let (s, v) = read(&w, &phone.client, id).await;
    assert_eq!((s, v["state"].as_str()), (200, Some("uploading")), "{v}");
    let id = ready(&w, &desktop, &phone, &bytes).await;
    for path in [
        format!("/transfers/api/transfers/{id}"),
        format!("/transfers/api/transfers/{id}/content"),
    ] {
        let (s, v) = call(&w, &tablet.client, "GET", &path, None).await;
        assert_eq!(
            (s, v["code"].as_str()),
            (404, Some("transfer_not_found")),
            "{path}: {v}"
        );
    }
    for action in ["received", "decline", "cancel"] {
        let path = format!("/transfers/api/transfers/{id}/{action}");
        let (s, v) = call(&w, &tablet.client, "POST", &path, None).await;
        assert_eq!(
            (s, v["code"].as_str()),
            (404, Some("transfer_not_found")),
            "{action}: {v}"
        );
    }
    let (s, v) = call(&w, &tablet.client, "GET", "/transfers/api/transfers", None).await;
    assert_eq!((s, v["transfers"].clone()), (200, json!([])), "{v}");
    // The sender cancels; the receiver declines; neither downloads for the
    // other.
    let (s, v) = call(
        &w,
        &phone.client,
        "POST",
        &format!("/transfers/api/transfers/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (404, Some("transfer_not_found")),
        "{v}"
    );
    let (s, v) = call(
        &w,
        &desktop.client,
        "GET",
        &format!("/transfers/api/transfers/{id}/content"),
        None,
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (404, Some("transfer_not_found")),
        "{v}"
    );
}

/// A plain API key is refused by the gate; the owner is refused a device's
/// route, and lists and cancels.
#[tokio::test]
async fn only_devices_send_and_receive() {
    let (w, desktop, phone) = two().await;
    let (s, v) = crate::device_chat::op(&w, "key_create", json!({"name": "ci"})).await;
    assert_eq!(s, 200, "{v}");
    let plain = crate::device_chat::bearer(v["key"].as_str().unwrap());
    let (s, v) = call(&w, &plain, "GET", "/transfers/api/transfers", None).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("transfer_device_only")),
        "{v}"
    );
    let (s, v) = call(
        &w,
        &plain,
        "GET",
        &format!("/transfers/api/targets?app_id={APP}"),
        None,
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("transfer_device_only")),
        "{v}"
    );

    let owner = w.gw.client();
    let (s, v) = call(
        &w,
        &owner,
        "GET",
        &format!("/transfers/api/targets?app_id={APP}"),
        None,
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("transfer_device_only")),
        "{v}"
    );
    let bytes = file(32);
    let id = ready(&w, &desktop, &phone, &bytes).await;
    let (s, v) = call(
        &w,
        &owner,
        "GET",
        &format!("/transfers/api/transfers/{id}/content"),
        None,
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("transfer_device_only")),
        "{v}"
    );
    let (s, v) = call(
        &w,
        &owner,
        "GET",
        "/transfers/api/transfers?role=incoming",
        None,
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (400, Some("bad_request")), "{v}");
    let (s, v) = call(&w, &owner, "GET", "/transfers/api/transfers", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["stored_bytes"], 32);
    let (s, v) = call(
        &w,
        &owner,
        "POST",
        &format!("/transfers/api/transfers/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!((s, v["state"].as_str()), (200, Some("canceled")), "{v}");
    assert_eq!(v["end_reason"], "canceled by the dashboard");
}

/// A batch agent with nothing to run: what is under test is its token.
fn agent_doc(id: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "{id}",
  "name": "Labeler",
  "model": {{ "alias": "{{{{config.model}}}}" }},
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "model": {{ "type": "string", "format": "model_alias" }}
  }} }} }},
  "tools": [],
  "run": {{ "kind": "batch",
    "source": {{ "tool": "gws__search" }},
    "item": {{ "id": "{{{{item.id}}}}", "columns": {{ "subject": "{{{{item.subject}}}}" }} }},
    "limits": {{ "deadline_seconds": 0 }} }}
}}"#
    )
}

/// With **Require API key** off, a request that presents nothing is 401
/// `session_required` on every transfers route (§10); an agent token is
/// 403 `transfer_device_only`.
#[tokio::test]
async fn nobody_and_an_agent_are_refused_by_the_gate() {
    let (w, desktop, phone) = two().await;
    assert!(!w.state.snapshot().settings.auth_enabled, "the default");
    let id = ready(&w, &desktop, &phone, &file(16)).await;
    let anon = w.gw.anon();
    for (method, path) in [
        ("GET", "/transfers/api/transfers".to_string()),
        ("GET", format!("/transfers/api/transfers/{id}/content")),
        ("PUT", format!("/transfers/api/transfers/{id}/content")),
        ("POST", format!("/transfers/api/transfers/{id}/cancel")),
    ] {
        let (s, v) = call(&w, &anon, method, &path, None).await;
        assert_eq!(
            (s, v["code"].as_str()),
            (401, Some("session_required")),
            "{method} {path}: {v}"
        );
    }
    let (s, v) = crate::device_chat::op(
        &w,
        "agent_set",
        json!({ "manifest": agent_doc("labeler"), "replace": true }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (_, minted) =
        crate::device_chat::op(&w, "agent_token_get", json!({ "id": "labeler" })).await;
    let agent = crate::device_chat::bearer(minted["token"].as_str().unwrap());
    let (s, v) = call(&w, &agent, "GET", "/transfers/api/transfers", None).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("transfer_device_only")),
        "{v}"
    );
    assert!(v["message"].as_str().unwrap().contains("labeler"), "{v}");
}

/// The sender was disabled, or its row went, after the gate let its offer
/// in (the offer reads its devices again in its transaction): the refusal
/// the gate gives that device's next request, not one about the target
/// (I29).
#[tokio::test]
async fn an_offer_from_a_device_that_just_ended_is_refused_as_the_gate_would() {
    let (w, desktop, phone) = two().await;
    let bytes = file(10);
    let body = json!({"to_device": phone.id, "app_id": APP, "kind": "save",
                      "filename": "a.bin", "size": 10, "sha256": sha(&bytes)});
    // Behind the snapshot's back, as a Disable that committed while this
    // request passed the gate.
    sqlx::query("UPDATE api_keys SET enabled = 0 WHERE id = ?1")
        .bind(desktop.id)
        .execute(&w.state.db)
        .await
        .unwrap();
    let (s, v) = call(
        &w,
        &desktop.client,
        "POST",
        "/transfers/api/transfers",
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (401, Some("device_disabled")),
        "{v}"
    );
    assert!(v["message"].as_str().unwrap().contains("'desktop'"), "{v}");
    sqlx::query("DELETE FROM api_keys WHERE id = ?1")
        .bind(desktop.id)
        .execute(&w.state.db)
        .await
        .unwrap();
    let (s, v) = call(
        &w,
        &desktop.client,
        "POST",
        "/transfers/api/transfers",
        Some(body),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (401, Some("device_key_unknown")),
        "{v}"
    );
    let (_, all) = call(&w, &w.gw.client(), "GET", "/transfers/api/transfers", None).await;
    assert_eq!(all["transfers"], json!([]), "no offer was made: {all}");
}
