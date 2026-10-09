//! A folder's `profile_id` and Settings → Chat's `chat_profile` (the
//! profiles review's fix 3): a folder's null is "take `chat_profile`", for
//! its new threads and for its current thread when a change is applied; an
//! Admin Chat thread never takes `chat_profile`, whether created or brought
//! along by a folder's change.

use serde_json::{json, Value};

use crate::chat_ongoing::{current_ok, message, ongoing_folder};
use crate::device_chat::{get, post};
use crate::realtime_chat_thread::{world, World};

async fn owner_post(w: &World, path: &str, body: Value) -> (u16, Value) {
    post(w, &w.gw.client(), path, body).await
}

async fn profile_of(w: &World, thread: i64) -> Value {
    let (_, t) = get(w, &w.gw.client(), &format!("/chat/api/threads/{thread}")).await;
    t["thread"]["profile_id"].clone()
}

/// Two profiles: Settings → Chat's `chat_profile` and a folder's own.
async fn two_profiles(w: &World) -> (i64, i64) {
    let mut ids = Vec::new();
    for name in ["Everyday", "Folder's"] {
        let (s, p) = owner_post(w, "/chat/api/profiles", json!({"name": name})).await;
        assert_eq!(s, 200, "{p}");
        ids.push(p["id"].as_i64().unwrap());
    }
    let (s, v) = owner_post(
        w,
        "/api/op/settings_set_full",
        json!({"chat_profile": ids[0]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    (ids[0], ids[1])
}

#[tokio::test]
async fn a_folder_s_null_takes_the_chat_s_profile_for_new_threads() {
    let w = world(|_| {}).await;
    let (everyday, own) = two_profiles(&w).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (current, _, _) = current_ok(&w, &owner, folder, false).await;
    assert_eq!(
        profile_of(&w, current).await,
        json!(everyday),
        "null: chat_profile"
    );

    let (s, v) = owner_post(
        &w,
        &format!("/chat/api/folders/{folder}"),
        json!({"defaults_patch": {"profile_id": own}}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(profile_of(&w, current).await, json!(own));
    // Back to null: the current thread goes back to chat_profile, not none.
    let (s, v) = owner_post(
        &w,
        &format!("/chat/api/folders/{folder}"),
        json!({"defaults_patch": {"profile_id": null}}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["applied"]["fields"], json!(["profile_id"]), "{v}");
    assert_eq!(profile_of(&w, current).await, json!(everyday));
    message(&w, current, 1).await;
    let (next, rolled, _) = current_ok(&w, &owner, folder, true).await;
    assert!(rolled && next != current);
    assert_eq!(profile_of(&w, next).await, json!(everyday));
}

#[tokio::test]
async fn an_admin_thread_never_takes_the_chat_s_profile() {
    let w = world(|_| {}).await;
    let (_, own) = two_profiles(&w).await;
    let owner = w.gw.client();
    // Created: none, whatever chat_profile says.
    let (s, v) = owner_post(
        &w,
        "/chat/api/threads",
        json!({"model_alias": "chatty", "kind": "admin"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["profile_id"], Value::Null, "{v}");

    // A folder's current thread that is Admin Chat (no route makes one; the
    // comparison must still treat it as its kind): a folder's change does
    // not bring it chat_profile.
    let folder = ongoing_folder(&w, &owner, "Ops", 30, json!({"profile_id": own})).await;
    let (current, _, _) = current_ok(&w, &owner, folder, false).await;
    sqlx::query("UPDATE chat_threads SET kind = 'admin', profile_id = NULL WHERE id = ?1")
        .bind(current)
        .execute(&w.state.db)
        .await
        .unwrap();
    let (s, v) = owner_post(
        &w,
        &format!("/chat/api/folders/{folder}"),
        json!({"defaults_patch": {"profile_id": null}}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["applied"], Value::Null, "nothing to bring it: {v}");
    assert_eq!(profile_of(&w, current).await, Value::Null);
}
