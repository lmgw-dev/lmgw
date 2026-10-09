//! Links that drop, devices that forget, a gateway that restarts (§1.6,
//! T7): lmgw ends a task only on the receiver's word.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use super::{
    call, executor, next, poll_every, result_text, row_in, task_world, thread, Script, TaskDevice,
};
use crate::device_chat::op;

/// A link that drops ends nothing; the device's next link is polled at
/// once, and the result arrives after the reconnect. A second link taking
/// over is polled the same way.
#[tokio::test]
async fn a_dropped_link_ends_nothing_and_the_next_link_is_polled_at_once() {
    let script = Script {
        poll_interval: Some(600_000),
        ..Script::default()
    };
    let (w, d, dev, server) = task_world(script).await;
    poll_every(&w, 600).await;
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    let script = dev.script.clone();
    dev.vanish();
    crate::common::patience::until_async("the row is offline", || async {
        !w.state.mcp.is_ready(server).await
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        super::the_row(&w).await.state,
        "open",
        "a drop ends nothing"
    );
    assert!(w.state.mcp.has_open_tasks(server));

    let mut again = TaskDevice::link(&w, &d, script.clone()).await;
    let got = next("the poll at the new link", &mut again.seen.gets).await;
    assert_eq!(got["params"]["taskId"], "t1");

    // A takeover: another link of the same device, polled at once too.
    let mut third = TaskDevice::link(&w, &d, script).await;
    next("the poll at the takeover", &mut third.seen.gets).await;
    third.complete("t1", "done after all", true);
    let row = row_in(&w, "ended").await;
    assert_eq!(
        result_text(&row),
        "job t1 (desktop__build) completed\ndone after all"
    );
    drop(again);
}

/// A device that restarted forgot its tasks: its `-32602` for the id
/// ends the task abandoned.
#[tokio::test]
async fn a_device_that_forgot_the_task_ends_it_abandoned() {
    let (w, d, dev, server) = task_world(Script::default()).await;
    poll_every(&w, 600).await;
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    dev.vanish();
    let _fresh = TaskDevice::link(&w, &d, Arc::new(Mutex::new(Script::default()))).await;
    let row = row_in(&w, "ended").await;
    assert_eq!(row.status, "abandoned");
    assert!(result_text(&row).contains("no longer knows the job"));
}

/// A disabled key closes the link; its tasks wait, since the key can be
/// enabled again.
#[tokio::test]
async fn a_disabled_key_s_tasks_wait() {
    let (w, d, _dev, server) = task_world(Script::default()).await;
    poll_every(&w, 1).await;
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    let (s, v) = op(&w, "key_set", json!({"id": d.id, "enabled": false})).await;
    assert_eq!(s, 200, "{v}");
    crate::common::patience::until_async("the link is closed", || async {
        !w.state.mcp.is_ready(server).await
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let row = super::the_row(&w).await;
    assert_eq!(
        (row.state.as_str(), row.status.as_str()),
        ("open", "working")
    );
}

/// lmgw restarted: the open rows resume, polled at once.
#[tokio::test]
async fn open_rows_resume_at_start() {
    let (w, _d, mut dev, server) = task_world(Script::default()).await;
    poll_every(&w, 600).await;
    let tid = thread(&w).await;
    // A row from before the restart, for a task the device runs.
    dev.script.lock().unwrap().tasks.insert(
        "t9".into(),
        super::FakeTask {
            status: "working".into(),
            message: None,
            poll_interval: None,
            payload: None,
        },
    );
    let new = lmgw_core::store::mcp_tasks::NewMcpTask {
        server_id: server,
        server_label: "desktop",
        task_id: "t9",
        thread_id: tid,
        tool: "desktop__build",
        call_id: "call_1",
        started_by: None,
        status: "working",
        status_message: None,
        poll_interval_ms: None,
        ttl_ms: None,
    };
    lmgw_core::store::mcp_tasks::insert(&w.state.db, &new)
        .await
        .unwrap();
    assert!(!w.state.mcp.has_open_tasks(server), "not followed yet");
    w.state.mcp.resume_tasks().await;
    assert!(w.state.mcp.has_open_tasks(server));
    let got = next("the resumed poll", &mut dev.seen.gets).await;
    assert_eq!(got["params"]["taskId"], "t9");
    dev.complete("t9", "finished while lmgw was away", true);
    let row = row_in(&w, "ended").await;
    assert_eq!(row.call_id, "call_1");
    assert_eq!(row.status, "completed");
}
