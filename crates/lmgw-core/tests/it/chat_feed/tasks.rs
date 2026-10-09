//! The feed's `task.started` and `task.done` (MCP Tasks design §4.1): in
//! commit order, rendered from the facts the record keeps after the task
//! row is gone, and kept from a device for an Admin Chat thread.

use serde_json::{json, Value};

use super::Feed;
use crate::device_chat::{chat_thread, pair, post};
use crate::mcp_tasks::{call, executor, task_world, Script};
use crate::realtime_chat_thread::World;

/// The `task.*` frames of `feed`, as `(event, data)`.
fn tasks(feed: &Feed) -> Vec<(String, Value)> {
    feed.frames
        .iter()
        .filter(|f| f.event.starts_with("task."))
        .map(|f| (f.event.clone(), f.data.clone()))
        .collect()
}

/// A thread of the owner's with the device's label: its id.
async fn labelled(w: &World, kind: &str) -> i64 {
    let owner = w.gw.client();
    let (s, v) = post(
        w,
        &owner,
        "/chat/api/threads",
        json!({"model_alias": "chatty", "kind": kind}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let tid = v["id"].as_i64().unwrap();
    let (s, v) = post(
        w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "desktop"}]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    tid
}

/// A job starts and ends: `task.started` then `task.done`, the result's
/// message named; read again from before them once the task row is gone,
/// they render the same.
#[tokio::test]
async fn task_records_come_in_commit_order_and_render_after_the_row_is_gone() {
    let (w, _d, dev, server) = task_world(Script::default()).await;
    let tid = labelled(&w, "chat").await;
    let owner = w.gw.client();
    let mut feed = Feed::open(&w, &owner, "", None).await;
    let from = feed.cursor();
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    dev.complete("t1", "42 files", true);
    feed.until(10, |f| f.iter().any(|f| f.event == "task.done"))
        .await;
    let got = tasks(&feed);
    assert_eq!(got.len(), 2, "{got:#?}");
    let (started, done) = (&got[0], &got[1]);
    assert_eq!(started.0, "task.started");
    let id = started.1["id"].as_i64().unwrap();
    assert_eq!(
        started.1,
        json!({"thread_id": tid, "id": id, "task_id": "t1", "server_label": "desktop",
               "tool": "desktop__build", "by": "the dashboard"})
    );
    assert_eq!(done.0, "task.done");
    let message_id = done.1["message_id"].as_i64().unwrap();
    assert_eq!(
        done.1,
        json!({"thread_id": tid, "message_id": message_id, "id": id, "task_id": "t1",
               "server_label": "desktop", "tool": "desktop__build", "status": "completed",
               "by": null})
    );
    let msgs = w.get(&format!("/chat/api/threads/{tid}")).await["messages"].clone();
    assert_eq!(msgs.as_array().unwrap().last().unwrap()["id"], message_id);
    assert!(
        crate::mcp_tasks::rows(&w).await.is_empty(),
        "the task row is gone"
    );

    let mut again = Feed::open(&w, &owner, "", Some(&from)).await;
    again
        .until(10, |f| f.iter().any(|f| f.event == "task.done"))
        .await;
    assert_eq!(tasks(&again), got, "rendered from the records alone");
}

/// An Admin Chat thread's task records reach the owner and never a device;
/// the device still reads the owner's next plain thread past them.
#[tokio::test]
async fn a_device_hears_no_task_of_an_admin_chat_thread() {
    let (w, _d, dev, server) = task_world(Script::default()).await;
    let phone = pair(&w, "phone", json!({})).await;
    let owner = w.gw.client();
    let mut mine = Feed::open(&w, &owner, "", None).await;
    let mut theirs = Feed::open(&w, &phone.client, "", None).await;
    let admin = labelled(&w, "admin").await;
    call(&executor(&w, server, Some(admin)), "desktop__build").await;
    dev.complete("t1", "done", true);
    mine.until(10, |f| f.iter().any(|f| f.event == "task.done"))
        .await;
    assert_eq!(tasks(&mine).len(), 2);

    let plain = chat_thread(&w, &owner, "chatty").await;
    theirs
        .until(10, |f| {
            f.iter()
                .any(|f| f.event == "thread.created" && f.data["id"] == plain)
        })
        .await;
    assert!(tasks(&theirs).is_empty(), "{:#?}", theirs.frames);
}
