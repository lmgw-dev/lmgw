//! A deleted profile's id never comes back (the profiles review's fix 7):
//! a folder write checks its default's profile on its own transaction, a
//! temporary thread loses the id with the delete, and a settings save made
//! from a snapshot that still held the id saves none.

use lmgw_core::store::{self, ChatFolderPatch, ThreadDefaults};
use serde_json::{json, Value};

use crate::device_chat::{get, post};
use crate::realtime_chat_thread::world;

#[tokio::test]
async fn a_folder_write_naming_a_gone_profile_is_refused_in_its_transaction() {
    let w = world(|_| {}).await;
    let pool = &w.state.db;
    let gone = ThreadDefaults {
        profile_id: Some(424_242),
        ..Default::default()
    };
    let e = store::create_chat_folder(pool, "late", &gone, None)
        .await
        .unwrap_err();
    assert_eq!(e.code(), "unknown_profile", "{e}");
    assert!(store::list_chat_folders(pool, store::AdminThreads::Shown)
        .await
        .unwrap()
        .is_empty());

    let id = store::create_chat_folder(pool, "f", &ThreadDefaults::default(), None)
        .await
        .unwrap();
    let e = store::update_chat_folder(
        pool,
        id,
        &ChatFolderPatch {
            name: Some("renamed".into()),
            defaults: Some(gone),
            ..Default::default()
        },
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(e.code(), "unknown_profile", "{e}");
    let f = store::get_chat_folder(pool, id).await.unwrap().unwrap();
    assert_eq!(
        (f.name.as_str(), f.defaults.profile_id),
        ("f", None),
        "nothing written"
    );
}

#[tokio::test]
async fn a_temporary_thread_loses_a_deleted_profile() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let (s, p) = post(&w, &owner, "/chat/api/profiles", json!({"name": "Brief"})).await;
    assert_eq!(s, 200, "{p}");
    let id = p["id"].as_i64().unwrap();
    let (s, t) = post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({"model_alias": "chatty", "temporary": true}),
    )
    .await;
    assert_eq!(s, 200, "{t}");
    let tid = t["id"].as_i64().unwrap();
    assert!(tid < 0, "temporary: {t}");
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"profile_id": id}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/profiles/{id}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (_, t) = get(&w, &owner, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(t["thread"]["profile_id"], Value::Null, "{t}");
}

#[tokio::test]
async fn a_settings_save_drops_a_gone_chat_profile() {
    let w = world(|_| {}).await;
    let pool = &w.state.db;
    // A snapshot that still holds a deleted profile's id, saved whole.
    let mut s = w.state.snapshot().settings.clone();
    s.chat_profile = Some(424_242);
    store::save_settings(pool, &s).await.unwrap();
    assert_eq!(store::load_settings(pool).await.unwrap().chat_profile, None);

    // A profile that exists is saved as it is.
    let (_, list) = get(&w, &w.gw.client(), "/chat/api/profiles").await;
    let concise = list["profiles"][0]["id"].as_i64().unwrap();
    s.chat_profile = Some(concise);
    store::save_settings(pool, &s).await.unwrap();
    assert_eq!(
        store::load_settings(pool).await.unwrap().chat_profile,
        Some(concise)
    );
}
