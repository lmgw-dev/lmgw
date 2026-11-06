//! How transfers end (device transfers design §1, §7, §8): `received`,
//! `decline` and `cancel` delete the bytes; the hourly sweep and a late
//! request expire an overdue one; a deleted or disabled device's transfers
//! are canceled, by the sweep too when the row went without that; a device
//! that pairs again as another app ends what was offered to it as the old
//! one; ended rows go after the feed's retention.

use serde_json::json;

use super::{call, device, file, offer, path_of, put, read, ready, two, APP};

#[tokio::test]
async fn received_decline_and_cancel_delete_the_bytes() {
    let (w, desktop, phone) = two().await;
    for (who, action, state) in [
        ("phone", "received", "delivered"),
        ("phone", "decline", "declined"),
        ("desktop", "cancel", "canceled"),
    ] {
        let id = ready(&w, &desktop, &phone, &file(4096)).await;
        assert!(path_of(&w, &id).exists());
        let client = if who == "phone" {
            &phone.client
        } else {
            &desktop.client
        };
        let (s, v) = call(
            &w,
            client,
            "POST",
            &format!("/transfers/api/transfers/{id}/{action}"),
            None,
        )
        .await;
        assert_eq!(
            (s, v["state"].as_str()),
            (200, Some(state)),
            "{action}: {v}"
        );
        assert!(!path_of(&w, &id).exists(), "{action}: the bytes are gone");
        let (s, v) = call(
            &w,
            client,
            "POST",
            &format!("/transfers/api/transfers/{id}/{action}"),
            None,
        )
        .await;
        assert_eq!(
            (s, v["code"].as_str(), v["state"].as_str()),
            (409, Some("transfer_state"), Some(state)),
            "{v}"
        );
    }
    // A cancel during the upload deletes what was uploaded.
    let bytes = file(1000);
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap();
    put(&w, &desktop.client, id, 0, &bytes[..400], 1000).await;
    assert!(path_of(&w, id).exists());
    let (s, v) = call(
        &w,
        &desktop.client,
        "POST",
        &format!("/transfers/api/transfers/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!((s, v["received"].as_u64()), (200, Some(400)), "{v}");
    assert!(!path_of(&w, id).exists());
}

/// Past `expires_at`: the sweep expires what is in progress and deletes its
/// bytes; a request that comes first expires it itself.
#[tokio::test]
async fn an_overdue_transfer_expires() {
    let (w, desktop, phone) = two().await;
    let a = ready(&w, &desktop, &phone, &file(100)).await;
    let t = offer(&w, &desktop, &phone, &file(100)).await;
    let b = t["id"].as_str().unwrap().to_string();
    let c = ready(&w, &desktop, &phone, &file(100)).await;
    for id in [&a, &b, &c] {
        sqlx::query(
            "UPDATE transfers SET created_at = '1999-12-25T00:00:00Z',
                                  expires_at = '2000-01-01T00:00:00Z' WHERE id = ?1",
        )
        .bind(id)
        .execute(&w.state.db)
        .await
        .unwrap();
    }
    // A late download finds it expired.
    let (s, v) = call(
        &w,
        &phone.client,
        "GET",
        &format!("/transfers/api/transfers/{c}/content"),
        None,
    )
    .await;
    assert_eq!((s, v["state"].as_str()), (409, Some("expired")), "{v}");
    lmgw_core::transfers::upkeep(&w.state).await;
    // A sentence for people: no setting's key, no raw time (the card
    // formats expires_at and ended_at itself).
    for (id, why) in [
        (&a, "not delivered within 7 days of the offer"),
        (&b, "its upload did not finish within 7 days of the offer"),
        (&c, "not delivered within 7 days of the offer"),
    ] {
        let (_, v) = read(&w, &desktop.client, id).await;
        assert_eq!(v["state"], "expired", "{v}");
        assert_eq!(v["end_reason"], why, "{v}");
        assert!(!path_of(&w, id).exists());
    }
    // Ended rows go after chat_feed_retention_days (7 by default).
    sqlx::query("UPDATE transfers SET ended_at = '2000-01-01T00:00:00Z' WHERE id = ?1")
        .bind(&a)
        .execute(&w.state.db)
        .await
        .unwrap();
    lmgw_core::transfers::upkeep(&w.state).await;
    let (s, _) = read(&w, &desktop.client, &a).await;
    assert_eq!(s, 404, "pruned");
    let (s, _) = read(&w, &desktop.client, &b).await;
    assert_eq!(s, 200, "ended today: kept");
}

/// A deleted device's transfers are canceled with the reason and their
/// bytes deleted; so are a disabled one's.
#[tokio::test]
async fn a_device_that_goes_ends_its_transfers() {
    let (w, desktop, phone) = two().await;
    let tablet = device(&w, "tablet", Some(APP)).await;
    let to_phone = ready(&w, &desktop, &phone, &file(100)).await;
    let from_phone = ready(&w, &phone, &desktop, &file(100)).await;
    let to_tablet = ready(&w, &desktop, &tablet, &file(100)).await;
    let (s, v) = crate::device_chat::op(&w, "key_delete", json!({"id": phone.id})).await;
    assert_eq!(s, 200, "{v}");
    for id in [&to_phone, &from_phone] {
        let (_, v) = read(&w, &desktop.client, id).await;
        assert_eq!(v["state"], "canceled", "{v}");
        assert_eq!(v["end_reason"], "device 'phone' was deleted");
        assert!(!path_of(&w, id).exists());
    }
    let (_, v) = read(&w, &desktop.client, &to_tablet).await;
    assert_eq!(v["state"], "ready", "another device's goes on: {v}");
    let (s, v) =
        crate::device_chat::op(&w, "key_set", json!({"id": tablet.id, "enabled": false})).await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = read(&w, &desktop.client, &to_tablet).await;
    assert_eq!(
        (v["state"].as_str(), v["end_reason"].as_str()),
        (Some("canceled"), Some("device 'tablet' was disabled"))
    );
    assert!(!path_of(&w, &to_tablet).exists());
}

/// The sweep deletes a file no transfer in progress names, and leaves any
/// other name alone.
#[tokio::test]
async fn the_sweep_deletes_stray_files_only() {
    let (w, desktop, phone) = two().await;
    let id = ready(&w, &desktop, &phone, &file(10)).await;
    let dir = lmgw_core::transfers::dir(&w.state.data_dir);
    let stray = lmgw_core::transfers::new_id();
    std::fs::write(dir.join(&stray), b"left over").unwrap();
    std::fs::write(dir.join("notes.txt"), b"not ours").unwrap();
    lmgw_core::transfers::upkeep(&w.state).await;
    assert!(!dir.join(&stray).exists());
    assert!(dir.join("notes.txt").exists());
    assert!(path_of(&w, &id).exists(), "a ready one's stays");
}

/// A device row that went without its transfers' end (a crash between the
/// two): the sweep cancels them with the reason and deletes their bytes
/// (§8's backstop).
#[tokio::test]
async fn the_sweep_ends_the_transfers_of_a_row_that_went() {
    let (w, desktop, phone) = two().await;
    let id = ready(&w, &desktop, &phone, &file(100)).await;
    sqlx::query("DELETE FROM api_keys WHERE id = ?1")
        .bind(phone.id)
        .execute(&w.state.db)
        .await
        .unwrap();
    let (_, v) = read(&w, &desktop.client, &id).await;
    assert_eq!(v["state"], "ready", "nothing ended it yet: {v}");
    lmgw_core::transfers::upkeep(&w.state).await;
    let (_, v) = read(&w, &desktop.client, &id).await;
    assert_eq!(
        (v["state"].as_str(), v["end_reason"].as_str()),
        (Some("canceled"), Some("device 'phone' was deleted")),
        "{v}"
    );
    assert!(!path_of(&w, &id).exists());
}

/// A device that pairs again without an app id stores none (I4), and one
/// whose waiting key declared another app id takes it at the owner's
/// Confirm (I3): what was offered to it as the old app ends then, canceled
/// with the reason, its bytes deleted and the sender told (I28). What it
/// sends goes on (I5).
#[tokio::test]
async fn a_device_that_pairs_again_as_another_app_ends_its_offers() {
    use crate::support::device_pair::{enrol_key_as, key, mint};
    let (w, desktop, phone) = two().await;
    let to_phone = ready(&w, &desktop, &phone, &file(100)).await;
    let from_phone = ready(&w, &phone, &desktop, &file(100)).await;
    let mut sender = crate::chat_feed::Feed::open(&w, &desktop.client, "", None).await;
    // The same key, renewed without an app id.
    let link = mint(&w.state, &w.gw, "device_token", json!({ "id": phone.id })).await;
    enrol_key_as(
        &link,
        phone.spki.clone(),
        phone.identity.1.clone_key(),
        None,
    )
    .await;
    let (_, keys) = call(&w, &w.gw.client(), "GET", "/api/usage/keys", None).await;
    let row = keys["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == phone.id)
        .unwrap()
        .clone();
    assert!(row["app_id"].is_null(), "{row}");
    let (_, v) = read(&w, &desktop.client, &to_phone).await;
    assert_eq!(
        (v["state"].as_str(), v["end_reason"].as_str()),
        (
            Some("canceled"),
            Some("device 'phone' paired again without an app id")
        ),
        "{v}"
    );
    assert!(!path_of(&w, &to_phone).exists());
    let (_, v) = read(&w, &desktop.client, &from_phone).await;
    assert_eq!(v["state"], "ready", "what it sends goes on: {v}");
    sender
        .until(|f| {
            f.iter().any(|f| {
                f.event == "transfer.updated"
                    && f.data["id"] == to_phone.as_str()
                    && f.data["state"] == "canceled"
            })
        })
        .await;

    // A waiting key of another app: nothing ends until the Confirm.
    let tablet = device(&w, "tablet", Some(APP)).await;
    let to_tablet = ready(&w, &desktop, &tablet, &file(100)).await;
    let link = mint(&w.state, &w.gw, "device_token", json!({ "id": tablet.id })).await;
    let (spki, private) = key();
    let waiting = enrol_key_as(&link, spki, private, Some("org.example.other")).await;
    let (_, v) = read(&w, &desktop.client, &to_tablet).await;
    assert_eq!(v["state"], "ready", "unconfirmed: {v}");
    let (s, v) = crate::device_chat::op(
        &w,
        "device_confirm",
        json!({ "id": tablet.id, "confirm_code": waiting.credentials.confirm_code }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = read(&w, &desktop.client, &to_tablet).await;
    assert_eq!(
        (v["state"].as_str(), v["end_reason"].as_str()),
        (
            Some("canceled"),
            Some("device 'tablet' paired again as another app (org.example.other)")
        ),
        "{v}"
    );
    assert!(!path_of(&w, &to_tablet).exists());
}
