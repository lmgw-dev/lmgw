//! Fakes and helpers for the realtime suites (realtime design §16): a
//! scripted, **streaming** OpenAI chat upstream, a gateway wired to it, and
//! a `tokio-tungstenite` client's few verbs.
//!
//! An axum app rather than a wiremock server, for the reason
//! `llama_fake.rs` gives: wiremock answers with the whole body at once, and
//! the session's behaviour depends on *when* deltas arrive — a cancel lands
//! mid-stream, a follow-up `response.create` lands between the finish chunk
//! and the end of the stream. Each request takes the next scripted [`Turn`];
//! a turn's steps are sent one chunk at a time, and a [`Step::Wait`] holds
//! the stream until the test releases it.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use lmgw_core::config::{hash_api_key, KeyPolicy, Settings};
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderName;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub const KEY: &str = "lmgw-realtime-test-key";

pub type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// One chunk of a scripted stream.
#[derive(Clone)]
pub enum Step {
    Text(&'static str),
    /// A reasoning delta, as llama-server streams it (`reasoning_content`).
    Reasoning(&'static str),
    /// A tool call's first chunk: its name, and its id when `Some`.
    CallStart {
        index: u32,
        id: Option<&'static str>,
        name: &'static str,
    },
    CallArgs {
        index: u32,
        args: &'static str,
    },
    /// The choice's `finish_reason`.
    Finish(&'static str),
    /// The usage chunk: prompt and completion tokens.
    Usage(u64, u64),
    /// Hold the stream until the test calls `notify_one` on this.
    Wait(Arc<Notify>),
    /// Break the connection mid-body, as a reset upstream does.
    Fail,
}

/// What one chat request is answered with.
#[derive(Clone)]
pub enum Turn {
    Stream(Vec<Step>),
    /// A refusal: this status and body.
    Status(u16, Value),
    /// No answer at all — not even its headers — until the test calls
    /// `notify_one`, then the inner turn: an upstream still prefilling.
    Held(Arc<Notify>, Box<Turn>),
}

impl Turn {
    /// `text` in one chunk per word, then `stop` and usage 12/8 — the
    /// captured stub's numbers.
    pub fn text(words: &[&'static str]) -> Self {
        Self::reasoned(&[], words)
    }

    /// [`Self::text`] after `reasoning`, one chunk per piece, as a model
    /// that reasons first streams it.
    pub fn reasoned(reasoning: &[&'static str], words: &[&'static str]) -> Self {
        let mut steps: Vec<Step> = reasoning.iter().map(|r| Step::Reasoning(r)).collect();
        steps.extend(words.iter().map(|w| Step::Text(w)));
        steps.push(Step::Finish("stop"));
        steps.push(Step::Usage(12, 8));
        Turn::Stream(steps)
    }
}

#[derive(Default)]
pub struct Seen {
    /// Every `/chat/completions` body, in arrival order.
    pub chats: Mutex<Vec<Value>>,
    /// Streams the gateway stopped reading before their end.
    pub closed_early: AtomicUsize,
    /// Streams that started.
    pub started: AtomicUsize,
    /// Notified (one stored permit) whenever a stream ends early.
    pub closed: Arc<Notify>,
}

impl Seen {
    pub fn chat(&self, n: usize) -> Value {
        self.chats.lock().unwrap()[n].clone()
    }

    pub fn chat_count(&self) -> usize {
        self.chats.lock().unwrap().len()
    }
}

struct Fake {
    script: Mutex<VecDeque<Turn>>,
    seen: Arc<Seen>,
}

pub struct ChatFake {
    pub url: String,
    pub seen: Arc<Seen>,
    fake: Arc<Fake>,
}

impl ChatFake {
    /// Queue the answer to the next request.
    pub fn push(&self, turn: Turn) {
        self.fake.script.lock().unwrap().push_back(turn);
    }
}

pub async fn chat_fake() -> ChatFake {
    let seen = Arc::new(Seen::default());
    let fake = Arc::new(Fake {
        script: Mutex::new(VecDeque::new()),
        seen: seen.clone(),
    });
    let app = Router::new()
        .route("/chat/completions", post(chat))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    ChatFake {
        url: format!("http://{addr}"),
        seen,
        fake,
    }
}

async fn chat(State(f): State<Arc<Fake>>, Json(body): Json<Value>) -> Response {
    f.seen.chats.lock().unwrap().push(body);
    let mut turn = f
        .script
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| Turn::text(&["ok"]));
    if let Turn::Held(release, inner) = turn {
        release.notified().await;
        turn = *inner;
    }
    let steps = match turn {
        Turn::Status(status, body) => {
            return (StatusCode::from_u16(status).unwrap(), Json(body)).into_response()
        }
        Turn::Stream(steps) => steps,
        Turn::Held(..) => unreachable!("released above"),
    };
    f.seen.started.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    let seen = f.seen.clone();
    tokio::spawn(async move {
        for step in steps {
            let chunk = match step {
                Step::Wait(n) => {
                    // Wait for the release — or for the gateway to hang up,
                    // which is what a cancel does.
                    let released = tokio::select! {
                        _ = n.notified() => true,
                        _ = tx.closed() => false,
                    };
                    if released {
                        continue;
                    }
                    break;
                }
                Step::Fail => {
                    let _ = tx
                        .send(Err(std::io::Error::other("the fake upstream fell over")))
                        .await;
                    return;
                }
                other => chunk(&other),
            };
            if tx
                .send(Ok(Bytes::from(format!("data: {chunk}\n\n"))))
                .await
                .is_err()
            {
                break;
            }
        }
        if tx.is_closed() {
            seen.closed_early.fetch_add(1, Ordering::SeqCst);
            seen.closed.notify_one();
            return;
        }
        let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
    });
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
    )
        .into_response()
}

fn chunk(step: &Step) -> Value {
    let choice = |delta: Value, finish: Option<&str>| {
        json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk", "model": "m",
               "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    match step {
        Step::Text(t) => choice(json!({"content": t}), None),
        Step::Reasoning(t) => choice(json!({"reasoning_content": t}), None),
        Step::CallStart { index, id, name } => {
            let mut call = json!({"index": index, "type": "function",
                                  "function": {"name": name, "arguments": ""}});
            if let Some(id) = id {
                call["id"] = json!(id);
            }
            choice(json!({"tool_calls": [call]}), None)
        }
        Step::CallArgs { index, args } => choice(
            json!({"tool_calls": [{"index": index, "function": {"arguments": args}}]}),
            None,
        ),
        Step::Finish(reason) => choice(json!({}), Some(reason)),
        Step::Usage(p, c) => json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk",
            "model": "m", "choices": [],
            "usage": {"prompt_tokens": p, "completion_tokens": c, "total_tokens": p + c}}),
        Step::Wait(_) | Step::Fail => unreachable!("handled by the sender"),
    }
}

/// A gateway with two chat aliases (`chatty`, `other`) on an upstream that
/// is `fake`, settings adjusted by `tweak`, and — when `policy` is given —
/// one client key ([`KEY`]) carrying it.
pub async fn gateway(
    fake: &ChatFake,
    auth: bool,
    policy: Option<KeyPolicy>,
    tweak: impl FnOnce(&mut Settings),
) -> (SharedState, String) {
    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.auth_enabled = auth;
    tweak(&mut settings);
    lmgw_core::store::save_settings(&state.db, &settings)
        .await
        .unwrap();
    add_chat_aliases(&state, fake).await;
    if let Some(p) = policy {
        sqlx::query(
            "INSERT INTO api_keys (name, key_hash, enabled, scope_mode, scope_patterns,
                 budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit)
             VALUES ('voice', ?1, 1, ?2, ?3, 0, ?4, ?5, ?6, ?7)",
        )
        .bind(hash_api_key(KEY))
        .bind(p.scope_mode.as_str())
        .bind(&p.scope_patterns)
        .bind(p.budget_period.as_str())
        .bind(p.rpm_limit)
        .bind(p.tpm_limit)
        .bind(p.concurrency_limit)
        .execute(&state.db)
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();

    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (state, addr.to_string())
}

/// The chat aliases `chatty` and `other`, on an upstream that is `fake`
/// (stored; the caller reloads the snapshot).
pub async fn add_chat_aliases(state: &SharedState, fake: &ChatFake) {
    sqlx::query(
        "INSERT INTO upstreams (name, protocol, kind, base_url, extra_headers, timeout_ms,
             enabled, expose_all, expose_prefix, created_at, updated_at, supports_responses)
         VALUES ('fake', 'openai', 'generic', ?1, '[]', 10000, 1, 0, '',
                 datetime('now'), datetime('now'), 0)",
    )
    .bind(&fake.url)
    .execute(&state.db)
    .await
    .unwrap();
    for alias in ["chatty", "other"] {
        sqlx::query(
            "INSERT INTO models (alias, upstream_id, upstream_model_id, param_overrides, enabled,
                 created_at, updated_at)
             VALUES (?1, (SELECT id FROM upstreams WHERE name='fake'), 'm', '{}', 1,
                     datetime('now'), datetime('now'))",
        )
        .bind(alias)
        .execute(&state.db)
        .await
        .unwrap();
    }
}

/// Serve the gateway of a fake GPU world (`gpu_world`), its settings
/// adjusted by `tweak`: sessions on its local models.
pub async fn gpu_gateway(g: &super::gpu_world::Gpu, tweak: impl FnOnce(&mut Settings)) -> String {
    let mut s = g.state.snapshot().settings.clone();
    tweak(&mut s);
    lmgw_core::store::save_settings(&g.state.db, &s)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();
    let app = build_router(g.state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr.to_string()
}

/// Open a session; panics on a refused handshake.
pub async fn open(addr: &str, path_and_query: &str, headers: &[(&str, &str)]) -> Ws {
    let mut req = format!("ws://{addr}{path_and_query}")
        .into_client_request()
        .unwrap();
    for (k, v) in headers {
        req.headers_mut().insert(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().expect("a header-safe test value"),
        );
    }
    tokio_tungstenite::connect_async(req)
        .await
        .unwrap_or_else(|e| panic!("expected a 101: {e}"))
        .0
}

/// A text session on `chatty`, past its `session.created` and
/// `session.updated`.
pub async fn text_session(addr: &str) -> Ws {
    let mut ws = open(addr, "/v1/realtime?model=chatty", &[]).await;
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "output_modalities": ["text"]}}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    ws
}

/// The next JSON event, as long as the test may wait for it
/// ([`crate::common::patience`]).
pub async fn next_event(ws: &mut Ws) -> Value {
    loop {
        let msg = crate::common::patience::within("the server's next event", ws.next())
            .await
            .expect("the socket closed")
            .expect("the socket failed");
        match msg {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected a text event, got {other:?}"),
        }
    }
}

/// Every event up to and including the first of type `until`.
pub async fn events_until(ws: &mut Ws, until: &str) -> Vec<Value> {
    let mut out = Vec::new();
    loop {
        let ev = next_event(ws).await;
        let done = ev["type"] == until;
        out.push(ev);
        if done {
            return out;
        }
    }
}

pub async fn send(ws: &mut Ws, event: Value) {
    ws.send(Message::text(event.to_string())).await.unwrap();
}

/// A user text item.
pub fn user_text(text: &str) -> Value {
    json!({"type": "conversation.item.create",
           "item": {"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": text}]}})
}

/// The `type`s of `events`, in order.
pub fn types(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

/// The client frames of a captured connection (`tests/fixtures/realtime/
/// clients/`), in order.
pub fn captured_client_frames(file: &str) -> Vec<Value> {
    let path = format!(
        "{}/tests/fixtures/realtime/clients/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let conns: Vec<Value> = serde_json::from_str(&raw).unwrap();
    conns[0]["frames"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["dir"] == "client->server")
        .map(|f| f["event"].clone())
        .collect()
}
