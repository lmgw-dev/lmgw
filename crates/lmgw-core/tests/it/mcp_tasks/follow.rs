//! Following a task (§1.3, §1.4, T8, T19, T20).

use std::time::{Duration, Instant};

use serde_json::json;

use super::{call, executor, next, poll_every, result_text, row_in, task_world, thread, Script};
use crate::common::patience;

/// A status notification ends the task at once, with the polls far apart:
/// the row is `ended` with §1.4's result, and the server is reapable again.
#[tokio::test]
async fn a_status_notification_ends_the_task_at_once() {
    let script = Script {
        poll_interval: Some(600_000),
        ..Script::default()
    };
    let (w, _d, dev, server) = task_world(script).await;
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    dev.complete("t1", "42 files built", true);
    let row = row_in(&w, "ended").await;
    assert_eq!(row.status, "completed");
    assert_eq!(
        result_text(&row),
        "job t1 (desktop__build) completed\n42 files built"
    );
    assert!(!w.state.mcp.has_open_tasks(server));
}

/// Without notifications, `tasks/get` runs at the task's own
/// `pollInterval`; without one, at `mcp.task_poll_interval_s`.
#[tokio::test]
async fn polls_run_at_the_task_s_interval_then_at_the_setting() {
    let script = Script {
        poll_interval: Some(100),
        ..Script::default()
    };
    let (w, _d, mut dev, server) = task_world(script).await;
    poll_every(&w, 600).await;
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    for _ in 0..3 {
        let got = next("a tasks/get at the task's interval", &mut dev.seen.gets).await;
        assert_eq!(got["params"]["taskId"], "t1");
    }
    dev.complete("t1", "done", false);
    assert_eq!(row_in(&w, "ended").await.status, "completed");

    // No pollInterval: the setting's.
    dev.script.lock().unwrap().poll_interval = None;
    poll_every(&w, 1).await;
    while dev.seen.gets.try_recv().is_ok() {}
    let tid = thread(&w).await;
    let started = Instant::now();
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    next("a tasks/get at the setting", &mut dev.seen.gets).await;
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(900),
        "the first poll came after {waited:?}, before the setting's 1 s"
    );
}

/// A `pollInterval` of 0 reads as not given: polls at the setting, not
/// back to back.
#[tokio::test]
async fn a_zero_poll_interval_polls_at_the_setting() {
    let script = Script {
        poll_interval: Some(0),
        ..Script::default()
    };
    let (w, _d, mut dev, server) = task_world(script).await;
    poll_every(&w, 1).await;
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    assert_eq!(super::the_row(&w).await.poll_interval_ms, None);
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let mut polls = 0;
    while dev.seen.gets.try_recv().is_ok() {
        polls += 1;
    }
    assert!(
        (1..=2).contains(&polls),
        "{polls} tasks/get in 1.6 s at a 1 s setting"
    );
}

/// `input_required` holds one `tasks/result` open, polling beside it; its
/// answer is the result.
#[tokio::test]
async fn input_required_holds_one_tasks_result() {
    let script = Script {
        poll_interval: Some(600_000),
        ..Script::default()
    };
    let (w, _d, mut dev, server) = task_world(script).await;
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    dev.set("t1", "input_required", Some("which branch?"), None, true);
    next("the held tasks/result", &mut dev.seen.results).await;
    patience::until_async("the status is stored", || async {
        super::the_row(&w).await.status == "input_required"
    })
    .await;
    assert_eq!(
        super::the_row(&w).await.status_message.as_deref(),
        Some("which branch?")
    );
    // Ended without a notification: the held request answers it.
    dev.complete("t1", "on main", false);
    let row = row_in(&w, "ended").await;
    assert_eq!(
        result_text(&row),
        "job t1 (desktop__build) completed\non main"
    );
    assert!(
        dev.seen.results.try_recv().is_err(),
        "one tasks/result only"
    );
}

/// Every row of §1.4's table: completed, failed with a result, failed with
/// a JSON-RPC error, cancelled on the server, `-32602`, the server row
/// removed.
#[tokio::test]
async fn every_ending_of_the_table() {
    let script = Script {
        poll_interval: Some(600_000),
        ..Script::default()
    };
    let (w, _d, dev, server) = task_world(script).await;
    let exec = executor(&w, server, Some(thread(&w).await));
    let ended = |n: usize| {
        let w = &w;
        async move {
            patience::until_async(&format!("{n} tasks ended"), || async {
                lmgw_core::store::mcp_tasks::all(&w.state.db)
                    .await
                    .unwrap()
                    .iter()
                    .filter(|r| r.state == "ended")
                    .count()
                    == n
            })
            .await;
            lmgw_core::store::mcp_tasks::all(&w.state.db)
                .await
                .unwrap()
                .remove(n - 1)
        }
    };

    call(&exec, "desktop__build").await;
    let failed = json!({"content": [{"type": "text", "text": "tests failed"}], "isError": true});
    dev.set("t1", "failed", None, Some(Ok(failed)), true);
    let row = ended(1).await;
    assert_eq!(row.status, "failed");
    assert_eq!(
        result_text(&row),
        "job t1 (desktop__build) failed\ntests failed"
    );

    call(&exec, "desktop__build").await;
    let err = Err((-32603, "disk full".to_string()));
    dev.set("t2", "failed", Some("at step 3"), Some(err), true);
    let row = ended(2).await;
    assert_eq!(
        (row.status.as_str(), result_text(&row).as_str()),
        (
            "failed",
            "job t2 (desktop__build) failed\ndisk full\nat step 3"
        )
    );

    call(&exec, "desktop__build").await;
    dev.set("t3", "cancelled", None, None, true);
    let row = ended(3).await;
    assert_eq!(
        result_text(&row),
        "job t3 (desktop__build) cancelled\ncancelled on the server"
    );

    // The device forgot it: the next poll's -32602.
    dev.script.lock().unwrap().poll_interval = Some(100);
    call(&exec, "desktop__build").await;
    dev.script.lock().unwrap().tasks.remove("t4");
    let row = ended(4).await;
    assert_eq!(row.status, "abandoned");
    assert_eq!(
        result_text(&row),
        "job t4 (desktop__build) abandoned\nserver 'device:desktop' no longer knows the job: \
         it was abandoned; it may or may not have finished"
    );

    // The server row removed: `server_gone` in the removal's transaction.
    dev.script.lock().unwrap().poll_interval = Some(600_000);
    call(&exec, "desktop__build").await;
    let mut conn = w.state.db.acquire().await.unwrap();
    lmgw_core::store::mcp_tasks::server_gone(&mut conn, server, |r| {
        lmgw_core::mcp::tasks::removed_result(
            &r.task_id,
            &r.tool,
            "device:desktop",
            "its key was deleted",
        )
    })
    .await
    .unwrap();
    drop(conn);
    let row = ended(5).await;
    assert_eq!(row.status, "abandoned");
    assert_eq!(
        result_text(&row),
        "job t5 (desktop__build) abandoned\nserver 'device:desktop' was removed (its key was \
         deleted): the job was abandoned; it may or may not have finished"
    );
}

/// The end writes the task's second request row (T20), under the starting
/// principal, with its status.
#[tokio::test]
async fn the_end_writes_its_request_row() {
    let (w, _d, dev, server) = task_world(Script::default()).await;
    let tid = thread(&w).await;
    call(&executor(&w, server, Some(tid)), "desktop__build").await;
    let failed = json!({"content": [{"type": "text", "text": "no"}], "isError": true});
    dev.set("t1", "failed", None, Some(Ok(failed)), true);
    row_in(&w, "ended").await;
    patience::until_async("two request rows", || async { rows(&w).await.len() == 2 }).await;
    let rows = rows(&w).await;
    assert_eq!(
        rows[0],
        ("chat-tool".into(), "desktop__build".into(), 200, None)
    );
    assert_eq!(
        (
            rows[1].0.as_str(),
            rows[1].1.as_str(),
            rows[1].2,
            rows[1].3.as_deref()
        ),
        ("chat-tool", "desktop__build", 502, Some("tool_error"))
    );
}

async fn rows(
    w: &crate::realtime_chat_thread::World,
) -> Vec<(String, String, i64, Option<String>)> {
    sqlx::query_as(
        "SELECT ingress_proto, requested_alias, status, error_kind FROM request_logs \
         WHERE mcp_tool IS NOT NULL ORDER BY id",
    )
    .fetch_all(&w.state.db)
    .await
    .unwrap()
}
