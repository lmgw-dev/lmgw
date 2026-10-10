//! The frames of the Chat API's event streams — one type per event name.
//!
//! A turn (send, continue, regenerate, an edit of a user message, the
//! decision of an approval, the answer to a job result) streams `turn`,
//! `retrieval`, `delta`, `reasoning`, `tool`, `usage`, `stop`, `stats`,
//! `error` and `done`; when the request asked to `speak`, the speech frames
//! `state`, `voice`, `speech`, `speech_done` and `speech_error` join them. A
//! voice warm-up streams `state` frames and `done` ([`WarmDone`]); a stored
//! reply's read-aloud the speech frames alone.
//!
//! The gateway writes every frame from these types and the API document is
//! generated from them. A reader parses `data` as the type of the SSE
//! `event` name; it reads leniently (a field a newer gateway adds is
//! ignored).

use serde::{Deserialize, Serialize};

use crate::chat_approvals::ApprovalRequest;
use crate::chat_threads::MessageContext;
use crate::mcp_apps::{ToolReadyFrame, ToolResultFrame};

/// `turn`: the first frame of a turn that wrote a user message (a send, an
/// edit of a user message).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TurnStarted {
    /// The user message the answer follows.
    pub user_message_id: i64,
}

/// `retrieval`: what the knowledge bases found for the user message, before
/// the model answers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RetrievalFrame {
    #[serde(flatten)]
    pub context: MessageContext,
    /// The user message the retrieval belongs to.
    pub message_id: i64,
    /// The message's stored retrieval was used again instead of searching.
    pub reused: bool,
}

/// `delta` (a piece of the answer) and `reasoning` (a piece of the model's
/// reasoning, kept apart from the answer).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TextFrame {
    pub text: String,
}

/// A tool call starting to stream: its position and name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolStartFrame {
    /// The call's position in its turn.
    pub index: usize,
    /// The model's id of the call; only a turn without tools says it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The tool's exposed name.
    pub name: String,
}

/// A fragment of a call's JSON arguments, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolArgsFrame {
    pub index: usize,
    pub fragment: String,
}

/// `tool`: a tool call of the turn, told apart by `event`. `start` and
/// `args` while the model writes the call; `ready` when its arguments are
/// complete (a call that waits for an approval says `needs_approval`);
/// `approval` for a call that waits for a decision
/// (`POST /chat/api/threads/{id}/approvals`); `result` when it has run. A
/// result's `task` says which MCP task a call started: its output is then
/// `started, job <task_id>`, and the result enters the thread later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ToolFrame {
    Start(ToolStartFrame),
    Args(ToolArgsFrame),
    Ready(ToolReadyFrame),
    Approval(ApprovalRequest),
    Result(ToolResultFrame),
}

/// `usage`: the tokens the model reported for the turn so far.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageFrame {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

/// `stop`: why the model stopped, in OpenAI's words (`stop`, `length`,
/// `tool_calls`, ...).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StopFrame {
    pub reason: String,
}

/// Server-measured prefill and decode timings of a local model: the `stats`
/// frame (live per token while it streams) and the `done` frame's final
/// `timings`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Timings {
    /// Prompt tokens actually processed this turn (the cache excluded).
    pub prompt_n: u64,
    pub prompt_ms: f64,
    pub prompt_per_second: f64,
    /// Tokens generated so far.
    pub predicted_n: u64,
    pub predicted_ms: f64,
    pub predicted_per_second: f64,
    /// Prompt tokens served from the KV cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_n: Option<u64>,
    /// Draft tokens proposed (speculative or MTP decoding).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_n: Option<u64>,
    /// Draft tokens accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_n_accepted: Option<u64>,
}

/// `error`: something went wrong; the stream goes on to `done`. `code` is
/// what a client branches on (`gpu_hold`, `context_length_exceeded`,
/// `vram_queue_timeout`, `not_saved`, `superseded`, ...); a failure that has
/// none sends only the message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ErrorFrame {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// What a turn that saved something says in `done`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DoneSaved {
    /// The reply the turn saved or appended to; `0` when it saved none.
    pub message_id: i64,
    /// Whether the reply is stored. `false`: its bubble is not a row, and no
    /// action on it can work.
    pub saved: bool,
    /// The thread's model alias.
    pub model: String,
    /// The alias that answered in its place (a fallback, a candidate
    /// alias's pick), when one did.
    pub answered_by: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// Milliseconds to the first byte of the upstream's answer.
    pub ttfb_ms: Option<i64>,
    pub total_ms: i64,
    /// The authoritative final timings of a local model; `null` for a cloud
    /// upstream.
    pub timings: Option<Timings>,
    /// The thread's reasoning and sampling overrides this route did not send.
    pub reasoning_ignored: Vec<String>,
    /// The model reasoned although off was asked, in a sentence.
    pub reasoning_note: Option<String>,
    /// A fallback that cannot see answered and got the images as
    /// placeholders: who, in a sentence. Only when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images_note: Option<String>,
    /// The calls the turn stopped on and waits for a decision about; only
    /// when there are some.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_approvals: Option<Vec<ApprovalRequest>>,
}

/// `done`: the turn's last frame. `aborted` is `true` when the turn ended
/// early (refused, stopped, failed before it said anything); a turn that
/// saved nothing says only that, with no `message_id`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DoneFrame {
    pub aborted: bool,
    #[serde(flatten)]
    pub saved: Option<DoneSaved>,
}

/// Read by hand: a flattened `Option` turns a saved part that does not
/// parse into `None`, which would read a damaged frame as a turn that saved
/// nothing. Here a frame with more than `aborted` must be a whole
/// [`DoneSaved`], or the read fails.
impl<'de> Deserialize<'de> for DoneFrame {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            aborted: bool,
            #[serde(flatten)]
            rest: serde_json::Map<String, serde_json::Value>,
        }
        let Raw { aborted, rest } = Raw::deserialize(d)?;
        let saved = if rest.is_empty() {
            None
        } else {
            Some(
                DoneSaved::deserialize(serde_json::Value::Object(rest))
                    .map_err(serde::de::Error::custom)?,
            )
        };
        Ok(Self { aborted, saved })
    }
}

impl DoneFrame {
    /// The end of a turn that saved nothing.
    pub fn aborted() -> Self {
        Self {
            aborted: true,
            saved: None,
        }
    }
}

/// `voice` (speech): the read-aloud's route is open.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceFrame {
    /// The text-to-speech alias.
    pub tts: String,
    /// The voice every clause is sent with.
    pub voice: Option<String>,
    /// The alias that answers in the TTS alias's place (a fallback).
    pub tts_answered_by: Option<String>,
}

/// `speech`: one clause as said, with its audio.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SpeechFrame {
    /// Counts from 0.
    pub seq: u64,
    pub text: String,
    /// The clause's audio, base64 of PCM16 little-endian, 24 kHz, mono.
    pub pcm: String,
}

/// `speech_done`: the read-aloud ended, whole or stopped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SpeechDone {
    /// Characters spoken.
    pub chars: u64,
    /// Length of the audio sent.
    pub audio_ms: u64,
    /// From the request's start to the first `speech` frame; `null` when
    /// nothing was spoken.
    pub first_audio_ms: Option<u64>,
    pub tts: String,
    pub tts_answered_by: Option<String>,
    /// Stopped (`POST .../speech/stop`, or the reader went away) before the
    /// text ended.
    pub stopped: bool,
}

/// `speech_error`: the read-aloud cannot speak or failed; it ends the
/// speech. Codes: `tts_not_configured`, `voice_not_found`,
/// `voice_not_configured`, `instructions_required`, a device key's own
/// refusals, or the failure's kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SpeechError {
    pub code: String,
    pub message: String,
}

/// `done` of a voice warm-up: every stage has settled. No fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(extend("additionalProperties" = false)))]
pub struct WarmDone {}
