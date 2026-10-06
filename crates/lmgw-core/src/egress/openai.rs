//! The OpenAI egress (§7): OpenAI and OpenAI-compatible providers, a near
//! pass-through composed from [`super::openai_wire`]. llama.cpp's servers
//! have an egress of their own ([`super::llama_cpp`], llama egress design
//! §3); this one speaks only OpenAI's vocabulary.

use serde_json::{json, Map, Value};

use super::openai_wire::{self, FlattenToolResults, ReasoningStep};
pub(crate) use super::openai_wire::{has_reasoning_object, parse_timings};
pub use super::openai_wire::{messages_json, OpenaiDecoder};
use crate::config::{Protocol, Upstream};
use crate::egress::{CountPlan, Egress, EgressStreamDecoder};
use crate::error::GatewayError;
use crate::ir::{
    ChatRequest, Completion, EmbeddingsRequest, EmbeddingsResponse, Params, ReasoningControl,
    RerankRequest, RerankResponse,
};

pub struct OpenaiEgress;

/// This egress's [`ReasoningStep`]: [`apply_reasoning`] before the
/// passthrough loop, and nothing after it.
struct OpenaiReasoning;

impl ReasoningStep for OpenaiReasoning {
    fn apply(&self, body: &mut Map<String, Value>, ir: &ChatRequest, c: &ReasoningControl) {
        apply_reasoning(body, ir, c);
    }

    /// Nothing to reconcile: the only reasoning key this vocabulary has,
    /// `reasoning_effort`, is set before the loop and wins over the client's
    /// copy. Every other key the client sent goes verbatim like any unknown
    /// key — llama-server's `thinking_budget_tokens` included, which this
    /// egress rewrote to the resolved budget until llama.cpp got its own
    /// (llama egress design I2). OpenRouter's `reasoning` object is the
    /// shared builder's to reconcile.
    fn reconcile(&self, _body: &mut Map<String, Value>, _c: &ReasoningControl) {}
}

/// Render the normalised reasoning triple (§5.3) into the OpenAI body.
///
/// An OpenAI-protocol provider knows `reasoning_effort` alone, with `"none"`
/// as that vocabulary's own way of saying off. A budget and a bare "on" have
/// no spelling here; the handler reports them back to the client as ignored
/// (`x-lmgw-reasoning-ignored`) rather than sending them somewhere they would
/// be meaningless.
///
/// A client that spoke OpenRouter's dialect gets OpenRouter's dialect back:
/// its `reasoning` object is reconciled to the resolved control by the shared
/// builder after the passthrough loop, and adding a second, scalar spelling
/// of the same thing would be lmgw inventing a field the client did not
/// send — which providers that read both have every right to resolve
/// differently than lmgw would.
///
/// Called **before** the passthrough loop so that the level, which the
/// gateway resolved from every tier, wins over a stale copy riding along in
/// `passthrough` (that loop only fills keys the body does not already have).
fn apply_reasoning(body: &mut Map<String, Value>, ir: &ChatRequest, c: &ReasoningControl) {
    if has_reasoning_object(ir) {
        return;
    }
    if c.enabled == Some(false) {
        body.insert("reasoning_effort".into(), json!("none"));
    } else if let Some(e) = &c.effort {
        body.insert("reasoning_effort".into(), json!(e));
    }
}

/// The chat body [`OpenaiEgress::build_chat`] posts: the shared builder
/// ([`openai_wire::build_chat_body`]) with this egress's reasoning step and
/// the text-only tool-result rendering, which is all OpenAI's `role: "tool"`
/// message takes.
pub fn chat_body(ir: &ChatRequest, model: &str, params: &Params, stream: bool) -> Value {
    openai_wire::build_chat_body(
        ir,
        model,
        params,
        stream,
        &OpenaiReasoning,
        &FlattenToolResults,
    )
}

impl Egress for OpenaiEgress {
    fn proto(&self) -> Protocol {
        Protocol::Openai
    }

    fn build_chat(
        &self,
        http: &reqwest::Client,
        up: &Upstream,
        model: &str,
        ir: &ChatRequest,
        params: &Params,
        stream: bool,
    ) -> Result<reqwest::RequestBuilder, GatewayError> {
        let body = chat_body(ir, model, params, stream);
        Ok(openai_wire::chat_request(http, up, &body))
    }

    fn parse_completion(&self, body: &[u8]) -> Result<Completion, GatewayError> {
        openai_wire::parse_completion(body)
    }

    fn new_decoder(&self) -> Box<dyn EgressStreamDecoder> {
        Box::new(OpenaiDecoder::default())
    }

    fn map_error(&self, status: u16, body: &[u8]) -> GatewayError {
        openai_wire::map_error(status, body)
    }

    fn build_embeddings(
        &self,
        http: &reqwest::Client,
        up: &Upstream,
        model: &str,
        req: &EmbeddingsRequest,
    ) -> Result<reqwest::RequestBuilder, GatewayError> {
        openai_wire::build_embeddings(http, up, model, req)
    }

    /// OpenAI has no token endpoint, and its tokenizer is public: count
    /// locally with the model's tiktoken encoding, or say it was a guess.
    fn build_count_tokens(
        &self,
        _http: &reqwest::Client,
        _up: &Upstream,
        model: &str,
        text: &str,
    ) -> Result<CountPlan, GatewayError> {
        Ok(match tiktoken_count(model, text) {
            (n, false) => CountPlan::Ready(n),
            (n, true) => CountPlan::Guessed(n),
        })
    }

    /// Not reached through this egress's own plan, which never asks the
    /// upstream (above). It reads a `/tokenize` answer as the llama.cpp
    /// egress does — a shape vLLM and SGLang share — rather than refusing, so
    /// what this egress answers did not change when llama.cpp's servers got
    /// their own (llama egress design, WP2c).
    fn parse_count(&self, body: &[u8]) -> Result<u64, GatewayError> {
        openai_wire::parse_tokenize_count(body)
    }

    fn parse_embeddings(&self, body: &[u8]) -> Result<EmbeddingsResponse, GatewayError> {
        openai_wire::parse_embeddings(body)
    }

    fn build_rerank(
        &self,
        http: &reqwest::Client,
        up: &Upstream,
        model: &str,
        req: &RerankRequest,
    ) -> Result<reqwest::RequestBuilder, GatewayError> {
        openai_wire::build_rerank(http, up, model, req)
    }

    fn parse_rerank(&self, body: &[u8]) -> Result<RerankResponse, GatewayError> {
        openai_wire::parse_rerank(body)
    }
}

/// Count `text` with the model's tiktoken encoding (OpenAI's tokenizer is
/// public). Unknown / future model names fall back to `o200k_base` — the
/// encoding of the current GPT-4o/o-series family — and say so: the `bool` is
/// `true` when the encoding was that guess rather than the model's own, which
/// [`CountPlan::Guessed`] carries to the response (api-docs design §5.1).
///
/// Both encoders are tiktoken-rs's process-wide singletons. `o200k_base()`
/// builds a fresh 200k-entry encoder on every call (450 ms in a debug build),
/// and the fallback is the common case here: no open-weights model's name is
/// known to tiktoken, and the knowledge bases ask for a count on each limit
/// lookup.
fn tiktoken_count(model: &str, text: &str) -> (u64, bool) {
    let (n, guessed) = match tiktoken_rs::bpe_for_model(model) {
        Ok(bpe) => (bpe.encode_ordinary(text).len(), false),
        Err(_) => {
            tracing::debug!("tiktoken: unknown model '{model}', counting with o200k_base");
            let n = match tiktoken_rs::bpe_for_tokenizer(
                tiktoken_rs::tokenizer::Tokenizer::O200kBase,
            ) {
                Ok(bpe) => bpe.encode_ordinary(text).len(),
                Err(e) => {
                    tracing::error!("tiktoken: o200k_base unavailable: {e}");
                    0
                }
            };
            (n, true)
        }
    };
    (n as u64, guessed)
}
