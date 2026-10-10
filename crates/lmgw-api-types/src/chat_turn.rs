//! The bodies of the Chat API's streaming routes: a send, a continue or a
//! regenerate, and the voice warm-up. Their answers are event streams whose
//! frames are [`crate::chat_frames`]'.
//!
//! The gateway parses its requests into these types and the API document is
//! generated from them, so the two cannot drift.

use serde::{Deserialize, Serialize};

fn is_false(b: &bool) -> bool {
    !*b
}

/// How a dictated message says it was dictated (`voice` of a send): the
/// facts of the transcribe answer, nothing a reply carries. Unknown fields
/// are refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DictatedVoice {
    /// Must be `dictation`: a realtime turn is written by the bound session
    /// and cannot be sent.
    pub via: String,
    /// The speech-to-text alias that was asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr: Option<String>,
    /// The alias that answered in its place, when a fallback did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_answered_by: Option<String>,
    /// How long the transcription took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_ms: Option<u64>,
    /// The recording's length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_ms: Option<u64>,
}

/// `POST /chat/api/threads/{id}/send`'s body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SendRequest {
    /// The message's text; may be empty when `attachments` is not (a message
    /// can be just a file).
    pub content: String,
    /// Draft attachment ids this message binds, in the order the files are
    /// read in; a repeated id counts once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<i64>,
    /// Knowledge bases picked for this message alone, on top of the thread's
    /// own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kb_refs: Vec<i64>,
    /// A dictated message's origin, stored on the user message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<DictatedVoice>,
    /// Read the reply aloud as it streams: the stream then carries the
    /// speech frames too.
    #[serde(default, skip_serializing_if = "is_false")]
    pub speak: bool,
}

/// The optional body of a continue and of a regenerate; an empty request is
/// accepted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TurnRequest {
    /// Read the answer aloud as it streams.
    #[serde(default, skip_serializing_if = "is_false")]
    pub speak: bool,
}

/// `POST /chat/api/threads/{id}/voice/warm`'s body. Unknown fields are
/// refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WarmRequest {
    /// The stages to warm, each named once and in this order: `asr`, `tts`
    /// and `chat` (the thread's own model). Empty or an unknown name is
    /// 400 bad_request.
    pub stages: Vec<String>,
}
