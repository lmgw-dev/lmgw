//! The bridge (§1.7, §1.8, T4, T18): a caller with no stored thread gets
//! the task's result inline, within the row's `timeout_ms`.

use std::time::Duration;

use serde_json::{json, Value};

use super::{call, executor, next, task_tool, task_world, text, Script};
use crate::mcp_host::{mcp_rpc, mcp_session, set_timeout};

/// A run with no thread (`/v1/responses`, an agent run, an unbound
/// realtime session, a temporary thread: an executor without `with_late`)
/// waits for the result; a JSON-RPC error stays an error. Nothing is
/// stored.
#[tokio::test]
async fn a_caller_without_a_thread_gets_the_result_inline() {
    let (w, _d, mut dev, server) = task_world(Script::default()).await;
    let exec = executor(&w, server, None);
    let running = tokio::spawn(async move { call(&exec, "desktop__build").await });
    let sent = next("the bridged call", &mut dev.seen.calls).await;
    assert_eq!(sent["params"]["_meta"]["lmgw/task"]["delivery"], "wait");
    dev.complete("t1", "inline", true);
    let out = running.await.unwrap();
    assert_eq!((text(&out).as_str(), out.is_error), ("inline", false));

    let exec = executor(&w, server, None);
    let running = tokio::spawn(async move { call(&exec, "desktop__build").await });
    next("the second bridged call", &mut dev.seen.calls).await;
    dev.set(
        "t2",
        "failed",
        None,
        Some(Err((-32000, "no disk".into()))),
        true,
    );
    let out = running.await.unwrap();
    assert!(out.is_error);
    assert!(text(&out).contains("no disk"), "{}", text(&out));
    assert!(lmgw_core::store::mcp_tasks::all(&w.state.db)
        .await
        .unwrap()
        .is_empty());
}

/// At the row's `timeout_ms` lmgw sends `tasks/cancel` and reports the
/// timeout naming the setting; a caller that stops waiting cancels too.
#[tokio::test]
async fn a_timeout_and_a_caller_gone_send_tasks_cancel() {
    let (w, d, mut dev, server) = task_world(Script::default()).await;
    set_timeout(&w, d.id, 800).await;
    let out = call(&executor(&w, server, None), "desktop__build").await;
    assert!(out.is_error);
    assert!(
        text(&out).contains("timed out after 800ms (its timeout_ms)"),
        "{}",
        text(&out)
    );
    let cancel = next("the timeout's tasks/cancel", &mut dev.seen.cancels).await;
    assert_eq!(cancel["params"]["taskId"], "t1");

    set_timeout(&w, d.id, 60_000).await;
    let exec = executor(&w, server, None);
    let running = tokio::spawn(async move { call(&exec, "desktop__build").await });
    next("the second bridged call", &mut dev.seen.calls).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    running.abort();
    let cancel = next("the dropped caller's tasks/cancel", &mut dev.seen.cancels).await;
    assert_eq!(cancel["params"]["taskId"], "t2");
    // `notifications/cancelled` is never a task's cancel.
    while let Ok(n) = dev.seen.notes.try_recv() {
        assert_ne!(n["method"], "notifications/cancelled", "{n}");
    }
}

/// `/mcp` lists every tool without `execution`, and bridges a `required`
/// tool's call.
#[tokio::test]
async fn mcp_lists_no_execution_and_bridges_the_call() {
    let script = Script {
        tools: vec![
            task_tool("build", Some("required")),
            task_tool("opt", Some("optional")),
        ],
        ..Script::default()
    };
    let (w, _d, mut dev, _) = task_world(script).await;
    let client = w.gw.client();
    let sid = mcp_session(&w, &client).await;
    let v = mcp_rpc(&w, &client, &sid, "tools/list", json!({})).await;
    let tools: Vec<&Value> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| {
            t["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("desktop__"))
        })
        .collect();
    assert_eq!(tools.len(), 2, "{v}");
    assert!(tools.iter().all(|t| t.get("execution").is_none()), "{v}");

    let gw = w.gw.to_string();
    let answer = tokio::spawn(async move {
        client
            .post(format!("{gw}/mcp"))
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", sid)
            .json(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                          "params": {"name": "desktop__build", "arguments": {}}}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    });
    next("the bridged call", &mut dev.seen.calls).await;
    dev.complete("t1", "from /mcp", false);
    let v: Value = serde_json::from_str(&answer.await.unwrap()).unwrap();
    assert_eq!(v["result"]["content"][0]["text"], "from /mcp", "{v}");
}
