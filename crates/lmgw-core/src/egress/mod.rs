//! Upstream protocol adapters (§7). One isolated module per protocol; all
//! implement [`Egress`]. N+M translation: ingress→IR here→provider wire.

pub mod anthropic;
pub mod gemini;
pub mod llama_cpp;
pub mod openai;
pub mod openai_wire;

use crate::config::{Protocol, Upstream};
use crate::error::GatewayError;
use crate::ir::{
    ChatRequest, Completion, EmbeddingsRequest, EmbeddingsResponse, FinishReason, Params,
    RerankRequest, RerankResponse, StreamDelta,
};
use crate::sse::SseEvent;

pub trait Egress: Send + Sync {
    fn proto(&self) -> Protocol;

    /// Build the upstream chat request (non-stream or SSE).
    fn build_chat(
        &self,
        http: &reqwest::Client,
        up: &Upstream,
        model: &str,
        ir: &ChatRequest,
        params: &Params,
        stream: bool,
    ) -> Result<reqwest::RequestBuilder, GatewayError>;

    /// Parse a non-streaming success body.
    fn parse_completion(&self, body: &[u8]) -> Result<Completion, GatewayError>;

    /// Stateful SSE → IR-delta decoder for one streamed response.
    fn new_decoder(&self) -> Box<dyn EgressStreamDecoder>;

    /// Map an upstream error body to a GatewayError with provider detail.
    fn map_error(&self, status: u16, body: &[u8]) -> GatewayError;

    fn build_embeddings(
        &self,
        _http: &reqwest::Client,
        _up: &Upstream,
        _model: &str,
        _req: &EmbeddingsRequest,
    ) -> Result<reqwest::RequestBuilder, GatewayError> {
        Err(GatewayError::Unsupported(format!(
            "embeddings via {} upstream",
            self.proto().as_str()
        )))
    }

    fn parse_embeddings(&self, _body: &[u8]) -> Result<EmbeddingsResponse, GatewayError> {
        Err(GatewayError::Unsupported("embeddings".into()))
    }

    /// Build a cross-encoder rerank request (quickdoc §9a): the Jina shape at
    /// `/v1/rerank`, which llama.cpp, vLLM, TEI, Infinity and Jina serve.
    /// Providers that have no such endpoint inherit the refusal below rather
    /// than being sent a request they would answer with something else.
    fn build_rerank(
        &self,
        _http: &reqwest::Client,
        _up: &Upstream,
        _model: &str,
        _req: &RerankRequest,
    ) -> Result<reqwest::RequestBuilder, GatewayError> {
        Err(GatewayError::Unsupported(format!(
            "reranking via {} upstream",
            self.proto().as_str()
        )))
    }

    fn parse_rerank(&self, _body: &[u8]) -> Result<RerankResponse, GatewayError> {
        Err(GatewayError::Unsupported("reranking".into()))
    }

    /// Plan how to count the tokens of `text` against `model` on this upstream
    /// (§6). Each protocol uses its native mechanism: llama.cpp `/tokenize`,
    /// Anthropic `/v1/messages/count_tokens`, Gemini `:countTokens`, or — for
    /// OpenAI and OpenAI-compatible providers, which expose no token
    /// endpoint — a local tiktoken count. Returns [`CountPlan::Ready`] when counted locally with
    /// the model's own encoding, [`CountPlan::Guessed`] when counted locally
    /// with a stand-in, or [`CountPlan::Request`] (then
    /// [`parse_count`](Egress::parse_count) reads the count from the
    /// response).
    fn build_count_tokens(
        &self,
        _http: &reqwest::Client,
        _up: &Upstream,
        _model: &str,
        _text: &str,
    ) -> Result<CountPlan, GatewayError> {
        Err(GatewayError::Unsupported(format!(
            "token counting via {} upstream",
            self.proto().as_str()
        )))
    }

    /// Read the token count from a [`CountPlan::Request`] success body.
    fn parse_count(&self, _body: &[u8]) -> Result<u64, GatewayError> {
        Err(GatewayError::Unsupported("token counting".into()))
    }
}

/// How to obtain a token count for a string on a given upstream.
pub enum CountPlan {
    /// Already counted locally (no upstream call needed), e.g. tiktoken for
    /// real OpenAI upstreams.
    Ready(u64),
    /// Counted locally, but with a tokenizer lmgw only **guessed** the model
    /// uses: tiktoken does not know the model's name, so `o200k_base` stood
    /// in. A separate variant rather than a debug line, because the counter
    /// has to say so on the response (api-docs design §5.1,
    /// `x-lmgw-count-approximate: tokenizer_guess`) — a number from a
    /// different tokenizer than the backend's is a silently wrong context
    /// window otherwise.
    Guessed(u64),
    /// Send this request; [`Egress::parse_count`] reads the count from the
    /// success body.
    Request(Box<reqwest::RequestBuilder>),
}

pub trait EgressStreamDecoder: Send {
    /// Translate one upstream SSE event into zero or more IR deltas.
    fn on_event(&mut self, ev: &SseEvent) -> Vec<StreamDelta>;
}

static OPENAI: openai::OpenaiEgress = openai::OpenaiEgress;
static LLAMA_CPP: llama_cpp::LlamaCppEgress = llama_cpp::LlamaCppEgress;
static ANTHROPIC: anthropic::AnthropicEgress = anthropic::AnthropicEgress;
static GEMINI: gemini::GeminiEgress = gemini::GeminiEgress;

pub fn for_protocol(p: Protocol) -> &'static dyn Egress {
    match p {
        Protocol::Openai => &OPENAI,
        Protocol::Anthropic => &ANTHROPIC,
        Protocol::Gemini => &GEMINI,
        Protocol::LlamaCpp => &LLAMA_CPP,
    }
}

/// Apply upstream extra headers to a request builder.
pub(crate) fn apply_extra_headers(
    mut rb: reqwest::RequestBuilder,
    up: &Upstream,
) -> reqwest::RequestBuilder {
    for (name, value) in &up.extra_headers {
        rb = rb.header(name, value);
    }
    rb
}

/// Bearer key + extra headers — the auth convention of every OpenAI-shaped
/// upstream. The OpenAI and llama.cpp egresses use it for chat, embeddings,
/// rerank, `/tokenize` and `/props`, and the raw passthrough routes in `proxy/legacy.rs` (`/v1/completions`) and
/// `proxy/audio.rs` (`/v1/audio/*`) use it too, so a change of auth style
/// can't miss one of them.
pub(crate) fn apply_bearer_auth(
    mut rb: reqwest::RequestBuilder,
    up: &Upstream,
) -> reqwest::RequestBuilder {
    if let Some(key) = up.api_key.as_deref().filter(|k| !k.is_empty()) {
        rb = rb.bearer_auth(key);
    }
    apply_extra_headers(rb, up)
}

/// Apply a per-request deadline to a builder, or leave it without one.
///
/// `None` is not "zero" and not "some default reqwest picks" — it is the
/// absence of a deadline, which is what `timeout_ms = 0` means
/// ([`Upstream::request_timeout`](crate::config::Upstream::request_timeout)).
/// A helper rather than an `if` at each call site because there are eight of
/// them and the `else` branch — *don't* call `.timeout()` — is the one a
/// hurried edit gets wrong.
pub(crate) fn with_timeout(
    rb: reqwest::RequestBuilder,
    timeout: Option<std::time::Duration>,
) -> reqwest::RequestBuilder {
    match timeout {
        Some(d) => rb.timeout(d),
        None => rb,
    }
}

/// Normalized finish-reason mapping helpers shared by adapters.
pub(crate) fn finish_from_openai(s: &str) -> FinishReason {
    match s {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolUse,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_string()),
    }
}

pub(crate) fn finish_from_anthropic(s: &str) -> FinishReason {
    match s {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolUse,
        "refusal" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_string()),
    }
}

pub(crate) fn finish_from_gemini(s: &str, saw_tool_call: bool) -> FinishReason {
    match s {
        "STOP" => {
            if saw_tool_call {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            }
        }
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "RECITATION" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_ascii_lowercase()),
    }
}
