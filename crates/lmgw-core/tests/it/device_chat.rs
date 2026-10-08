//! A paired device in the Chat (client-apps design §1.3, WP3): what it
//! reaches, and what it runs as.
//!
//! Driven as the device, over HTTP and a real WebSocket through the router
//! and the gate — the way a client app reaches lmgw — on the realtime-thread
//! world's fakes (the chat fake `chatty`/`other`, the ASR fake `hear`, the
//! TTS fake `speak`). The owner's side of each check is the dashboard's
//! bearer (`Gw::client`).
//!
//! - `walk`: every thread-, message- and attachment-addressed `/chat/api`
//!   route, enumerated from `CAPABILITY_TABLE`, answers a device 404 for an
//!   Admin Chat thread (L3), and not for a chat thread;
//! - `collections`: the list, folder counts, search, exports and a folder
//!   delete leave Admin Chat out;
//! - `writes`: creating an admin thread, tool labels and knowledge bases a
//!   device writes (L5, review W2-4);
//! - `turns`: a device's turn runs as its key — rows, scope, rpm, its tool
//!   scope, the knowledge skip — and its non-turn routes do too (W2-3);
//! - `bind`: `/v1/realtime?chat_thread=` as a device, and the takeover
//!   naming its binder (§1.7);
//! - `revoke`: a Rotate ends a running Chat stream and fails its next call
//!   (W2-2); `/api/events` says so, and says when a device connects;
//! - `self_admin`: a thread or a folder with the self-admin toolset attached
//!   is out of a device's reach like Admin Chat (review W3-1), and attaching
//!   it takes a thread a device holds out of its reach at once;
//! - `admin_tools`: a device allowed lmgw's admin tools (`self_admin`,
//!   2026-10-07) sees and uses those threads and folders, in text and in
//!   voice, never Admin Chat; the switch moves its reach at once;
//! - `admin_reach`: what its feed hears as the switch moves — a catch-up
//!   across it, a live switch-off with a turn running, another device's
//!   folder delete (the pre-merge review's P-1, P-4, P-7, P-9);
//! - `admin_levels`: the device's level — read only and full, capped by the
//!   gateway's — and what a write, a program on this machine and a lowered
//!   level come to (the pre-merge review's P-3);
//! - `admin_surfaces`: the switch on every other surface — its own
//!   patterns, the self-admin level, `/v1/responses`, `/v1/mcp/servers`, an
//!   unbound realtime session — and what stops when it is turned off or
//!   the device disabled (P-6, P-8, P-10);
//! - `admin_guards`: the settings that decide who reaches lmgw are no
//!   tool's to change, and the gateway's self-admin level moving plays as
//!   the device's own level moving (2026-10-07);
//! - `admin_agents`: a device's agent runs are its own, at its level, and it
//!   replaces only the agents it created (review G-1);
//! - `admin_steer`: a device at read only reads and uses a toolset thread
//!   but does not change what drives the owner's turns there (review G-2);
//! - `admin_gaps`: the review's test gaps (G-9): the refusals on every
//!   surface, the turn a gateway's off cancels, mixed catch-ups, the first
//!   save, a failed reload, and the revocation kinds;
//! - `reach_order`: a bound session's 4004 follows the write that took its
//!   thread out of reach — an attach, a lowered level — so the folder's
//!   current thread has moved on when the device hears it; a deleted
//!   thread ends the session the same way (§1.6's close-code note).

use std::time::Duration;

use serde_json::{json, Value};

use crate::realtime_chat_thread::{world, World};

mod admin_agents;
mod admin_attach;
mod admin_gaps;
mod admin_guards;
mod admin_levels;
mod admin_reach;
mod admin_steer;
mod admin_surfaces;
mod admin_tools;
mod bind;
mod collections;
mod reach_order;
mod revoke;
mod self_admin;
mod turns;
mod walk;
mod writes;

/// A device paired on `w` with `policy` (`key_set`'s fields) — its id and
/// key, and a client that presents the key.
pub(crate) struct Device {
    pub id: i64,
    pub key: String,
    pub client: reqwest::Client,
}

pub(crate) async fn pair(w: &World, name: &str, policy: Value) -> Device {
    let mut body = json!({ "kind": "device", "name": name });
    for (k, v) in policy.as_object().cloned().unwrap_or_default() {
        body[k] = v;
    }
    let (status, v) = op(w, "key_create", body).await;
    assert_eq!(status, 200, "{v}");
    let key = v["key"].as_str().unwrap().to_string();
    Device {
        id: v["id"].as_i64().unwrap(),
        client: bearer(&key),
        key,
    }
}

pub(crate) fn bearer(key: &str) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {key}").parse().unwrap(),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

/// An `/api/op/*` call as the owner.
pub(crate) async fn op(w: &World, name: &str, body: Value) -> (u16, Value) {
    let resp =
        w.gw.client()
            .post(format!("{}/api/op/{name}", w.gw))
            .json(&body)
            .send()
            .await
            .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `GET` as `client`: the status and the JSON body (`Null` when it is none).
pub(crate) async fn get(w: &World, client: &reqwest::Client, path: &str) -> (u16, Value) {
    let resp = client.get(format!("{}{path}", w.gw)).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `POST` with a JSON body as `client`: the status and the JSON body.
pub(crate) async fn post(
    w: &World,
    client: &reqwest::Client,
    path: &str,
    body: Value,
) -> (u16, Value) {
    let resp = client
        .post(format!("{}{path}", w.gw))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// An SSE route read to its end as `client`: the status and every
/// `(event, data)` it sent.
pub(crate) async fn sse(
    w: &World,
    client: &reqwest::Client,
    path: &str,
    body: Value,
) -> (u16, Vec<(String, Value)>) {
    let resp = client
        .post(format!("{}{path}", w.gw))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = tokio::time::timeout(Duration::from_secs(30), resp.text())
        .await
        .expect("the stream ends")
        .unwrap();
    (status, frames(&text))
}

/// The `(event, data)` of every SSE record in `text`.
pub(crate) fn frames(text: &str) -> Vec<(String, Value)> {
    text.split("\n\n")
        .filter_map(|record| {
            let mut event = None;
            let mut data = String::new();
            for line in record.lines() {
                if let Some(e) = line.strip_prefix("event:") {
                    event = Some(e.trim().to_string());
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push_str(d.trim_start());
                }
            }
            let event = event?;
            Some((event, serde_json::from_str(&data).unwrap_or(Value::Null)))
        })
        .collect()
}

/// The request rows charged to key `key_id`: `(ingress_proto,
/// requested_alias, status, error_kind)`, oldest first.
pub(crate) async fn rows_of(w: &World, key_id: i64) -> Vec<(String, String, i64, Option<String>)> {
    // A row is written on its own task (`recording/write.rs`): give the
    // last one a moment to land.
    tokio::time::sleep(Duration::from_millis(150)).await;
    sqlx::query_as(
        "SELECT ingress_proto, requested_alias, status, error_kind FROM request_logs \
         WHERE key_id = ?1 ORDER BY id",
    )
    .bind(key_id)
    .fetch_all(&w.state.db)
    .await
    .unwrap()
}

/// An Admin Chat thread of the owner's, with a user message (its id) and a
/// draft attachment (its id) — what a device must never reach.
pub(crate) struct AdminThread {
    pub id: i64,
    pub message: i64,
    pub attachment: i64,
}

pub(crate) async fn admin_thread(w: &World) -> AdminThread {
    let (status, v) = post(
        w,
        &w.gw.client(),
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "kind": "admin" }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["kind"], "admin", "{v}");
    let id = v["id"].as_i64().unwrap();
    let (message, attachment) = seed(w, id, "the owner's provider key is sk-secret-zebra").await;
    AdminThread {
        id,
        message,
        attachment,
    }
}

/// A chat thread of the owner's with the self-admin toolset attached, seeded
/// as [`admin_thread`] seeds one: Admin Chat in all but its kind, and as
/// absent for a device (L3, review W3-1).
pub(crate) async fn self_admin_thread(w: &World) -> AdminThread {
    let owner = w.gw.client();
    let id = chat_thread(w, &owner, "chatty").await;
    let (status, v) = post(
        w,
        &owner,
        &format!("/chat/api/threads/{id}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let (message, attachment) = seed(w, id, "the owner's provider key is sk-secret-zebra").await;
    AdminThread {
        id,
        message,
        attachment,
    }
}

/// The two kinds of thread a device never reaches (L3): an Admin Chat
/// thread, and a chat thread with the self-admin toolset attached.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Hidden {
    Admin,
    SelfAdmin,
}

impl Hidden {
    pub(crate) const BOTH: [Hidden; 2] = [Hidden::Admin, Hidden::SelfAdmin];

    pub(crate) async fn make(self, w: &World) -> AdminThread {
        match self {
            Self::Admin => admin_thread(w).await,
            Self::SelfAdmin => self_admin_thread(w).await,
        }
    }
}

/// A user message `text` and a draft text attachment in thread `id`, written
/// as the owner: their ids.
pub(crate) async fn seed(w: &World, id: i64, text: &str) -> (i64, i64) {
    let sent =
        lmgw_core::store::append_user_message_with_voice(&w.state.db, id, text, &[], &[], None)
            .await
            .unwrap();
    let lmgw_core::store::SendMessageOutcome::Sent(message) = sent else {
        panic!("the message was not written");
    };
    let resp =
        w.gw.client()
            .post(format!(
                "{}/chat/api/threads/{id}/attachments?name=notes.txt",
                w.gw
            ))
            .body("zebra notes")
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    let attachment = resp.json::<Value>().await.unwrap()["id"].as_i64().unwrap();
    (message, attachment)
}

/// A chat thread on `model`, created by `client`: its id.
pub(crate) async fn chat_thread(w: &World, client: &reqwest::Client, model: &str) -> i64 {
    let (status, v) = post(
        w,
        client,
        "/chat/api/threads",
        json!({ "model_alias": model }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    v["id"].as_i64().unwrap()
}

/// A world with one device paired: unscoped, unbudgeted (the pairing
/// form's prefill).
pub(crate) async fn device_world() -> (World, Device) {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    (w, d)
}
