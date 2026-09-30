//! The lmgw client: everything this agent asks of the gateway, over `/v1`
//! with the agent token as the bearer.
//!
//! - [`Gateway::model_info`] — `GET /v1/models/{alias}`: `context_length` and
//!   `max_output_tokens`, each `None` when lmgw does not know it ("absent means
//!   unknown" is that route's contract, so it is never defaulted here).
//! - [`Gateway::connect_embedder`] — [`GatewayEmbedder`], the agent's own
//!   quickdoc `Embedder` on `/v1/embeddings` (it mirrors quickdoc's
//!   `HttpEmbedder`, plus the two things that one cannot know: the
//!   `x-lmgw-fallback` header, and the probe vector the index fingerprints
//!   its model with).
//! - [`GatewayReranker`] — quickdoc's `Reranker` over `/v1/rerank` in the Jina
//!   shape lmgw documents (`{model, query, documents}` in,
//!   `{results: [{index, relevance_score}]}` out). A rerank answered through
//!   a hold fallback is refused ([`RERANK_FALLBACK_PREFIX`]); the question
//!   then keeps the fused order ([`crate::chat::search`]).
//! - [`Gateway::chat_stream`] — `POST /v1/chat/completions` with
//!   `stream: true`, as a stream of [`ChatChunk`]s.
//! - [`Gateway::read_page`] — `POST /v1/chat/completions`, not streamed: one
//!   page image and a prompt to the vision alias ([`crate::vision`]).
//!
//! **A 503 whose error code is `gpu_hold`** is its own error,
//! [`GatewayError::GpuHold`], worded [`GPU_HOLD_MESSAGE`]: the owner has
//! paused local models on purpose, it is not an outage, and nothing here
//! retries it.
//!
//! **An embedding answered through a hold fallback is a hold too.** When the
//! embedding alias's local model is held and the owner configured a fallback,
//! lmgw answers `/v1/embeddings` with 200 and `x-lmgw-fallback: <alias>`:
//! vectors from another model (possibly a cloud one, i.e. the folder's text
//! left the machine to get them). [`GatewayEmbedder`] refuses those vectors
//! as [`GatewayError::EmbedFallback`], which counts as a hold
//! ([`GatewayError::is_gpu_hold`]): the sync stops rather than mixing two
//! models' vectors in one index, and a question is refused rather than
//! matched against the folder with a vector from the wrong model. **So is a
//! page reading answered through one** ([`GatewayError::VisionFallback`]):
//! the reading is never stored, and the sync stops.

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use futures::stream::BoxStream;
use futures::StreamExt;
use quickdoc_core::embed::{validate_vector, EmbedIdentity, Embedder, Reranker};
use quickdoc_core::QuickdocError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::GatewayEnv;

/// How a `gpu_hold` refusal is said to the owner.
pub const GPU_HOLD_MESSAGE: &str = "the GPU is held; local models are paused";

/// The `upstream` half of the embedding identity a folder's corpus is pinned
/// to. The alias is the other half: lmgw resolves aliases itself, so from in
/// here the name of "which model" is the alias the owner picked — and what
/// tells an alias re-pointed to another model of the same width apart is the
/// probe vector ([`EMBED_PROBE_TEXT`]) the index keeps beside it.
pub const EMBED_UPSTREAM_LABEL: &str = "lmgw";

/// lmgw's header naming the alias that actually answered when a held local
/// model fell back to a cloud one.
pub const FALLBACK_HEADER: &str = "x-lmgw-fallback";

/// The fixed text [`GatewayEmbedder::connect`] embeds to learn the vector
/// width, and whose vector the index keeps as its model's fingerprint
/// (`sync::PROBE_SAME_MODEL_MIN_COSINE`). The index records the text with the
/// vector, so changing it here re-records the fingerprint rather than
/// resetting every index.
pub const EMBED_PROBE_TEXT: &str =
    "folder-chat embedding probe: which model turns this sentence into a vector?";

/// How [`GatewayError::EmbedFallback`] begins — the one piece of its wording
/// that [`classify_quickdoc_error`] and a question's error recognise it by.
pub const EMBED_FALLBACK_PREFIX: &str = "lmgw answered with the fallback alias '";

/// How [`GatewayReranker`]'s refusal of a rerank answered through a hold
/// fallback begins — what [`rerank_fallback_alias`] recognises it by.
pub const RERANK_FALLBACK_PREFIX: &str = "lmgw answered /v1/rerank with the fallback alias '";

/// What follows the alias in that refusal.
const RERANK_FALLBACK_SUFFIX: &str = "' — the rerank model is held";

/// The fallback alias that answered a rerank, when `e` is
/// [`GatewayReranker`]'s refusal of one; `None` for any other error.
pub fn rerank_fallback_alias(e: &QuickdocError) -> Option<String> {
    match e {
        QuickdocError::Reranker(msg) => msg
            .strip_prefix(RERANK_FALLBACK_PREFIX)?
            .rsplit_once(RERANK_FALLBACK_SUFFIX)
            .map(|(alias, _)| alias.to_string()),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GatewayError {
    /// A 503 with error code `gpu_hold`.
    #[error("{GPU_HOLD_MESSAGE} (lmgw: {message})")]
    GpuHold { message: String },
    /// `/v1/embeddings` answered 200 with `x-lmgw-fallback`: the embedding
    /// model is held and another alias produced the vectors. A hold, never a
    /// result.
    #[error(
        "{EMBED_FALLBACK_PREFIX}{alias}' — the embedding model is held; the sync stops rather \
         than mixing models or sending files elsewhere"
    )]
    EmbedFallback { alias: String },
    /// A page reading ([`Gateway::read_page`]) answered 200 with
    /// `x-lmgw-fallback`: the vision model is held and another alias read the
    /// page. A hold, never a reading.
    #[error(
        "lmgw answered the page reading with the fallback alias '{alias}' — the vision model is \
         held; the reading is not stored and the sync stops (lmgw routed the page image to that \
         fallback before the agent could see it; pick a vision alias without a cloud fallback to \
         keep the folder on this machine)"
    )]
    VisionFallback { alias: String },
    /// Any other non-success answer, with lmgw's own error code and message.
    #[error("{url} answered {status}{}: {message}", code_suffix(.code))]
    Http {
        url: String,
        status: u16,
        code: Option<String>,
        message: String,
    },
    #[error("cannot reach lmgw at {url}: {message}")]
    Transport { url: String, message: String },
    /// lmgw answered, but not in a shape this client understands.
    #[error("unexpected answer from {url}: {message}")]
    Protocol { url: String, message: String },
}

fn code_suffix(code: &Option<String>) -> String {
    code.as_deref()
        .map(|c| format!(" ({c})"))
        .unwrap_or_default()
}

impl GatewayError {
    /// A `gpu_hold` refusal, or an embedding or a page reading answered by a
    /// hold fallback.
    pub fn is_gpu_hold(&self) -> bool {
        matches!(
            self,
            Self::GpuHold { .. } | Self::EmbedFallback { .. } | Self::VisionFallback { .. }
        )
    }

    /// How a hold is said where a sync stops: the fallback's own words for a
    /// fallback, [`GPU_HOLD_MESSAGE`] otherwise.
    pub fn hold_reason(&self) -> String {
        match self {
            Self::EmbedFallback { .. } | Self::VisionFallback { .. } => self.to_string(),
            _ => GPU_HOLD_MESSAGE.to_string(),
        }
    }

    /// The HTTP status of a non-success answer, when that is what this is.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// A refusal that is about this one input (too long, malformed) rather
    /// than about the gateway — the sync reports the file and carries on.
    pub fn is_per_input(&self) -> bool {
        matches!(self, Self::Http { status, .. } if matches!(status, 400 | 413 | 422))
    }
}

/// Build the error for a non-success answer. lmgw speaks the OpenAI error
/// shape on `/v1`: `{"error": {"message", "type", "code"}}`.
pub fn error_from_body(url: &str, status: u16, body: &str) -> GatewayError {
    let v: Option<Value> = serde_json::from_str(body).ok();
    let err = v.as_ref().and_then(|v| v.get("error"));
    let code = err
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .map(String::from);
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| body.trim().to_string());
    if status == 503 && code.as_deref() == Some("gpu_hold") {
        return GatewayError::GpuHold { message };
    }
    GatewayError::Http {
        url: url.to_string(),
        status,
        code,
        message,
    }
}

/// Recover the gateway error inside a quickdoc embedder error.
///
/// An embedder reports a non-success answer as `"<status line>: <body>"` and
/// a failed send as `"POST <url>/embeddings: <reason>"` (quickdoc's
/// `HttpEmbedder` and [`GatewayEmbedder`] alike, see [`to_quickdoc`]); a
/// fallback answer is [`GatewayError::EmbedFallback`]'s own text; quickdoc's
/// own checks (a zero vector, a wrong width, a wrong count) are other
/// variants. This is the one place that reads that text back, so a change in
/// its wording breaks one function and its test, not the sync.
pub fn classify_quickdoc_error(api_base: &str, e: &QuickdocError) -> GatewayError {
    let url = format!("{api_base}/embeddings");
    if let QuickdocError::Embedder(msg) = e {
        if let Some(rest) = msg.strip_prefix(EMBED_FALLBACK_PREFIX) {
            let alias = rest
                .rsplit_once("' — the embedding model is held")
                .map_or(rest, |(a, _)| a);
            return GatewayError::EmbedFallback {
                alias: alias.to_string(),
            };
        }
        let status = msg
            .get(..3)
            .and_then(|s| s.parse::<u16>().ok())
            .filter(|s| (100..=599).contains(s));
        if let (Some(status), Some((_, body))) = (status, msg.split_once(": ")) {
            return error_from_body(&url, status, body);
        }
        if msg.starts_with("POST ") {
            return GatewayError::Transport {
                url,
                message: msg.clone(),
            };
        }
    }
    GatewayError::Protocol {
        url,
        message: e.to_string(),
    }
}

/// A gateway error as the `Embedder` trait has to carry it — a quickdoc
/// error whose text [`classify_quickdoc_error`] turns back into the same
/// [`GatewayError`] (minus the URL, which the caller knows).
pub fn to_quickdoc(e: &GatewayError) -> QuickdocError {
    let body = |code: Option<&str>, message: &str| {
        json!({ "error": { "message": message, "code": code } }).to_string()
    };
    QuickdocError::Embedder(match e {
        GatewayError::GpuHold { message } => {
            format!(
                "503 Service Unavailable: {}",
                body(Some("gpu_hold"), message)
            )
        }
        GatewayError::Http {
            status,
            code,
            message,
            ..
        } => format!("{status}: {}", body(code.as_deref(), message)),
        GatewayError::Transport { url, message } => format!("POST {url}: {message}"),
        GatewayError::Protocol { url, message } => {
            format!("unexpected answer from {url}: {message}")
        }
        GatewayError::EmbedFallback { .. } | GatewayError::VisionFallback { .. } => e.to_string(),
    })
}

/// What `/v1/models/{id}` knows about one alias.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub alias: String,
    pub context_length: Option<u64>,
    pub max_output_tokens: Option<u64>,
}

/// One chat message on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireMessage {
    pub role: String,
    pub content: String,
}

/// Token counts as the upstream reported them — each `None` when it did not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

/// One piece of a streaming answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatChunk {
    /// Answer text, in order.
    Text {
        text: String,
    },
    /// A reasoning model's thinking (`reasoning_content`), kept apart from the
    /// answer so a UI can fold it away rather than lose it.
    Reasoning {
        text: String,
    },
    /// `stop`, `length` (the context ran out), …
    Finish {
        reason: String,
    },
    Usage {
        usage: Usage,
    },
}

/// What the vision alias answered for one page ([`Gateway::read_page`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageReading {
    /// `message.content`, as sent — a reasoning alias's thinking is not in it.
    pub text: String,
    /// `stop`, `length` (the model's context ran out mid-reading), …; `None`
    /// when the server gave none.
    pub finish_reason: Option<String>,
}

/// A streaming answer as it begins.
pub struct ChatStream {
    /// `x-lmgw-fallback`: set when the requested (held) local model fell back
    /// to another alias, which is then what actually answered.
    pub fallback: Option<String>,
    pub chunks: BoxStream<'static, Result<ChatChunk, GatewayError>>,
}

#[derive(Clone)]
pub struct Gateway {
    http: reqwest::Client,
    env: GatewayEnv,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway").field("env", &self.env).finish()
    }
}

impl Gateway {
    /// No request timeout: a local model's first call waits for its container
    /// to load weights, which takes as long as the file is big, and a clock
    /// here would turn that into a false failure. The owner sees a slow
    /// answer; a stuck one is cancelled by closing the request.
    pub fn new(env: GatewayEnv) -> Self {
        Self {
            http: reqwest::Client::new(),
            env,
        }
    }

    pub fn api_base(&self) -> &str {
        &self.env.api_base
    }

    fn token(&self) -> Option<&str> {
        self.env.token.as_deref()
    }

    fn authed(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.token() {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }

    /// Send, and turn a non-success answer into its error.
    async fn send(
        &self,
        url: &str,
        rb: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, GatewayError> {
        let resp = self.send_raw(url, rb).await?;
        checked(url, resp).await
    }

    /// Send, with only a failed send as an error: the answer comes back
    /// whatever its status, so a caller can look at its headers first.
    async fn send_raw(
        &self,
        url: &str,
        rb: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, GatewayError> {
        self.authed(rb)
            .send()
            .await
            .map_err(|e| GatewayError::Transport {
                url: url.to_string(),
                message: e.to_string(),
            })
    }

    async fn json(&self, url: &str, rb: reqwest::RequestBuilder) -> Result<Value, GatewayError> {
        let resp = self.send(url, rb).await?;
        resp.json::<Value>()
            .await
            .map_err(|e| GatewayError::Protocol {
                url: url.to_string(),
                message: format!("not JSON: {e}"),
            })
    }

    /// `GET /v1/models/{alias}`.
    pub async fn model_info(&self, alias: &str) -> Result<ModelInfo, GatewayError> {
        let url = format!("{}/models/{}", self.api_base(), encode_path(alias));
        let v = self.json(&url, self.http.get(&url)).await?;
        Ok(ModelInfo {
            alias: alias.to_string(),
            context_length: v.get("context_length").and_then(Value::as_u64),
            max_output_tokens: v.get("max_output_tokens").and_then(Value::as_u64),
        })
    }

    /// The embedder on this gateway. Connecting embeds [`EMBED_PROBE_TEXT`]
    /// to measure the vector width, so a held GPU (or a fallback answer)
    /// surfaces here already.
    pub async fn connect_embedder(
        &self,
        alias: &str,
    ) -> Result<Arc<GatewayEmbedder>, GatewayError> {
        GatewayEmbedder::connect(self, alias).await.map(Arc::new)
    }

    /// `POST /v1/embeddings` for `texts`, in order. An answer that carries
    /// [`FALLBACK_HEADER`] — whatever its status: lmgw sets it on an error
    /// from the fallback too — is [`GatewayError::EmbedFallback`], and
    /// nothing in it is looked at.
    async fn embeddings(
        &self,
        alias: &str,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, GatewayError> {
        let url = format!("{}/embeddings", self.api_base());
        let body = json!({ "model": alias, "input": texts });
        let resp = self
            .send_raw(&url, self.http.post(&url).json(&body))
            .await?;
        if let Some(alias) = fallback_alias(&resp) {
            return Err(GatewayError::EmbedFallback { alias });
        }
        let resp = checked(&url, resp).await?;
        let protocol = |message: String| GatewayError::Protocol {
            url: url.clone(),
            message,
        };
        let v: Value = resp
            .json()
            .await
            .map_err(|e| protocol(format!("not JSON: {e}")))?;
        let data = v
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| protocol("no 'data' array".into()))?;
        // Placed by `index` when the server gives one (OpenAI does, in
        // order); by position otherwise.
        let mut rows: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
        for (pos, d) in data.iter().enumerate() {
            let at = d
                .get("index")
                .and_then(Value::as_u64)
                .map_or(pos, |i| i as usize);
            let emb = d
                .get("embedding")
                .and_then(Value::as_array)
                .ok_or_else(|| protocol(format!("data[{pos}] has no 'embedding' array")))?;
            let vector = emb
                .iter()
                .map(|f| f.as_f64().map(|f| f as f32))
                .collect::<Option<Vec<f32>>>()
                .ok_or_else(|| protocol(format!("data[{pos}].embedding is not all numbers")))?;
            rows.push((at, vector));
        }
        rows.sort_by_key(|(i, _)| *i);
        Ok(rows.into_iter().map(|(_, v)| v).collect())
    }

    pub fn reranker(&self, alias: &str) -> GatewayReranker {
        GatewayReranker {
            gateway: self.clone(),
            model: alias.to_string(),
        }
    }

    /// `POST /v1/chat/completions`, streamed. No `max_tokens` is sent: the
    /// answer is bounded by the model's own context, and a guessed ceiling
    /// here would cut answers off invisibly.
    pub async fn chat_stream(
        &self,
        alias: &str,
        messages: &[WireMessage],
    ) -> Result<ChatStream, GatewayError> {
        let url = format!("{}/chat/completions", self.api_base());
        let body = json!({
            "model": alias,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        let resp = self.send(&url, self.http.post(&url).json(&body)).await?;
        let fallback = resp
            .headers()
            .get(FALLBACK_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let bytes: BoxStream<'static, reqwest::Result<Vec<u8>>> =
            resp.bytes_stream().map(|r| r.map(|b| b.to_vec())).boxed();
        let state = SseState {
            bytes,
            decoder: SseDecoder::default(),
            pending: VecDeque::new(),
            done: false,
            url,
        };
        let chunks = futures::stream::unfold(state, |mut st| async move {
            loop {
                if let Some(item) = st.pending.pop_front() {
                    return Some((item, st));
                }
                if st.done {
                    return None;
                }
                match st.bytes.next().await {
                    Some(Ok(b)) => {
                        for data in st.decoder.push(&b) {
                            st.accept(&data);
                        }
                    }
                    Some(Err(e)) => {
                        st.done = true;
                        st.pending.push_back(Err(GatewayError::Transport {
                            url: st.url.clone(),
                            message: format!("the answer stream broke off: {e}"),
                        }));
                    }
                    None => {
                        // A final event without its blank line still counts.
                        for data in st.decoder.finish() {
                            st.accept(&data);
                        }
                        st.done = true;
                    }
                }
            }
        })
        .boxed();
        Ok(ChatStream { fallback, chunks })
    }
}

impl Gateway {
    /// `POST /v1/chat/completions` to the vision alias: one user message of
    /// the page image (`data:image/png;base64,…`) and `prompt`, `temperature`
    /// 0 so the same page reads the same way twice, not streamed. No
    /// `max_tokens`: a reading is as long as the page, and a guessed ceiling
    /// would cut a dense page off invisibly — a reading the model's own
    /// context stops says so in its `finish_reason`, which the caller checks.
    ///
    /// An answer that carries [`FALLBACK_HEADER`] — whatever its status: lmgw
    /// sets it on an error from the fallback too, and taking that for a page
    /// failure would send every later page to the fallback — is
    /// [`GatewayError::VisionFallback`], and nothing in it is looked at.
    pub async fn read_page(
        &self,
        alias: &str,
        png: &[u8],
        prompt: &str,
    ) -> Result<PageReading, GatewayError> {
        let url = format!("{}/chat/completions", self.api_base());
        let image = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png)
        );
        let body = json!({
            "model": alias,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "image_url", "image_url": { "url": image } },
                    { "type": "text", "text": prompt },
                ],
            }],
            "temperature": 0,
            "stream": false,
        });
        let resp = self
            .send_raw(&url, self.http.post(&url).json(&body))
            .await?;
        if let Some(alias) = fallback_alias(&resp) {
            return Err(GatewayError::VisionFallback { alias });
        }
        let resp = checked(&url, resp).await?;
        let protocol = |message: String| GatewayError::Protocol {
            url: url.clone(),
            message,
        };
        let v: Value = resp
            .json()
            .await
            .map_err(|e| protocol(format!("not JSON: {e}")))?;
        let choice = v
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .ok_or_else(|| protocol("no 'choices' in the answer".into()))?;
        let content = choice.get("message").and_then(|m| m.get("content"));
        // A string, as llama-server and OpenAI answer; an array of parts is
        // read for its text parts.
        let text = match content {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
            Some(Value::Null) | None => String::new(),
            Some(other) => {
                return Err(protocol(format!(
                    "choices[0].message.content is neither text nor parts: {other}"
                )))
            }
        };
        Ok(PageReading {
            text,
            finish_reason: choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .map(String::from),
        })
    }
}

/// A non-success answer as its error; a success as it is.
async fn checked(url: &str, resp: reqwest::Response) -> Result<reqwest::Response, GatewayError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(error_from_body(url, status.as_u16(), &body))
}

/// The alias [`FALLBACK_HEADER`] names, when the answer carries it.
fn fallback_alias(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get(FALLBACK_HEADER)
        .map(|fb| fb.to_str().unwrap_or("(not text)").to_string())
}

struct SseState {
    bytes: BoxStream<'static, reqwest::Result<Vec<u8>>>,
    decoder: SseDecoder,
    pending: VecDeque<Result<ChatChunk, GatewayError>>,
    done: bool,
    url: String,
}

impl SseState {
    fn accept(&mut self, data: &str) {
        if self.done {
            return;
        }
        for item in parse_chat_event(&self.url, data) {
            let stop = matches!(item, Ok(ChatStop::Done) | Err(_));
            match item {
                Ok(ChatStop::Chunk(c)) => self.pending.push_back(Ok(c)),
                Ok(ChatStop::Done) => {}
                Err(e) => self.pending.push_back(Err(e)),
            }
            if stop {
                self.done = true;
                return;
            }
        }
    }
}

enum ChatStop {
    Chunk(ChatChunk),
    Done,
}

/// One SSE `data:` payload of an OpenAI chat stream → what it means.
fn parse_chat_event(url: &str, data: &str) -> Vec<Result<ChatStop, GatewayError>> {
    if data.trim() == "[DONE]" {
        return vec![Ok(ChatStop::Done)];
    }
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            return vec![Err(GatewayError::Protocol {
                url: url.to_string(),
                message: format!("a stream event is not JSON ({e}): {data}"),
            })]
        }
    };
    // lmgw reports an upstream failure mid-stream as an `error` frame; a
    // gpu_hold can arrive this way too if the hold lands between two calls.
    if let Some(err) = v.get("error") {
        let body = json!({ "error": err }).to_string();
        let status = if err.get("code").and_then(Value::as_str) == Some("gpu_hold") {
            503
        } else {
            502
        };
        return vec![Err(error_from_body(url, status, &body))];
    }
    let mut out = Vec::new();
    if let Some(choice) = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
    {
        let delta = choice.get("delta");
        let text = |k: &str| {
            delta
                .and_then(|d| d.get(k))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        if let Some(t) = text("reasoning_content") {
            out.push(Ok(ChatStop::Chunk(ChatChunk::Reasoning { text: t })));
        }
        if let Some(t) = text("content") {
            out.push(Ok(ChatStop::Chunk(ChatChunk::Text { text: t })));
        }
        if let Some(r) = choice.get("finish_reason").and_then(Value::as_str) {
            out.push(Ok(ChatStop::Chunk(ChatChunk::Finish {
                reason: r.to_string(),
            })));
        }
    }
    if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
        let n = |k: &str| u.get(k).and_then(Value::as_u64);
        out.push(Ok(ChatStop::Chunk(ChatChunk::Usage {
            usage: Usage {
                prompt_tokens: n("prompt_tokens"),
                completion_tokens: n("completion_tokens"),
                total_tokens: n("total_tokens"),
            },
        })));
    }
    out
}

/// A minimal Server-Sent Events decoder: bytes in, `data` payloads out.
/// Events end at a blank line; several `data:` lines in one event are joined
/// with `\n`, as the SSE spec says; comments and other fields are ignored.
#[derive(Default)]
struct SseDecoder {
    buf: Vec<u8>,
    data: Vec<String>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=i).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            self.line(&line, &mut out);
        }
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<String>) {
        if line.is_empty() {
            if !self.data.is_empty() {
                out.push(std::mem::take(&mut self.data).join("\n"));
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            self.data
                .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
    }

    fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
            self.line(rest.trim_end_matches('\r'), &mut out);
        }
        self.line("", &mut out);
        out
    }
}

/// Percent-encode an alias for the `/v1/models/{*id}` path. `/` stays: the
/// route is a catch-all precisely so `embed/bge-m3` works as written.
fn encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// quickdoc's `Embedder` over lmgw's `/v1/embeddings`, as the agent needs it.
///
/// Mirrors quickdoc's `HttpEmbedder` — the same identity (`lmgw/<alias>
/// (<dims>d)`, so an index built with either opens with either), the same
/// count and vector checks — and adds what that one cannot know about lmgw:
///
/// - a response with [`FALLBACK_HEADER`] is refused as
///   [`GatewayError::EmbedFallback`] (a hold), never used;
/// - the probe it embeds at connect time is kept ([`GatewayEmbedder::probe`])
///   for the index's model fingerprint, so an alias re-pointed to another
///   model of the same width is noticed.
///
/// No request timeout, for the reason [`Gateway::new`] gives.
#[derive(Debug, Clone)]
pub struct GatewayEmbedder {
    gateway: Gateway,
    alias: String,
    identity: EmbedIdentity,
    /// `None` for one built by [`GatewayEmbedder::unprobed`].
    probe: Option<Vec<f32>>,
}

impl GatewayEmbedder {
    /// Embed [`EMBED_PROBE_TEXT`], measuring the width and keeping the
    /// vector. A held GPU, a fallback answer, or a vector quickdoc would
    /// refuse (empty, all zeros — what a reranker answers) is an error here.
    pub async fn connect(gateway: &Gateway, alias: &str) -> Result<Self, GatewayError> {
        let probe = gateway
            .embeddings(alias, &[EMBED_PROBE_TEXT.to_string()])
            .await?
            .into_iter()
            .next()
            .unwrap_or_default();
        let protocol = |message: String| GatewayError::Protocol {
            url: format!("{}/embeddings", gateway.api_base()),
            message,
        };
        if probe.is_empty() {
            return Err(protocol(format!(
                "{alias} returned no vector for the probe"
            )));
        }
        validate_vector(alias, 0, &probe, probe.len()).map_err(|e| protocol(e.to_string()))?;
        Ok(Self {
            gateway: gateway.clone(),
            alias: alias.to_string(),
            identity: EmbedIdentity::new(EMBED_UPSTREAM_LABEL, alias, probe.len()),
            probe: Some(probe),
        })
    }

    /// The embedder for `alias` at a width the caller already knows — the
    /// index's own corpus row — built without a request, so without a probe.
    /// For loading an index while the embedding model is held and the probe
    /// cannot run ([`crate::FolderChat::load_existing`]): nothing has checked
    /// which model the alias names now, so the agent probes again before it
    /// trusts it, and meanwhile a question's own embedding reports the hold.
    pub fn unprobed(gateway: &Gateway, alias: &str, dims: usize) -> Self {
        Self {
            gateway: gateway.clone(),
            alias: alias.to_string(),
            identity: EmbedIdentity::new(EMBED_UPSTREAM_LABEL, alias, dims),
            probe: None,
        }
    }

    /// The vector of [`EMBED_PROBE_TEXT`] as this model answered it when
    /// connecting; `None` for an [`GatewayEmbedder::unprobed`] one.
    pub fn probe(&self) -> Option<&[f32]> {
        self.probe.as_deref()
    }
}

#[async_trait]
impl Embedder for GatewayEmbedder {
    fn identity(&self) -> EmbedIdentity {
        self.identity.clone()
    }

    async fn embed(&self, texts: &[String]) -> quickdoc_core::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let out = self
            .gateway
            .embeddings(&self.alias, texts)
            .await
            .map_err(|e| to_quickdoc(&e))?;
        if out.len() != texts.len() {
            return Err(QuickdocError::EmbedCount {
                want: texts.len(),
                got: out.len(),
            });
        }
        for (i, v) in out.iter().enumerate() {
            validate_vector(&self.identity.model, i, v, self.identity.dims)?;
        }
        Ok(out)
    }
}

/// quickdoc's rerank stage over lmgw's `/v1/rerank`.
#[derive(Debug, Clone)]
pub struct GatewayReranker {
    gateway: Gateway,
    model: String,
}

#[async_trait]
impl Reranker for GatewayReranker {
    fn model(&self) -> String {
        self.model.clone()
    }

    /// Every document is scored — no `top_n` is sent, because the retriever
    /// already decided how deep the window is (`k_rerank`) and a second cut
    /// here would be invisible in its trace.
    ///
    /// A 200 with [`FALLBACK_HEADER`] is refused ([`RERANK_FALLBACK_PREFIX`],
    /// read back by [`rerank_fallback_alias`]) and its scores are never
    /// looked at: the rerank model is held and another alias scored the
    /// excerpts. By then they have reached that alias — lmgw routed the
    /// request before anything here could see it — which is what the
    /// question's note says ([`crate::chat::search`]).
    async fn rerank(&self, query: &str, documents: &[String]) -> quickdoc_core::Result<Vec<f32>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        let g = &self.gateway;
        let url = format!("{}/rerank", g.api_base());
        let body = json!({ "model": self.model, "query": query, "documents": documents });
        let resp = g
            .send(&url, g.http.post(&url).json(&body))
            .await
            .map_err(|e| QuickdocError::Reranker(e.to_string()))?;
        if let Some(fb) = resp.headers().get(FALLBACK_HEADER) {
            return Err(QuickdocError::Reranker(format!(
                "{RERANK_FALLBACK_PREFIX}{}{RERANK_FALLBACK_SUFFIX}",
                fb.to_str().unwrap_or("(not text)")
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| QuickdocError::Reranker(format!("{url}: not JSON: {e}")))?;
        let rows = v
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| QuickdocError::Reranker(format!("{url}: no 'results' array")))?;
        let mut scores: Vec<Option<f32>> = vec![None; documents.len()];
        for r in rows {
            let i = r.get("index").and_then(Value::as_u64).map(|i| i as usize);
            let s = r
                .get("relevance_score")
                .or_else(|| r.get("score"))
                .and_then(Value::as_f64);
            if let (Some(i), Some(s)) = (i, s) {
                if let Some(slot) = scores.get_mut(i) {
                    *slot = Some(s as f32);
                }
            }
        }
        let got = scores.iter().filter(|s| s.is_some()).count();
        if got != documents.len() {
            return Err(QuickdocError::RerankCount {
                want: documents.len(),
                got,
            });
        }
        Ok(scores.into_iter().flatten().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_hold_is_recognised_in_the_embedder_error_text() {
        let body = r#"{"error":{"message":"local models are held","type":"api_error","param":null,"code":"gpu_hold"}}"#;
        let e = QuickdocError::Embedder(format!("503 Service Unavailable: {body}"));
        let g = classify_quickdoc_error("http://x/v1", &e);
        assert!(g.is_gpu_hold(), "{g:?}");
        assert!(g.to_string().starts_with(GPU_HOLD_MESSAGE));

        let e = QuickdocError::Embedder(
            r#"400 Bad Request: {"error":{"message":"input too long","code":"upstream"}}"#.into(),
        );
        assert!(classify_quickdoc_error("http://x/v1", &e).is_per_input());

        let e = QuickdocError::Embedder("POST http://x/v1/embeddings: connection refused".into());
        assert!(matches!(
            classify_quickdoc_error("http://x/v1", &e),
            GatewayError::Transport { .. }
        ));
    }

    #[test]
    fn gateway_errors_survive_the_trip_through_quickdoc() {
        let base = "http://x/v1";
        for e in [
            GatewayError::GpuHold {
                message: "held".into(),
            },
            GatewayError::EmbedFallback {
                alias: "openai/text-embedding-3-small".into(),
            },
            GatewayError::Http {
                url: format!("{base}/embeddings"),
                status: 500,
                code: Some("upstream_error".into()),
                message: "boom".into(),
            },
            GatewayError::Transport {
                url: format!("{base}/embeddings"),
                message: "connection refused".into(),
            },
        ] {
            let back = classify_quickdoc_error(base, &to_quickdoc(&e));
            match (&e, &back) {
                (GatewayError::Transport { .. }, GatewayError::Transport { message, .. }) => {
                    assert!(message.contains("connection refused"))
                }
                _ => assert_eq!(back, e),
            }
        }
        let fb = GatewayError::EmbedFallback { alias: "x".into() };
        assert!(fb.is_gpu_hold());
        assert!(fb.hold_reason().starts_with(EMBED_FALLBACK_PREFIX));
        let vf = GatewayError::VisionFallback {
            alias: "openai/gpt-5".into(),
        };
        assert!(vf.is_gpu_hold());
        assert!(vf.hold_reason().contains("'openai/gpt-5'"), "{vf}");
        assert!(vf.hold_reason().contains("not stored"), "{vf}");
        assert!(fb
            .to_string()
            .ends_with("the sync stops rather than mixing models or sending files elsewhere"));
    }

    #[test]
    fn sse_frames_split_anywhere_still_decode() {
        let wire = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\r\n\r\n: comment\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        for cut in 1..wire.len() {
            let mut d = SseDecoder::default();
            let mut events = d.push(&wire.as_bytes()[..cut]);
            events.extend(d.push(&wire.as_bytes()[cut..]));
            events.extend(d.finish());
            assert_eq!(events.len(), 3, "cut at {cut}: {events:?}");
            assert_eq!(events[2], "[DONE]");
        }
    }

    #[test]
    fn a_rerank_fallback_is_recognised_by_its_alias_and_nothing_else_is() {
        let e = QuickdocError::Reranker(format!(
            "{RERANK_FALLBACK_PREFIX}cohere/rerank-v3{RERANK_FALLBACK_SUFFIX}"
        ));
        assert_eq!(
            rerank_fallback_alias(&e).as_deref(),
            Some("cohere/rerank-v3")
        );
        let held = QuickdocError::Reranker(
            GatewayError::GpuHold {
                message: "held".into(),
            }
            .to_string(),
        );
        assert_eq!(rerank_fallback_alias(&held), None);
        let embed =
            QuickdocError::Embedder(format!("{RERANK_FALLBACK_PREFIX}x{RERANK_FALLBACK_SUFFIX}"));
        assert_eq!(rerank_fallback_alias(&embed), None);
    }

    #[test]
    fn aliases_keep_their_slash() {
        assert_eq!(encode_path("embed/bge-m3"), "embed/bge-m3");
        assert_eq!(encode_path("a b?"), "a%20b%3F");
    }
}
