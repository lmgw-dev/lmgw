//! Client-facing protocol surfaces (§6). Each ingress has a parser
//! (request → IR) and a serializer (IR / IR-deltas → that protocol's
//! response + SSE framing).

pub mod anthropic;
pub mod openai;
pub mod responses;

use crate::error::GatewayError;
use crate::ir::{Completion, StreamDelta};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientProto {
    OpenaiChat,
    AnthropicMessages,
}

impl ClientProto {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai",
            Self::AnthropicMessages => "anthropic",
        }
    }

    /// Non-streaming response body for a completed request.
    pub fn serialize_completion(&self, alias: &str, c: &Completion) -> serde_json::Value {
        match self {
            Self::OpenaiChat => openai::serialize_completion(alias, c),
            Self::AnthropicMessages => anthropic::serialize_completion(alias, c),
        }
    }

    /// Error body in the client's protocol shape (§14).
    pub fn serialize_error(&self, e: &GatewayError) -> serde_json::Value {
        match self {
            Self::OpenaiChat => e.to_openai_json(),
            Self::AnthropicMessages => e.to_anthropic_json(),
        }
    }

    /// Stateful SSE encoder translating IR deltas into this protocol's
    /// wire framing.
    pub fn new_stream_encoder(&self, alias: &str) -> Box<dyn ClientStreamEncoder> {
        match self {
            Self::OpenaiChat => Box::new(openai::OpenaiStreamEncoder::new(alias)),
            Self::AnthropicMessages => Box::new(anthropic::AnthropicStreamEncoder::new(alias)),
        }
    }
}

/// Translates IR stream deltas into pre-framed SSE text for the client.
pub trait ClientStreamEncoder: Send {
    /// Frames to emit before any delta (e.g. Anthropic `message_start`).
    fn start(&mut self) -> String;
    fn delta(&mut self, d: &StreamDelta) -> String;
    /// Closing frames (OpenAI `[DONE]`, Anthropic `message_stop`).
    fn finish(&mut self) -> String;
}

/// Random id suffix for synthesized response ids.
pub(crate) fn rand_id() -> String {
    let n: u128 = rand::random();
    format!("{n:032x}")
}

pub(crate) fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}
