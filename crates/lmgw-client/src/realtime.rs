//! The realtime protocol as a client of a session bound to a Chat thread
//! speaks it (`GET /v1/realtime?chat_thread=<id>`; chat-voice design §8,
//! realtime design §2.3): the server events a client acts on, parsed into
//! [`ServerEvent`], and the client events it sends ([`ClientEvent`]).
//!
//! Only what a client acts on is typed; every other event, and one of
//! those whose fields do not read, is [`ServerEvent::Unknown`] with its
//! `type` and the event as sent. The `lmgw.*` events are a bound
//! session's own (chat-voice design §8.6). A few payloads a client passes on
//! whole (a chat frame's data, a message's voice, the timing line) stay
//! `serde_json::Value`s; an FFI wrapper hands them over as JSON text.

use serde_json::Value;

pub use lmgw_api_types::chat_voice::ModelState;
pub use lmgw_api_types::realtime::{
    RevokeKind, CLOSE_GOING_AWAY, CLOSE_OUT_OF_REACH, CLOSE_REVOKED, CLOSE_TAKEN_OVER,
    SHUTTING_DOWN,
};

mod client;
mod close;
#[cfg(test)]
mod tests;

pub use client::{ClientEvent, TurnDetection, Updates};
pub use close::{close_kind, CloseKind};

/// One server event a client acts on.
///
/// Non-exhaustive: a newer version of this crate may type more events; a
/// client matches the ones it acts on and lets the rest pass.
#[derive(Debug, Clone, PartialEq)]
// Owned values, no boxes: an FFI wrapper maps each variant as it is.
#[allow(clippy::large_enum_variant)]
#[non_exhaustive]
pub enum ServerEvent {
    /// `session.created`: the session before the client's first update,
    /// bound to its thread.
    SessionCreated(SessionFacts),
    /// `session.updated`: the answer to a `session.update` (see
    /// [`Updates`]).
    SessionUpdated(SessionFacts),
    /// `input_audio_buffer.speech_started`.
    SpeechStarted,
    /// `input_audio_buffer.speech_stopped`.
    SpeechStopped,
    /// `input_audio_buffer.committed`.
    Committed,
    /// `input_audio_buffer.cleared`: the open turn and its audio are gone.
    Cleared,
    /// `conversation.item.input_audio_transcription.delta`.
    TranscriptDelta(String),
    /// `….completed`: the user's words, final.
    Transcript(String),
    /// `….failed`: its error (`asr_not_configured`, `gpu_hold`, …).
    TranscriptFailed(ErrorFacts),
    /// `response.created`.
    ResponseCreated { response_id: String },
    /// `response.output_audio.delta`, decoded: PCM16 little-endian bytes,
    /// mono at 24 kHz ([`crate::base64::pcm16_samples`] makes samples of
    /// them).
    AudioDelta {
        response_id: String,
        item_id: String,
        pcm: Vec<u8>,
    },
    /// `response.output_audio.done`.
    AudioDone { response_id: String },
    /// `response.output_audio_transcript.delta`: the reply's words, paced
    /// with its audio (the captions).
    SpokenDelta { response_id: String, delta: String },
    /// `response.done`: its status, and the reason it gives — the status
    /// details' `reason`, else its error's message, else the error's code.
    ResponseDone {
        response_id: String,
        status: String,
        reason: Option<String>,
    },
    /// `conversation.item.truncated`: the server's cut.
    Truncated { item_id: String, audio_end_ms: u64 },
    /// `error`.
    Error(ErrorFacts),
    /// `lmgw.chat.frame`: a chat-turn frame of a response, verbatim (the
    /// frames a text send streams: `delta`, `reasoning`, `tool`, …).
    ChatFrame {
        response_id: String,
        event: String,
        data: Value,
    },
    /// `lmgw.chat.user`: the user message a response's turns became, with
    /// the response's id when it heard them as audio (its row then races
    /// its reply).
    ChatUser {
        message_id: i64,
        content: String,
        voice: Value,
        response_id: Option<String>,
    },
    /// `lmgw.chat.input`: how a response's turns reach the chat model —
    /// `audio` or `transcript` — and why the transcript.
    ChatInput {
        response_id: String,
        input: String,
        why: Option<String>,
    },
    /// `lmgw.chat.reply`: a spoken reply as stored. A message may get more
    /// than one: the first when its response ended, cut to what the gateway
    /// knew was heard, and another when the client's truncate arrives after
    /// that and cuts it again at the heard position. A later
    /// `lmgw.chat.reply` for the same `message_id` replaces the earlier one.
    ChatReply(ChatReply),
    /// `lmgw.model.state`: a stage's model state.
    ModelState(ModelState),
    /// `lmgw.response.timing`: a response's timing line, its fields as they
    /// came.
    Timing(Value),
    /// `lmgw.chat.thread`: the thread as a response re-read it.
    Thread(ThreadFacts),
    /// Any other `type`, or a typed one whose fields do not read: its
    /// `type` and the whole event as sent, for a client to skip and log.
    Unknown { kind: String, data: Value },
}

/// An `error` event's fields (`error.type`, `error.code`, `error.message`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorFacts {
    pub kind: String,
    pub code: Option<String>,
    pub message: String,
}

/// `session.lmgw.resolved.chat_thread` and `lmgw.chat.thread`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadFacts {
    pub id: i64,
    pub title: String,
    /// The thread carries the self-admin toolset (shown, never a refusal).
    pub admin_tools: bool,
}

/// What a client reads of the session in effect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFacts {
    /// `audio.input.turn_detection.type`; `None`: push-to-talk (manual
    /// turns).
    pub turn_detection: Option<String>,
    /// `audio.input.turn_detection.prefix_padding_ms`, when the session
    /// runs a detector.
    pub prefix_padding_ms: Option<u32>,
    /// `session.lmgw.half_duplex`.
    pub half_duplex: Option<bool>,
    /// `session.lmgw.resolved`: the aliases that answer, and the voice.
    pub chat: Option<String>,
    pub asr: Option<String>,
    pub tts: Option<String>,
    pub voice: Option<String>,
    /// `session.lmgw.resolved.chat_thread`.
    pub thread: Option<ThreadFacts>,
}

/// `lmgw.chat.reply`: a spoken reply as finalized — or finalized again: a
/// later one for the same `message_id` replaces the earlier
/// ([`ServerEvent::ChatReply`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatReply {
    pub message_id: i64,
    /// The words heard (the reply's text from now on).
    pub content: Option<String>,
    /// The words sent but not heard.
    pub unheard: Option<String>,
    /// The message's voice record, verbatim (`Null` when absent).
    pub voice: Value,
    /// Heard by nobody, and no tool ran: the reply is gone from the thread.
    pub removed: bool,
    /// The reply was not cut to what was heard, and why: it stays whole.
    pub skipped: Option<String>,
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

fn opt(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

impl ThreadFacts {
    fn of(v: &Value) -> Option<Self> {
        v.is_object().then(|| ThreadFacts {
            id: v["id"].as_i64().unwrap_or_default(),
            title: s(v, "title"),
            admin_tools: v["admin_tools"].as_bool().unwrap_or(false),
        })
    }
}

impl SessionFacts {
    /// The facts of a `session` object.
    pub fn of(session: &Value) -> Self {
        let td = &session["audio"]["input"]["turn_detection"];
        let r = &session["lmgw"]["resolved"];
        SessionFacts {
            turn_detection: opt(&td["type"]),
            prefix_padding_ms: td["prefix_padding_ms"].as_u64().map(|n| n as u32),
            half_duplex: session["lmgw"]["half_duplex"].as_bool(),
            chat: opt(&r["chat"]),
            asr: opt(&r["asr"]),
            tts: opt(&r["tts"]),
            voice: opt(&r["voice"]),
            thread: ThreadFacts::of(&r["chat_thread"]),
        }
    }
}

impl ErrorFacts {
    fn of(e: &Value) -> Self {
        ErrorFacts {
            kind: s(e, "type"),
            code: opt(&e["code"]),
            message: s(e, "message"),
        }
    }
}

/// Parse one text frame; `None` for one that is no JSON object with a
/// `type`.
pub fn parse(text: &str) -> Option<ServerEvent> {
    let v: Value = serde_json::from_str(text).ok()?;
    let kind = v["type"].as_str()?.to_string();
    Some(match kind.as_str() {
        "session.created" => ServerEvent::SessionCreated(SessionFacts::of(&v["session"])),
        "session.updated" => ServerEvent::SessionUpdated(SessionFacts::of(&v["session"])),
        "input_audio_buffer.speech_started" => ServerEvent::SpeechStarted,
        "input_audio_buffer.speech_stopped" => ServerEvent::SpeechStopped,
        "input_audio_buffer.committed" => ServerEvent::Committed,
        "input_audio_buffer.cleared" => ServerEvent::Cleared,
        "conversation.item.input_audio_transcription.delta" => {
            ServerEvent::TranscriptDelta(s(&v, "delta"))
        }
        "conversation.item.input_audio_transcription.completed" => {
            ServerEvent::Transcript(s(&v, "transcript"))
        }
        "conversation.item.input_audio_transcription.failed" => {
            ServerEvent::TranscriptFailed(ErrorFacts::of(&v["error"]))
        }
        "response.created" => ServerEvent::ResponseCreated {
            response_id: s(&v["response"], "id"),
        },
        "response.output_audio.delta" => ServerEvent::AudioDelta {
            response_id: s(&v, "response_id"),
            item_id: s(&v, "item_id"),
            pcm: crate::base64::decode(v["delta"].as_str().unwrap_or_default()).unwrap_or_default(),
        },
        "response.output_audio.done" => ServerEvent::AudioDone {
            response_id: s(&v, "response_id"),
        },
        "response.output_audio_transcript.delta" => ServerEvent::SpokenDelta {
            response_id: s(&v, "response_id"),
            delta: s(&v, "delta"),
        },
        "response.done" => {
            let r = &v["response"];
            let d = &r["status_details"];
            ServerEvent::ResponseDone {
                response_id: s(r, "id"),
                status: s(r, "status"),
                // The message says it in words; the code is the fallback.
                reason: opt(&d["reason"])
                    .or_else(|| opt(&d["error"]["message"]))
                    .or_else(|| opt(&d["error"]["code"])),
            }
        }
        "error" => ServerEvent::Error(ErrorFacts::of(&v["error"])),
        "lmgw.chat.frame" => ServerEvent::ChatFrame {
            response_id: s(&v, "response_id"),
            event: s(&v, "event"),
            data: v["data"].clone(),
        },
        "lmgw.chat.user" => ServerEvent::ChatUser {
            message_id: v["message_id"].as_i64().unwrap_or_default(),
            content: s(&v, "content"),
            voice: v["voice"].clone(),
            response_id: opt(&v["response_id"]),
        },
        "lmgw.chat.input" => ServerEvent::ChatInput {
            response_id: s(&v, "response_id"),
            input: s(&v, "input"),
            why: opt(&v["why"]),
        },
        "lmgw.chat.reply" => ServerEvent::ChatReply(ChatReply {
            message_id: v["message_id"].as_i64().unwrap_or_default(),
            content: v["content"].as_str().map(str::to_string),
            unheard: v["unheard"].as_str().map(str::to_string),
            voice: v["voice"].clone(),
            removed: v["removed"].as_bool() == Some(true),
            skipped: v["skipped"].as_str().map(str::to_string),
        }),
        "conversation.item.truncated" => ServerEvent::Truncated {
            item_id: s(&v, "item_id"),
            audio_end_ms: v["audio_end_ms"].as_u64().unwrap_or_default(),
        },
        "lmgw.model.state" => match serde_json::from_value::<ModelState>(strip(v.clone())) {
            Ok(m) => ServerEvent::ModelState(m),
            Err(_) => ServerEvent::Unknown { kind, data: v },
        },
        "lmgw.response.timing" => ServerEvent::Timing(strip(v)),
        "lmgw.chat.thread" => match ThreadFacts::of(&v["chat_thread"]) {
            Some(t) => ServerEvent::Thread(t),
            None => ServerEvent::Unknown { kind, data: v },
        },
        _ => ServerEvent::Unknown { kind, data: v },
    })
}

/// A flat event's own fields, without `type` and `event_id`.
fn strip(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.remove("type");
        o.remove("event_id");
    }
    v
}
