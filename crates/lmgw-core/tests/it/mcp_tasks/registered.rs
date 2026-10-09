//! A registered server's tasks (§1.1, §1.3, §1.7): `call_server`'s task
//! branch on the late path and the bridge, and a follower that reconnects a
//! session gone idle through the lazy connect, as a call would.

use std::collections::HashMap;

use lmgw_core::mcp::exec::McpExecutor;
use lmgw_core::proxy::RequestCtx;

use super::{call, http_server, next, result_text, row_in, text, thread, Script};
use crate::mcp_host::host_world;
use crate::realtime_chat_thread::World;
use crate::support::mcp_stub::register;

/// Register `url` as server `builder` (prefix `builder`): its row id.
async fn builder(w: &World, url: &str) -> i64 {
    register(&w.state, "builder", "builder", url, true, None).await;
    w.state
        .snapshot()
        .mcp_servers
        .values()
        .find(|s| s.name == "builder")
        .expect("the row")
        .id
}

fn exec(w: &World, server: i64, late: Option<i64>) -> McpExecutor {
    let listed: HashMap<String, i64> = [("builder__build".to_string(), server)].into();
    let exec = McpExecutor::new(w.state.clone(), RequestCtx::default())
        .with_proto(lmgw_core::telemetry::CHAT_TOOL_PROTO)
        .with_listed(listed);
    match late {
        Some(tid) => exec.with_late(tid, true),
        None => exec,
    }
}

/// A Chat turn's call of a registered server's `required` tool: answered
/// `started, job t1`, stored, and followed by polling — through a fresh
/// connect after its session was torn down.
#[tokio::test]
async fn a_registered_server_s_task_is_followed_through_the_lazy_connect() {
    let (w, _d) = host_world().await;
    let mut srv = http_server(Script {
        poll_interval: Some(100),
        ..Script::default()
    })
    .await;
    let server = builder(&w, &srv.url).await;
    let tid = thread(&w).await;
    let out = call(&exec(&w, server, Some(tid)), "builder__build").await;
    assert_eq!(text(&out), "started, job t1");
    let sent = next("the augmented call", &mut srv.seen.calls).await;
    assert!(sent["params"].get("task").is_some(), "{sent}");
    next("a poll", &mut srv.seen.gets).await;
    assert_eq!(srv.inits(), 1);

    w.state.mcp.stop_server(server).await;
    srv.complete("t1", "built on the server");
    let row = row_in(&w, "ended").await;
    assert_eq!(
        result_text(&row),
        "job t1 (builder__build) completed\nbuilt on the server"
    );
    assert!(srv.inits() >= 2, "the follower connected again");
    assert!(!w.state.mcp.has_open_tasks(server));
}

/// A caller with no thread: the bridge answers the call with the task's
/// result.
#[tokio::test]
async fn a_registered_server_s_task_is_bridged_without_a_thread() {
    let (w, _d) = host_world().await;
    let mut srv = http_server(Script {
        poll_interval: Some(100),
        ..Script::default()
    })
    .await;
    let server = builder(&w, &srv.url).await;
    let e = exec(&w, server, None);
    let running = tokio::spawn(async move { call(&e, "builder__build").await });
    next("the augmented call", &mut srv.seen.calls).await;
    srv.complete("t1", "inline from the server");
    let out = running.await.unwrap();
    assert_eq!(
        (text(&out).as_str(), out.is_error),
        ("inline from the server", false)
    );
    assert!(lmgw_core::store::mcp_tasks::all(&w.state.db)
        .await
        .unwrap()
        .is_empty());
}
