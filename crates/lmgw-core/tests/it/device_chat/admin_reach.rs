//! What a device allowed lmgw's admin tools hears as its switch moves, in
//! its feed (the pre-merge review's P-1, P-4, P-7 and P-9, decided by the
//! owner 2026-10-07):
//!
//! - a device that was away across a switch-off, or across an on and then
//!   an off, catches up at the narrower of its reach then and now: nothing
//!   of the toolset's threads as they are now, nothing written while it was
//!   not allowed, and one `resync` at each switch record;
//! - `hello` and `state` are at the reach it has now, and `state` says the
//!   switch (`self_admin`), so a connected client learns it moved;
//! - a live switch-off leaves no `state` behind that lists a turn on a
//!   toolset thread;
//! - a folder delete by a device without the switch tells a switched
//!   device the toolset threads that stay are in no folder now.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use super::{chat_thread, op, pair, post, self_admin_thread};
use crate::chat_feed::{about_thread, subject, Feed, Frame};
use crate::realtime_chat_thread::{world, World};
use crate::support::realtime_fakes::{Step, Turn};

/// Set `tid`'s system prompt as the owner, the toolset kept.
async fn prompt(w: &World, tid: i64, text: &str) {
    let (s, v) = post(
        w,
        &w.gw.client(),
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }], "system_prompt": text }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
}

/// A plain thread of the owner's, created last: once its `thread.created`
/// came, the stream has read everything written before it.
async fn marker(w: &World) -> i64 {
    chat_thread(w, &w.gw.client(), "chatty").await
}

fn created(frames: &[Frame], id: i64) -> bool {
    frames
        .iter()
        .any(|f| f.event == "thread.created" && subject(f) == id)
}

fn says(frames: &[Frame], text: &str) -> bool {
    frames.iter().any(|f| f.data.to_string().contains(text))
}

/// A turn as `client` on `tid` whose reply holds until the notify: its
/// stream, read until the reply began (the turn is live then).
async fn held_turn(
    w: &World,
    client: &reqwest::Client,
    tid: i64,
) -> (reqwest::Response, Arc<Notify>) {
    let hold = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Thinking"),
        Step::Wait(hold.clone()),
        Step::Text(" more."),
        Step::Finish("stop"),
        Step::Usage(12, 8),
    ]));
    let mut resp = client
        .post(format!("{}/chat/api/threads/{tid}/send", w.gw))
        .json(&json!({ "content": "a long one" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut got = String::new();
    while !got.contains("Thinking") {
        let chunk = tokio::time::timeout_at(deadline, resp.chunk())
            .await
            .unwrap_or_else(|_| panic!("the reply never began: {got}"))
            .unwrap()
            .expect("the stream ended before the reply began");
        got.push_str(&String::from_utf8_lossy(&chunk));
    }
    (resp, hold)
}

/// Away across a switch-off: what the owner wrote to the toolset thread
/// before and after the off never reaches the device, the off is one
/// `resync` carrying its cursor, and `hello` is at the reach it has now —
/// no turn running on the toolset thread.
#[tokio::test]
async fn a_device_away_across_a_switch_off_catches_up_at_its_reach_now() {
    let w = world(|_| {}).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let tools = self_admin_thread(&w).await;
    let feed = Feed::open(&w, &desk.client, "", None).await;
    let cursor = feed.cursor();
    drop(feed);

    prompt(&w, tools.id, "written-before-the-off").await;
    let (s, v) = op(&w, "key_set", json!({ "id": desk.id, "self_admin": "off" })).await;
    assert_eq!(s, 200, "{v}");
    prompt(&w, tools.id, "written-after-the-off").await;
    let (turn, hold) = held_turn(&w, &w.gw.client(), tools.id).await;
    let last = marker(&w).await;

    let mut back = Feed::open(&w, &desk.client, "", Some(&cursor)).await;
    assert_eq!(back.hello()["self_admin"], json!("off"));
    assert_eq!(
        back.hello()["turns"],
        json!([]),
        "the toolset thread's turn is not the device's to see now"
    );
    back.until(10, |f| created(f, last)).await;
    assert!(
        about_thread(&back.frames, tools.id).is_empty(),
        "nothing of the toolset thread: {:#?}",
        back.frames
    );
    assert!(!says(&back.frames, "written-before-the-off"));
    assert!(!says(&back.frames, "written-after-the-off"));
    let resyncs = back.named("resync");
    assert_eq!(resyncs.len(), 1, "one at the switch: {:#?}", back.frames);
    assert!(resyncs[0].id.is_some(), "carrying the switch's cursor");
    let at = back
        .frames
        .iter()
        .position(|f| f.event == "resync")
        .unwrap();
    let marker_at = back
        .frames
        .iter()
        .position(|f| f.event == "thread.created" && subject(f) == last)
        .unwrap();
    assert!(at < marker_at, "at its place in the order");
    hold.notify_one();
    drop(turn);
}

/// Away across an on and then an off: the toolset's threads never arrive
/// — not as the "on" would have brought them, nor anything written while it
/// was on — and each switch is a `resync`.
#[tokio::test]
async fn a_device_away_across_an_on_and_an_off_hears_nothing_of_the_toolset() {
    let w = world(|_| {}).await;
    let phone = pair(&w, "phone", json!({})).await;
    let tools = self_admin_thread(&w).await;
    let feed = Feed::open(&w, &phone.client, "", None).await;
    let cursor = feed.cursor();
    drop(feed);

    let (s, _) = op(
        &w,
        "key_set",
        json!({ "id": phone.id, "self_admin": "read_only" }),
    )
    .await;
    assert_eq!(s, 200);
    prompt(&w, tools.id, "written-while-it-was-on").await;
    let (s, _) = op(
        &w,
        "key_set",
        json!({ "id": phone.id, "self_admin": "off" }),
    )
    .await;
    assert_eq!(s, 200);
    let last = marker(&w).await;

    let mut back = Feed::open(&w, &phone.client, "", Some(&cursor)).await;
    assert_eq!(back.hello()["self_admin"], json!("off"));
    back.until(10, |f| created(f, last)).await;
    assert!(
        about_thread(&back.frames, tools.id).is_empty(),
        "no thread.created, nothing at all: {:#?}",
        back.frames
    );
    assert!(!says(&back.frames, "written-while-it-was-on"));
    assert_eq!(back.named("resync").len(), 2, "{:#?}", back.frames);
}

/// Away across an on: what was written before it is not played back (it
/// was not the device's then), the switch is a `resync` — the client
/// reloads, at the reach it has now — and what comes after it is.
#[tokio::test]
async fn a_device_away_across_an_on_reloads_at_the_switch() {
    let w = world(|_| {}).await;
    let phone = pair(&w, "phone", json!({})).await;
    let tools = self_admin_thread(&w).await;
    let feed = Feed::open(&w, &phone.client, "", None).await;
    let cursor = feed.cursor();
    drop(feed);

    prompt(&w, tools.id, "written-before-the-on").await;
    let (s, _) = op(
        &w,
        "key_set",
        json!({ "id": phone.id, "self_admin": "read_only" }),
    )
    .await;
    assert_eq!(s, 200);
    prompt(&w, tools.id, "written-after-the-on").await;

    let mut back = Feed::open(&w, &phone.client, "", Some(&cursor)).await;
    assert_eq!(back.hello()["self_admin"], json!("read_only"));
    back.until(10, |f| says(f, "written-after-the-on")).await;
    assert!(!says(&back.frames, "written-before-the-on"));
    let order: Vec<&str> = back
        .frames
        .iter()
        .filter(|f| f.event == "resync" || subject(f) == tools.id)
        .map(|f| f.event.as_str())
        .collect();
    assert_eq!(order, ["resync", "thread.updated"], "{:#?}", back.frames);
}

/// A live switch-off while a turn runs on a toolset thread: after the
/// thread's removal, no `state` lists that turn, and the last one says the
/// switch is off; turned on again, a `state` says so.
#[tokio::test]
async fn a_live_switch_off_leaves_no_state_with_the_toolset_s_turn() {
    let w = world(|_| {}).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let tools = self_admin_thread(&w).await;
    let mut feed = Feed::open(&w, &desk.client, "", None).await;
    let (turn, hold) = held_turn(&w, &w.gw.client(), tools.id).await;
    feed.until(10, |f| {
        f.iter()
            .any(|f| f.event == "turn.started" && f.data["thread_id"] == tools.id)
    })
    .await;

    let (s, _) = op(&w, "key_set", json!({ "id": desk.id, "self_admin": "off" })).await;
    assert_eq!(s, 200);
    feed.until(10, |f| {
        f.iter()
            .skip_while(|f| f.event != "thread.deleted")
            .any(|f| f.event == "state" && f.data["self_admin"] == json!("off"))
    })
    .await;
    let gone = feed
        .frames
        .iter()
        .position(|f| f.event == "thread.deleted" && subject(f) == tools.id)
        .expect("the toolset thread went");
    let lists_it = |f: &Frame| {
        f.data["turns"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["thread_id"] == tools.id))
    };
    for f in &feed.frames[gone..] {
        assert!(
            !(f.event == "state" && lists_it(f)),
            "a state after the removal lists its turn: {:#?}",
            &feed.frames[gone..]
        );
        assert!(
            !f.event.starts_with("turn.") || f.data["thread_id"] != tools.id,
            "{:#?}",
            &feed.frames[gone..]
        );
    }

    let seen = feed.frames.len();
    let (s, _) = op(
        &w,
        "key_set",
        json!({ "id": desk.id, "self_admin": "read_only" }),
    )
    .await;
    assert_eq!(s, 200);
    feed.until(10, |f| {
        f[seen..]
            .iter()
            .any(|f| f.event == "state" && f.data["self_admin"] == json!("read_only"))
    })
    .await;
    let state = feed.frames[seen..]
        .iter()
        .rev()
        .find(|f| f.event == "state")
        .unwrap();
    assert!(
        lists_it(state),
        "on again, the turn is its to see: {state:?}"
    );
    hold.notify_one();
    drop(turn);
}

/// Review P-7: a device without the switch deletes a folder that also
/// holds a toolset thread; the folder stays, hidden from devices, and a
/// switched device's feed hears that thread in no folder, then the
/// folder's removal.
#[tokio::test]
async fn a_folder_delete_by_another_device_moves_a_switched_device_s_threads_out() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let phone = pair(&w, "phone", json!({})).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let (s, v) = post(&w, &owner, "/chat/api/folders", json!({ "name": "Mixed" })).await;
    assert_eq!(s, 200, "{v}");
    let folder = v["id"].as_i64().unwrap();
    let tools = self_admin_thread(&w).await;
    let plain = chat_thread(&w, &phone.client, "chatty").await;
    for t in [tools.id, plain] {
        let (s, v) = post(
            &w,
            &owner,
            &format!("/chat/api/threads/{t}/move"),
            json!({ "folder_id": folder }),
        )
        .await;
        assert_eq!(s, 200, "{v}");
    }
    let mut feed = Feed::open(&w, &desk.client, "", None).await;

    let (s, v) = post(
        &w,
        &phone.client,
        &format!("/chat/api/folders/{folder}/delete"),
        json!({ "threads": "keep" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    feed.until(10, |f| f.iter().any(|f| f.event == "folder.deleted"))
        .await;
    let order: Vec<(String, i64)> = feed
        .frames
        .iter()
        .filter(|f| f.id.is_some() && f.event != ":")
        .map(|f| (f.event.clone(), subject(f)))
        .collect();
    assert_eq!(
        order,
        [
            ("thread.updated".to_string(), plain),
            ("thread.updated".to_string(), tools.id),
            ("folder.deleted".to_string(), folder),
        ],
        "{:#?}",
        feed.frames
    );
    let moved = feed
        .named("thread.updated")
        .into_iter()
        .find(|f| subject(f) == tools.id)
        .unwrap();
    assert_eq!(
        moved.data["thread"]["folder_id"],
        Value::Null,
        "in no folder"
    );
    let (s, v) = super::get(&w, &desk.client, &format!("/chat/api/threads/{}", tools.id)).await;
    assert_eq!((s, &v["thread"]["folder_id"]), (200, &Value::Null), "{v}");
}
