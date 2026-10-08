//! The sweep and an ongoing folder (client-apps design §3.5), and a
//! folder's own retention (§11 Q2: archive and purge days on the folder,
//! empty for the global setting).

use lmgw_core::store;
use serde_json::{json, Value};

use super::{current_ok, ongoing_folder};
use crate::device_chat::{chat_thread, get, post};
use crate::realtime_chat_thread::{world, World};

/// Make thread `id` look idle for `days` and, with `archived`, archived
/// `days` ago.
async fn age(w: &World, id: i64, days: i64, archived: bool) {
    sqlx::query(
        "UPDATE chat_threads SET updated_at = datetime('now', ?2),
                archived_at = CASE WHEN ?3 THEN datetime('now', ?2) END WHERE id = ?1",
    )
    .bind(id)
    .bind(format!("-{days} days"))
    .bind(archived)
    .execute(&w.state.db)
    .await
    .unwrap();
}

async fn state_of(w: &World, id: i64) -> Option<Value> {
    let (s, v) = get(w, &w.gw.client(), &format!("/chat/api/threads/{id}")).await;
    (s == 200).then(|| v["thread"].clone())
}

/// A thread in `folder`, made by the owner.
async fn in_folder(w: &World, folder: i64) -> i64 {
    let id = chat_thread(w, &w.gw.client(), "chatty").await;
    let (s, _) = post(
        w,
        &w.gw.client(),
        &format!("/chat/api/threads/{id}/move"),
        json!({ "folder_id": folder }),
    )
    .await;
    assert_eq!(s, 200);
    id
}

#[tokio::test]
async fn the_sweep_never_archives_a_folder_s_current_thread() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 0, json!({})).await;
    let (current, _, _) = current_ok(&w, &owner, folder, false).await;
    let past = in_folder(&w, folder).await;
    age(&w, current, 100, false).await;
    age(&w, past, 100, false).await;

    let (archived, _) = store::sweep_chat_threads(&w.state.db, 14, 30)
        .await
        .unwrap();
    assert_eq!(archived, 1);
    assert!(state_of(&w, past).await.unwrap()["archived_at"].is_string());
    assert_eq!(
        state_of(&w, current).await.unwrap()["archived_at"],
        json!(null)
    );
    assert_eq!(current_ok(&w, &owner, folder, false).await.0, current);
}

#[tokio::test]
async fn a_folder_s_own_days_drive_the_sweep_and_purge_at() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = |name: &str, archive: Value, purge: Value| {
        let (w, owner) = (&w, &owner);
        let body = json!({ "name": name, "archive_days": archive, "purge_days": purge });
        async move {
            let (s, v) = post(w, owner, "/chat/api/folders", body).await;
            assert_eq!(s, 200, "{v}");
            v["id"].as_i64().unwrap()
        }
    };
    // Keeps its history: never archived, or archived and kept a year.
    let never = folder("Never", json!(0), json!(null)).await;
    let year = folder("Year", json!(null), json!(365)).await;
    let quick = folder("Quick", json!(2), json!(0)).await;
    let (_, list) = get(&w, &owner, "/chat/api/folders").await;
    let year_row = list["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == year)
        .unwrap()
        .clone();
    assert_eq!(
        (&year_row["archive_days"], &year_row["purge_days"]),
        (&json!(null), &json!(365))
    );

    let idle_never = in_folder(&w, never).await;
    age(&w, idle_never, 400, false).await;
    let kept = in_folder(&w, year).await;
    age(&w, kept, 200, true).await;
    let idle_quick = in_folder(&w, quick).await;
    age(&w, idle_quick, 3, false).await;
    let old_quick = in_folder(&w, quick).await;
    age(&w, old_quick, 9_000, true).await;
    let loose = chat_thread(&w, &owner, "chatty").await;
    age(&w, loose, 40, true).await;

    let (archived, purged) = store::sweep_chat_threads(&w.state.db, 14, 30)
        .await
        .unwrap();
    assert_eq!(
        (archived, purged),
        (1, 1),
        "quick's idle one; the loose one"
    );
    assert_eq!(
        state_of(&w, idle_never).await.unwrap()["archived_at"],
        json!(null)
    );
    assert!(state_of(&w, idle_quick).await.unwrap()["archived_at"].is_string());
    assert!(state_of(&w, old_quick).await.is_some(), "0 never deletes");
    assert!(state_of(&w, loose).await.is_none(), "the global 30 days");

    // `purge_at` says the folder's days.
    let t = state_of(&w, kept).await.unwrap();
    let archived_at = t["archived_at"].as_str().unwrap();
    assert_eq!(
        t["purge_at"].as_str(),
        store::chat_thread_purge_at(archived_at, 365).as_deref()
    );
    assert_eq!(
        state_of(&w, old_quick).await.unwrap()["purge_at"],
        json!(null)
    );
    let (_, archived_list) = get(&w, &owner, "/chat/api/threads?archived=1").await;
    let row = archived_list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == kept)
        .unwrap()
        .clone();
    assert_eq!(row["purge_at"], t["purge_at"]);

    // Back to the global setting with `null`.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{year}"),
        json!({ "purge_days": null }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["purge_days"], json!(null));
    assert_eq!(
        state_of(&w, kept).await.unwrap()["purge_at"].as_str(),
        store::chat_thread_purge_at(archived_at, 30).as_deref()
    );
}

#[tokio::test]
async fn a_negative_count_of_days_is_refused_by_name() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    for field in ["archive_days", "purge_days"] {
        let (s, v) = post(
            &w,
            &owner,
            "/chat/api/folders",
            json!({ "name": "F", field: -1 }),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        assert!(v["message"].as_str().unwrap().starts_with(field), "{v}");
    }
    let (_, v) = post(&w, &owner, "/chat/api/folders", json!({ "name": "F" })).await;
    let id = v["id"].as_i64().unwrap();
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{id}"),
        json!({ "archive_days": -3 }),
    )
    .await;
    assert_eq!(s, 400, "{v}");
}
