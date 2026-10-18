//! The realtime protocol as a client of a session bound to a Chat thread
//! speaks it (`GET /v1/realtime?chat_thread=<id>`; chat-voice design §8,
//! realtime design §2.3): the server events a client acts on, parsed into
//! [`ServerEvent`], and the client events it sends ([`ClientEvent`]).
//!
//! Only what a client acts on is typed; every other event, and one of
//! those whose fields do not read, is [`ServerEvent::Unknown`] with its
//! `type` and the event as sent. The `lmgw.*` events are a bound
//! session's own (chat-voice design §8.6); their payloads are
//! `lmgw-api-types`' types ([`MessageVoice`], [`VoiceTiming`], [`ChatReply`]),
//! read strictly: an event whose fields do not read is
//! [`ServerEvent::Unknown`]. A chat frame's data stays a
//! `serde_json::Value`; an FFI wrapper hands it over as JSON text.
//!
//! **A WebRTC call** (`POST /v1/realtime/calls`, realtime calls design;
//! [`crate::requests::calls`]) carries the same events on its `oai-events`
//! data channel, without the audio deltas (the audio is on RTP), and adds:
//! - OpenAI's `output_audio_buffer.started`, `.stopped` and `.cleared`, the
//!   player facts a call's client reads instead of its own playback;
//! - on a bound call, lmgw's own: [`ServerEvent::BindPending`],
//!   [`ServerEvent::SessionClosed`] (its end, read by [`call_close`]),
//!   [`ServerEvent::Starved`], and `lmgw.event.part`, the parts an event
//!   larger than the channel's message size travels in, which a
//!   [`Joiner`] joins before [`parse`] (module [`parts`]);
//! - the client's own, among [`ClientEvent`]'s: the stop
//!   ([`ClientEvent::ResponseStop`]), the take-over, the playout reports,
//!   the turn mode switch ([`ClientEvent::BoundSessionUpdate`]) and OpenAI's
//!   [`ClientEvent::OutputAudioBufferClear`], split by a [`Splitter`] where
//!   one is larger than lmgw's message size.
//!
//! **Late MCP task results** (MCP Tasks design §3.4, §4.2). A tool whose
//! server runs it as a task answers its turn at once (`started, job <id>`,
//! the `tool` frame's `task`), and its result enters the thread later as a
//! message of role `tool` — only while no turn of the thread runs. The
//! bound session then says [`ServerEvent::TaskDone`]. lmgw starts no turn
//! for it: the client picks the moment — never while the user talks or the
//! model speaks — and sends [`ClientEvent::ResponseCreate`] with no new
//! words (no commit, no item). That response is a **continuation**: it
//! answers every result no reply answered yet, the thread's model seeing
//! each as a call of `lmgw__job_result` and its result, and its reply is
//! stored as any spoken reply. A client that binds to speak a result it
//! learned of from the feed (`task.done`; bind with `takeover=never`, so it
//! never takes the voice from another device) does the same: the results
//! no reply answered are owed from the bind on. The continuation's
//! refusals:
//! - `empty_turn` — nothing to answer: an `error` event before any
//!   `response.created`, or `response.done {failed}` when a turn of
//!   another window answered the results meanwhile;
//! - `turn_running` — a turn of the thread is running (a text send, another
//!   device's answer); `response.done {failed}`, and nothing is cancelled:
//!   the results that wait enter when it ends, and its reply may answer
//!   them.
//!
//! Words said before the `response.create` make it an ordinary turn, whose
//! reply answers the words and the results alike. Only the client's own
//! `response.create` continues, and only one with no turn the client
//! committed before it: the automatic response turn detection creates for
//! a turn without words (a cough) stays `empty_turn`, and so does a
//! push-to-talk `input_audio_buffer.commit` of silence and its
//! `response.create` — a cough's equal. A continuation a cough cuts before
//! anything of it was heard runs again after the cough. A
//! [`ServerEvent::TaskDone`] that comes while a response is open (its audio
//! still playing included) is not answered by that response: a result that
//! ends mid-turn enters the thread only after that turn ends (lmgw-core's
//! `realtime::thread::tasks`). Keep its continuation owed and send
//! `response.create` at the next quiet point; should the result have been
//! answered after all, that response ends `empty_turn`.

use lmgw_api_types::realtime::{
    BIND_PENDING, OUTPUT_AUDIO_BUFFER_CLEARED, OUTPUT_AUDIO_BUFFER_STARTED,
    OUTPUT_AUDIO_BUFFER_STOPPED, OUTPUT_AUDIO_STARVED, SESSION_CLOSED,
};
use serde::Deserialize;
use serde_json::Value;

pub use lmgw_api_types::chat::TASK_DONE_EVENT;
pub use lmgw_api_types::chat_approvals::{ApprovalDecidedEvent, ApprovalRequest};
pub use lmgw_api_types::chat_feed::TaskDone;
pub use lmgw_api_types::chat_threads::{MessageVoice, VoiceTiming};
pub use lmgw_api_types::chat_voice::{ChatReply, ModelState};
pub use lmgw_api_types::realtime::{
    BindPending, ClosedKind, EventPart, OutputAudioStarved, RevokeKind, SessionClosed, TurnMode,
    CLOSE_GOING_AWAY, CLOSE_OUT_OF_REACH, CLOSE_REVOKED, CLOSE_TAKEN_OVER, SHUTTING_DOWN,
};

mod call;
mod client;
mod close;
pub mod parts;
#[cfg(test)]
mod tests;

pub use call::{BoundSession, PlayoutReport};
pub use client::{ClientEvent, TurnDetection, Updates};
pub use close::{call_close, close_kind, CallClose, CloseKind};
pub use parts::{Joined, Joiner, Splitter};

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
        voice: Box<MessageVoice>,
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
    ChatReply(Box<ChatReply>),
    /// `lmgw.model.state`: a stage's model state.
    ModelState(ModelState),
    /// `lmgw.response.timing`: a response's timing line, with the models
    /// that answered each stage.
    Timing(Box<VoiceTiming>),
    /// `lmgw.chat.thread`: the thread as a response re-read it.
    Thread(ThreadFacts),
    /// `conversation.item.done` of an `mcp_approval_request` item: a call
    /// of the thread's turn waits for an approval (client-apps design
    /// §6.4). Its `approval_request_id` is the item's id; its `call_id` is
    /// `None` (OpenAI's item has none): the [`Self::ChatFrame`] relaying the
    /// turn's `tool {event: "approval"}` frame, sent right after the item,
    /// carries it beside the same `approval_request_id`. Answer with
    /// [`ClientEvent::ApprovalResponse`], then [`ClientEvent::ResponseCreate`];
    /// keep a spoken answer out of the session (no commit while it is open:
    /// a user turn declines the call) — transcribe it with
    /// [`crate::requests::transcribe`].
    ApprovalRequest(ApprovalRequest),
    /// `lmgw.approval.decided`: a call this session showed was decided by
    /// another client; close what asks for it.
    ApprovalDecided(ApprovalDecidedEvent),
    /// `lmgw.task.done` ([`TASK_DONE_EVENT`]): a late MCP task result
    /// entered the thread as message `message_id` (role `tool`), with the
    /// feed's `task.done` facts — `id` the gateway's id of the task, `task_id`
    /// the server's, `status` `completed`, `failed`, `cancelled` or
    /// `abandoned`, `by` who cancelled it for a cancel the gateway sent. A
    /// [`ClientEvent::ResponseCreate`] with no new words answers it (module
    /// doc).
    TaskDone(TaskDone),
    /// `output_audio_buffer.started` (a WebRTC call's): lmgw sent response
    /// `response_id`'s first audio frame, so its audio is in the player. On
    /// a call this is the player fact `response.output_audio.delta` is on a
    /// WebSocket ([`crate::voice::Machine::output_started`]).
    OutputAudioBufferStarted { response_id: String },
    /// `output_audio_buffer.stopped`, right after the `response.done` of a
    /// response whose audio played to its end: the client has played it
    /// ([`crate::voice::Machine::output_stopped`]).
    OutputAudioBufferStopped { response_id: String },
    /// `output_audio_buffer.cleared`, instead of `stopped`, right after the
    /// `response.done` of a response whose audio was cut before its end: lmgw
    /// stopped sending it (a confirmed barge-in, a stop, a cancel, a
    /// `clear`). The barge-in verdict: mute the remote track until the next
    /// `started` ([`crate::voice::Machine::output_cleared`]).
    OutputAudioBufferCleared { response_id: String },
    /// `lmgw.output_audio.starved` (a bound call's): the response's audio ran
    /// dry, or came again ([`crate::voice::Machine::starved`]).
    Starved(OutputAudioStarved),
    /// `lmgw.session.bind_pending` (a call bound with `takeover=ask`): the
    /// thread is held by `held_by`, and no session runs until the client's
    /// [`ClientEvent::TakeOver`], or until the holder ends and
    /// `session.created` follows by itself. Every other client event
    /// meanwhile is answered `error` `bind_pending`.
    BindPending(BindPending),
    /// `lmgw.session.closed` (a bound call's last event): why lmgw ends the
    /// call; [`call_close`] reads it as a [`CloseKind`].
    SessionClosed(SessionClosed),
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
    /// `session.lmgw.turn_mode`, on a session bound to a Chat thread: how
    /// its turns end, as lmgw resolved it from the thread's voice settings
    /// (realtime calls design §2.2). A client shows it and switches it with
    /// [`ClientEvent::BoundSessionUpdate`]; it derives no turn detection
    /// itself.
    /// `None` on an unbound session, or from a gateway before the field.
    pub turn_mode: Option<TurnMode>,
    /// `session.lmgw.resolved`: the aliases that answer, and the voice.
    pub chat: Option<String>,
    pub asr: Option<String>,
    pub tts: Option<String>,
    pub voice: Option<String>,
    /// `session.lmgw.resolved.chat_thread`.
    pub thread: Option<ThreadFacts>,
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
            turn_mode: TurnMode::deserialize(&session["lmgw"]["turn_mode"]).ok(),
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
        "lmgw.chat.user" => match serde_json::from_value::<MessageVoice>(v["voice"].clone()) {
            Ok(voice) => ServerEvent::ChatUser {
                message_id: v["message_id"].as_i64().unwrap_or_default(),
                content: s(&v, "content"),
                voice: Box::new(voice),
                response_id: opt(&v["response_id"]),
            },
            Err(_) => ServerEvent::Unknown { kind, data: v },
        },
        "lmgw.chat.input" => ServerEvent::ChatInput {
            response_id: s(&v, "response_id"),
            input: s(&v, "input"),
            why: opt(&v["why"]),
        },
        "lmgw.chat.reply" => match serde_json::from_value::<ChatReply>(strip(v.clone())) {
            Ok(r) => ServerEvent::ChatReply(Box::new(r)),
            Err(_) => ServerEvent::Unknown { kind, data: v },
        },
        "conversation.item.truncated" => ServerEvent::Truncated {
            item_id: s(&v, "item_id"),
            audio_end_ms: v["audio_end_ms"].as_u64().unwrap_or_default(),
        },
        "lmgw.model.state" => match serde_json::from_value::<ModelState>(strip(v.clone())) {
            Ok(m) => ServerEvent::ModelState(m),
            Err(_) => ServerEvent::Unknown { kind, data: v },
        },
        "lmgw.response.timing" => match serde_json::from_value::<VoiceTiming>(strip(v.clone())) {
            Ok(t) => ServerEvent::Timing(Box::new(t)),
            Err(_) => ServerEvent::Unknown { kind, data: v },
        },
        "lmgw.chat.thread" => match ThreadFacts::of(&v["chat_thread"]) {
            Some(t) => ServerEvent::Thread(t),
            None => ServerEvent::Unknown { kind, data: v },
        },
        "conversation.item.done" if v["item"]["type"] == "mcp_approval_request" => {
            let item = &v["item"];
            ServerEvent::ApprovalRequest(ApprovalRequest {
                approval_request_id: s(item, "id"),
                server_label: s(item, "server_label"),
                name: s(item, "name"),
                arguments: s(item, "arguments"),
                // OpenAI's item has none: the `lmgw.chat.frame` relaying
                // the turn's `approval` frame, right after it, has it.
                call_id: None,
            })
        }
        TASK_DONE_EVENT => match serde_json::from_value::<TaskDone>(strip(v.clone())) {
            Ok(d) => ServerEvent::TaskDone(d),
            Err(_) => ServerEvent::Unknown { kind, data: v },
        },
        "lmgw.approval.decided" => {
            match serde_json::from_value::<ApprovalDecidedEvent>(strip(v.clone())) {
                Ok(d) => ServerEvent::ApprovalDecided(d),
                Err(_) => ServerEvent::Unknown { kind, data: v },
            }
        }
        OUTPUT_AUDIO_BUFFER_STARTED => ServerEvent::OutputAudioBufferStarted {
            response_id: s(&v, "response_id"),
        },
        OUTPUT_AUDIO_BUFFER_STOPPED => ServerEvent::OutputAudioBufferStopped {
            response_id: s(&v, "response_id"),
        },
        OUTPUT_AUDIO_BUFFER_CLEARED => ServerEvent::OutputAudioBufferCleared {
            response_id: s(&v, "response_id"),
        },
        OUTPUT_AUDIO_STARVED => match OutputAudioStarved::deserialize(&v) {
            Ok(e) => ServerEvent::Starved(e),
            Err(_) => ServerEvent::Unknown { kind, data: v },
        },
        BIND_PENDING => match BindPending::deserialize(&v) {
            Ok(e) => ServerEvent::BindPending(e),
            Err(_) => ServerEvent::Unknown { kind, data: v },
        },
        SESSION_CLOSED => match SessionClosed::deserialize(&v) {
            Ok(e) => ServerEvent::SessionClosed(e),
            Err(_) => ServerEvent::Unknown { kind, data: v },
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
