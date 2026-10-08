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
    /// `GET /v1/realtime`'s model calls and refusals (realtime design §11):
    /// its own `request_logs` label, so a voice session's chat, ASR and TTS
    /// rows are told from plain `openai` traffic. It never parses or
    /// serializes a chat body itself — the session speaks its own events —
    /// so the body shapes below are OpenAI's, whose error object the
    /// Realtime protocol shares.
    Realtime,
    /// The Chat tab's in-process speech calls (chat-voice design §5, §6):
    /// dictation's transcription and read-aloud's clauses. Labelled `chat`,
    /// like the Chat's own model turns, so its voice traffic is told apart
    /// from `openai` clients in Logs and is charged to the same internal
    /// identity as its turns. Never a chat body of its own: OpenAI's shapes,
    /// like `Realtime`.
    Chat,
    /// An Admin Chat thread's in-process speech calls: labelled `admin`, like
    /// that thread's model turns (`telemetry::ADMIN_PROTO`), and charged to
    /// the same internal identity. OpenAI's shapes, like `Chat`.
    AdminChat,
}

impl ClientProto {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai",
            Self::AnthropicMessages => "anthropic",
            Self::Realtime => "realtime",
            Self::Chat => "chat",
            Self::AdminChat => crate::telemetry::ADMIN_PROTO,
        }
    }

    /// Non-streaming response body for a completed request.
    pub fn serialize_completion(&self, alias: &str, c: &Completion) -> serde_json::Value {
        match self {
            Self::OpenaiChat | Self::Realtime | Self::Chat | Self::AdminChat => {
                openai::serialize_completion(alias, c)
            }
            Self::AnthropicMessages => anthropic::serialize_completion(alias, c),
        }
    }

    /// Error body in the client's protocol shape (§14).
    pub fn serialize_error(&self, e: &GatewayError) -> serde_json::Value {
        match self {
            Self::OpenaiChat | Self::Realtime | Self::Chat | Self::AdminChat => e.to_openai_json(),
            Self::AnthropicMessages => e.to_anthropic_json(),
        }
    }

    /// Stateful SSE encoder translating IR deltas into this protocol's
    /// wire framing. `include_usage` is OpenAI's
    /// `stream_options.include_usage` ([`openai::stream_include_usage`]); an
    /// Anthropic stream carries its usage in `message_delta` either way.
    pub fn new_stream_encoder(
        &self,
        alias: &str,
        include_usage: bool,
    ) -> Box<dyn ClientStreamEncoder> {
        match self {
            Self::OpenaiChat | Self::Realtime | Self::Chat | Self::AdminChat => {
                Box::new(openai::OpenaiStreamEncoder::new(alias, include_usage))
            }
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
