//! A fake llama-server for the benchmark engine suite (`bench_engine.rs`),
//! and a GPU probe with a scripted energy counter.
//!
//! An axum app rather than a wiremock server, for one reason: wiremock
//! answers with the whole body at once, and the phases measure *time
//! between streamed tokens* — a concurrent window, a mixed stall. This one
//! streams `/completion` a token at a time, spends a real delay per prompt
//! token on "prefill", and holds a shared lock while it does, so decoding
//! streams stall exactly while another request prefills (what the mixed
//! phase exists to measure). Its answers follow the two engines' real shapes
//! (benchmark design §2.1): official sends `tokens` arrays, `cache_n`,
//! `build_info`, `model_ftype`; ik sends none of them and a top-level
//! `n_ctx`.

#![allow(dead_code)]

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{ready, Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use lmgw_core::vram::{GpuMemory, GpuPower, GpuProbe};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Official,
    Ik,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub shape: Shape,
    pub n_slots: u32,
    pub per_slot_ctx: u64,
    /// Tokens `/tokenize` answers for the corpus (ids 0, 1, …).
    pub corpus_tokens: u32,
    /// The vocabulary adds a BOS ([`BOS`]) when asked for special tokens.
    pub bos: bool,
    pub token_delay: Duration,
    pub prefill_per_token: Duration,
    /// `/apply-template`'s status: 200 renders, anything else refuses.
    pub template_status: u16,
    /// Whether the rendered template keeps an earlier turn's reasoning.
    pub template_keeps_reasoning: bool,
    /// `/props` `chat_template_caps.supports_tools` (official shape only).
    pub supports_tools: bool,
    /// `/completion` answers this status to prompts of exactly this length.
    pub fail_prompt_len: Option<(usize, u16)>,
    /// `(len, n, status)`: the same, but only for the `n`-th request of that
    /// length (1-based) — one of several streams released together fails,
    /// the others run on.
    pub fail_prompt_len_nth: Option<(usize, usize, u16)>,
    /// `/completion` never answers — except the first request, the engine's
    /// unmeasured warm-up — it only notices the client leaving.
    pub hang_completion: bool,
    /// `/completion` never answers a request generating exactly this many
    /// tokens — a measured prefill's `1` — while the unmeasured ones (the
    /// warm-up, the slot resets) are answered.
    pub hang_n_predict: Option<u64>,
    /// Draft statistics in the timings (speculative decoding).
    pub drafting: bool,
    /// The slots share one KV pool of this many cells (unified KV). A
    /// request that would overflow it aborts every running request with
    /// "Context size has been exceeded.", as llama-server does
    /// (unified-KV design §2.1 fact 3); a request that ends frees its cells
    /// (idle slots are cleared from a unified pool, fact 5).
    pub unified_pool: Option<u64>,
    /// The `deterministic` probe's chat answer changes on a repeated request
    /// whose `cache_prompt` allows reuse (the default), and stays fixed when
    /// `cache_prompt: false` — live testing's finding (§2.1) that prompt-cache
    /// reuse is not bit-deterministic on some rows. `false` (the default)
    /// answers the same content every time, whatever `cache_prompt` says.
    pub chat_cache_nondeterministic: bool,
    /// The `/completion` requests (1-based, counted as their request lines
    /// reach the socket) that get no answer: the fake closes the connection
    /// under them, unread — what a request sent on a connection llama-server
    /// had already closed after its previous stream meets (§13 decision 62).
    pub hang_up_completions: Vec<usize>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            shape: Shape::Official,
            n_slots: 2,
            per_slot_ctx: 4096,
            corpus_tokens: 20_000,
            bos: true,
            token_delay: Duration::from_millis(2),
            prefill_per_token: Duration::from_micros(50),
            template_status: 200,
            template_keeps_reasoning: false,
            supports_tools: true,
            fail_prompt_len: None,
            fail_prompt_len_nth: None,
            hang_completion: false,
            hang_n_predict: None,
            drafting: false,
            chat_cache_nondeterministic: false,
            unified_pool: None,
            hang_up_completions: Vec::new(),
        }
    }
}

/// The fake vocabulary's BOS id (outside the corpus's ids).
pub const BOS: u64 = 1_000_000;

/// The id the fake gives any short text (one word).
pub const WORD: u64 = 999_999;

#[derive(Default)]
pub struct Seen {
    /// Every `/completion` body.
    pub completions: Mutex<Vec<Value>>,
    /// `/completion` requests that said `Connection: close`.
    pub completions_closing: AtomicUsize,
    /// `/completion` request lines that reached the socket, the ones hung up
    /// on included.
    wire_completions: AtomicUsize,
    /// `/completion` requests hung up on (`hang_up_completions`).
    pub hung_up: AtomicUsize,
    /// Every `/tokenize` body.
    pub tokenizes: Mutex<Vec<Value>>,
    /// Every `/v1/chat/completions` body.
    pub chats: Mutex<Vec<Value>>,
    pub inflight: AtomicUsize,
    pub max_inflight: AtomicUsize,
    /// Streams the client closed before their final chunk.
    pub closed_early: AtomicUsize,
    /// Notified when "the container" is removed: a hanging `/completion`
    /// then breaks its connection, as a killed llama-server's does.
    pub killed: Arc<tokio::sync::Notify>,
    /// `/completion` requests hanging right now.
    pub hanging: AtomicUsize,
    /// Requests of `fail_prompt_len_nth`'s length seen so far.
    nth_seen: AtomicUsize,
    /// `unified_pool`: cells in use, and a generation bumped by every abort.
    pool: Mutex<(u64, u64)>,
    /// Times a full `unified_pool` aborted the running requests.
    pub pool_aborts: AtomicUsize,
    /// The most cells `unified_pool` ever held at once.
    pub pool_peak: AtomicU64,
    /// Prompts a `cache_prompt: true` request sent before (the "KV cache").
    cached: Mutex<HashSet<Vec<u64>>>,
    /// `/v1/chat/completions` texts already answered once with cache reuse
    /// allowed — `chat_cache_nondeterministic`'s "KV cache" for the
    /// `deterministic` probe.
    chat_cache_hit: Mutex<HashSet<String>>,
}

struct Fake {
    cfg: Config,
    seen: Arc<Seen>,
    /// Held for writing while a request prefills; every emitted token takes
    /// it for reading.
    compute: RwLock<()>,
}

pub struct FakeLlama {
    pub url: String,
    pub seen: Arc<Seen>,
}

pub async fn start(cfg: Config) -> FakeLlama {
    let seen = Arc::new(Seen::default());
    let fake = Arc::new(Fake {
        cfg,
        seen: seen.clone(),
        compute: RwLock::new(()),
    });
    let listening = HangUpListener {
        inner: TcpListener::bind("127.0.0.1:0").await.unwrap(),
        fake: fake.clone(),
    };
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/props", get(props))
        .route("/slots", get(slots))
        .route("/tokenize", post(tokenize))
        .route("/detokenize", post(detokenize))
        .route("/apply-template", post(apply_template))
        .route("/v1/chat/completions", post(chat))
        .route("/completion", post(completion))
        .with_state(fake);
    let addr = listening.inner.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listening, app).await.unwrap() });
    FakeLlama {
        url: format!("http://{addr}"),
        seen,
    }
}

type St = State<Arc<Fake>>;

/// The fake's listener: each connection it hands out can hang up on a
/// `/completion` request ([`Config::hang_up_completions`]).
struct HangUpListener {
    inner: TcpListener,
    fake: Arc<Fake>,
}

impl axum::serve::Listener for HangUpListener {
    type Io = HangUpIo;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            if let Ok((inner, addr)) = self.inner.accept().await {
                let io = HangUpIo {
                    inner,
                    fake: self.fake.clone(),
                    hung_up: false,
                };
                return (io, addr);
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// A connection that watches for `/completion` request lines. On one it is
/// to hang up on, it drops what it read and answers the server's next read
/// with the end of the stream: the server closes the connection without a
/// word, the rest of the request still unread.
struct HangUpIo {
    inner: TcpStream,
    fake: Arc<Fake>,
    hung_up: bool,
}

const COMPLETION_LINE: &[u8] = b"POST /completion ";

impl AsyncRead for HangUpIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.hung_up {
            return Poll::Ready(Ok(()));
        }
        let before = buf.filled().len();
        ready!(Pin::new(&mut self.inner).poll_read(cx, buf))?;
        let read = &buf.filled()[before..];
        if read
            .windows(COMPLETION_LINE.len())
            .any(|w| w == COMPLETION_LINE)
        {
            let seen = &self.fake.seen;
            let n = seen.wire_completions.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fake.cfg.hang_up_completions.contains(&n) {
                seen.hung_up.fetch_add(1, Ordering::SeqCst);
                self.hung_up = true;
                buf.set_filled(before);
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for HangUpIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.hung_up {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        Pin::new(&mut self.inner).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

async fn props(State(f): St) -> Json<Value> {
    let c = &f.cfg;
    Json(match c.shape {
        Shape::Official => json!({
            "default_generation_settings": {"params": {}, "n_ctx": c.per_slot_ctx},
            "total_slots": c.n_slots,
            "model_ftype": "Q4_K - Medium",
            "modalities": {"vision": true, "audio": false},
            "chat_template_caps": {"supports_tools": c.supports_tools},
            "build_info": "b11226-0c6a6a7",
        }),
        Shape::Ik => json!({
            "default_generation_settings": {"n_ctx": c.per_slot_ctx},
            "total_slots": c.n_slots,
            "chat_template_caps": {},
            "modalities": {"vision": false, "audio": false},
            "n_ctx": c.per_slot_ctx * c.n_slots as u64,
        }),
    })
}

async fn slots(State(f): St) -> Json<Value> {
    let c = &f.cfg;
    Json(Value::Array(
        (0..c.n_slots)
            .map(|id| match c.shape {
                Shape::Official => json!({"id": id, "n_ctx": c.per_slot_ctx,
                    "speculative": c.drafting, "is_processing": false}),
                Shape::Ik => json!({"id": id, "n_ctx": c.per_slot_ctx, "state": 0}),
            })
            .collect(),
    ))
}

/// Reads the body before it answers, as llama-server does: a server that
/// answers a 437 KB upload without reading it closes the connection under
/// the client's feet. The corpus comes as an array of pieces; a short
/// string is one word, with the BOS before it when special tokens are asked
/// for and the vocabulary has one.
async fn tokenize(State(f): St, Json(body): Json<Value>) -> Json<Value> {
    f.seen.tokenizes.lock().unwrap().push(body.clone());
    let tokens: Vec<u64> = match &body["content"] {
        Value::Array(pieces) => {
            assert!(pieces.iter().all(Value::is_string), "{pieces:?}");
            (0..u64::from(f.cfg.corpus_tokens)).collect()
        }
        Value::String(_) => {
            let special = body["add_special"].as_bool().unwrap_or(false);
            let mut t = Vec::new();
            if special && f.cfg.bos {
                t.push(BOS);
            }
            t.push(WORD);
            t
        }
        other => panic!("/tokenize content: {other}"),
    };
    Json(json!({ "tokens": tokens }))
}

async fn detokenize(Json(body): Json<Value>) -> Json<Value> {
    let n = body["tokens"].as_array().map_or(0, Vec::len);
    Json(json!({"content": format!("[a haystack of {n} tokens]")}))
}

async fn apply_template(State(f): St, Json(body): Json<Value>) -> Response {
    if f.cfg.template_status != 200 {
        return (
            StatusCode::from_u16(f.cfg.template_status).unwrap(),
            Json(json!({"error": {"code": f.cfg.template_status, "message": "template failed"}})),
        )
            .into_response();
    }
    let reasoning = body["messages"][1]["reasoning_content"]
        .as_str()
        .unwrap_or("");
    let kept = if f.cfg.template_keeps_reasoning {
        format!("<think>{reasoning}</think>")
    } else {
        String::new()
    };
    Json(json!({"prompt": format!("<|user|>2+2<|assistant|>{kept}4<|user|>3+3<|assistant|>")}))
        .into_response()
}

async fn chat(State(f): St, Json(body): Json<Value>) -> Json<Value> {
    f.seen.chats.lock().unwrap().push(body.clone());
    let text = body["messages"][0]["content"].to_string();
    let thinking = body["chat_template_kwargs"]["enable_thinking"].as_bool() == Some(true);
    let mut msg = json!({"role": "assistant", "content": "Paris is the capital of France."});
    let mut finish = "stop";
    if body.get("tools").is_some() {
        msg["content"] = json!("");
        msg["tool_calls"] = json!([{"type": "function", "id": "c1",
            "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]);
        finish = "tool_calls";
    } else if body.get("response_format").is_some() {
        msg["content"] = json!("{\"name\": \"Alice\", \"age\": 30}");
    } else if text.contains("image_url") {
        msg["content"] = json!("Red.");
    } else if text.contains("passcode") {
        msg["content"] = json!("83125947");
    } else if text.contains("sea") && f.cfg.chat_cache_nondeterministic {
        // The `deterministic` probe's own text: a repeated request whose
        // `cache_prompt` allows reuse (the default — absent, or `true`) gets
        // a different answer than the first; `cache_prompt: false` never
        // hits the "cache", so it is always the fresh answer.
        let cache_prompt = body
            .get("cache_prompt")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let hit = cache_prompt && {
            let mut seen = f.seen.chat_cache_hit.lock().unwrap();
            !seen.insert(text.clone())
        };
        msg["content"] = json!(if hit {
            "The sea is deep and full of mystery."
        } else {
            "The sea is vast and blue."
        });
    } else if thinking {
        msg["reasoning_content"] = json!("17 + 25 = 42");
        msg["content"] = json!("42");
    }
    Json(json!({"choices": [{"index": 0, "message": msg, "finish_reason": finish}]}))
}

fn sse(v: &Value) -> Bytes {
    Bytes::from(format!("data: {v}\n\n"))
}

async fn completion(State(f): St, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let closing = headers
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("close"));
    if closing {
        f.seen.completions_closing.fetch_add(1, Ordering::SeqCst);
    }
    let nth = {
        let mut seen = f.seen.completions.lock().unwrap();
        seen.push(body.clone());
        seen.len()
    };
    let hang = f.cfg.hang_completion && nth > 1;
    let prompt: Vec<u64> = body["prompt"]
        .as_array()
        .expect("the engine sends token-id arrays")
        .iter()
        .map(|t| t.as_u64().unwrap())
        .collect();
    let refuse = match (f.cfg.fail_prompt_len, f.cfg.fail_prompt_len_nth) {
        (Some((len, status)), _) if prompt.len() == len => Some(status),
        (_, Some((len, n, status)))
            if prompt.len() == len && f.seen.nth_seen.fetch_add(1, Ordering::SeqCst) + 1 == n =>
        {
            Some(status)
        }
        _ => None,
    };
    if let Some(status) = refuse {
        return (
            StatusCode::from_u16(status).unwrap(),
            Json(json!({"error": {"code": status, "message": "the fake refuses this length"}})),
        )
            .into_response();
    }
    let n_predict = body["n_predict"].as_u64().unwrap();
    let hang = hang || f.cfg.hang_n_predict == Some(n_predict);
    let cache_prompt = body["cache_prompt"].as_bool().unwrap_or(false);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let fake = f.clone();
    tokio::spawn(async move {
        let seen = &fake.seen;
        let now = seen.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        seen.max_inflight.fetch_max(now, Ordering::SeqCst);
        let finished = stream(&fake, &tx, prompt, n_predict, cache_prompt, hang).await;
        if !finished {
            seen.closed_early.fetch_add(1, Ordering::SeqCst);
        }
        seen.inflight.fetch_sub(1, Ordering::SeqCst);
    });
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap()
}

/// Stream one completion; `false` when the client left first.
async fn stream(
    f: &Fake,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    prompt: Vec<u64>,
    n_predict: u64,
    cache_prompt: bool,
    hang: bool,
) -> bool {
    let c = &f.cfg;
    if hang {
        f.seen.hanging.fetch_add(1, Ordering::SeqCst);
        let _gone = scopeguard(|| {
            f.seen.hanging.fetch_sub(1, Ordering::SeqCst);
        });
        tokio::select! {
            () = tx.closed() => {}
            () = f.seen.killed.notified() => {
                let _ = tx.send(Err(std::io::Error::other("the container was removed"))).await;
            }
        }
        return false;
    }
    let len = prompt.len() as u64;
    let hit = cache_prompt && f.seen.cached.lock().unwrap().contains(&prompt);
    if cache_prompt {
        f.seen.cached.lock().unwrap().insert(prompt);
    }
    let prompt_n = if hit { 1 } else { len };
    // The prompt's cells (the whole prompt: a cache hit's prefix is held
    // too), freed when this request ends.
    let mut claim = Claim::new(f);
    if !claim.take(len) {
        let _ = tx.send(Ok(sse(&context_exceeded()))).await;
        return true;
    }

    let t0 = Instant::now();
    {
        let _w = f.compute.write().await;
        tokio::time::sleep(c.prefill_per_token * prompt_n as u32).await;
    }
    let prompt_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t1 = Instant::now();
    for i in 0..n_predict {
        {
            let _r = f.compute.read().await;
            tokio::time::sleep(c.token_delay).await;
        }
        if !claim.take(1) {
            let _ = tx.send(Ok(sse(&context_exceeded()))).await;
            return true;
        }
        let chunk = match c.shape {
            Shape::Official => json!({"index": 0, "content": " tok", "tokens": [100 + i],
                "stop": false, "id_slot": -1}),
            Shape::Ik => json!({"content": " tok", "stop": false, "id_slot": 0}),
        };
        if tx.send(Ok(sse(&chunk))).await.is_err() {
            return false;
        }
    }
    let predicted_ms = t1.elapsed().as_secs_f64() * 1000.0;
    let rate = |n: u64, ms: f64| {
        if ms > 0.0 {
            n as f64 * 1000.0 / ms
        } else {
            0.0
        }
    };
    let mut timings = json!({
        "prompt_n": prompt_n,
        "prompt_ms": prompt_ms,
        "prompt_per_second": rate(prompt_n, prompt_ms),
        "predicted_n": n_predict,
        "predicted_ms": predicted_ms,
        "predicted_per_second": rate(n_predict, predicted_ms),
    });
    let final_chunk = match c.shape {
        Shape::Official => {
            timings["cache_n"] = json!(len - prompt_n);
            if c.drafting {
                timings["draft_n"] = json!(n_predict);
                timings["draft_n_accepted"] = json!(n_predict / 2);
            }
            json!({"index": 0, "content": "", "tokens": [], "stop": true, "timings": timings})
        }
        Shape::Ik => {
            timings["n_ctx"] = json!(c.per_slot_ctx);
            timings["n_past"] = json!(len + n_predict);
            json!({"content": "", "generated_text": "", "stop": true, "timings": timings})
        }
    };
    tx.send(Ok(sse(&final_chunk))).await.is_ok()
}

fn context_exceeded() -> Value {
    json!({"error": {"code": 500, "message": "Context size has been exceeded.", "type": "server_error"}})
}

/// One request's cells in `unified_pool`, given back when it ends — unless
/// an abort cleared the pool since.
struct Claim<'a> {
    f: &'a Fake,
    generation: u64,
    cells: u64,
}

impl<'a> Claim<'a> {
    fn new(f: &'a Fake) -> Self {
        let generation = f.seen.pool.lock().unwrap().1;
        Self {
            f,
            generation,
            cells: 0,
        }
    }

    /// Take `n` more cells; `false` when this request was aborted, or when
    /// the pool is full — which aborts every running request.
    fn take(&mut self, n: u64) -> bool {
        let Some(size) = self.f.cfg.unified_pool else {
            return true;
        };
        let mut pool = self.f.seen.pool.lock().unwrap();
        if pool.1 != self.generation {
            return false;
        }
        if pool.0 + n > size {
            *pool = (0, pool.1 + 1);
            self.f.seen.pool_aborts.fetch_add(1, Ordering::SeqCst);
            return false;
        }
        pool.0 += n;
        self.cells += n;
        self.f.seen.pool_peak.fetch_max(pool.0, Ordering::SeqCst);
        true
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut pool = self.f.seen.pool.lock().unwrap_or_else(|p| p.into_inner());
        if pool.1 == self.generation {
            pool.0 = pool.0.saturating_sub(self.cells);
        }
    }
}

/// A GPU probe whose energy counter and power follow a constant draw from
/// the moment it was made — so every window's joules are known exactly:
/// `watts × seconds`. `counter: false` answers power only (energy is then
/// integrated); `clock_events` is what every reading reports.
pub struct ScriptedGpu {
    pub t0: Instant,
    pub watts: u64,
    pub counter: bool,
    pub clock_events: u64,
    pub reads: AtomicU64,
    pub fail: AtomicBool,
}

impl ScriptedGpu {
    pub fn new(watts: u64, counter: bool, clock_events: u64) -> Arc<Self> {
        Arc::new(Self {
            t0: Instant::now(),
            watts,
            counter,
            clock_events,
            reads: AtomicU64::new(0),
            fail: AtomicBool::new(false),
        })
    }
}

impl GpuProbe for ScriptedGpu {
    fn devices(&self) -> Result<Vec<GpuMemory>, String> {
        Ok(vec![GpuMemory {
            index: 0,
            name: "FakeGPU 4090".into(),
            total_bytes: 24 << 30,
            used_bytes: 2 << 30,
            free_bytes: 22 << 30,
        }])
    }

    fn source(&self) -> String {
        "ScriptedGpu".into()
    }

    fn power(&self) -> Result<Vec<GpuPower>, String> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let ms = self.t0.elapsed().as_secs_f64() * 1000.0;
        Ok(vec![GpuPower {
            index: 0,
            power_mw: Some(self.watts as u32 * 1000),
            // W × ms = mJ
            energy_mj: self
                .counter
                .then(|| 1_000_000 + (self.watts as f64 * ms) as u64),
            temperature_c: Some(50),
            clock_events: Some(self.clock_events),
            power_limit_mw: Some(450_000),
        }])
    }

    fn driver_version(&self) -> Option<String> {
        Some("615.71.09".into())
    }
}

/// Runs `f` when dropped.
fn scopeguard(f: impl FnOnce()) -> impl Drop {
    struct Guard<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for Guard<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f()
            }
        }
    }
    Guard(Some(f))
}
