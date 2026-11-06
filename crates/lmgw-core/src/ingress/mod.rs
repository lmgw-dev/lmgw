//! Client-facing protocol surfaces (§6). Each ingress has a parser
//! (request → IR) and a serializer (IR / IR-deltas → that protocol's
//! response + SSE framing).

pub mod anthropic;
pub mod openai;
pub mod responses;

use crate::error::GatewayError;
use crate::ir::{Completion, Role, StreamDelta};

/// A `refusal` content part on a message that is not the assistant's: the
/// model's refusal is an assistant part on both of OpenAI's wires, and
/// OpenAI refuses it anywhere else with a 400, so lmgw does too rather than
/// drop it (openai egress design §7.2.3).
pub(crate) fn refusal_part_misplaced(role: Role) -> GatewayError {
    let role = match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    GatewayError::BadRequest(format!(
        "a 'refusal' content part belongs to an assistant message, not to a '{role}' one"
    ))
}

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
    /// `POST /v1/systemone` (decision-models design §2.6): its own
    /// `request_logs` label, so decisions are told from `openai` traffic, as
    /// [`Self::Realtime`]'s are. It never parses or serializes a chat body:
    /// the route relays the System One wire, and answers lmgw's own refusals
    /// in llama.cpp's error shape (`proxy::systemone`). The shared `/v1`
    /// layers' refusals, which reach it before its handler, are OpenAI's.
    SystemOne,
}

impl ClientProto {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai",
            Self::AnthropicMessages => "anthropic",
            Self::Realtime => "realtime",
            Self::Chat => "chat",
            Self::AdminChat => crate::telemetry::ADMIN_PROTO,
            Self::SystemOne => "systemone",
        }
    }

    /// Non-streaming response body for a completed request.
    pub fn serialize_completion(&self, alias: &str, c: &Completion) -> serde_json::Value {
        match self {
            Self::OpenaiChat | Self::Realtime | Self::Chat | Self::AdminChat | Self::SystemOne => {
                openai::serialize_completion(alias, c)
            }
            Self::AnthropicMessages => anthropic::serialize_completion(alias, c),
        }
    }

    /// An error's status in the client's protocol: Anthropic's wire answers
    /// an exhausted upstream quota with its `402`
    /// ([`GatewayError::anthropic_status`]), every other wire and error
    /// with [`GatewayError::http_status`].
    pub fn error_status(&self, e: &GatewayError) -> axum::http::StatusCode {
        match self {
            Self::AnthropicMessages => e.anthropic_status(),
            Self::OpenaiChat | Self::Realtime | Self::Chat | Self::AdminChat | Self::SystemOne => {
                e.http_status()
            }
        }
    }

    /// Error body in the client's protocol shape (§14).
    pub fn serialize_error(&self, e: &GatewayError) -> serde_json::Value {
        match self {
            Self::OpenaiChat | Self::Realtime | Self::Chat | Self::AdminChat | Self::SystemOne => {
                e.to_openai_json()
            }
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
            Self::OpenaiChat | Self::Realtime | Self::Chat | Self::AdminChat | Self::SystemOne => {
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
