//! A client that goes away ends its model's work at once, whatever the model
//! is producing at the time.
//!
//! The incident of 2026-10-22: a streamed `/v1/chat/completions` with a
//! `json_schema` response format ran away in its thinking on a one-slot
//! llama-server, and its client was killed. lmgw only heard of it at the
//! next frame it failed to write — and the stream wrote the client nothing
//! while the model thought — so the slot generated for fifteen more
//! minutes, every other request queued behind it.
//!
//! Each test serves its model itself: an axum llama-server that answers in
//! one of the ways [`Says`] lists for as long as its client stays, and
//! counts the answers whose connection was closed under it.
//!
//! - [`streamed`]: a streamed chat on a local model, while the model sends
//!   reasoning the client reads, frames that carry nothing for the client,
//!   or nothing at all; `/v1/messages`, and the legacy completions relay.
//! - [`unary`]: a request that does not stream.
//! - [`device`]: a stream over the device listener's TLS.
//! - [`finished`]: a client that goes once it read the answer's last
//!   frame, with the usage still to come — the answer was whole.
//! - [`audio`]: the media relay, while the engine sends nothing.
//! - [`responses`]: the owner's streamed `/v1/responses` run, whose loop
//!   runs in a task of its own.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use lmgw_core::store;
use serde_json::{json, Value};

use crate::common::{self, Gw};
use crate::support::gpu_world::{Gpu, GIB};

mod audio;
mod device;
mod finished;
mod responses;
mod streamed;
mod unary;

/// How long a model may go on after its client went: lmgw's notice is a
/// matter of milliseconds, and a model here never stops by itself, so the
/// bound only tells "stopped" from "never stops" on a loaded box.
const HANG_UP_BOUND: Duration = Duration::from_secs(5);

/// What a streamed answer carries, a frame every 20 ms.
#[derive(Clone, Copy)]
enum Says {
    /// `reasoning_content` deltas: a model thinking, and the client reads it.
    Reasoning,
    /// Chunks with an empty delta: frames that carry nothing the client is
    /// sent.
    Nothing,
    /// The answer's head, then not a byte: a long prefill.
    Silence,
    /// The answer's last frame, then — [`USAGE_AFTER`] later, in a chunk of
    /// its own, as OpenAI sends it — the usage, `[DONE]`, and the end.
    Finishes,
}

/// How long a [`Says::Finishes`] answer holds its usage after its last
/// frame: ample for lmgw to hear a client that went on reading that frame.
const USAGE_AFTER: Duration = Duration::from_millis(200);

/// What the model saw.
struct Model {
    says: Says,
    /// Answers under way (streaming, or not yet answered).
    live: AtomicUsize,
    /// Answers whose client went away before they ended.
    closed: AtomicUsize,
}

/// Counts an answer while it is under way, and as closed once its
/// connection went: dropped by the stream's producer, or by hyper with the
/// handler of a request not yet answered.
struct Answering(Arc<Model>);

impl Answering {
    fn new(model: &Arc<Model>) -> Self {
        model.live.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(model))
    }
}

impl Drop for Answering {
    /// `live` first: [`model_stops`] reads it as soon as `closed` moved.
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::SeqCst);
        self.0.closed.fetch_add(1, Ordering::SeqCst);
    }
}

/// Serve the model on a port of its own.
async fn model(says: Says) -> (u16, Arc<Model>) {
    let seen = Arc::new(Model {
        says,
        live: AtomicUsize::new(0),
        closed: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/slots", get(slots))
        .route("/v1/chat/completions", post(answer))
        .route("/v1/completions", post(answer))
        // audio.cpp's: its health route, and speech it never says.
        .route("/v1/models", get(|| async { Json(json!({"data": []})) }))
        .route("/v1/audio/speech", post(speak))
        .with_state(Arc::clone(&seen));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (port, seen)
}

/// One slot, processing while an answer is under way.
async fn slots(State(seen): State<Arc<Model>>) -> Json<Value> {
    let busy = seen.live.load(Ordering::SeqCst) > 0;
    Json(json!([{"id": 0, "id_task": 0, "is_processing": busy}]))
}

/// A stream that goes on until its body is dropped — the connection closed;
/// a request that does not stream is never answered.
async fn answer(State(seen): State<Arc<Model>>, Json(body): Json<Value>) -> Response {
    let answering = Answering::new(&seen);
    if body["stream"] != json!(true) {
        let _answering = answering;
        return std::future::pending().await;
    }
    let says = seen.says;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    tokio::spawn(async move {
        let _answering = answering;
        let frame: &'static [u8] = match says {
            Says::Reasoning => {
                b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"hm\"}}]}\n\n"
            }
            Says::Nothing => b"data: {\"choices\":[{\"index\":0,\"delta\":{}}]}\n\n",
            Says::Silence => return tx.closed().await,
            Says::Finishes => return finishes(tx).await,
        };
        loop {
            tokio::select! {
                () = tx.closed() => break,
                () = tokio::time::sleep(Duration::from_millis(20)) => {
                    if tx.send(Ok(Bytes::from_static(frame))).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap()
}

/// [`Says::Finishes`]'s answer: the frames are a chat stream's and a legacy
/// completion's alike, which only read `finish_reason` and `usage` here.
async fn finishes(tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>) {
    const LAST: &[u8] =
        b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
    const USAGE: &[u8] = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\
        \"completion_tokens\":2,\"total_tokens\":13}}\n\ndata: [DONE]\n\n";
    if tx.send(Ok(Bytes::from_static(LAST))).await.is_err() {
        return;
    }
    tokio::time::sleep(USAGE_AFTER).await;
    let _ = tx.send(Ok(Bytes::from_static(USAGE))).await;
}

/// Speech the engine is still working on: the answer's head, then not a
/// byte until its connection closes.
async fn speak(State(seen): State<Arc<Model>>) -> Response {
    let answering = Answering::new(&seen);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    tokio::spawn(async move {
        let _answering = answering;
        tx.closed().await;
    });
    Response::builder()
        .header("content-type", "audio/wav")
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap()
}

/// A gateway with the local chat model `m`, served by a model that `says`.
async fn local(says: Says) -> (Gpu, Gw, Arc<Model>) {
    let g = Gpu::new(24 * GIB, 0, 30).await;
    g.model("m", 4 * GIB).await;
    let (port, model) = model(says).await;
    g.next_start_on(port);
    let gw = common::serve(g.state.clone()).await;
    (g, gw, model)
}

/// The model sees its answer's connection close within [`HANG_UP_BOUND`]
/// of the client's going.
async fn model_stops(model: &Model, gone: Instant) {
    while model.closed.load(Ordering::SeqCst) == 0 {
        assert!(
            gone.elapsed() < HANG_UP_BOUND,
            "the model still generates {:?} after its client went",
            gone.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(model.live.load(Ordering::SeqCst), 0);
}

/// `m`'s runtime entry has no request in flight any more: the claim went
/// with the client.
async fn claim_released(g: &Gpu) {
    let runtime = g.state.runtime();
    let started = Instant::now();
    loop {
        let view = runtime.list().into_iter().find(|v| v.model_id == "m");
        if view.as_ref().is_some_and(|v| v.in_flight == 0) {
            return;
        }
        assert!(
            started.elapsed() < HANG_UP_BOUND,
            "the claim stays after its client went: {view:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The newest request-log row for `m` (written once the relay ended).
async fn newest_row(state: &lmgw_core::state::SharedState) -> store::RequestLogRow {
    newest_row_of(state, "m").await
}

/// The newest request-log row for `alias`.
async fn newest_row_of(state: &lmgw_core::state::SharedState, alias: &str) -> store::RequestLogRow {
    let started = Instant::now();
    loop {
        let rows = store::query_logs(
            &state.db,
            &store::LogFilter {
                alias: Some(alias.into()),
                limit: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if let Some(row) = rows.into_iter().next() {
            return row;
        }
        assert!(
            started.elapsed() < HANG_UP_BOUND,
            "no request-log row for '{alias}'"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A chat request's body for `m`, streamed or not, with `extra` merged in.
fn chat_body(stream: bool, extra: Value) -> Value {
    let mut body = json!({
        "model": "m",
        "stream": stream,
        "messages": [{"role": "user", "content": "write the story's outline as JSON"}],
    });
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        b.extend(e.clone());
    }
    body
}

/// The incident's `response_format`.
fn json_schema() -> Value {
    json!({"response_format": {
        "type": "json_schema",
        "json_schema": {"name": "outline", "strict": true, "schema": {
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "required": ["title"],
        }},
    }})
}
