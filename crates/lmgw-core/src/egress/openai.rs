//! The OpenAI platform's egress (openai egress design §3.2): api.openai.com
//! and its regional hosts, the protocol `openai`. Every other OpenAI-shaped
//! provider leaves through [`super::openai_compatible`].
//!
//! It composes [`super::openai_wire`] as the compatible egress does; the
//! egress corpus (`tests/it/egress_golden.rs`, the `openai_platform` cases)
//! started byte-identical to the compatible egress's (WP1), and each change
//! the platform takes on top (the design's §3.3) landed as a commit of its
//! own that re-blessed only its own platform cases: the cap as
//! `max_completion_tokens` ([`DIALECT`], P1), the llama samplers left out
//! and named as ignored (P2), OpenRouter's `reasoning` object translated
//! into `reasoning_effort` (`reasoning`, P3), and OpenAI's error codes with
//! a structured-output classifier of OpenAI's shape only (`errors`, P4), and
//! the answer's `service_tier` read whole and streamed (P5, WP4), which the
//! row is priced at (§5). Since WP7 (P7, decision 16) its chat sends leave
//! on OpenAI's Responses wire ([`responses`]) unless a send asks for
//! something only Chat Completions has; that switch re-blessed the platform
//! cases of both corpora and nothing else.
//!
//! It offers no rerank and no System One: OpenAI serves neither, so both stay
//! the trait's `Unsupported`, and the System One route guard refuses an
//! `openai` row before admission.

mod count;
mod errors;
mod reasoning;
pub mod responses;

pub use count::{chat_prompt_text, completion_text, count_chat, count_locally, local_count};

use serde_json::Value;

use super::openai_compatible::tiktoken_count;
use super::openai_wire::{self, BodyDialect, FlattenToolResults, ReadTier};
use crate::config::{Protocol, Upstream};
use crate::egress::{apply_bearer_auth, watch, CountPlan, Egress, EgressStreamDecoder};
use crate::error::GatewayError;
use crate::ir::{
    ChatRequest, Completion, EmbeddingsRequest, EmbeddingsResponse, Params, SealScope,
};
use reasoning::{drop_reasoning_object, PlatformReasoning};
use responses::Wire;

pub struct OpenaiEgress;

/// The platform's spelling of the modelled fields (design §7.1, P1): the cap
/// is `max_completion_tokens`. The reference (checked 2026-10-24,
/// <https://developers.openai.com/api/reference/resources/chat.md>, "Create
/// chat completion") calls `max_tokens` "deprecated in favor of
/// `max_completion_tokens`, and … not compatible with o-series models", and
/// `max_completion_tokens` "an upper bound for the number of tokens that can
/// be generated for a completion, including visible output tokens and
/// reasoning tokens". Only the spelling changes: the gate's clamp and fit
/// read the IR's `max_tokens` as before.
///
/// The modelled llama.cpp samplers are not written (P2): the same reference
/// has no `top_k`, `min_p` or `repeat_penalty`, and OpenAI rejects a
/// parameter it does not know. They are never dropped silently: the API
/// routes name them in `x-lmgw-params-ignored`
/// ([`crate::proxy::params_ignored`]), the Chat with its turn
/// (`web/chat_sampling.rs`). A client's own unknown keys still go verbatim
/// through passthrough: the client sent them.
pub const DIALECT: BodyDialect = BodyDialect {
    cap: "max_completion_tokens",
    llama_samplers: false,
};

/// The body [`OpenaiEgress::build_chat`] posts to `up`, on the wire the send
/// takes ([`responses::wire_for`], P7): the Responses body
/// ([`responses::responses_body`]), or the Chat body ([`chat_wire_body`]).
pub fn chat_body(
    ir: &ChatRequest,
    model: &str,
    params: &Params,
    stream: bool,
    up: &Upstream,
) -> Value {
    match responses::wire_for(ir, params) {
        Wire::Responses => responses::responses_body(ir, model, params, stream, &SealScope::of(up)),
        Wire::Chat => chat_wire_body(ir, model, params, stream),
    }
}

/// The platform's Chat Completions body: the shared builder with the
/// platform's [`DIALECT`], its reasoning vocabulary (`reasoning`, P3) and
/// text-only tool results, which is all OpenAI's `role: "tool"` message
/// takes.
pub fn chat_wire_body(ir: &ChatRequest, model: &str, params: &Params, stream: bool) -> Value {
    let mut body = openai_wire::build_chat_body(
        ir,
        model,
        params,
        stream,
        DIALECT,
        &PlatformReasoning,
        &FlattenToolResults,
    );
    drop_reasoning_object(&mut body);
    ask_for_the_tier(&mut body);
    body
}

/// The platform answers with the tier that served the request only when the
/// request carried a `service_tier`; `auto`, which is the project's setting
/// and so what an unset one means, is sent when the client set none (design
/// §5.2, decision 14). The request's own tier stays what the note names.
pub fn ask_for_the_tier(body: &mut Value) {
    if let Some(o) = body.as_object_mut() {
        o.entry("service_tier").or_insert_with(|| "auto".into());
    }
}

/// A whole answer on either wire, its seals naming the upstream row `scope`
/// names (the default scope: none).
fn parse_platform_completion(body: &[u8], scope: &SealScope) -> Result<Completion, GatewayError> {
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        if responses::is_response_object(&v) {
            return responses::parse_response(&v, scope);
        }
    }
    openai_wire::parse_completion_with(body, ReadTier::Yes)
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
        match responses::chat_only(ir, params) {
            None => {
                let body = responses::responses_body(ir, model, params, stream, &SealScope::of(up));
                let url = format!("{}/responses", up.base());
                let rb = apply_bearer_auth(http.post(url).json(&body), up);
                Ok(watch::built(up, &ir.passthrough, rb))
            }
            Some(why) => {
                tracing::debug!(
                    upstream = %up.name,
                    model,
                    "this send takes the Chat Completions wire for {}",
                    why.describe()
                );
                let body = chat_wire_body(ir, model, params, stream);
                let rb = openai_wire::chat_request(http, up, &body);
                Ok(watch::built(up, &ir.passthrough, rb))
            }
        }
    }

    /// A Response object (the Responses wire, P7) or a chat completion (the
    /// Chat wire), told apart by its shape; the answer's `service_tier` is
    /// read on both (P5, openai egress design §5.2): the tier the row is
    /// priced at.
    fn parse_completion(&self, body: &[u8]) -> Result<Completion, GatewayError> {
        parse_platform_completion(body, &SealScope::default())
    }

    /// A Response's reasoning seals name `up` (openai egress design §7.3.1).
    fn parse_completion_from(
        &self,
        up: &Upstream,
        body: &[u8],
    ) -> Result<Completion, GatewayError> {
        parse_platform_completion(body, &SealScope::of(up))
    }

    /// Either wire's stream, each event read by its shape
    /// ([`responses::PlatformDecoder`]); a chat chunk's `service_tier` is
    /// read on every chunk, a Responses stream's from its terminal event
    /// (P5). A decoder no send site made: its seals name no row.
    fn new_decoder(&self) -> Box<dyn EgressStreamDecoder> {
        Box::new(responses::PlatformDecoder::default())
    }

    /// The stream's reasoning seals name `up` (§7.3.1).
    fn decoder_for(&self, up: &Upstream) -> Box<dyn EgressStreamDecoder> {
        Box::new(responses::PlatformDecoder::for_scope(SealScope::of(up)))
    }

    /// OpenAI's own error codes (`errors`, P4): `context_length_exceeded`
    /// and the `429`s told apart.
    fn map_error(&self, status: u16, body: &[u8]) -> GatewayError {
        errors::map_error(status, body)
    }

    /// OpenAI's structured-output refusals in OpenAI's own shape only (P4):
    /// the aggregator and llama.cpp texts are the compatible egress's.
    fn structured_refusal(&self, status: u16, body: &[u8]) -> Option<GatewayError> {
        openai_wire::structured::openai_refusal(status, body)
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
    /// upstream (above). It reads a `/tokenize` answer as the compatible
    /// egress does, so the two answer the same.
    fn parse_count(&self, body: &[u8]) -> Result<u64, GatewayError> {
        openai_wire::parse_tokenize_count(body)
    }

    fn parse_embeddings(&self, body: &[u8]) -> Result<EmbeddingsResponse, GatewayError> {
        openai_wire::parse_embeddings(body)
    }
}
