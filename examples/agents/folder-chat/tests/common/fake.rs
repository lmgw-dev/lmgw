//! A fake lmgw for the HTTP tests: model info, embeddings (with a `gpu_hold`
//! 503 from a call on or while switched on, a held gate, a 500 for one file
//! or for everything, a hold-fallback answer, or another model's vectors on
//! demand), rerank (optionally answered by a hold fallback), a streaming
//! chat completion cut into awkward pieces, and — for a chat completion that
//! is not streamed — a vision model's page reading, canned per mode
//! ([`OCR_READING`], [`STRUCTURE_READING`]), optionally failing, held or
//! answered by a hold fallback.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use folder_chat::config::{AgentConfig, GatewayEnv};
use folder_chat::gateway::Gateway;
use quickdoc_core::embed::{FixtureEmbedder, FixtureReranker, Reranker};
use serde_json::{json, Value};

use super::{gpu_hold_body, write, DIMS};

pub const TOKEN: &str = "lmgw-agent-test";

/// What the fake vision model reads off a page given the OCR prompt. It is
/// sent wrapped in a code fence, which the agent strips.
pub const OCR_READING: &str = "Kittiwake ledger: 14 nests on the north cliff";

/// What it reads off a page given the structure prompt, before the line
/// [`structure_reading`] adds.
pub const STRUCTURE_READING: &str = "Gross | Month: 3.250,00\nGross | Year to date: 29.250,00";

/// The whole structure-mode reading of a page whose extracted text is
/// `page_text`: [`STRUCTURE_READING`] and the page's first line, so two pages
/// never read the same (identical text in one file would be one chunk).
pub fn structure_reading(page_text: &str) -> String {
    let first = page_text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    format!("{STRUCTURE_READING}\nfrom: {}", first.trim())
}

#[derive(Default)]
pub struct Fake {
    pub models: HashMap<String, Value>,
    /// `/v1/embeddings` answers `gpu_hold` from this call on (0 = always).
    pub hold_embeddings_from: Option<usize>,
    /// `/v1/embeddings` answers `gpu_hold` while this is set — a hold the
    /// test switches on and off.
    pub hold_embeddings: AtomicBool,
    /// `/v1/embeddings` waits, from this call on, until the gate opens
    /// ([`Gate::open`]) — how a test holds a sync mid-way.
    pub gate: Option<(usize, tokio::sync::watch::Receiver<bool>)>,
    pub embed_calls: AtomicUsize,
    /// `/v1/embeddings` answers 500 for any request with an input containing
    /// this text.
    pub fail_embeddings_containing: Option<String>,
    /// `/v1/embeddings` answers 500 from this call on (0 = always) — a
    /// gateway that is down.
    pub fail_embeddings_from: Option<usize>,
    /// `/v1/embeddings` answers 200 with `x-lmgw-fallback: <alias>` from this
    /// call on — a held embedding model with a fallback configured.
    pub embed_fallback_from: Option<(usize, String)>,
    /// Negate every vector — the same alias and width, another model.
    pub negate_vectors: AtomicBool,
    pub rerank_calls: AtomicUsize,
    /// `/v1/rerank` answers 200 with `x-lmgw-fallback: <alias>` — a held
    /// rerank model with a fallback configured.
    pub rerank_fallback: Option<String>,
    pub fallback: Option<String>,
    pub chat_bodies: Mutex<Vec<Value>>,
    pub bearers: Mutex<Vec<String>>,
    /// Page readings (`/v1/chat/completions` without `stream: true`) asked
    /// for, and their bodies — kept apart from `chat_bodies`.
    pub vision_calls: AtomicUsize,
    pub vision_bodies: Mutex<Vec<Value>>,
    /// A page reading — and the probe image — answers this status (with an
    /// error body) while it is not 0: a model that refuses every image.
    pub vision_status: AtomicU16,
    /// A page reading, but not the probe image, answers this status while it
    /// is not 0: a model that refuses those pages.
    pub vision_page_status: AtomicU16,
    /// A page reading answers 200 with no text while this is set.
    pub vision_empty: AtomicBool,
    /// A page reading — or probe — whose image has more pixels than this is
    /// refused `400 exceed_context_size`, as llama-server refuses an image
    /// its context has no room for.
    pub vision_max_pixels: Option<u64>,
    /// A page reading answers `gpu_hold` from this call on (0 = always).
    pub vision_hold_from: Option<usize>,
    /// Every page reading carries `x-lmgw-fallback: <alias>`, whatever its
    /// status — as lmgw sends it on an error from the fallback too.
    pub vision_fallback: Option<String>,
}

/// The sending half of [`Fake::gate`].
pub struct Gate(tokio::sync::watch::Sender<bool>);

impl Gate {
    /// A closed gate, and what to put in [`Fake::gate`] to hold embeddings
    /// from call `from` on.
    pub fn closed(from: usize) -> (Self, (usize, tokio::sync::watch::Receiver<bool>)) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        (Self(tx), (from, rx))
    }

    pub fn open(&self) {
        let _ = self.0.send(true);
    }
}

pub type Shared = Arc<Fake>;

fn note_bearer(f: &Fake, h: &HeaderMap) {
    if let Some(v) = h.get("authorization").and_then(|v| v.to_str().ok()) {
        f.bearers.lock().unwrap().push(v.to_string());
    }
}

async fn model(State(f): State<Shared>, Path(id): Path<String>, h: HeaderMap) -> Response {
    note_bearer(&f, &h);
    match f.models.get(&id) {
        Some(m) => Json(m.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(
                json!({"error": {"message": format!("model '{id}' is not exposed by this gateway"),
                                   "type": "invalid_request_error", "code": "not_found"}}),
            ),
        )
            .into_response(),
    }
}

async fn embeddings(State(f): State<Shared>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    note_bearer(&f, &h);
    let n = f.embed_calls.fetch_add(1, Ordering::SeqCst);
    if let Some((from, rx)) = &f.gate {
        if n >= *from {
            let mut rx = rx.clone();
            let _ = rx.wait_for(|open| *open).await;
        }
    }
    if f.hold_embeddings_from.is_some_and(|from| n >= from)
        || f.hold_embeddings.load(Ordering::SeqCst)
    {
        return (StatusCode::SERVICE_UNAVAILABLE, gpu_hold_body()).into_response();
    }
    let inputs: Vec<&str> = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    let poisoned = f
        .fail_embeddings_containing
        .as_deref()
        .is_some_and(|needle| inputs.iter().any(|t| t.contains(needle)));
    if poisoned || f.fail_embeddings_from.is_some_and(|from| n >= from) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                json!({"error": {"message": "upstream exploded", "type": "api_error",
                                  "code": "upstream_error"}}),
            ),
        )
            .into_response();
    }
    let e = FixtureEmbedder::new(DIMS);
    let negate = f.negate_vectors.load(Ordering::SeqCst);
    let data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut v = e.embed_one(t);
            if negate {
                v.iter_mut().for_each(|x| *x = -*x);
            }
            json!({"index": i, "embedding": v})
        })
        .collect();
    let mut resp = Json(json!({"object": "list", "data": data})).into_response();
    if let Some((from, alias)) = &f.embed_fallback_from {
        if n >= *from {
            resp.headers_mut()
                .insert("x-lmgw-fallback", alias.parse().unwrap());
        }
    }
    resp
}

async fn rerank(State(f): State<Shared>, Json(body): Json<Value>) -> Response {
    f.rerank_calls.fetch_add(1, Ordering::SeqCst);
    let docs: Vec<String> = serde_json::from_value(body["documents"].clone()).unwrap();
    let scores = FixtureReranker
        .rerank(body["query"].as_str().unwrap(), &docs)
        .await
        .unwrap();
    // Out of order on purpose: the client must place scores by `index`.
    let mut results: Vec<Value> = scores
        .iter()
        .enumerate()
        .map(|(i, s)| json!({"index": i, "relevance_score": s}))
        .collect();
    results.reverse();
    let mut resp = Json(json!({"object": "list", "results": results})).into_response();
    if let Some(fb) = &f.rerank_fallback {
        resp.headers_mut()
            .insert("x-lmgw-fallback", fb.parse().unwrap());
    }
    resp
}

/// A page reading: [`OCR_READING`] (fenced) for the OCR prompt,
/// [`STRUCTURE_READING`] for the structure prompt — with a reasoning model's
/// thinking beside it, which is not the reading.
fn vision(f: &Fake, body: Value) -> Response {
    let n = f.vision_calls.fetch_add(1, Ordering::SeqCst);
    f.vision_bodies.lock().unwrap().push(body.clone());
    let mut resp = vision_answer(f, n, &body);
    if let Some(fb) = &f.vision_fallback {
        resp.headers_mut()
            .insert("x-lmgw-fallback", fb.parse().unwrap());
    }
    resp
}

fn vision_answer(f: &Fake, n: usize, body: &Value) -> Response {
    if f.vision_hold_from.is_some_and(|from| n >= from) {
        return (StatusCode::SERVICE_UNAVAILABLE, gpu_hold_body()).into_response();
    }
    let prompt = body["messages"][0]["content"][1]["text"]
        .as_str()
        .unwrap_or_default();
    let probe = prompt == folder_chat::vision::PROBE_PROMPT;
    if let Some(max) = f.vision_max_pixels {
        use base64::Engine;
        let url = body["messages"][0]["content"][0]["image_url"]["url"]
            .as_str()
            .unwrap_or_default();
        let png = base64::engine::general_purpose::STANDARD
            .decode(url.trim_start_matches("data:image/png;base64,"))
            .unwrap_or_default();
        let (w, h) = folder_chat::vision::png_size(&png).unwrap_or((0, 0));
        if u64::from(w) * u64::from(h) > max {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {
                    "message": "the request exceeds the available context size, try increasing it",
                    "type": "invalid_request_error", "code": "exceed_context_size"}})),
            )
                .into_response();
        }
    }
    let mut status = f.vision_status.load(Ordering::SeqCst);
    if status == 0 && !probe {
        status = f.vision_page_status.load(Ordering::SeqCst);
    }
    if status != 0 {
        let (message, code) = if status < 500 {
            ("the image is not a valid PNG", "invalid_request_error")
        } else {
            ("the projector ran out of memory", "upstream_error")
        };
        return (
            StatusCode::from_u16(status).unwrap(),
            Json(json!({"error": {"message": message, "type": "api_error", "code": code}})),
        )
            .into_response();
    }
    let text = if probe {
        "Square".to_string()
    } else if f.vision_empty.load(Ordering::SeqCst) {
        "```\n\n```".to_string()
    } else {
        match prompt.strip_prefix(folder_chat::vision::STRUCTURE_PROMPT) {
            Some(page_text) => structure_reading(page_text),
            None => format!("```\n{OCR_READING}\n```"),
        }
    };
    Json(json!({
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text,
                        "reasoning_content": "looking at the page"},
            "finish_reason": "stop"
        }]
    }))
    .into_response()
}

async fn chat(State(f): State<Shared>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    note_bearer(&f, &h);
    if body["stream"] != json!(true) {
        return vision(&f, body);
    }
    f.chat_bodies.lock().unwrap().push(body);
    let wire = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Use the \"}}]}\r\n\r\n",
        ": keep-alive\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"zeta flag [1].\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":7,\"total_tokens\":127}}\n\n",
        "data: [DONE]\n\n",
    );
    // Seven-byte pieces: frames, lines and UTF-8 are all split mid-way.
    let pieces: Vec<Result<Bytes, std::convert::Infallible>> = wire
        .as_bytes()
        .chunks(7)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    let mut resp = Response::new(Body::from_stream(futures::stream::iter(pieces)));
    resp.headers_mut()
        .insert("content-type", "text/event-stream".parse().unwrap());
    if let Some(fb) = &f.fallback {
        resp.headers_mut()
            .insert("x-lmgw-fallback", fb.parse().unwrap());
    }
    resp
}

pub async fn serve(fake: Fake) -> (String, Shared) {
    let shared = Arc::new(fake);
    let app = Router::new()
        .route("/v1/models/{*id}", get(model))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/rerank", post(rerank))
        .route("/v1/chat/completions", post(chat))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1"), shared)
}

pub fn models(chat: Value) -> HashMap<String, Value> {
    HashMap::from([
        (
            "embed/fixture".to_string(),
            json!({"id": "embed/fixture", "object": "model", "context_length": 512}),
        ),
        ("chat-small".to_string(), chat),
        (
            "rerank/fixture".to_string(),
            json!({"id": "rerank/fixture", "object": "model"}),
        ),
    ])
}

pub fn gateway(base: &str) -> Gateway {
    Gateway::new(GatewayEnv::new(base, Some(TOKEN.into())))
}

pub fn config(folder: &std::path::Path, rerank: bool) -> AgentConfig {
    AgentConfig {
        folder: folder.to_path_buf(),
        embed_model: "embed/fixture".into(),
        chat_model: "chat-small".into(),
        rerank_model: rerank.then(|| "rerank/fixture".to_string()),
        vision_model: None,
        vision_every_page: false,
        chunk_tokens: 400,
        allow_remote: false,
    }
}

pub fn seed(root: &std::path::Path) {
    write(
        root,
        "notes.md",
        "# Setup\n\nInstall podman first.\n\n## SELinux\n\nUse the zeta relabel flag on bind mounts.\n",
    );
    write(root, "todo.txt", "buy milk\n\ncall the gamma office\n");
}
