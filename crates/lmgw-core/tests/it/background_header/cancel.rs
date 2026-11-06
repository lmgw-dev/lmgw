//! Never preempted, cancelled by closing the request (per-request background
//! design §9, the owner's decision B).
//!
//! The guest's model runs on a container this test serves itself
//! ([`Gpu::next_start_on`]): an axum llama-server that streams a token every
//! 20 ms for as long as its client reads, and counts the answers whose
//! connection was closed under it — what wiremock, which answers with one
//! whole body, cannot tell.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use lmgw_core::runtime::registry::Origin;
use serde_json::{json, Value};

use super::*;
use crate::support::gpu_world::GIB;

/// What the streaming container saw.
#[derive(Default)]
struct Streaming {
    /// Answers streaming now.
    live: AtomicUsize,
    /// Tokens sent, over all answers.
    sent: AtomicUsize,
    /// Answers whose client went away before they ended.
    closed: AtomicUsize,
}

/// Serve the streaming llama-server on a port of its own.
async fn streaming_container() -> (u16, Arc<Streaming>) {
    let seen = Arc::new(Streaming::default());
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/slots", get(slots))
        .route("/v1/chat/completions", post(stream))
        .with_state(Arc::clone(&seen));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (port, seen)
}

/// One slot, processing while an answer streams.
async fn slots(State(seen): State<Arc<Streaming>>) -> Json<Value> {
    let busy = seen.live.load(Ordering::SeqCst) > 0;
    Json(json!([{"id": 0, "id_task": 0, "is_processing": busy}]))
}

/// A token every 20 ms until the body is dropped — the connection closed.
async fn stream(State(seen): State<Arc<Streaming>>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    seen.live.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(async move {
        let token = Bytes::from_static(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\n",
        );
        loop {
            tokio::select! {
                () = tx.closed() => break,
                () = tokio::time::sleep(Duration::from_millis(20)) => {
                    if tx.send(Ok(token.clone())).await.is_err() {
                        break;
                    }
                    seen.sent.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        seen.closed.fetch_add(1, Ordering::SeqCst);
        seen.live.fetch_sub(1, Ordering::SeqCst);
    });
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap()
}

/// A guest streams on a model it started. A foreground request that needs
/// that model's memory waits for it — draining it — and the guest's stream
/// keeps flowing meanwhile: foreground work never interrupts it. The client
/// then closes its request: the model sees its connection close, the guest's
/// claim goes with it, and the waiting request is served at once, the
/// guest's now idle model evicted for it.
#[tokio::test]
async fn foreground_work_waits_for_a_guest_and_a_closed_request_frees_its_claim() {
    let (g, gw) = world_with_queue(24 * GIB, 2, 30).await;
    g.model("bg", 16 * GIB).await;
    g.model("fg", 16 * GIB).await;
    let (port, model) = streaming_container().await;
    g.next_start_on(port);

    let resp = gw
        .client()
        .post(format!("{gw}/v1/chat/completions"))
        .header(BACKGROUND, "on")
        .json(&json!({"model": "bg", "stream": true,
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    let mut body = resp.bytes_stream();
    let first = body.next().await.expect("a first token").unwrap();
    assert!(!first.is_empty());
    assert_eq!(view(&g, "bg").unwrap().owner, Origin::Background);

    let waiting = tokio::spawn({
        let gw = gw.clone();
        async move { chat(&gw, "fg", None).await }
    });
    let runtime = g.state.runtime();
    until("the foreground request waits for room", || {
        runtime.draining_for_owner()
    })
    .await;
    let before = model.sent.load(Ordering::SeqCst);
    for _ in 0..3 {
        body.next().await.expect("the stream goes on").unwrap();
    }
    assert!(model.sent.load(Ordering::SeqCst) > before);
    assert_eq!(model.closed.load(Ordering::SeqCst), 0, "never preempted");
    assert!(!waiting.is_finished(), "the foreground request waits");

    drop(body);
    until("the model sees its client go", || {
        model.closed.load(Ordering::SeqCst) == 1
    })
    .await;
    let served = tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .expect("served once the guest's claim went, not at the queue timeout")
        .unwrap();
    answered_locally(served, "fg").await;
    assert_eq!(g.stops(), ["bg"], "the guest's idle model made room");
    assert_eq!(model.live.load(Ordering::SeqCst), 0);
}

/// The same for a request that does not stream: the guest's answer is still
/// on its way when the client gives up, and the foreground request waiting
/// for that model's room is served at once.
#[tokio::test]
async fn a_closed_unary_request_frees_its_claim() {
    let (g, gw) = world_with_queue(24 * GIB, 3, 30).await;
    g.model("bg", 16 * GIB).await;
    g.model("fg", 16 * GIB).await;
    // Only the guest's answer is slow: the delay is read as it arrives.
    g.world().chat_delay = Duration::from_secs(60);
    let guest = tokio::spawn({
        let gw = gw.clone();
        async move { guest_chat(&gw, "bg").await }
    });
    until("the guest's answer is on its way", || {
        view(&g, "bg").is_some_and(|v| v.in_flight == 1) && !g.world().chats.is_empty()
    })
    .await;
    g.world().chat_delay = Duration::ZERO;
    assert_eq!(view(&g, "bg").unwrap().owner, Origin::Background);

    let waiting = tokio::spawn({
        let gw = gw.clone();
        async move { chat(&gw, "fg", None).await }
    });
    let runtime = g.state.runtime();
    until("the foreground request waits for room", || {
        runtime.draining_for_owner()
    })
    .await;
    assert!(!waiting.is_finished(), "never preempted: it waits");

    guest.abort();
    let served = tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .expect("served once the guest's claim went, not at the queue timeout")
        .unwrap();
    answered_locally(served, "fg").await;
    assert_eq!(g.stops(), ["bg"]);
}
