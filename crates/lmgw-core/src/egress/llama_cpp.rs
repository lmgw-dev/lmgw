//! The llama.cpp egress (llama egress design §3): llama-server and
//! ik_llama.cpp's server, one protocol (`llama_cpp`) and one wire dialect for
//! both (§6).
//!
//! Composed from [`super::openai_wire`], the OpenAI wire pieces it shares with
//! the OpenAI egress, and overriding only where llama.cpp differs: the
//! reasoning control (`reasoning`, the `enable_thinking` kwarg as the one
//! switch), the token count (`count`'s `/tokenize` and `/apply-template`)
//! and how a tool result's media is sent ([`tool_results`], §8: as images
//! where the route's frozen decision lets them go). Parsing, the stream
//! decoder, embeddings, the Jina-shape rerank and the error mapping — the
//! `exceed_context_size_error` refusal first — are the shared ones. [`props`]
//! reads what a server says it can do.

pub(crate) mod count;
pub mod props;
mod reasoning;
#[cfg(test)]
mod tests;
pub mod tool_results;

use serde_json::Value;

use super::openai_wire::{self, FlattenToolResults, OpenaiDecoder};
pub use super::openai_wire::{parse_exceed_context, ExceedContext};
use crate::config::{Protocol, Upstream};
use crate::egress::{CountPlan, Egress, EgressStreamDecoder};
use crate::error::GatewayError;
use crate::ir::{
    ChatRequest, Completion, EmbeddingsRequest, EmbeddingsResponse, Params, RerankRequest,
    RerankResponse,
};

pub struct LlamaCppEgress;

/// The chat body [`LlamaCppEgress::build_chat`] posts to `up`: the shared
/// builder ([`openai_wire::build_chat_body`]) with llama.cpp's reasoning step
/// and the tool-result rendering the route's frozen decision picks.
///
/// It is also the body the gate counts ([`crate::gate::fit_chat`], the
/// ladder's count beside the send, `/v1/messages/count_tokens` on a local
/// row): ladder design §3.3 step 1 counts "the exact chat body egress is about
/// to send", so `build_chat` calls this and nothing else. `up` is the route's
/// upstream: what a server is known to take, and what was decided on it,
/// arrive on it (§3.2, `Upstream.llama`). A decision renders tool results
/// with [`tool_results::LlamaToolResults`] (§8.1); no decision, or one made
/// on facts that do not say whether the server sees, renders today's text
/// ([`FlattenToolResults`], decision 14).
pub fn chat_body(
    ir: &ChatRequest,
    model: &str,
    params: &Params,
    stream: bool,
    up: &Upstream,
) -> Value {
    let images = up
        .llama
        .as_deref()
        .and_then(tool_results::ToolImageDecision::of);
    let renderer: &dyn openai_wire::ToolResultRenderer = match &images {
        Some(decision) => &tool_results::LlamaToolResults(*decision),
        None => &FlattenToolResults,
    };
    openai_wire::build_chat_body(
        ir,
        model,
        params,
        stream,
        &reasoning::LlamaReasoning,
        renderer,
    )
}

impl Egress for LlamaCppEgress {
    fn proto(&self) -> Protocol {
        Protocol::LlamaCpp
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
        let body = chat_body(ir, model, params, stream, up);
        Ok(openai_wire::chat_request(http, up, &body))
    }

    fn parse_completion(&self, body: &[u8]) -> Result<Completion, GatewayError> {
        openai_wire::parse_completion(body)
    }

    fn new_decoder(&self) -> Box<dyn EgressStreamDecoder> {
        Box::new(OpenaiDecoder::default())
    }

    /// llama-server's `exceed_context_size_error` as
    /// [`GatewayError::ContextExceeded`] first, then the generic mapping —
    /// the shared [`openai_wire::map_error`], since a `generic` row in front
    /// of llama-server maps the same refusal.
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

    /// llama-server's native `/tokenize` at the server root (not under
    /// `/v1`); the count is the number of token ids it returns.
    fn build_count_tokens(
        &self,
        http: &reqwest::Client,
        up: &Upstream,
        model: &str,
        text: &str,
    ) -> Result<CountPlan, GatewayError> {
        Ok(count::tokenize_count_plan(http, up, model, text))
    }

    fn parse_count(&self, body: &[u8]) -> Result<u64, GatewayError> {
        openai_wire::parse_tokenize_count(body)
    }
}
