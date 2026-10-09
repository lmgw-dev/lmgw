//! `device.revoked` (MCP Tasks design §4.1): a paired device's key deleted
//! is said, in the delete's own record, to the owner, to a device the feed
//! showed the deleted device's changes to, and to a device hosting a task
//! the deleted device started; to no other device. Disable says nothing (a
//! disabled key comes back).

use serde_json::{json, Value};

use super::Feed;
use crate::device_chat::{chat_thread, op, pair};
use crate::mcp_tasks::task_world;
use crate::realtime_chat_thread::world;

/// The `device.revoked` frames of `feed`, their data.
fn revoked(feed: &Feed) -> Vec<Value> {
    feed.frames
        .iter()
        .filter(|f| f.event == "device.revoked")
        .map(|f| {
            assert!(f.id.is_some(), "a stored event: {f:?}");
            f.data.clone()
        })
        .collect()
}

/// The data of device `name`'s `device.revoked`.
fn said(name: &str) -> Value {
    json!({"device": {"kind": "device", "name": name}, "by": "the dashboard"})
}

/// Wait until `feed` holds the `thread.created` of thread `id`.
async fn until_thread(feed: &mut Feed, id: i64) {
    feed.until(10, |f| {
        f.iter()
            .any(|f| f.event == "thread.created" && f.data["id"] == id)
    })
    .await;
}

/// `phone` acted where every device sees it (a thread it created), `watch`
/// never did: the owner hears both deletes, `tablet` only the phone's.
/// Disabling the phone first says nothing.
#[tokio::test]
async fn a_delete_reaches_the_owner_and_the_devices_shown_the_device() {
    let w = world(|_| {}).await;
    let phone = pair(&w, "phone", json!({})).await;
    let watch = pair(&w, "watch", json!({})).await;
    let tablet = pair(&w, "tablet", json!({})).await;
    let owner = w.gw.client();
    let mut mine = Feed::open(&w, &owner, "", None).await;
    let mut theirs = Feed::open(&w, &tablet.client, "", None).await;
    let made = chat_thread(&w, &phone.client, "chatty").await;
    until_thread(&mut theirs, made).await;

    let (s, v) = op(&w, "key_set", json!({"id": phone.id, "enabled": false})).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = op(&w, "key_set", json!({"id": phone.id, "enabled": true})).await;
    assert_eq!(s, 200, "{v}");
    let after = chat_thread(&w, &owner, "chatty").await;
    until_thread(&mut mine, after).await;
    assert!(
        revoked(&mine).is_empty(),
        "a disable is no delete: {:#?}",
        mine.frames
    );

    let (s, v) = op(&w, "key_delete", json!({"id": watch.id})).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = op(&w, "key_delete", json!({"id": phone.id})).await;
    assert_eq!(s, 200, "{v}");
    mine.until(10, |f| {
        f.iter().filter(|f| f.event == "device.revoked").count() == 2
    })
    .await;
    assert_eq!(revoked(&mine), [said("watch"), said("phone")]);
    theirs
        .until(10, |f| f.iter().any(|f| f.event == "device.revoked"))
        .await;
    // The phone's came after the watch's: the tablet was never shown the
    // watch, so it hears only the phone.
    assert_eq!(revoked(&theirs), [said("phone")]);

    // Caught up from where the tablet's feed began: the same.
    let from = theirs.hello()["cursor"].as_str().unwrap().to_string();
    let mut again = Feed::open(&w, &tablet.client, "", Some(&from)).await;
    again
        .until(10, |f| f.iter().any(|f| f.event == "device.revoked"))
        .await;
    assert_eq!(revoked(&again), [said("phone")]);
}

/// A device that hosts a task the deleted device started hears of the
/// delete although the task's thread is out of its reach (its `lmgw/caller`
/// named the device); a device neither shown nor hosting hears nothing.
#[tokio::test]
async fn a_delete_reaches_the_host_of_a_task_the_device_started() {
    let (w, desktop, mut dev, server) = task_world(crate::mcp_tasks::Script::default()).await;
    let phone = pair(&w, "phone", json!({})).await;
    let tablet = pair(&w, "tablet", json!({})).await;
    let owner = w.gw.client();
    // A thread no device sees (Admin Chat), with a task the phone started
    // on the desktop, still open.
    let (s, v) = crate::device_chat::post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({"model_alias": "chatty", "kind": "admin"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let admin = v["id"].as_i64().unwrap();
    // The job runs on the desktop (it knows it, so it stays open); its row
    // names the phone as its starter, as a turn of the phone's would.
    crate::mcp_tasks::call(
        &crate::mcp_tasks::executor(&w, server, Some(admin)),
        "desktop__build",
    )
    .await;
    let open = crate::mcp_tasks::row_in(&w, "open").await;
    sqlx::query("UPDATE mcp_tasks SET started_by = 'device:phone' WHERE id = ?1")
        .bind(open.id)
        .execute(&w.state.db)
        .await
        .unwrap();
    let mut host = Feed::open(&w, &desktop.client, "", None).await;
    let mut other = Feed::open(&w, &tablet.client, "", None).await;
    let mut mine = Feed::open(&w, &owner, "", None).await;

    let (s, v) = op(&w, "key_delete", json!({"id": phone.id})).await;
    assert_eq!(s, 200, "{v}");
    mine.until(10, |f| f.iter().any(|f| f.event == "device.revoked"))
        .await;
    host.until(10, |f| f.iter().any(|f| f.event == "device.revoked"))
        .await;
    assert_eq!(revoked(&mine), [said("phone")]);
    assert_eq!(revoked(&host), [said("phone")]);

    let after = chat_thread(&w, &owner, "chatty").await;
    until_thread(&mut other, after).await;
    assert!(revoked(&other).is_empty(), "{:#?}", other.frames);
    // The job runs on until its host ends it.
    assert_eq!(crate::mcp_tasks::the_row(&w).await.state, "open");
    crate::mcp_tasks::next("t1's call", &mut dev.seen.calls).await;
}

/// A task the deleted device started that was cancelled while its host was
/// offline owes its host the cancel (`cancel_owed`): the host may still be
/// running it, so it hears of the delete as the host of an open task does.
#[tokio::test]
async fn a_delete_reaches_the_host_owed_the_cancel_of_a_task_the_device_started() {
    let (w, desktop, dev, server) = task_world(crate::mcp_tasks::cancel::far()).await;
    crate::mcp_tasks::poll_every(&w, 600).await;
    let phone = pair(&w, "phone", json!({})).await;
    let owner = w.gw.client();
    let tid = crate::mcp_tasks::thread(&w).await;
    crate::mcp_tasks::call(
        &crate::mcp_tasks::executor(&w, server, Some(tid)),
        "desktop__build",
    )
    .await;
    dev.vanish();
    crate::common::patience::until_async("the desktop is offline", || async {
        !w.state.mcp.is_ready(server).await
    })
    .await;
    let row = crate::mcp_tasks::the_row(&w).await;
    let out = w.state.mcp.cancel_task(tid, row.id, "the owner").await;
    assert_eq!(out, Ok(lmgw_core::mcp::tasks::CancelOutcome::Owed));
    let owed = crate::mcp_tasks::row_in(&w, "cancel_owed").await;
    sqlx::query("UPDATE mcp_tasks SET started_by = 'device:phone' WHERE id = ?1")
        .bind(owed.id)
        .execute(&w.state.db)
        .await
        .unwrap();
    let mut host = Feed::open(&w, &desktop.client, "", None).await;
    let mut mine = Feed::open(&w, &owner, "", None).await;

    let (s, v) = op(&w, "key_delete", json!({"id": phone.id})).await;
    assert_eq!(s, 200, "{v}");
    mine.until(10, |f| f.iter().any(|f| f.event == "device.revoked"))
        .await;
    host.until(10, |f| f.iter().any(|f| f.event == "device.revoked"))
        .await;
    assert_eq!(revoked(&host), [said("phone")]);
    assert_eq!(crate::mcp_tasks::the_row(&w).await.state, "cancel_owed");
}
