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
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
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
    /// Milliseconds spent on the prompt; absent when llama.cpp gave no finite number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub prompt_ms: Option<f64>,
    /// Prompt tokens processed per second; absent when llama.cpp gave no finite number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub prompt_per_second: Option<f64>,
    /// Tokens generated so far.
    pub predicted_n: u64,
    /// Milliseconds spent generating; absent when llama.cpp gave no finite number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub predicted_ms: Option<f64>,
    /// Tokens generated per second; absent when llama.cpp gave no finite number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub predicted_per_second: Option<f64>,
    /// Prompt tokens served from the KV cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub cache_n: Option<u64>,
    /// Draft tokens proposed (speculative or MTP decoding).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub draft_n: Option<u64>,
    /// Draft tokens accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub draft_n_accepted: Option<u64>,
}

/// `error`: something went wrong; the stream goes on to `done`. `code` is
/// what a client branches on (`gpu_hold`, `context_length_exceeded`,
/// `vram_queue_timeout`, `not_saved`, `superseded`, ...); a failure that has
/// none sends only the message. A stream whose key was disabled, rotated,
/// deleted or expired, or whose gateway stops, ends with a last `error`
/// frame instead of `done`: `revoked` (with `kind`) or `gateway_stopping`.
/// An MCP label the thread or its folder attached that offers the turn no
/// tools gets one frame each, with `server_label` and a [`LabelCode`]
/// (`tool_unreachable`, `tool_offline`, `tool_disabled`, `tool_unknown`,
/// `tool_failed`), and the turn goes on without it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ErrorFrame {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub code: Option<String>,
    /// What a client does about a `revoked` frame, the token a realtime
    /// session's 4003 close starts its reason with (`key_unknown` means pair
    /// the device again). Sent with `revoked` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub kind: Option<crate::realtime::RevokeKind>,
    /// The MCP label the frame is about, as the thread or folder attached
    /// it; sent with a [`LabelCode`] only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub server_label: Option<String>,
}

impl ErrorFrame {
    /// The code of a frame about an attached label, typed; `None` for every
    /// other frame.
    pub fn label_code(&self) -> Option<LabelCode> {
        self.code.as_deref().and_then(LabelCode::parse)
    }
}

/// Why an MCP label a thread or its folder attached offers a turn no tools:
/// the `code` of the turn's `error` frame about it, beside its
/// `server_label`. The turn goes on without the label's tools. When every
/// label failed only because its device is offline or is a device the
/// caller is not granted, a turn left with no tool runs as a plain chat
/// turn; after any other failure it is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum LabelCode {
    /// `tool_unreachable`: the caller may not use it — a paired device's
    /// hosted tools its key is not granted (Device tools in its policy), or
    /// a server none of whose tools its tool scope admits.
    #[serde(rename = "tool_unreachable")]
    Unreachable,
    /// `tool_offline`: a paired device's hosted tools, the device not
    /// connected now.
    #[serde(rename = "tool_offline")]
    Offline,
    /// `tool_disabled`: the server, the toolset or every tool of it is
    /// switched off by the owner.
    #[serde(rename = "tool_disabled")]
    Disabled,
    /// `tool_unknown`: nothing registers the label.
    #[serde(rename = "tool_unknown")]
    Unknown,
    /// `tool_failed`: a server the caller may use that offered nothing
    /// usable: its connection's error, or nothing left by the entry's
    /// `allowed_tools`, or tools for its app views only.
    #[serde(rename = "tool_failed")]
    Failed,
}

impl LabelCode {
    /// Every code, in the order the type lists them.
    pub const ALL: [Self; 5] = [
        Self::Unreachable,
        Self::Offline,
        Self::Disabled,
        Self::Unknown,
        Self::Failed,
    ];

    /// The code as the frame carries it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unreachable => "tool_unreachable",
            Self::Offline => "tool_offline",
            Self::Disabled => "tool_disabled",
            Self::Unknown => "tool_unknown",
            Self::Failed => "tool_failed",
        }
    }

    /// The code a frame carries, if it is one of these.
    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == code)
    }
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
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub images_note: Option<String>,
    /// The calls the turn stopped on and waits for a decision about; only
    /// when there are some.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
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

/// Every `event:` name a turn's stream sends (send, continue, regenerate,
/// an edit of a user message, the decision of an approval, the answer to a
/// task result), as [`TurnEvent`] reads them.
pub const TURN_EVENTS: &[&str] = &[
    "turn",
    "state",
    "retrieval",
    "delta",
    "reasoning",
    "tool",
    "usage",
    "stop",
    "stats",
    "error",
    "done",
    "voice",
    "speech",
    "speech_done",
    "speech_error",
];

/// The `event` values of a `tool` frame this build reads ([`ToolFrame`]).
const TOOL_EVENTS: &[&str] = &["start", "args", "ready", "approval", "result"];

/// One frame of a turn's stream, by its `event:` name, its data typed: what
/// a client makes of each record an SSE decoder (`lmgw-client`'s
/// `SseDecoder`) hands it. The voice warm-up's stream is not a turn's: its
/// `done` is [`WarmDone`].
///
/// Non-exhaustive: a newer gateway adds frames, and a client built against
/// this one reads them, and a `tool` frame of an `event` it does not know,
/// as [`TurnEvent::Unknown`], to skip.
#[derive(Debug, Clone, PartialEq)]
// Owned values, no boxes: a client (or an FFI wrapper) matches them as they are.
#[allow(clippy::large_enum_variant)]
#[non_exhaustive]
pub enum TurnEvent {
    Turn(TurnStarted),
    State(crate::chat_voice::ModelState),
    Retrieval(RetrievalFrame),
    Delta(TextFrame),
    Reasoning(TextFrame),
    Tool(ToolFrame),
    Usage(UsageFrame),
    Stop(StopFrame),
    Stats(Timings),
    Error(ErrorFrame),
    Done(DoneFrame),
    Voice(VoiceFrame),
    Speech(SpeechFrame),
    SpeechDone(SpeechDone),
    SpeechError(SpeechError),
    /// A frame this build does not know (a newer gateway's), as sent.
    Unknown {
        event: String,
        data: String,
    },
}

/// A known frame whose data does not read as its type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnEventError {
    pub event: String,
    pub message: String,
}

impl std::fmt::Display for TurnEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "turn frame '{}' unreadable: {}",
            self.event, self.message
        )
    }
}

impl std::error::Error for TurnEventError {}

impl TurnEvent {
    /// The frame `name` with `data` (an SSE record's `event:` and `data:`).
    /// An unknown name, or a `tool` frame of an unknown `event`, is
    /// [`TurnEvent::Unknown`]; a known one whose data does not read is an
    /// error naming it.
    pub fn parse(name: &str, data: &str) -> Result<Self, TurnEventError> {
        fn read<T: serde::de::DeserializeOwned>(
            name: &str,
            data: &str,
        ) -> Result<T, TurnEventError> {
            serde_json::from_str(data).map_err(|e| TurnEventError {
                event: name.to_string(),
                message: e.to_string(),
            })
        }
        let unknown = || Self::Unknown {
            event: name.to_string(),
            data: data.to_string(),
        };
        Ok(match name {
            "turn" => Self::Turn(read(name, data)?),
            "state" => Self::State(read(name, data)?),
            "retrieval" => Self::Retrieval(read(name, data)?),
            "delta" => Self::Delta(read(name, data)?),
            "reasoning" => Self::Reasoning(read(name, data)?),
            "tool" => {
                let v: serde_json::Value = read(name, data)?;
                let known = v
                    .get("event")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|e| TOOL_EVENTS.contains(&e));
                if !known {
                    return Ok(unknown());
                }
                Self::Tool(read(name, data)?)
            }
            "usage" => Self::Usage(read(name, data)?),
            "stop" => Self::Stop(read(name, data)?),
            "stats" => Self::Stats(read(name, data)?),
            "error" => Self::Error(read(name, data)?),
            "done" => Self::Done(read(name, data)?),
            "voice" => Self::Voice(read(name, data)?),
            "speech" => Self::Speech(read(name, data)?),
            "speech_done" => Self::SpeechDone(read(name, data)?),
            "speech_error" => Self::SpeechError(read(name, data)?),
            _ => unknown(),
        })
    }

    /// Its `event:` name.
    pub fn name(&self) -> &str {
        match self {
            Self::Turn(_) => "turn",
            Self::State(_) => "state",
            Self::Retrieval(_) => "retrieval",
            Self::Delta(_) => "delta",
            Self::Reasoning(_) => "reasoning",
            Self::Tool(_) => "tool",
            Self::Usage(_) => "usage",
            Self::Stop(_) => "stop",
            Self::Stats(_) => "stats",
            Self::Error(_) => "error",
            Self::Done(_) => "done",
            Self::Voice(_) => "voice",
            Self::Speech(_) => "speech",
            Self::SpeechDone(_) => "speech_done",
            Self::SpeechError(_) => "speech_error",
            Self::Unknown { event, .. } => event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A turn's stream as a client reads it (client-apps design §4.3,
    /// WP10): each frame by its name, a newer gateway's frame (or `tool`
    /// event) skipped as unknown, a known one that does not read an error.
    #[test]
    fn a_turn_frame_reads_by_its_name() {
        let ev = TurnEvent::parse("turn", r#"{"user_message_id": 12}"#).unwrap();
        assert_eq!(
            ev,
            TurnEvent::Turn(TurnStarted {
                user_message_id: 12
            })
        );
        let ev = TurnEvent::parse("delta", r#"{"text": "Hel"}"#).unwrap();
        assert_eq!(ev, TurnEvent::Delta(TextFrame { text: "Hel".into() }));
        let ev = TurnEvent::parse(
            "tool",
            r#"{"event": "start", "index": 0, "name": "desktop__open"}"#,
        )
        .unwrap();
        assert!(matches!(ev, TurnEvent::Tool(ToolFrame::Start(_))), "{ev:?}");
        let ev = TurnEvent::parse("tool", r#"{"event": "progress", "index": 0}"#).unwrap();
        assert!(matches!(ev, TurnEvent::Unknown { .. }), "{ev:?}");
        assert_eq!(ev.name(), "tool");
        let ev = TurnEvent::parse(
            "done",
            r#"{"aborted": false, "message_id": 13, "saved": true, "model": "m",
                "answered_by": null, "prompt_tokens": 3, "completion_tokens": 2,
                "ttfb_ms": 40, "total_ms": 90, "timings": null,
                "reasoning_ignored": [], "reasoning_note": null}"#,
        )
        .unwrap();
        let TurnEvent::Done(done) = &ev else {
            panic!("done: {ev:?}")
        };
        assert_eq!(done.saved.as_ref().map(|s| s.message_id), Some(13));
        let ev = TurnEvent::parse("echo", "{}").unwrap();
        assert_eq!(
            ev,
            TurnEvent::Unknown {
                event: "echo".into(),
                data: "{}".into()
            }
        );
        let e = TurnEvent::parse("error", "[1]").unwrap_err();
        assert_eq!(e.event, "error");
        for name in TURN_EVENTS {
            let parsed = TurnEvent::parse(name, "{}");
            if let Ok(ev) = parsed {
                assert_eq!(ev.name(), *name);
            }
        }
    }

    /// An attached label's `error` frame reads with its code typed and its
    /// label; any other frame has no label code.
    #[test]
    fn a_label_error_frame_reads_its_code() {
        let ev = TurnEvent::parse(
            "error",
            r#"{"message": "MCP server 'kai': device 'kai desktop' is offline (never seen)",
                "code": "tool_offline", "server_label": "kai"}"#,
        )
        .unwrap();
        let TurnEvent::Error(e) = &ev else {
            panic!("error: {ev:?}")
        };
        assert_eq!(e.label_code(), Some(LabelCode::Offline));
        assert_eq!(e.server_label.as_deref(), Some("kai"));
        for code in LabelCode::ALL {
            assert_eq!(LabelCode::parse(code.as_str()), Some(code));
            let wire = serde_json::to_value(code).unwrap();
            assert_eq!(wire, json!(code.as_str()));
        }
        let held = ErrorFrame {
            message: "held".into(),
            code: Some("gpu_hold".into()),
            ..Default::default()
        };
        assert_eq!(held.label_code(), None);
        assert_eq!(
            serde_json::to_value(&held).unwrap(),
            json!({"message": "held", "code": "gpu_hold"}),
            "no label on a frame about none"
        );
    }

    #[test]
    fn a_stat_llama_cpp_gave_no_number_for_is_left_out_of_the_frame() {
        let sent = Timings {
            prompt_n: 12,
            prompt_ms: Some(3.5),
            prompt_per_second: None,
            predicted_n: 4,
            predicted_ms: None,
            predicted_per_second: Some(9.25),
            ..Timings::default()
        };
        let wire = serde_json::to_value(&sent).unwrap();
        assert_eq!(
            wire,
            json!({"prompt_n": 12, "prompt_ms": 3.5, "predicted_n": 4, "predicted_per_second": 9.25})
        );
        let read: Timings = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(read, sent);
        assert_eq!(serde_json::to_value(&read).unwrap(), wire);
    }

    #[test]
    fn a_null_or_missing_stat_reads_as_unknown() {
        let read: Timings = serde_json::from_value(json!({
            "prompt_n": 20, "prompt_ms": 8.5, "prompt_per_second": null, "predicted_n": 3
        }))
        .unwrap();
        assert_eq!(
            (
                read.prompt_ms,
                read.prompt_per_second,
                read.predicted_ms,
                read.predicted_per_second
            ),
            (Some(8.5), None, None, None)
        );
    }
}
