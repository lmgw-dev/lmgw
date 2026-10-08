//! The WP4 review's test gaps (W4-13): what the feed did not yet show
//! through the router — a flip read back in catch-up, a device's lag state,
//! the feed as a device's link (and a Rotate closing it unseen), an owner
//! key's revoked feed, the sweep's purge, and the keep-alive's read of a
//! write that never woke the feed.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use super::{about_thread, Feed};
use crate::device_chat::{chat_thread, get, op, pair, post};
use crate::realtime_chat_thread::world;
use crate::support::realtime_fakes::{Step, Turn};

/// Records written across a flip, read back by a device that catches up
/// after it: the thread arrives, goes and comes back, as the writes did.
#[tokio::test]
async fn a_device_catching_up_across_a_flip_hears_the_thread_go_and_come() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    let owner = w.gw.client();
    let start = Feed::open(&w, &d.client, "", None).await.cursor();
    let tid = chat_thread(&w, &owner, "chatty").await;
    for tools in [json!([{ "server_label": "lmgw" }]), json!([])] {
        let (s, v) = post(
            &w,
            &owner,
            &format!("/chat/api/threads/{tid}/settings"),
            json!({ "mcp_tools": tools }),
        )
        .await;
        assert_eq!(s, 200, "{v}");
    }
    let mut late = Feed::open(&w, &d.client, &format!("?since={start}"), None).await;
    late.until(10, |f| about_thread(f, tid).len() >= 3).await;
    let seen: Vec<&str> = about_thread(&late.frames, tid)
        .iter()
        .map(|f| f.event.as_str())
        .collect();
    assert_eq!(
        seen,
        ["thread.created", "thread.deleted", "thread.created"],
        "{:#?}",
        late.frames
    );
}

/// A device that fell behind the live buffer gets a `state` of its own:
/// an Admin Chat turn running is not in it.
#[tokio::test]
async fn a_device_s_lag_state_leaves_admin_chat_out() {
    let w = world(|s| s.chat_feed_live_buffer = 2).await;
    let d = pair(&w, "phone", json!({})).await;
    let owner = w.gw.client();
    let (_, v) = post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "kind": "admin" }),
    )
    .await;
    let admin = v["id"].as_i64().unwrap();
    let hold = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Checking"),
        Step::Wait(hold.clone()),
        Step::Text(" done."),
        Step::Finish("stop"),
        Step::Usage(5, 3),
    ]));
    let send = owner
        .post(format!("{}/chat/api/threads/{admin}/send", w.gw))
        .json(&json!({ "content": "status?" }))
        .send()
        .await
        .unwrap();
    assert_eq!(send.status(), 200);
    let mut device = Feed::open(&w, &d.client, "", None).await;
    let publish = |fallback: Option<&str>| {
        let mut snap = (*w.state.snapshot()).clone();
        snap.settings.hold.fallback_alias = fallback.map(str::to_string);
        w.state.set_snapshot_for_tests(snap);
    };
    for i in 0..6 {
        publish((i % 2 == 0).then_some("other"));
    }
    device
        .until(10, |f| f.iter().any(|f| f.event == "state"))
        .await;
    let state = &device.named("state")[0].data;
    assert_eq!(state["turns"], json!([]), "{state}");
    let reason = state["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("some live events were not delivered"),
        "{reason}"
    );
    hold.notify_one();
    drop(send);
}

/// `device:phone`'s row of the Keys list.
async fn row(w: &crate::realtime_chat_thread::World) -> Value {
    let (_, v) = get(w, &w.gw.client(), "/api/usage/keys").await;
    v["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["name"] == "device:phone")
        .cloned()
        .unwrap()
}

/// The feed is a device's link: online while it is open, `last_seen_at`
/// stamped as it opens.
#[tokio::test]
async fn a_device_s_feed_is_online_and_stamps_last_seen() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    let feed = Feed::open(&w, &d.client, "", None).await;
    let open = row(&w).await;
    assert_eq!(open["online"], json!(["feed"]), "{open}");
    assert_eq!(open["open_links"], json!([{ "kind": "feed", "count": 1 }]));
    assert!(open["last_seen_at"].is_string(), "{open}");
    // A second feed of the same device is counted, so a reconnect pile-up
    // shows on the card (review W4-25).
    let second = Feed::open(&w, &d.client, "", None).await;
    assert_eq!(
        row(&w).await["open_links"],
        json!([{ "kind": "feed", "count": 2 }])
    );
    drop(second);
    drop(feed);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let r: Value = row(&w).await;
        if r["online"] == json!([]) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never offline: {r}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A Rotate closes a device's feed without stamping `last_seen_at` (review
/// W2-19, the W4-13 residue named by W5-6): the old key's last moment is
/// not the device being seen.
#[tokio::test]
async fn a_rotate_closes_a_device_s_feed_unseen() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    let mut feed = Feed::open(&w, &d.client, "", None).await;
    let long_ago = "2020-01-01T00:00:00Z";
    sqlx::query("UPDATE api_keys SET last_seen_at = ?1 WHERE id = ?2")
        .bind(long_ago)
        .bind(d.id)
        .execute(&w.state.db)
        .await
        .unwrap();
    let (s, v) = op(&w, "key_rotate", json!({ "id": d.id })).await;
    assert_eq!(s, 200, "{v}");
    feed.until(10, |_| false).await;
    assert_eq!(feed.frames.last().unwrap().event, "revoked");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let r: Value = row(&w).await;
        if r["online"] == json!([]) {
            // The close's own work (it says `link` once done) has run.
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(row(&w).await["last_seen_at"], long_ago);
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never offline: {r}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// An owner key's feed ends with `revoked` when the key is rotated, as a
/// device's does (every key kind is watched).
#[tokio::test]
async fn an_owner_key_s_feed_ends_with_revoked() {
    let w = world(|_| {}).await;
    let (s, v) = op(&w, "key_create", json!({ "kind": "owner", "name": "cli" })).await;
    assert_eq!(s, 200, "{v}");
    let id = v["id"].as_i64().unwrap();
    let client = crate::device_chat::bearer(v["plaintext"].as_str().unwrap());
    let mut feed = Feed::open(&w, &client, "", None).await;
    assert_eq!(
        feed.hello()["principal"],
        json!({ "kind": "owner", "name": "cli" })
    );
    let (s, v) = op(&w, "key_rotate", json!({ "id": id })).await;
    assert_eq!(s, 200, "{v}");
    feed.until(10, |_| false).await;
    assert!(feed.ended);
    assert_eq!(feed.frames.last().unwrap().event, "revoked");
}

/// The sweep's purge is in the feed as `thread.deleted`, the gateway's own.
#[tokio::test]
async fn the_sweep_s_purge_is_in_the_feed() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    sqlx::query(
        "UPDATE chat_threads SET archived_at = datetime('now', '-90 days'),
                updated_at = datetime('now', '-90 days') WHERE id = ?1",
    )
    .bind(tid)
    .execute(&w.state.db)
    .await
    .unwrap();
    let mut feed = Feed::open(&w, &owner, "", None).await;
    lmgw_core::server::chat_upkeep(&w.state).await;
    feed.until(10, |f| f.iter().any(|f| f.event == "thread.deleted"))
        .await;
    assert_eq!(
        feed.named("thread.deleted")[0].data,
        json!({ "thread_id": tid, "deleted": true, "by": null })
    );
}

/// A write that recorded and never woke the feed is delivered at the next
/// keep-alive: the stream reads the table at every tick.
#[tokio::test]
async fn the_keep_alive_reads_a_write_that_never_woke_the_feed() {
    let w = world(|_| {}).await;
    super::set(&w, |s| s.chat_feed_keepalive_s = 1).await;
    let mut feed = Feed::open(&w, &w.gw.client(), "", None).await;
    // The store alone records; only the routes wake the feed.
    let id = lmgw_core::store::create_chat_thread(&w.state.db, "chatty", "chat")
        .await
        .unwrap();
    feed.until(5, |f| {
        f.iter()
            .any(|f| f.event == "thread.created" && f.data["id"] == id)
    })
    .await;
}
