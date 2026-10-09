//! A server that answers a new task with the id of one lmgw still follows
//! (WP1's decision on reused ids): the older row ends `abandoned` saying
//! so, the new task is followed under a row of its own, and nothing is
//! cancelled for it — on the late path and the bridge alike.

use std::time::Duration;

use super::{
    call, executor, next, result_text, rows, task_world, text, thread, Script, TaskDevice,
};
use crate::common::patience;

fn far() -> Script {
    Script {
        poll_interval: Some(600_000),
        ..Script::default()
    }
}

/// The device's next task gets the id `t1` again.
fn reuse_ids(dev: &TaskDevice) {
    dev.script.lock().unwrap().next = 0;
}

const REUSED: &str = "job t1 (desktop__build) abandoned\nserver 'device:desktop' reused its task \
                      id for a new job: this one was abandoned; it may or may not have finished";

/// Two late calls answered with one id: the first row ends abandoned, the
/// second is followed to its own result.
#[tokio::test]
async fn a_reused_id_ends_the_older_row_and_follows_the_new_task() {
    let (w, _d, mut dev, server) = task_world(far()).await;
    let exec = executor(&w, server, Some(thread(&w).await));
    call(&exec, "desktop__build").await;
    reuse_ids(&dev);
    let second = call(&exec, "desktop__build").await;
    assert_eq!(text(&second), "started, job t1");
    let all = rows(&w).await;
    assert_eq!(all.len(), 2, "{all:?}");
    assert_eq!(
        (all[0].state.as_str(), all[0].status.as_str()),
        ("ended", "abandoned")
    );
    assert_eq!(result_text(&all[0]), REUSED);
    assert_eq!(all[1].state, "open");
    assert!(w.state.mcp.has_open_tasks(server));

    dev.complete("t1", "the second build", true);
    patience::until_async("the new row ends", || async {
        rows(&w).await[1].state == "ended"
    })
    .await;
    let all = rows(&w).await;
    assert_eq!(
        result_text(&all[1]),
        "job t1 (desktop__build) completed\nthe second build"
    );
    assert_eq!(result_text(&all[0]), REUSED, "the older row is not touched");
    assert!(dev.seen.cancels.try_recv().is_err(), "nothing cancelled");
}

/// A bridged task whose id an open row holds: the row ends abandoned, the
/// bridged caller gets its result.
#[tokio::test]
async fn a_bridged_task_reusing_an_open_row_s_id() {
    let (w, _d, mut dev, server) = task_world(far()).await;
    call(
        &executor(&w, server, Some(thread(&w).await)),
        "desktop__build",
    )
    .await;
    reuse_ids(&dev);
    let exec = executor(&w, server, None);
    let running = tokio::spawn(async move { call(&exec, "desktop__build").await });
    next("the late call", &mut dev.seen.calls).await;
    next("the bridged call", &mut dev.seen.calls).await;
    patience::until_async("the row ends", || async {
        rows(&w).await[0].state == "ended"
    })
    .await;
    assert_eq!(result_text(&rows(&w).await[0]), REUSED);
    dev.complete("t1", "inline", true);
    let out = running.await.unwrap();
    assert_eq!((text(&out).as_str(), out.is_error), ("inline", false));
    assert!(dev.seen.cancels.try_recv().is_err(), "nothing cancelled");
}

/// A late task whose id a bridged wait follows: the wait ends abandoned
/// (no `tasks/cancel`, which would cancel the new task), the new row is
/// followed.
#[tokio::test]
async fn a_late_task_reusing_a_bridged_task_s_id_ends_the_wait() {
    let (w, _d, mut dev, server) = task_world(far()).await;
    let exec = executor(&w, server, None);
    let running = tokio::spawn(async move { call(&exec, "desktop__build").await });
    next("the bridged call", &mut dev.seen.calls).await;
    reuse_ids(&dev);
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    let out = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("the wait ends")
        .unwrap();
    assert!(out.is_error);
    assert!(
        text(&out).contains("reused its task id for a new job"),
        "{}",
        text(&out)
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(dev.seen.cancels.try_recv().is_err(), "nothing cancelled");
    assert_eq!(rows(&w).await[0].state, "open");
    dev.complete("t1", "the late one", true);
    patience::until_async("the row ends", || async {
        rows(&w).await[0].state == "ended"
    })
    .await;
}
