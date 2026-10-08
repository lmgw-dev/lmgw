//! Ongoing-conversation folders (client-apps design §3, WP5), through the
//! router and the gate as a client reaches them — the dashboard's bearer
//! for the owner, a paired device's key for a device.
//!
//! - `current`: `POST /chat/api/folders/{id}/current` — `first`, an empty
//!   thread reused on `new`, `requested`, `idle` from the newest message,
//!   `not_ongoing`, `folder_no_model`, the model an ongoing folder needs,
//!   two callers at once getting one thread, and a thread created in the
//!   folder by hand;
//! - `ends`: a delete, a move out, an archive by hand and a folder that
//!   stops being ongoing end the current thread, each recording
//!   `folder.current` in the feed;
//! - `defaults`: a defaults change reaching the current thread (L9), the
//!   opt-out, a refusal failing the whole patch, the live flip;
//! - `retention`: the sweep skips current threads, and a folder's own
//!   archive and purge days drive the sweep and `purge_at`;
//! - `device`: L3 for `current` — the 404 of an admin folder, a rollover
//!   never handing a device an admin thread, the new thread the device's
//!   own, and `folder.current` never reaching a device about a folder or
//!   thread out of its reach;
//! - `device_writes`: what a device may write — no folder retention, a
//!   defaults change within its scopes, no stale write over an attach.

use serde_json::{json, Value};

use crate::device_chat::post;
use crate::realtime_chat_thread::World;

mod current;
mod defaults;
mod device;
mod device_writes;
mod ends;
mod retention;

/// An ongoing folder `name` made by `client`, its defaults naming `chatty`
/// plus `defaults`, rolling over after `idle` minutes: its id.
pub(crate) async fn ongoing_folder(
    w: &World,
    client: &reqwest::Client,
    name: &str,
    idle: i64,
    defaults: Value,
) -> i64 {
    let mut d = json!({ "model_alias": "chatty" });
    for (k, v) in defaults.as_object().cloned().unwrap_or_default() {
        d[k] = v;
    }
    let (s, v) = post(
        w,
        client,
        "/chat/api/folders",
        json!({ "name": name, "defaults": d, "ongoing": { "idle_minutes": idle } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["ongoing"]["idle_minutes"], idle, "{v}");
    v["id"].as_i64().unwrap()
}

/// `POST /chat/api/folders/{folder}/current` as `client`.
pub(crate) async fn current(
    w: &World,
    client: &reqwest::Client,
    folder: i64,
    new: bool,
) -> (u16, Value) {
    post(
        w,
        client,
        &format!("/chat/api/folders/{folder}/current"),
        json!({ "new": new }),
    )
    .await
}

/// `current` that must answer: the thread's id, whether it rolled over, and
/// its reason.
pub(crate) async fn current_ok(
    w: &World,
    client: &reqwest::Client,
    folder: i64,
    new: bool,
) -> (i64, bool, Value) {
    let (s, v) = current(w, client, folder, new).await;
    assert_eq!(s, 200, "{v}");
    (
        v["thread"]["id"].as_i64().unwrap(),
        v["rolled_over"].as_bool().unwrap(),
        v["reason"].clone(),
    )
}

/// A user message in thread `id`, written `ago` minutes ago: its id.
pub(crate) async fn message(w: &World, id: i64, ago: i64) -> i64 {
    let sent =
        lmgw_core::store::append_user_message_with_voice(&w.state.db, id, "hello", &[], &[], None)
            .await
            .unwrap();
    let lmgw_core::store::SendMessageOutcome::Sent(m) = sent else {
        panic!("the message was not written");
    };
    sqlx::query("UPDATE chat_messages SET created_at = datetime('now', ?2) WHERE id = ?1")
        .bind(m)
        .bind(format!("-{ago} minutes"))
        .execute(&w.state.db)
        .await
        .unwrap();
    m
}

/// The folder `id` as `GET /chat/api/folders` lists it for `client`.
pub(crate) async fn folder_of(w: &World, client: &reqwest::Client, id: i64) -> Option<Value> {
    let (s, v) = crate::device_chat::get(w, client, "/chat/api/folders").await;
    assert_eq!(s, 200, "{v}");
    v["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == id)
        .cloned()
}
