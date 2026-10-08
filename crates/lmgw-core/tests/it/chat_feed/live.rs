//! Live events: `hello`, keep-alive comments, turns with who started them,
//! the hold, and `state` for a reader that fell behind the live buffer.

use serde_json::{json, Value};

use super::Feed;
use crate::device_chat::{chat_thread, op, pair, sse};
use crate::realtime_chat_thread::world;

#[tokio::test]
async fn hello_says_who_the_feed_is_for_and_the_limits_it_runs_under() {
    let w = world(|s| {
        s.chat_feed_keepalive_s = 1;
        s.chat_feed_retention_days = 3;
    })
    .await;
    let owner = Feed::open(&w, &w.gw.client(), "", None).await;
    let hello = owner.hello();
    assert_eq!(
        hello["principal"],
        json!({ "kind": "owner", "name": "dashboard" })
    );
    assert_eq!(hello["hosts_label"], Value::Null);
    assert_eq!(hello["keepalive_s"], 1);
    assert_eq!(hello["retention_days"], 3);
    assert_eq!(
        hello["hold"],
        json!({ "active": false, "fallback_alias": null })
    );
    assert_eq!(
        (hello["voice"].clone(), hello["turns"].clone()),
        (json!([]), json!([]))
    );
    assert!(hello["cursor"]
        .as_str()
        .unwrap()
        .starts_with(hello["epoch"].as_str().unwrap()));

    let d = pair(&w, "desk", json!({ "hosts_label": "desk-tools" })).await;
    let mut device = Feed::open(&w, &d.client, "", None).await;
    assert_eq!(
        device.hello()["principal"],
        json!({ "kind": "device", "name": "desk" })
    );
    assert_eq!(device.hello()["hosts_label"], "desk-tools");

    // Keep-alive comments at the interval `hello` stated.
    let started = std::time::Instant::now();
    device.until(5, |f| f.iter().any(|f| f.event == ":")).await;
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
}

#[tokio::test]
async fn a_turn_is_live_with_who_started_it_and_how_it_ended() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let mut owner = Feed::open(&w, &w.gw.client(), "", None).await;

    let (status, said) = sse(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "hello there" }),
    )
    .await;
    assert_eq!(status, 200);
    let done = &said.iter().find(|(e, _)| e == "done").unwrap().1;
    let mid = done["message_id"].as_i64().unwrap();

    owner
        .until(10, |f| f.iter().any(|f| f.event == "turn.done"))
        .await;
    let started = &owner.named("turn.started")[0].data;
    assert_eq!(
        started,
        &json!({ "thread_id": tid, "by": "device 'phone'", "voice": false })
    );
    let finished = &owner.named("turn.done")[0].data;
    assert_eq!(
        finished,
        &json!({ "thread_id": tid, "message_id": mid, "saved": true, "code": null })
    );
    // The first send named the thread: a stored change, by the device.
    let titled = owner
        .named("thread.updated")
        .into_iter()
        .find(|f| f.data["id"] == tid)
        .expect("the title's thread.updated");
    assert_eq!(titled.data["by"], "device 'phone'");
    assert_eq!(titled.data["title"], "hello there");
}

#[tokio::test]
async fn the_hold_is_live_and_a_fallback_changed_through_the_settings_patch_counts() {
    let w = world(|_| {}).await;
    let mut feed = Feed::open(&w, &w.gw.client(), "", None).await;
    let (s, v) = op(
        &w,
        "settings_set_full",
        json!({ "hold": { "fallback_alias": "other" } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    feed.until(10, |f| f.iter().any(|f| f.event == "hold"))
        .await;
    assert_eq!(
        feed.named("hold")[0].data,
        json!({ "active": false, "fallback_alias": "other" })
    );
    let (s, v) = op(&w, "hold_set", json!({ "active": true })).await;
    assert_eq!(s, 200, "{v}");
    feed.until(10, |f| f.iter().filter(|f| f.event == "hold").count() >= 2)
        .await;
    assert_eq!(
        feed.named("hold")[1].data,
        json!({ "active": true, "fallback_alias": "other" })
    );
    // A new feed's hello carries it.
    let again = Feed::open(&w, &w.gw.client(), "", None).await;
    assert_eq!(again.hello()["hold"]["active"], true);
}

#[tokio::test]
async fn a_reader_behind_the_live_buffer_gets_state_naming_it_never_a_silent_gap() {
    let w = world(|s| s.chat_feed_live_buffer = 2).await;
    let mut feed = Feed::open(&w, &w.gw.client(), "", None).await;
    // Six hold changes without a yield: the stream, on this test's one
    // thread, cannot read between them, and the buffer holds two.
    let publish = |fallback: Option<&str>, buffer: u32| {
        let mut snap = (*w.state.snapshot()).clone();
        snap.settings.hold.fallback_alias = fallback.map(str::to_string);
        snap.settings.chat_feed_live_buffer = buffer;
        w.state.set_snapshot_for_tests(snap);
    };
    for i in 0..6 {
        publish((i % 2 == 0).then_some("other"), 2);
    }
    feed.until(10, |f| f.iter().any(|f| f.event == "state"))
        .await;
    let state = &feed.named("state")[0].data;
    let reason = state["reason"].as_str().unwrap();
    assert!(
        reason.contains("Settings → Chat → Change feed → Live buffer: 2 events"),
        "{reason}"
    );
    assert_eq!(
        state["hold"],
        json!({ "active": false, "fallback_alias": null }),
        "the state as it is after the last change"
    );

    // A resized buffer moves the open feeds over, with a state saying so,
    // and live events flow on the new one.
    publish(Some("other"), 8);
    feed.until(10, |f| f.iter().filter(|f| f.event == "state").count() >= 2)
        .await;
    let resized = &feed.named("state")[1].data;
    assert!(
        resized["reason"].as_str().unwrap().contains("resized"),
        "{resized}"
    );
    assert_eq!(resized["hold"]["fallback_alias"], "other");
    publish(None, 8);
    feed.until(10, |f| {
        f.iter()
            .rev()
            .any(|f| f.event == "hold" && f.data["fallback_alias"].is_null())
    })
    .await;
}

/// Reviews W4-13, W4-22: `voice.bound` and `voice.ended` through the feed,
/// and a folder deleted with its threads ends a bound session's thread as
/// a thread's own delete does — `thread_gone`, not `closed`.
#[tokio::test]
async fn a_folder_deleted_with_its_threads_says_a_bound_thread_went() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let (_, v) =
        crate::device_chat::post(&w, &owner, "/chat/api/folders", json!({ "name": "F" })).await;
    let folder = v["id"].as_i64().unwrap();
    let (_, v) = crate::device_chat::post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "folder_id": folder }),
    )
    .await;
    let tid = v["id"].as_i64().unwrap();
    let mut feed = Feed::open(&w, &owner, "", None).await;
    let (_ws, _) = w.bind(tid).await;
    feed.until(10, |f| f.iter().any(|f| f.event == "voice.bound"))
        .await;
    assert_eq!(
        feed.named("voice.bound")[0].data,
        json!({ "thread_id": tid, "by": "the dashboard" })
    );
    let (s, v) = crate::device_chat::post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}/delete"),
        json!({ "threads": "delete" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    drop(_ws);
    feed.until(10, |f| f.iter().any(|f| f.event == "voice.ended"))
        .await;
    let ended = &feed.named("voice.ended")[0].data;
    assert_eq!(
        (&ended["thread_id"], &ended["reason"]),
        (&json!(tid), &json!("thread_gone")),
        "{ended}"
    );
}
