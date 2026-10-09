//! A server row removed while one of its tasks was on its way (the
//! review's L2): the delete's `server_gone` ran before the task's row
//! existed, so nothing would end it, and a follower would wait for a
//! server that is gone, polled forever, its cancel owed forever.
//!
//! - the late path's insert finds the row gone and ends the new task
//!   `abandoned` in the same write, saying the server was removed;
//! - a follower whose server row is gone from the store ends its open row
//!   the same way, and stops.

use lmgw_core::mcp::tasks::removed_result;
use lmgw_core::store::mcp_tasks::{self, NewMcpTask};

use super::{result_text, row_in, rows, task_world, thread, Script};

/// No server row has this id.
const GONE: i64 = 4242;

fn new_task(thread_id: i64) -> NewMcpTask<'static> {
    NewMcpTask {
        server_id: GONE,
        server_label: "desktop",
        task_id: "t7",
        thread_id,
        tool: "desktop__build",
        call_id: "call_1",
        started_by: None,
        status: "working",
        status_message: None,
        poll_interval_ms: None,
        ttl_ms: None,
    }
}

/// The insert of a task whose server row went while its call was
/// answered ends it at once, as the delete would have.
#[tokio::test]
async fn a_task_whose_server_went_before_its_insert_ends_abandoned() {
    let (w, _d, _dev, _server) = task_world(Script::default()).await;
    let tid = thread(&w).await;
    let inserted = mcp_tasks::insert_reusing(
        &w.state.db,
        &new_task(tid),
        |_| unreachable!("no row holds its id"),
        |r| {
            removed_result(
                &r.task_id,
                &r.tool,
                "desktop",
                "while the call was answered",
            )
        },
    )
    .await
    .unwrap();
    assert!(inserted.server_gone);
    assert!(inserted.reused.is_empty());
    let row = row_in(&w, "ended").await;
    assert_eq!(row.id, inserted.id);
    assert_eq!(row.status, "abandoned");
    assert_eq!(
        result_text(&row),
        "job t7 (desktop__build) abandoned\nserver 'desktop' was removed (while the call was \
         answered): the job was abandoned; it may or may not have finished"
    );
}

/// An open row whose server row is gone from the store (written before
/// the late path checked it) is ended by its follower at the first look,
/// and nothing goes on following it.
#[tokio::test]
async fn a_follower_whose_server_row_is_gone_ends_its_task() {
    let (w, _d, _dev, _server) = task_world(Script::default()).await;
    let tid = thread(&w).await;
    mcp_tasks::insert(&w.state.db, &new_task(tid))
        .await
        .unwrap();
    w.state.mcp.resume_tasks().await;
    let row = row_in(&w, "ended").await;
    assert_eq!(row.status, "abandoned");
    assert_eq!(
        result_text(&row),
        "job t7 (desktop__build) abandoned\nserver 'desktop' was removed (while lmgw followed \
         the job): the job was abandoned; it may or may not have finished"
    );
    crate::common::patience::until("its follower stopped", || !w.state.mcp.has_open_tasks(GONE))
        .await;
    assert_eq!(rows(&w).await.len(), 1);
}
