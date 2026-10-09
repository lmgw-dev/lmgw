//! The cancel as the Chat's clients reach it (MCP Tasks design §1.5, §3.6,
//! §5.1, MCP Tasks WP2): `POST /chat/api/threads/{id}/tasks/{task}/cancel`
//! and the thread's delete and purge.
//!
//! - the owner's cancel sends `tasks/cancel`, and the result `cancelled by
//!   the dashboard` enters the idle thread; a second one is `409
//!   task_ended`;
//! - a cancel that loses to completion delivers the real result;
//! - one made while the device is offline delivers at once and owes the
//!   cancel, sent at the next link;
//! - a thread deleted by hand and one the sweep purges owe cancels for
//!   their open tasks and drop their ended results;
//! - a device cancels only tasks of threads it reaches: an Admin Chat
//!   thread's, or one of another thread, is `404`;
//! - the refusals of a server that cannot or will not cancel.

use std::time::Duration;

use serde_json::{json, Value};

use super::super::thread::{build_call, messages, tool_thread, until_results};
use super::super::{call, executor, next, poll_every, rows, task_world, Script, TaskDevice};
use super::far;
use crate::chat_approvals::send;
use crate::common::patience;
use crate::device_chat::{get, pair, post};
use crate::realtime_chat_thread::World;
use crate::support::realtime_fakes::Turn;
use lmgw_core::store::mcp_tasks;

/// `POST …/tasks/{task}/cancel` as `client`: the status and the answer.
async fn cancel(w: &World, client: &reqwest::Client, tid: i64, task: i64) -> (u16, Value) {
    post(
        w,
        client,
        &format!("/chat/api/threads/{tid}/tasks/{task}/cancel"),
        json!({}),
    )
    .await
}

/// The only open task's lmgw id.
async fn task_id(w: &World) -> i64 {
    let rows = rows(w).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    rows[0].id
}

/// The owner's cancel: `tasks/cancel` goes out, the answer says the result
/// is in the thread, and it is, `cancelled by the dashboard`. A task whose
/// result is in the thread is gone: a second cancel is `404 task_not_found`. One that
/// ended and waits for a turn to end is `409 task_ended`.
#[tokio::test]
async fn the_owner_s_cancel_sends_tasks_cancel_and_delivers_the_result() {
    let (w, _d, mut dev, server) = task_world(far()).await;
    let tid = tool_thread(&w, json!({})).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    let task = task_id(&w).await;
    let owner = w.gw.client();
    let (s, v) = cancel(&w, &owner, tid, task).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["delivered"], true, "{v}");
    assert_eq!(v["task"]["status"], "cancelled");
    assert_eq!(v["task"]["id"], task);
    assert!(
        v["note"].as_str().unwrap().contains("cancelled the job"),
        "{v}"
    );
    let sent = next("the tasks/cancel", &mut dev.seen.cancels).await;
    assert_eq!(sent["params"]["taskId"], "t1");
    let msgs = messages(&w, tid).await;
    let r = msgs.last().unwrap();
    assert_eq!(
        r["content"],
        "job t1 (desktop__build) cancelled\ncancelled by the dashboard"
    );
    assert_eq!(r["task"]["ended_by"], "the dashboard");
    let (s, v) = cancel(&w, &owner, tid, task).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (404, Some("task_not_found")),
        "{v}"
    );

    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    let held = w.state.chat_turn_held_for_tests(tid).await;
    dev.complete("t2", "done", true);
    patience::until_async("t2 ended", || async {
        rows(&w).await.iter().any(|r| r.state == mcp_tasks::ENDED)
    })
    .await;
    let task = task_id(&w).await;
    let (s, v) = cancel(&w, &owner, tid, task).await;
    assert_eq!((s, v["code"].as_str()), (409, Some("task_ended")), "{v}");
    drop(held);
}

/// The work finished before the cancel reached it: the real result enters
/// the thread, and the answer says so.
#[tokio::test]
async fn a_cancel_that_loses_to_completion_delivers_the_real_result() {
    let (w, _d, dev, server) = task_world(far()).await;
    let tid = tool_thread(&w, json!({})).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    dev.complete("t1", "done first", false);
    let task = task_id(&w).await;
    let (s, v) = cancel(&w, &w.gw.client(), tid, task).await;
    assert_eq!(s, 200, "{v}");
    assert!(
        v["note"].as_str().unwrap().contains("finished before"),
        "{v}"
    );
    assert_eq!(v["delivered"], true);
    let msgs = until_results(&w, tid, 1).await;
    assert_eq!(
        msgs.last().unwrap()["content"],
        "job t1 (desktop__build) completed\ndone first"
    );
}

/// The device offline: the job ends cancelled here at once, its result
/// enters the thread, and the owed cancel goes at the next link.
#[tokio::test]
async fn a_cancel_while_offline_delivers_at_once_and_is_sent_at_the_next_link() {
    let (w, d, dev, server) = task_world(far()).await;
    poll_every(&w, 600).await;
    let tid = tool_thread(&w, json!({})).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    let script = dev.script.clone();
    dev.vanish();
    patience::until_async("the row is offline", || async {
        !w.state.mcp.is_ready(server).await
    })
    .await;
    let task = task_id(&w).await;
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    assert!(
        v["tasks"][0]["waiting_for"]
            .as_str()
            .is_some_and(|s| s.contains("not connected")),
        "{v}"
    );
    let (s, v) = cancel(&w, &w.gw.client(), tid, task).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["delivered"], true, "{v}");
    assert!(v["note"].as_str().unwrap().contains("not connected"), "{v}");
    let msgs = messages(&w, tid).await;
    assert!(msgs.last().unwrap()["content"].as_str().unwrap().ends_with(
        "cancelled by the dashboard; the server was not connected, and is told when it is"
    ));
    let row = &rows(&w).await[0];
    assert_eq!(
        (row.state.as_str(), row.thread_id, row.result.as_deref()),
        (mcp_tasks::CANCEL_OWED, None, None),
        "delivered: the owed cancel alone waits"
    );
    let mut again = TaskDevice::link(&w, &d, script).await;
    let sent = next("the owed tasks/cancel", &mut again.seen.cancels).await;
    assert_eq!(sent["params"]["taskId"], "t1");
    patience::until_async("the row goes", || async { rows(&w).await.is_empty() }).await;
}

/// A thread deleted by hand: its open task owes a cancel, sent at once to
/// the linked device; its ended result is dropped with it.
#[tokio::test]
async fn a_deleted_thread_cancels_its_open_tasks_and_drops_its_results() {
    let (w, _d, mut dev, server) = task_world(far()).await;
    let tid = tool_thread(&w, json!({})).await;
    let exec = executor(&w, server, Some(tid));
    call(&exec, "desktop__build").await;
    call(&exec, "desktop__build").await;
    // t2 ended while a turn ran: its result waits in its row.
    let held = w.state.chat_turn_held_for_tests(tid).await;
    dev.complete("t2", "done", true);
    patience::until_async("t2 ended", || async {
        rows(&w).await.iter().any(|r| r.state == mcp_tasks::ENDED)
    })
    .await;
    let owner = w.gw.client();
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    drop(held);
    let sent = next("the delete's tasks/cancel", &mut dev.seen.cancels).await;
    assert_eq!(sent["params"]["taskId"], "t1");
    patience::until_async("both rows go", || async { rows(&w).await.is_empty() }).await;
}

/// The sweep's purge owes the cancels of the threads it deletes, as a
/// delete by hand does.
#[tokio::test]
async fn the_sweep_s_purge_cancels_its_threads_open_tasks() {
    let (w, _d, mut dev, server) = task_world(far()).await;
    let tid = tool_thread(&w, json!({})).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    sqlx::query(
        "UPDATE chat_threads SET archived_at = datetime('now', '-400 days'),
                updated_at = datetime('now', '-400 days') WHERE id = ?1",
    )
    .bind(tid)
    .execute(&w.state.db)
    .await
    .unwrap();
    crate::chat_feed::set(&w, |s| s.chat_purge_days = 30).await;
    lmgw_core::server::chat_upkeep(&w.state).await;
    let sent = next("the purge's tasks/cancel", &mut dev.seen.cancels).await;
    assert_eq!(sent["params"]["taskId"], "t1");
    patience::until_async("the row goes", || async { rows(&w).await.is_empty() }).await;
}

/// A device cancels only tasks of threads it reaches: one on an Admin Chat
/// thread does not exist for it, and a task named under another thread is
/// no task of that thread — `404`, the thread's and the task's, and nothing
/// is cancelled.
#[tokio::test]
async fn a_device_cannot_cancel_a_task_of_a_thread_it_does_not_reach() {
    let (w, _d, mut dev, server) = task_world(far()).await;
    let phone = pair(&w, "phone", json!({})).await;
    let owner = w.gw.client();
    let (s, v) = post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({"model_alias": "chatty", "kind": "admin"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let admin = v["id"].as_i64().unwrap();
    call(&executor(&w, server, Some(admin)), "desktop__build").await;
    let task = task_id(&w).await;
    let (s, v) = cancel(&w, &phone.client, admin, task).await;
    assert_eq!((s, v["code"].as_str()), (404, Some("not_found")), "{v}");

    let mine = tool_thread(&w, json!({})).await;
    let (s, v) = cancel(&w, &phone.client, mine, task).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (404, Some("task_not_found")),
        "{v}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        dev.seen.cancels.try_recv().is_err(),
        "nothing was cancelled"
    );
    assert_eq!(rows(&w).await[0].state, mcp_tasks::OPEN);
}

/// A server that cannot cancel (`409 task_cancel_unsupported`) or will not
/// (`502 task_cancel_refused`) is said so, and the task goes on.
#[tokio::test]
async fn the_refusals_of_a_server_that_cannot_or_will_not_cancel() {
    for (script, status, code) in [
        (
            Script {
                cancel_cap: false,
                ..far()
            },
            409,
            "task_cancel_unsupported",
        ),
        (
            Script {
                cancel_error: Some((-32603, "busy".into())),
                ..far()
            },
            502,
            "task_cancel_refused",
        ),
    ] {
        let (w, _d, _dev, server) = task_world(script).await;
        let tid = tool_thread(&w, json!({})).await;
        call(&executor(&w, server, Some(tid)), "desktop__build").await;
        let task = task_id(&w).await;
        let (s, v) = cancel(&w, &w.gw.client(), tid, task).await;
        assert_eq!((s, v["code"].as_str()), (status, Some(code)), "{v}");
        assert_eq!(rows(&w).await[0].state, mcp_tasks::OPEN);
    }
}

/// A device's own turn starts a task, and the device cancels it: `by` names
/// the device, on the row and in the result.
#[tokio::test]
async fn a_device_cancels_the_task_its_own_turn_started() {
    let (w, d, mut dev, _server) = task_world(far()).await;
    let tid = tool_thread(&w, json!({})).await;
    build_call(&w);
    w.chat.push(Turn::text(&["Started."]));
    send(&w, &d.client, tid, "build it").await;
    next("the device's call", &mut dev.seen.calls).await;
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    assert_eq!(v["tasks"][0]["by"], "device 'desktop'", "{v}");
    let task = task_id(&w).await;
    let (s, v) = cancel(&w, &d.client, tid, task).await;
    assert_eq!(s, 200, "{v}");
    let msgs = until_results(&w, tid, 1).await;
    assert_eq!(msgs.last().unwrap()["task"]["ended_by"], "device 'desktop'");
}

/// `GET …/threads/{id}/tasks` lists what `GET …/threads/{id}` lists as
/// `tasks`, without the history, and holds the same reach: a device gets its
/// own thread's, an Admin Chat thread is `404 not_found` for it.
#[tokio::test]
async fn the_tasks_read_lists_the_threads_jobs_within_a_callers_reach() {
    let (w, _d, _dev, server) = task_world(far()).await;
    let phone = pair(&w, "phone", json!({})).await;
    let owner = w.gw.client();
    let mine = tool_thread(&w, json!({})).await;
    call(&executor(&w, server, Some(mine)), "desktop__build").await;
    let task = task_id(&w).await;
    let (s, v) = get(&w, &owner, &format!("/chat/api/threads/{mine}/tasks")).await;
    assert_eq!(s, 200, "{v}");
    let whole = w.get(&format!("/chat/api/threads/{mine}")).await;
    assert_eq!(v, whole["tasks"], "the same list as the thread's");
    assert_eq!(v[0]["id"], task, "{v}");
    assert!(v.is_array() && v.as_array().unwrap().len() == 1, "{v}");

    let (_, v) = post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({"model_alias": "chatty", "kind": "admin"}),
    )
    .await;
    let admin = v["id"].as_i64().unwrap();
    let path = format!("/chat/api/threads/{admin}/tasks");
    let (s, v) = get(&w, &phone.client, &path).await;
    assert_eq!((s, v["code"].as_str()), (404, Some("not_found")), "{v}");
    assert_eq!(get(&w, &owner, &path).await, (200, json!([])));
    let (s, v) = get(&w, &phone.client, "/chat/api/threads/99999/tasks").await;
    assert_eq!((s, v["code"].as_str()), (404, Some("not_found")), "{v}");
}
