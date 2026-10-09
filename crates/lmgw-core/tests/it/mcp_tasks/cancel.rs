//! The cancel from lmgw (MCP Tasks design §1.5), as WP1 builds it:
//! `McpManager::cancel_task(thread, row, by)` and the owed cancel's
//! follower. A linked cancel, one that loses to completion, one owed while
//! the server is offline and sent at its next link, one the server refuses
//! or cannot take, the route's thread check, a row not followed yet, and a
//! thread delete's owed cancel. The routes are [`route`]'s (MCP Tasks
//! WP2).

use std::time::Duration;

use lmgw_core::mcp::tasks::{CancelOutcome, CancelRefusal};
use lmgw_core::store::mcp_tasks;

use super::{
    call, executor, next, poll_every, result_text, row_in, rows, task_world, the_row, thread,
    Script, TaskDevice,
};
use crate::common::patience;

mod route;

pub(crate) fn far() -> Script {
    Script {
        poll_interval: Some(600_000),
        ..Script::default()
    }
}

/// A linked cancel: `tasks/cancel`, and the row ends `cancelled` by who.
#[tokio::test]
async fn a_linked_cancel_ends_the_row_cancelled_by_who() {
    let (w, _d, mut dev, server) = task_world(far()).await;
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    let row = the_row(&w).await;
    let out = w.state.mcp.cancel_task(tid, row.id, "the owner").await;
    assert_eq!(out, Ok(CancelOutcome::Cancelled));
    let sent = next("the tasks/cancel", &mut dev.seen.cancels).await;
    assert_eq!(sent["params"]["taskId"], "t1");
    let row = row_in(&w, "ended").await;
    assert_eq!(
        (row.status.as_str(), row.ended_by.as_deref()),
        ("cancelled", Some("the owner"))
    );
    assert_eq!(
        result_text(&row),
        "job t1 (desktop__build) cancelled\ncancelled by the owner"
    );
    assert!(!w.state.mcp.has_open_tasks(server));
}

/// The work finished first: the device's `-32602` to the cancel, and the
/// real result is fetched.
#[tokio::test]
async fn a_cancel_that_loses_to_completion_keeps_the_result() {
    let (w, _d, dev, server) = task_world(far()).await;
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    dev.complete("t1", "done first", false);
    let row = the_row(&w).await;
    let out = w.state.mcp.cancel_task(tid, row.id, "the owner").await;
    assert_eq!(out, Ok(CancelOutcome::FinishedFirst));
    let row = row_in(&w, "ended").await;
    assert_eq!(row.status, "completed");
    assert_eq!(
        result_text(&row),
        "job t1 (desktop__build) completed\ndone first"
    );
}

/// The server offline: the row ends `cancelled` at once and owes the
/// cancel, which goes at the next link; the result still waits for its
/// thread, so the row stays `ended`.
#[tokio::test]
async fn a_cancel_while_offline_is_owed_and_sent_at_the_next_link() {
    let (w, d, dev, server) = task_world(far()).await;
    poll_every(&w, 600).await;
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    let script = dev.script.clone();
    dev.vanish();
    patience::until_async("the row is offline", || async {
        !w.state.mcp.is_ready(server).await
    })
    .await;
    let row = the_row(&w).await;
    let out = w.state.mcp.cancel_task(tid, row.id, "the owner").await;
    assert_eq!(out, Ok(CancelOutcome::Owed));
    let row = the_row(&w).await;
    assert_eq!(
        (row.state.as_str(), row.status.as_str(), row.thread_id),
        (mcp_tasks::CANCEL_OWED, "cancelled", Some(tid))
    );
    assert_eq!(
        result_text(&row),
        "job t1 (desktop__build) cancelled\ncancelled by the owner; the server was not \
         connected, and is told when it is"
    );
    assert!(
        !w.state.mcp.has_open_tasks(server),
        "an owed cancel holds no server"
    );
    assert_eq!(
        w.state.mcp.cancel_task(tid, row.id, "the owner").await,
        Err(CancelRefusal::Ended)
    );

    let mut again = TaskDevice::link(&w, &d, script).await;
    let sent = next("the owed tasks/cancel", &mut again.seen.cancels).await;
    assert_eq!(sent["params"]["taskId"], "t1");
    let row = row_in(&w, mcp_tasks::ENDED).await;
    assert!(result_text(&row).contains("cancelled by the owner"));
}

/// A `tasks/cancel` answered with a JSON-RPC error is refused with the
/// server's message, and the task goes on being followed.
#[tokio::test]
async fn a_cancel_the_server_refuses_keeps_the_task_followed() {
    let script = Script {
        cancel_error: Some((-32603, "cannot stop a build half way".into())),
        ..far()
    };
    let (w, _d, mut dev, server) = task_world(script).await;
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    let row = the_row(&w).await;
    let out = w.state.mcp.cancel_task(tid, row.id, "the owner").await;
    assert_eq!(
        out,
        Err(CancelRefusal::Server("cannot stop a build half way".into()))
    );
    next("the refused tasks/cancel", &mut dev.seen.cancels).await;
    assert_eq!(the_row(&w).await.state, mcp_tasks::OPEN);
    assert!(w.state.mcp.has_open_tasks(server));
    dev.complete("t1", "built anyway", true);
    let row = row_in(&w, "ended").await;
    assert_eq!(
        result_text(&row),
        "job t1 (desktop__build) completed\nbuilt anyway"
    );
}

/// A server that does not declare `tasks.cancel` is sent none: refused,
/// saying so, and still followed.
#[tokio::test]
async fn a_server_without_tasks_cancel_is_sent_none() {
    let script = Script {
        cancel_cap: false,
        ..far()
    };
    let (w, _d, mut dev, server) = task_world(script).await;
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    let row = the_row(&w).await;
    let Err(CancelRefusal::Unsupported(why)) =
        w.state.mcp.cancel_task(tid, row.id, "the owner").await
    else {
        panic!("refused as unsupported")
    };
    assert!(why.contains("does not declare `tasks.cancel`"), "{why}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(dev.seen.cancels.try_recv().is_err(), "no tasks/cancel sent");
    assert_eq!(the_row(&w).await.state, mcp_tasks::OPEN);
    assert!(w.state.mcp.has_open_tasks(server));
}

/// The route's check lives in `cancel_task`: a row of another thread, or
/// none at all, is not found; an ended one is refused as ended.
#[tokio::test]
async fn a_row_is_cancelled_only_for_its_own_thread_and_while_open() {
    let (w, _d, dev, server) = task_world(far()).await;
    let tid = thread(&w).await;
    let other = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    let row = the_row(&w).await;
    let mcp = &w.state.mcp;
    assert_eq!(
        mcp.cancel_task(other, row.id, "x").await,
        Err(CancelRefusal::NotFound)
    );
    assert_eq!(
        mcp.cancel_task(tid, row.id + 100, "x").await,
        Err(CancelRefusal::NotFound)
    );
    assert_eq!(
        the_row(&w).await.state,
        mcp_tasks::OPEN,
        "nothing cancelled"
    );
    dev.complete("t1", "done", true);
    row_in(&w, "ended").await;
    assert_eq!(
        mcp.cancel_task(tid, row.id, "x").await,
        Err(CancelRefusal::Ended)
    );
}

/// A row that is open but not followed yet (a route racing the resume):
/// `cancel_task` resumes the followers, then cancels.
#[tokio::test]
async fn a_row_not_followed_yet_is_followed_then_cancelled() {
    let (w, _d, mut dev, server) = task_world(far()).await;
    let tid = thread(&w).await;
    dev.script.lock().unwrap().tasks.insert(
        "t9".into(),
        super::FakeTask {
            status: "working".into(),
            message: None,
            poll_interval: None,
            payload: None,
        },
    );
    let new = mcp_tasks::NewMcpTask {
        server_id: server,
        server_label: "desktop",
        task_id: "t9",
        thread_id: tid,
        tool: "desktop__build",
        call_id: "call_9",
        started_by: None,
        status: "working",
        status_message: None,
        poll_interval_ms: None,
        ttl_ms: None,
    };
    let id = mcp_tasks::insert(&w.state.db, &new).await.unwrap();
    assert!(!w.state.mcp.has_open_tasks(server), "not followed yet");
    let out = w.state.mcp.cancel_task(tid, id, "a phone").await;
    assert_eq!(out, Ok(CancelOutcome::Cancelled));
    let sent = next("the tasks/cancel", &mut dev.seen.cancels).await;
    assert_eq!(sent["params"]["taskId"], "t9");
    assert_eq!(
        row_in(&w, "ended").await.ended_by.as_deref(),
        Some("a phone")
    );
}

/// A thread deleted while its device is offline: its open task owes a
/// cancel, which the resumed follower sends at the next link; the row then
/// goes, since no result waits.
#[tokio::test]
async fn a_deleted_thread_s_owed_cancel_is_sent_at_the_next_link() {
    let (w, d, dev, server) = task_world(far()).await;
    poll_every(&w, 600).await;
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    let script = dev.script.clone();
    dev.vanish();
    patience::until_async("the row is offline", || async {
        !w.state.mcp.is_ready(server).await
    })
    .await;
    let mut conn = w.state.db.acquire().await.unwrap();
    let owing = mcp_tasks::thread_gone(&mut conn, tid).await.unwrap();
    drop(conn);
    assert_eq!(owing.len(), 1);
    w.state.mcp.resume_tasks().await;
    patience::until_async("the follower owes the cancel", || async {
        !w.state.mcp.has_open_tasks(server)
    })
    .await;
    assert_eq!(the_row(&w).await.state, mcp_tasks::CANCEL_OWED);

    let mut again = TaskDevice::link(&w, &d, script).await;
    let sent = next("the owed tasks/cancel", &mut again.seen.cancels).await;
    assert_eq!(sent["params"]["taskId"], "t1");
    patience::until_async("the row goes", || async { rows(&w).await.is_empty() }).await;
}
