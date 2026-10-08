//! A level's publish while the MCP reconcile that follows it hangs (the
//! branch review's N-1). An autostart MCP server that takes a connection
//! and never answers holds the reconcile for as long as its connect waits.
//! What a level's move does — a device's feed hears its threads go, its
//! session on such a thread closes, a feed opened in between opens at the
//! new level — follows the publish, not the reconcile. And a feed opened
//! while the snapshot is behind sends its first bytes at once.

use std::time::Duration;

use lmgw_core::config::{DeviceAdmin, McpTransport};
use lmgw_core::store::NewMcpServer;
use serde_json::json;

use super::super::{pair, self_admin_thread};
use super::{bind, ended, out_of_reach};
use crate::chat_feed::{Feed, Frame};
use crate::realtime_chat_thread::World;

/// A world whose feeds keep alive once a minute: what a feed hears within
/// the tests' few seconds came from a wake, never from a keep-alive tick.
async fn world() -> World {
    crate::realtime_chat_thread::world(|s| s.chat_feed_keepalive_s = 60).await
}

/// An MCP server that takes a connection and never answers, registered to
/// start with the gateway and not yet reconciled: the next reload's
/// reconcile waits on its connect, for the server's `timeout_ms`. Last in a
/// test's setup, since every reload from here waits on it. Its id.
async fn silent_mcp(w: &World, timeout_ms: u64) -> i64 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((conn, _)) = listener.accept().await {
            held.push(conn);
        }
    });
    lmgw_core::store::insert_mcp_server(
        &w.state.db,
        &NewMcpServer {
            name: "silent".into(),
            enabled: true,
            transport: McpTransport::Http,
            command: None,
            args: vec![],
            env: vec![],
            cwd: None,
            container_image: None,
            extra_run_args: vec![],
            url: Some(url),
            headers: vec![],
            tool_prefix: "silent".into(),
            timeout_ms,
            autostart: true,
            idle_seconds: 0,
            allow_sampling: false,
            sampling_alias: None,
            agent_id: None,
        },
    )
    .await
    .unwrap()
}

/// A `timeout_ms` longer than any test here runs: the reconcile still
/// waits on the silent server when the test ends.
const HANGS: u64 = 120_000;

fn says_gone(frames: &[Frame], tid: i64) -> bool {
    frames
        .iter()
        .any(|f| f.event == "thread.deleted" && f.data["thread_id"] == tid)
}

/// The owner lowers a device's level while the reconcile after the publish
/// hangs: the device's feed hears the toolset's thread go, and its session
/// there closes with the 4004, while the level's request still waits on the
/// reconcile. They used to wait for it.
#[tokio::test]
async fn a_level_drop_plays_at_its_publish_while_the_mcp_reconcile_hangs() {
    let w = world().await;
    let d = pair(&w, "phone", json!({ "self_admin": "read_only" })).await;
    let tools = self_admin_thread(&w).await;
    let mut feed = Feed::open(&w, &d.client, "", None).await;
    let (mut ws, _links) = bind(&w, &d, tools.id).await;
    silent_mcp(&w, HANGS).await;

    let req =
        w.gw.client()
            .post(format!("{}/api/op/key_set", w.gw))
            .json(&json!({ "id": d.id, "self_admin": "off" }));
    let level = tokio::spawn(async move { req.send().await.map(|r| r.status().as_u16()) });
    feed.until(10, |f| says_gone(f, tools.id)).await;
    let end = ended(&mut ws, Duration::from_secs(10))
        .await
        .expect("the session closes at the publish");
    assert_eq!((end.code, end.reason.clone()), out_of_reach(tools.id));
    assert!(
        !level.is_finished(),
        "the reconcile still hangs: what was heard followed the publish"
    );
    level.abort();
    let _ = level.await;
}

/// A device's feed opened between its level's commit and the publish of
/// the snapshot that says it: its response starts at once with a keep-alive
/// comment, and `hello` follows the publish, at the new level, while the
/// reconcile after it still hangs.
#[tokio::test]
async fn a_feed_opened_before_the_publish_starts_at_once_and_opens_at_the_new_level() {
    let w = world().await;
    let d = pair(&w, "phone", json!({ "self_admin": "read_only" })).await;
    silent_mcp(&w, HANGS).await;
    // The level's write as `key_set` commits it, and not yet its publish.
    let key = w
        .state
        .snapshot()
        .api_keys
        .iter()
        .find(|k| k.id == d.id)
        .cloned()
        .unwrap();
    lmgw_core::store::update_key_policy(
        &w.state.db,
        d.id,
        &key.policy,
        key.enabled,
        &key.note,
        key.hosts_label.as_deref(),
        (DeviceAdmin::Off, Some(key.self_admin)),
    )
    .await
    .unwrap();

    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        d.client.get(format!("{}/chat/api/feed", w.gw)).send(),
    )
    .await
    .expect("the response starts at once")
    .unwrap();
    assert_eq!(resp.status(), 200);
    let mut feed = Feed::reading(resp);
    feed.until(5, |f| !f.is_empty()).await;
    assert_eq!(
        feed.frames[0].event, ":",
        "a keep-alive comment first, while the snapshot is behind: {:#?}",
        feed.frames
    );

    let state = w.state.clone();
    let reload = tokio::spawn(async move { state.reload_snapshot().await.map(|_| ()) });
    feed.until(10, |f| f.iter().any(|f| f.event == "hello"))
        .await;
    let hello = feed.named("hello")[0].data.clone();
    assert_eq!(hello["self_admin"], "off", "{hello}");
    assert!(
        !reload.is_finished(),
        "the reconcile still hangs: hello followed the publish"
    );
    reload.abort();
    let _ = reload.await;
}

/// The last review's follow-up: the request whose reload reconciles MCP is
/// dropped by its client while the reconcile waits on a server that never
/// answers. The reconcile runs to its end on a task of its own, and the
/// server's handshake fails at its `timeout_ms`: the server ends `error`,
/// never `connecting` for good. A dropped request used to cut the connect,
/// and every later start bailed on the claim it left until a restart.
#[tokio::test]
async fn a_reconcile_whose_request_is_dropped_still_settles_its_servers() {
    let w = world().await;
    let d = pair(&w, "phone", json!({ "self_admin": "read_only" })).await;
    let silent = silent_mcp(&w, 2_000).await;
    let (base, owner, _stop) = super::hangup::counted(&w).await;
    let req = owner
        .post(format!("{base}/api/op/key_set"))
        .json(&json!({ "id": d.id, "self_admin": "off" }));
    let level = tokio::spawn(async move { req.send().await.map(|r| r.status().as_u16()) });
    mcp_until(&w, silent, "the reconcile reaches the connect", |s| {
        s == "connecting"
    })
    .await;
    // The client hangs up, and the server drops its request.
    level.abort();
    let _ = level.await;
    let at = w.state.stops.now();
    tokio::time::timeout(Duration::from_secs(10), async {
        while w
            .state
            .stops
            .open_requests(at)
            .contains(&"POST /api/op/key_set".to_string())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the server drops the request");
    assert_eq!(
        mcp_status(&w, silent).await.map(|(s, _)| s),
        Some("connecting"),
        "dropped while the connect waits"
    );

    mcp_until(&w, silent, "the connect settles", |s| s != "connecting").await;
    let (s, detail) = mcp_status(&w, silent).await.unwrap();
    assert_eq!(s, "error", "{detail:?}");
    assert!(
        detail
            .as_deref()
            .is_some_and(|d| d.contains("no answer to the MCP handshake within 2000 ms")),
        "{detail:?}"
    );
}

/// MCP server `id`'s badge: its status and detail.
async fn mcp_status(w: &World, id: i64) -> Option<(&'static str, Option<String>)> {
    w.state
        .mcp
        .status_view(id, &w.state.snapshot())
        .await
        .map(|v| (v.status, v.detail))
}

/// Wait until MCP server `id`'s status passes `done`; fail after 10 s.
async fn mcp_until(w: &World, id: i64, what: &str, done: impl Fn(&str) -> bool) {
    let reached = tokio::time::timeout(Duration::from_secs(10), async {
        while !mcp_status(w, id).await.is_some_and(|(s, _)| done(s)) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(reached.is_ok(), "{what}: {:?}", mcp_status(w, id).await);
}
