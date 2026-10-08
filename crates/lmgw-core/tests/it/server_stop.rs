//! A graceful shutdown with long-lived streams open: the server stops at
//! once, and every stream ends (`server::Stops`). Before, an open Chat
//! change feed kept a headless gateway's Ctrl-C waiting for as long as its
//! client stayed.

use std::sync::Arc;
use std::time::Duration;

use lmgw_core::server::{build_router, serve_app, serve_app_within};
use lmgw_core::state::{AppState, SharedState};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::common::dashboard_key;
use crate::support::realtime_fakes::{Step, Turn};

/// `state` served on a port of its own by `serve_app_within(within)`: its
/// address, the stop, and the server's task.
async fn stoppable(
    state: &SharedState,
    within: Duration,
) -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let app = build_router(state.clone());
    let st = state.clone();
    let server = tokio::spawn(async move {
        serve_app_within(
            &st,
            listener,
            app,
            async {
                let _ = stopped.await;
            },
            within,
        )
        .await
    });
    (addr, stop, server)
}

/// Read `resp` until `what` came; everything read.
async fn read_until(resp: &mut reqwest::Response, what: &str) -> String {
    let mut got = String::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !got.contains(what) {
            match resp.chunk().await {
                Ok(Some(c)) => got.push_str(&String::from_utf8_lossy(&c)),
                _ => break,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {what:?} within 10 s: {got}"));
    got
}

/// Read `resp` to its end: everything read.
async fn read_to_end(resp: &mut reqwest::Response) -> String {
    let mut got = String::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Ok(Some(c)) = resp.chunk().await {
            got.push_str(&String::from_utf8_lossy(&c));
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the stream did not end within 10 s: {got}"));
    got
}

/// Read `resp` until it ends; `false` when it is still open after `secs`.
async fn ends_within(resp: &mut reqwest::Response, secs: u64) -> bool {
    tokio::time::timeout(Duration::from_secs(secs), async {
        while let Ok(Some(_)) = resp.chunk().await {}
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn a_shutdown_with_a_feed_open_ends_it_and_stops_at_once() {
    let state = AppState::init_for_tests().await.unwrap();
    let key = dashboard_key(&state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let app = build_router(state.clone());
    let st = state.clone();
    let server = tokio::spawn(async move {
        serve_app(&st, listener, app, async {
            let _ = stopped.await;
        })
        .await
    });

    let client = reqwest::Client::new();
    let open = |path: &str| {
        client
            .get(format!("{base}{path}"))
            .bearer_auth(&key)
            .header("accept", "text/event-stream")
            .send()
    };
    let mut feed = open("/chat/api/feed").await.unwrap();
    assert_eq!(feed.status(), 200);
    let first = feed.chunk().await.unwrap().unwrap();
    assert!(
        String::from_utf8_lossy(&first).contains("event: hello"),
        "the feed is up"
    );
    let mut events = open("/api/events").await.unwrap();
    assert_eq!(events.status(), 200);
    events.chunk().await.unwrap().unwrap();

    let _ = stop.send(());
    let done = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the server stopped within 5 s with a feed open");
    done.unwrap().unwrap();
    assert!(ends_within(&mut feed, 5).await, "the feed ended");
    assert!(ends_within(&mut events, 5).await, "/api/events ended");

    // A stream opened on the next server (the shell's restart) is not ended
    // by the stop before it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let app = build_router(state.clone());
    let st = state.clone();
    let server = tokio::spawn(async move {
        serve_app(&st, listener, app, async {
            let _ = stopped.await;
        })
        .await
    });
    let mut feed = client
        .get(format!("{base}/chat/api/feed"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    feed.chunk().await.unwrap().unwrap();
    assert!(
        !ends_within(&mut feed, 1).await,
        "a stream of the next server runs on"
    );
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("and stops at its own server's stop")
        .unwrap()
        .unwrap();
}

/// The same with a bound realtime session and a feed open, as a desktop
/// client holds them: the session closes with 1001 and says the gateway is
/// stopping, the feed ends, and the server stops at once.
#[tokio::test]
async fn a_shutdown_closes_a_bound_session_with_going_away() {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::Message;

    let w = crate::realtime_chat_thread::world(|_| {}).await;
    let tid = w.thread("chatty", serde_json::json!({})).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let app = build_router(w.state.clone());
    let st = w.state.clone();
    let server = tokio::spawn(async move {
        serve_app(&st, listener, app, async {
            let _ = stopped.await;
        })
        .await
    });

    let mut req = format!("ws://{addr}/v1/realtime?chat_thread={tid}")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("cookie", w.cookie().parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let created = crate::support::realtime_fakes::next_event(&mut ws).await;
    assert_eq!(created["type"], "session.created", "{created}");
    let mut feed = reqwest::Client::new()
        .get(format!("http://{addr}/chat/api/feed"))
        .bearer_auth(&w.gw.key)
        .send()
        .await
        .unwrap();
    feed.chunk().await.unwrap().unwrap();

    let _ = stop.send(());
    // The server returns only once the session has ended: its close was
    // sent before a process that exits next could reset it.
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the server stopped within 5 s")
        .unwrap()
        .unwrap();
    assert_eq!(
        w.state.stops.ended(u64::MAX, Duration::ZERO).await,
        0,
        "no session still running when the server returned"
    );
    let close = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(c))) => break c,
                Some(Ok(_)) => continue,
                other => panic!("expected a close frame, got {other:?}"),
            }
        }
    })
    .await
    .expect("the session closed within 5 s")
    .expect("with a code and a reason");
    assert_eq!(close.code, CloseCode::Away, "1001: going away");
    assert_eq!(close.reason.as_str(), "lmgw is stopping or restarting");
    assert!(ends_within(&mut feed, 5).await, "the feed ended");
}

/// Review F-2, F-9: a Chat turn cut by a stop. Its stream's last event says
/// why (`error {code: "gateway_stopping"}`), and the server returns only
/// once the turn has saved what it had and written its request row: both
/// are there the moment it has returned, which is what a process that
/// exits next relies on.
#[tokio::test]
async fn a_chat_turn_at_a_stop_says_so_and_is_saved_before_the_server_returns() {
    let w = crate::realtime_chat_thread::world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let held = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Half "),
        Step::Text("a reply"),
        Step::Wait(held.clone()),
        Step::Text(" never sent"),
    ]));
    let (addr, stop, server) = stoppable(&w.state, Duration::from_secs(10)).await;
    let mut turn = reqwest::Client::new()
        .post(format!("http://{addr}/chat/api/threads/{tid}/send"))
        .bearer_auth(&w.gw.key)
        .json(&json!({ "content": "hi" }))
        .send()
        .await
        .unwrap();
    assert_eq!(turn.status(), 200);
    read_until(&mut turn, "a reply").await;
    let rows_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_logs WHERE requested_alias = 'chatty'")
            .fetch_one(&w.state.db)
            .await
            .unwrap();

    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the server returned within its bound")
        .unwrap()
        .unwrap();
    // Read nothing more, wait for nothing: it is all there now.
    let saved: Vec<String> = sqlx::query_scalar(
        "SELECT content FROM chat_messages WHERE thread_id = ?1 AND role = 'assistant'",
    )
    .bind(tid)
    .fetch_all(&w.state.db)
    .await
    .unwrap();
    assert_eq!(saved, ["Half a reply"], "the partial reply was saved");
    let rows_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_logs WHERE requested_alias = 'chatty'")
            .fetch_one(&w.state.db)
            .await
            .unwrap();
    assert_eq!(rows_after, rows_before + 1, "and its row written");

    let rest = read_to_end(&mut turn).await;
    let last = rest
        .split("\n\n")
        .filter(|f| f.contains("event:"))
        .last()
        .unwrap_or_default()
        .to_string();
    assert!(last.contains("event: error"), "{rest}");
    let data: Value = serde_json::from_str(
        last.lines()
            .find_map(|l| l.strip_prefix("data: "))
            .unwrap_or("null"),
    )
    .unwrap();
    assert_eq!(data["code"], "gateway_stopping", "{data}");
    assert_eq!(data["message"], lmgw_core::server::STOPPING);
}

/// Review F-9: `/mcp`'s notification stream ends at a stop, and the server
/// does not wait for it.
#[tokio::test]
async fn the_mcp_notification_stream_ends_at_a_stop() {
    let state = AppState::init_for_tests().await.unwrap();
    let (addr, stop, server) = stoppable(&state, Duration::from_secs(10)).await;
    let client = reqwest::Client::new();
    let init = client
        .post(format!("http://{addr}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                       "clientInfo": {"name": "stop-test", "version": "0"}}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(init.status(), 200);
    let sid = init.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    let mut stream = client
        .get(format!("http://{addr}/mcp"))
        .header("accept", "text/event-stream")
        .header("mcp-session-id", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    // Open and quiet: it ends only by the stop.
    assert!(!ends_within(&mut stream, 1).await, "it stays open");
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the server stopped at once with the stream open")
        .unwrap()
        .unwrap();
    assert!(ends_within(&mut stream, 5).await, "the stream ended");
}

/// Review F-4: a response that ends neither by itself nor at the stop — a
/// `/v1` stream whose upstream stopped answering — does not hold the stop:
/// the server returns once its bound has run out, and the request is the
/// one counted as still open.
#[tokio::test]
async fn a_stalled_stream_does_not_hold_the_stop_past_its_bound() {
    let w = crate::realtime_chat_thread::world(|_| {}).await;
    let held = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Hello"),
        Step::Wait(held.clone()),
    ]));
    let (addr, stop, server) = stoppable(&w.state, Duration::from_millis(800)).await;
    let mut resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .bearer_auth(&w.gw.key)
        .json(&json!({
            "model": "chatty", "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    read_until(&mut resp, "Hello").await;
    let served_at = w.state.stops.now();
    assert_eq!(
        w.state.stops.open_requests(served_at),
        ["POST /v1/chat/completions"],
        "counted in flight, as it came in"
    );
    let started = std::time::Instant::now();
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the server returned once its bound ran out")
        .unwrap()
        .unwrap();
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(700) && took < Duration::from_secs(3),
        "the bound, not the stream, ended the wait: {took:?}"
    );
    held.notify_one();
}

/// Review F-5: the same device's rebind with `takeover=never` after a
/// restart — the first server's stop closed its session (1001) — binds on
/// the next server.
#[tokio::test]
async fn a_rebind_that_never_takes_over_after_a_restart_binds() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let w = crate::realtime_chat_thread::world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let connect = |addr: std::net::SocketAddr, query: String| {
        let mut req = format!("ws://{addr}/v1/realtime?{query}")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("cookie", w.cookie().parse().unwrap());
        tokio_tungstenite::connect_async(req)
    };
    let (first, stop, server) = stoppable(&w.state, Duration::from_secs(10)).await;
    let (mut ws, _) = connect(first, format!("chat_thread={tid}")).await.unwrap();
    let created = crate::support::realtime_fakes::next_event(&mut ws).await;
    assert_eq!(created["type"], "session.created", "{created}");
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the first server stopped")
        .unwrap()
        .unwrap();
    let (next, _stop, _server) = stoppable(&w.state, Duration::from_secs(10)).await;
    let (mut again, _) = connect(next, format!("chat_thread={tid}&takeover=never"))
        .await
        .expect("the rebind is not refused");
    let created = crate::support::realtime_fakes::next_event(&mut again).await;
    assert_eq!(created["type"], "session.created", "{created}");
}
