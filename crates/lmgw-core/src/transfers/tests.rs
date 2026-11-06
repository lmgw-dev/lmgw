//! The ends under a held slot (device transfers design I9, I18, I24), and
//! an end whose caller goes (I30).

use std::time::Duration;

use chrono::Utc;

use super::*;
use crate::state::AppState;
use crate::store::transfers::{NewTransfer, Offered};

/// Two device rows of one app and an upload in progress between them, with
/// `bytes` on disk, past its `expires_at`.
async fn overdue_upload(state: &AppState, bytes: &[u8]) -> String {
    for (id, name) in [(9010, "device:desktop"), (9011, "device:phone")] {
        sqlx::query(
            "INSERT INTO api_keys (id, name, key_hash, enabled, kind, app_id)
             VALUES (?1, ?2, ?3, 1, 'device', 'dev.example.app')",
        )
        .bind(id)
        .bind(name)
        .bind(format!("spki:{id}"))
        .execute(&state.db)
        .await
        .unwrap();
    }
    let id = new_id();
    let past = Utc::now() - chrono::Duration::days(1);
    let made = table::insert(
        &state.db,
        &NewTransfer {
            id: &id,
            app_id: "dev.example.app",
            kind: "save",
            title: "",
            filename: "f.bin",
            size: 10,
            sha256: &"0".repeat(64),
            from_key_id: 9010,
            from_name: "desktop",
            to_key_id: 9011,
            to_name: "phone",
            created_at: past - chrono::Duration::days(7),
            expires_at: past,
        },
    )
    .await
    .unwrap();
    assert!(matches!(made, Offered::Made(_)), "{made:?}");
    let f = open_write(&state.data_dir, &id).await.unwrap();
    write_at(&f, 0, bytes::Bytes::copy_from_slice(bytes))
        .await
        .unwrap();
    id
}

/// An upload that finds its transfer overdue under its own slot expires it
/// there, without asking for the slot it holds (6bc306e9), and the slot
/// goes with it.
#[tokio::test]
async fn an_upload_expires_an_overdue_transfer_under_its_own_slot() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = overdue_upload(&state, b"abc").await;
    let held = state.transfers.uploads.take(&id, Taker::Upload).await;
    let row = tokio::time::timeout(Duration::from_secs(5), load_holding(&state, &id))
        .await
        .expect("no wait on its own slot")
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "expired");
    assert_eq!(
        row.end_reason.as_deref(),
        Some("its upload did not finish within 7 days of the offer")
    );
    assert_eq!(row.received, 3);
    assert_eq!(len(&state.data_dir, &id).await, 0, "its bytes are deleted");
    drop(held);
    assert_eq!(state.transfers.upload_slots_for_tests(), 0);
}

/// An end finishes though its caller goes (I30): dropped while the end
/// waits for the slot (a handler hyper dropped), the end still commits,
/// closes the transfer's downloads, deletes its bytes and lets go of the
/// slot once it has it.
#[tokio::test]
async fn an_end_finishes_though_its_caller_goes() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = overdue_upload(&state, b"abc").await;
    let reading = state
        .transfers
        .downloads
        .open(&id, open_read(&state.data_dir, &id).await.unwrap());
    let held = state.transfers.uploads.take(&id, Taker::Upload).await;
    // The caller of `end` is the handle it awaits. Dropping that handle
    // leaves the task running, so the suite keeps it and waits for the task
    // itself: "committed" is never read as "finished".
    let ending = spawn_end(
        &state,
        &id,
        EndsFrom::Active,
        TransferState::Canceled,
        Some("canceled by the owner"),
    );
    drop(held);
    let row = ending
        .await
        .unwrap()
        .unwrap()
        .expect("it ended the transfer");
    assert_eq!(
        (row.state.as_str(), row.end_reason.as_deref()),
        ("canceled", Some("canceled by the owner"))
    );
    assert_eq!(len(&state.data_dir, &id).await, 0, "its bytes are deleted");
    assert_eq!(state.transfers.downloads_open_for_tests(), 0);
    assert!(reading.read_at(0, 3).await.is_err());
    assert_eq!(state.transfers.upload_slots_for_tests(), 0, "the slot goes");
}
