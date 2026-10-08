//! The dashboard's `chat` frame on `/api/events` (client-apps design §3.6,
//! 2026-10-08): what another writer changes in the Chat — a paired
//! device, as the desktop client does — names the threads, folders and
//! messages the Chat page reads again, without the page asking.
//!
//! - the stream opens with a `resync`, so a page reads everything again on
//!   every (re)connect;
//! - a device's new thread, its turn, its message delete, its ongoing
//!   folder rolling over, and its spoken turn over
//!   `/v1/realtime?chat_thread=` are each named. A turn's user row and its
//!   saved reply are each named by a frame of their own: the upstream holds
//!   the reply until the user row's frame came (review CL-6);
//! - the owner sees every thread: Admin Chat is named too;
//! - a burst — a folder deleted with its threads — is one frame naming
//!   them all;
//! - the read the page follows with answers a failed read as a 500, never
//!   as "not there" or an empty history (review CL-4).
//!
//! That each message write of the Chat's repository marks its thread is
//! pinned one by one in `web/chat_repo/marks_tests.rs`, and that no write
//! goes past it by `chat_repo_seam_scan`.

use std::sync::Arc;

use lmgw_core::state::AppState;
use lmgw_core::store;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::chat_feed::{Feed as Events, Frame};
use crate::chat_ongoing::{current_ok, ongoing_folder};
use crate::common::serve;
use crate::device_chat::{chat_thread, device_world, post, sse};
use crate::realtime_chat_thread::{manual, next, say, until_type, world, World};
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::Turn;

/// The owner's `/api/events`, read up to its first `chat` frame.
async fn events(w: &World) -> Events {
    let resp =
        w.gw.client()
            .get(format!("{}/api/events", w.gw))
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    let mut ev = Events::reading(resp);
    ev.until(10, |f| f.iter().any(|f| f.event == "chat")).await;
    ev
}

/// The `chat` frames read so far.
fn chats(frames: &[Frame]) -> Vec<&Value> {
    frames
        .iter()
        .filter(|f| f.event == "chat")
        .map(|f| &f.data)
        .collect()
}

/// Whether a `chat` frame after the first `skip` names `id` under `key`
/// (`threads`, `folders` or `messages`).
fn named(frames: &[Frame], skip: usize, key: &str, id: i64) -> bool {
    chats(frames)
        .iter()
        .skip(skip)
        .any(|c| c[key].as_array().is_some_and(|a| a.contains(&json!(id))))
}

#[tokio::test]
async fn the_stream_opens_with_a_resync() {
    let w = world(|_| {}).await;
    let ev = events(&w).await;
    assert_eq!(
        chats(&ev.frames)[0],
        &json!({ "threads": [], "folders": [], "messages": [], "resync": true }),
        "{:#?}",
        ev.frames
    );
}

#[tokio::test]
async fn a_device_s_thread_and_turn_are_named_as_they_are_stored() {
    let (w, d) = device_world().await;
    let mut ev = events(&w).await;
    let seen = chats(&ev.frames).len();

    // A new conversation.
    let tid = chat_thread(&w, &d.client, "chatty").await;
    ev.until(10, |f| named(f, seen, "threads", tid)).await;
    let seen = chats(&ev.frames).len();

    // A turn: its user row is named as it is stored, while the upstream
    // still holds the reply; the saved reply by a later frame of its own.
    let release = Arc::new(Notify::new());
    w.chat.push(Turn::Held(
        release.clone(),
        Box::new(Turn::text(&["Hello."])),
    ));
    let path = format!("/chat/api/threads/{tid}/send");
    let send = sse(&w, &d.client, &path, json!({ "content": "hi" }));
    let user_row = async {
        ev.until(10, |f| named(f, seen, "messages", tid)).await;
        let seen = chats(&ev.frames).len();
        release.notify_one();
        seen
    };
    let ((s, frames), seen) = tokio::join!(send, user_row);
    assert_eq!(s, 200);
    let done = frames
        .iter()
        .find(|(e, _)| e == "done")
        .unwrap_or_else(|| panic!("{frames:?}"));
    let reply = done.1["message_id"].as_i64().expect("the reply was saved");
    ev.until(10, |f| named(f, seen, "messages", tid)).await;
    let seen = chats(&ev.frames).len();

    // A message deleted.
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/messages/{reply}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    ev.until(10, |f| named(f, seen, "messages", tid)).await;
}

#[tokio::test]
async fn a_device_rolling_its_folder_over_names_the_folder_and_the_new_thread() {
    let (w, d) = device_world().await;
    let folder = ongoing_folder(&w, &d.client, "desktop", 0, json!({})).await;
    let (first, _, _) = current_ok(&w, &d.client, folder, false).await;
    let mut ev = events(&w).await;
    let seen = chats(&ev.frames).len();

    // A message in it, then "new conversation": the empty thread is not
    // reused, so the folder rolls over.
    w.chat.push(Turn::text(&["Hi."]));
    sse(
        &w,
        &d.client,
        &format!("/chat/api/threads/{first}/send"),
        json!({ "content": "hello" }),
    )
    .await;
    let (next_thread, rolled, _) = current_ok(&w, &d.client, folder, true).await;
    assert!(rolled);
    assert_ne!(next_thread, first);
    ev.until(10, |f| {
        named(f, seen, "folders", folder) && named(f, seen, "threads", next_thread)
    })
    .await;
}

#[tokio::test]
async fn a_device_s_spoken_turn_names_the_thread_s_messages() {
    let (w, d) = device_world().await;
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let bearer = format!("Bearer {}", d.key);
    let mut ws = w
        .connect(
            &format!("chat_thread={tid}"),
            &[("authorization", bearer.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut ws).await["type"], "session.created");
    manual(&mut ws, 60_000).await;
    let mut ev = events(&w).await;
    let seen = chats(&ev.frames).len();

    // The heard turn's user row is named while the upstream holds the
    // reply; the saved reply by a later frame.
    let release = Arc::new(Notify::new());
    w.asr.push(Asr::Text("What time is it?"));
    w.chat.push(Turn::Held(
        release.clone(),
        Box::new(Turn::text(&["Late."])),
    ));
    say(&mut ws).await;
    ev.until(10, |f| named(f, seen, "messages", tid)).await;
    let seen = chats(&ev.frames).len();
    release.notify_one();
    until_type(&mut ws, "lmgw.response.timing").await;
    ev.until(10, |f| named(f, seen, "messages", tid)).await;
}

#[tokio::test]
async fn the_owner_hears_of_admin_chat_and_a_burst_is_one_frame() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let mut ev = events(&w).await;
    let seen = chats(&ev.frames).len();

    // Admin Chat is out of every device's reach, never the owner's.
    let (s, v) = post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "kind": "admin" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let admin = v["id"].as_i64().unwrap();
    ev.until(10, |f| named(f, seen, "threads", admin)).await;

    // A folder with three conversations, deleted with them in one write.
    let (s, v) = post(
        &w,
        &owner,
        "/chat/api/folders",
        json!({ "name": "project", "defaults": { "model_alias": "chatty" } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let folder = v["id"].as_i64().unwrap();
    let mut threads = Vec::new();
    for _ in 0..3 {
        let (s, v) = post(
            &w,
            &owner,
            "/chat/api/threads",
            json!({ "model_alias": "chatty", "folder_id": folder }),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        threads.push(v["id"].as_i64().unwrap());
    }
    let last = *threads.last().unwrap();
    ev.until(10, |f| named(f, 0, "threads", last)).await;
    let seen = chats(&ev.frames).len();
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}/delete"),
        json!({ "threads": "delete" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    ev.until(10, |f| named(f, seen, "folders", folder)).await;
    let frame = chats(&ev.frames)
        .into_iter()
        .skip(seen)
        .find(|c| c["folders"].as_array().unwrap().contains(&json!(folder)))
        .unwrap()
        .clone();
    assert_eq!(
        frame,
        json!({ "threads": threads, "folders": [folder], "messages": [], "resync": false }),
        "one frame names the folder and every thread it took"
    );
}

/// The read the Chat page follows another writer with: a thread that is not
/// there is a 404, and a read that failed is a 500 `internal` — never a 404
/// (the page would leave a thread it takes for deleted) nor an empty
/// history (it would drop every message it shows) (review CL-4).
#[tokio::test]
async fn a_failed_read_of_a_thread_is_a_500_never_a_404_nor_an_empty_history() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let tid = store::create_chat_thread(&state.db, "m", "chat")
        .await
        .unwrap();
    store::append_chat_message(&state.db, tid, "user", "hi", "", None, None, None)
        .await
        .unwrap();
    let read = |id: i64| {
        let (client, url) = (gw.client(), format!("{gw}/chat/api/threads/{id}"));
        async move {
            let r = client.get(url).send().await.unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap())
        }
    };
    let (s, v) = read(tid).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["messages"].as_array().map(Vec::len), Some(1), "{v}");
    let (s, v) = read(tid + 1000).await;
    assert_eq!((s, v["code"].as_str()), (404, Some("not_found")), "{v}");

    // Each read of the route failing in turn: the thread's row, its
    // messages, its attachments.
    for (away, back) in [
        (
            "ALTER TABLE chat_threads RENAME TO chat_threads_away",
            "ALTER TABLE chat_threads_away RENAME TO chat_threads",
        ),
        (
            "ALTER TABLE chat_messages RENAME TO chat_messages_away",
            "ALTER TABLE chat_messages_away RENAME TO chat_messages",
        ),
        (
            "ALTER TABLE chat_attachments RENAME TO chat_attachments_away",
            "ALTER TABLE chat_attachments_away RENAME TO chat_attachments",
        ),
    ] {
        sqlx::query(away).execute(&state.db).await.unwrap();
        let (s, v) = read(tid).await;
        assert_eq!(
            (s, v["code"].as_str()),
            (500, Some("internal")),
            "{away}: {v}"
        );
        sqlx::query(back).execute(&state.db).await.unwrap();
    }
    let (s, v) = read(tid).await;
    assert_eq!(s, 200, "readable again: {v}");
    assert_eq!(v["messages"].as_array().map(Vec::len), Some(1), "{v}");
}
