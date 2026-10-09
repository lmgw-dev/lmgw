//! Server → client events (realtime design §2.3), the response object, and
//! the error object.
//!
//! **`event_id` is not a field of any variant.** Every server event must
//! carry one, minted server-side, and the SDK's schemas drop an event without
//! it — so it is added in exactly one place, the writer, by wrapping the
//! event in a [`ServerFrame`]. No code path that builds an event can forget
//! it.
//!
//! **Delta events share [`PartRef`]** (`response_id`, `item_id`,
//! `output_index`, `content_index`), flattened in, for the same reason: the
//! SDK silently drops a delta missing any of them, audio included.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::item::{Item, ResponsePart};
use super::session::{ChatThreadRef, MaxOutputTokens, Modality, Session};

/// What goes on the wire: one event plus its server-minted `event_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerFrame {
    pub event_id: String,
    #[serde(flatten)]
    pub event: ServerEvent,
}

/// Where a content delta belongs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartRef {
    pub response_id: String,
    pub item_id: String,
    pub output_index: u32,
    pub content_index: u32,
}

/// One server event, tagged on `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerEvent {
    // -- Session ---------------------------------------------------------
    #[serde(rename = "error")]
    Error { error: ErrorObject },
    #[serde(rename = "session.created")]
    SessionCreated { session: Box<Session> },
    #[serde(rename = "session.updated")]
    SessionUpdated { session: Box<Session> },

    // -- Input buffer ----------------------------------------------------
    /// `item_id` names the user item that *will* be created when speech
    /// stops; `audio_start_ms` includes the prefix padding (§2.3).
    #[serde(rename = "input_audio_buffer.speech_started")]
    SpeechStarted {
        audio_start_ms: u64,
        item_id: String,
    },
    #[serde(rename = "input_audio_buffer.speech_stopped")]
    SpeechStopped { audio_end_ms: u64, item_id: String },
    #[serde(rename = "input_audio_buffer.committed")]
    Committed {
        previous_item_id: Option<String>,
        item_id: String,
    },
    #[serde(rename = "input_audio_buffer.cleared")]
    Cleared {},
    #[serde(rename = "input_audio_buffer.timeout_triggered")]
    TimeoutTriggered {
        audio_start_ms: u64,
        audio_end_ms: u64,
        item_id: String,
    },

    // -- Items -----------------------------------------------------------
    #[serde(rename = "conversation.item.added")]
    ItemAdded {
        previous_item_id: Option<String>,
        item: Item,
    },
    #[serde(rename = "conversation.item.done")]
    ItemDone {
        previous_item_id: Option<String>,
        item: Item,
    },
    #[serde(rename = "conversation.item.retrieved")]
    ItemRetrieved { item: Item },
    #[serde(rename = "conversation.item.truncated")]
    ItemTruncated {
        item_id: String,
        content_index: u32,
        audio_end_ms: u64,
    },
    #[serde(rename = "conversation.item.deleted")]
    ItemDeleted { item_id: String },

    // -- MCP listing (realtime-server-tools §1.2) -------------------------
    /// The `mcp_list_tools` item a label's listing fills in.
    #[serde(rename = "mcp_list_tools.in_progress")]
    McpListToolsInProgress { item_id: String },
    #[serde(rename = "mcp_list_tools.completed")]
    McpListToolsCompleted { item_id: String },
    /// Carries only the item: the reason goes out as an `error` with code
    /// `mcp_list_tools_failed` beside it.
    #[serde(rename = "mcp_list_tools.failed")]
    McpListToolsFailed { item_id: String },

    // -- Input transcription ---------------------------------------------
    #[serde(rename = "conversation.item.input_audio_transcription.delta")]
    TranscriptionDelta {
        item_id: String,
        content_index: u32,
        delta: String,
    },
    #[serde(rename = "conversation.item.input_audio_transcription.completed")]
    TranscriptionCompleted {
        item_id: String,
        content_index: u32,
        transcript: String,
        usage: TranscriptionUsage,
    },
    #[serde(rename = "conversation.item.input_audio_transcription.failed")]
    TranscriptionFailed {
        item_id: String,
        content_index: u32,
        error: TranscriptionError,
    },

    // -- Response, in order (§2.3) ---------------------------------------
    #[serde(rename = "response.created")]
    ResponseCreated { response: Box<ResponseObject> },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        response_id: String,
        output_index: u32,
        item: Item,
    },
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {
        #[serde(flatten)]
        at: PartRef,
        part: ResponsePart,
    },
    #[serde(rename = "response.output_audio.delta")]
    OutputAudioDelta {
        #[serde(flatten)]
        at: PartRef,
        /// Base64 PCM16 at the session's output rate.
        delta: String,
    },
    #[serde(rename = "response.output_audio.done")]
    OutputAudioDone {
        #[serde(flatten)]
        at: PartRef,
    },
    #[serde(rename = "response.output_audio_transcript.delta")]
    OutputAudioTranscriptDelta {
        #[serde(flatten)]
        at: PartRef,
        delta: String,
    },
    #[serde(rename = "response.output_audio_transcript.done")]
    OutputAudioTranscriptDone {
        #[serde(flatten)]
        at: PartRef,
        transcript: String,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        #[serde(flatten)]
        at: PartRef,
        delta: String,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        #[serde(flatten)]
        at: PartRef,
        text: String,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        response_id: String,
        item_id: String,
        output_index: u32,
        call_id: String,
        delta: String,
    },
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        response_id: String,
        item_id: String,
        output_index: u32,
        call_id: String,
        name: String,
        arguments: String,
    },
    /// A server-side call's arguments (realtime-server-tools §2.2). No
    /// `obfuscation`: `@openai/agents` files the event as a generic one,
    /// which it ignores anyway, as it does `function_call_arguments.delta`.
    #[serde(rename = "response.mcp_call_arguments.delta")]
    McpCallArgumentsDelta {
        response_id: String,
        item_id: String,
        output_index: u32,
        delta: String,
    },
    #[serde(rename = "response.mcp_call_arguments.done")]
    McpCallArgumentsDone {
        response_id: String,
        item_id: String,
        output_index: u32,
        arguments: String,
    },
    /// A server-side call runs, and how it ended (realtime-server-tools
    /// §2.4). The GA reference gives these no `response_id`.
    #[serde(rename = "response.mcp_call.in_progress")]
    McpCallInProgress { item_id: String, output_index: u32 },
    #[serde(rename = "response.mcp_call.completed")]
    McpCallCompleted { item_id: String, output_index: u32 },
    #[serde(rename = "response.mcp_call.failed")]
    McpCallFailed { item_id: String, output_index: u32 },
    #[serde(rename = "response.content_part.done")]
    ContentPartDone {
        #[serde(flatten)]
        at: PartRef,
        part: ResponsePart,
    },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        response_id: String,
        output_index: u32,
        item: Item,
    },
    #[serde(rename = "response.done")]
    ResponseDone { response: Box<ResponseObject> },

    // -- Optional --------------------------------------------------------
    #[serde(rename = "rate_limits.updated")]
    RateLimitsUpdated { rate_limits: Vec<Value> },

    // -- lmgw's own (chat-voice design §8.7) ------------------------------
    // Sent by a session bound to a chat thread only: the binding is the
    // opt-in, so no stock client ever sees one.
    /// A chat-turn frame, verbatim: `turn`, `retrieval`, `delta`,
    /// `reasoning`, `tool`, `usage`, `stats`, `stop`, `state`, `error`,
    /// `done` — and the response it belongs to.
    #[serde(rename = "lmgw.chat.frame")]
    LmgwChatFrame {
        response_id: String,
        event: String,
        data: Value,
    },
    /// The user message a response's spoken turns were written as.
    #[serde(rename = "lmgw.chat.user")]
    LmgwChatUser {
        message_id: i64,
        content: String,
        voice: Value,
        /// The response whose turns it holds, when that response heard them
        /// as audio (voice-audio-input design §3.3): its row is written
        /// once they are transcribed, racing the reply's release, so the
        /// page puts it before that response's reply.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
    },
    /// How a bound response's turns reach the chat model, while audio
    /// input is on (voice-audio-input design §5): `audio` or `transcript`,
    /// and why the transcript. Sent at the response's launch, and again
    /// when its audio was refused and it goes as its transcript.
    #[serde(rename = "lmgw.chat.input")]
    LmgwChatInput {
        response_id: String,
        input: crate::store::InputPath,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        why: Option<String>,
    },
    /// A spoken reply as finalized: `{content, unheard, voice}`,
    /// `{removed: true}` or `{skipped, voice}` (`voice` when the finalize
    /// wrote it).
    #[serde(rename = "lmgw.chat.reply")]
    LmgwChatReply {
        message_id: i64,
        #[serde(flatten)]
        reply: Map<String, Value>,
    },
    /// A model's state (§4.3): `{stage, alias, state, ms, …}`.
    #[serde(rename = "lmgw.model.state")]
    LmgwModelState {
        #[serde(flatten)]
        state: Map<String, Value>,
    },
    /// A response's timing line as data (§8.7), its served models with it.
    #[serde(rename = "lmgw.response.timing")]
    LmgwResponseTiming {
        #[serde(flatten)]
        timing: Map<String, Value>,
    },
    /// The thread as a response re-read it, when that differs from what the
    /// session said of it (`session.lmgw.resolved.chat_thread`, which it
    /// replaces): a title its first spoken turn named, or its admin tools
    /// switched on or off (§8.7, WP8 review m10).
    #[serde(rename = "lmgw.chat.thread")]
    LmgwChatThread { chat_thread: ChatThreadRef },
    /// A call whose `mcp_approval_request` item this session showed was
    /// decided by another client (client-apps design §6.4): the deciding
    /// client's request runs the turn on, and the session's next response
    /// renders it. `by` names who, as the feed names authors. `approve` is
    /// `false` for a call approved whose turn could not start; a call whose
    /// reply went or lost its calls (edited, deleted) is said with
    /// `approve: false` and `by: null`.
    #[serde(rename = "lmgw.approval.decided")]
    LmgwApprovalDecided {
        approval_request_id: String,
        approve: bool,
        by: Option<String>,
    },
    /// A late MCP task result entered the session's thread (MCP Tasks
    /// design §4.2): the feed's `task.done` facts, flat — `{thread_id,
    /// message_id, id, task_id, server_label, tool, status, by}`. Sent once
    /// no turn of the thread runs (the result enters only then), so never
    /// in the middle of a response that did not see it: one sent while a
    /// response runs entered at its turn's start, and that response answers
    /// it. Otherwise the client's `response.create` with no new words answers
    /// it (`lifecycle::bound`'s continuation).
    #[serde(rename = "lmgw.task.done")]
    LmgwTaskDone {
        #[serde(flatten)]
        done: lmgw_api_types::chat_feed::TaskDone,
    },
}

impl ServerEvent {
    /// An `error` event — the shape every failure inside an open session
    /// takes (§4.1), echoing the client event it is about.
    pub fn error(e: ErrorObject) -> Self {
        Self::Error { error: e }
    }
}

/// `error.error` (§2.3): `{type, code, message, param, event_id}`. All five
/// keys are always written, `null` where empty, as OpenAI does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorObject {
    #[serde(rename = "type")]
    pub kind: String,
    pub code: Option<String>,
    pub message: String,
    pub param: Option<String>,
    /// The client event this error is about, when it carried an id.
    pub event_id: Option<String>,
}

impl ErrorObject {
    /// An `invalid_request_error` — the type every client-caused error in an
    /// open session has.
    pub fn invalid(code: &str, message: impl Into<String>) -> Self {
        Self {
            kind: "invalid_request_error".into(),
            code: Some(code.into()),
            message: message.into(),
            param: None,
            event_id: None,
        }
    }

    pub fn with_param(mut self, param: impl Into<String>) -> Self {
        self.param = Some(param.into());
        self
    }

    pub fn for_event(mut self, event_id: Option<&str>) -> Self {
        self.event_id = event_id.map(str::to_string);
        self
    }

    /// A gateway error — a policy refusal, an unknown alias — in the
    /// session's error shape, with the code and type its HTTP form would
    /// have had.
    pub fn from_gateway(e: &crate::error::GatewayError) -> Self {
        Self {
            kind: e.openai_type().into(),
            code: Some(e.code().into()),
            message: e.to_string(),
            param: None,
            event_id: None,
        }
    }
}

/// `conversation.item.input_audio_transcription.completed`'s `usage`:
/// openai-python declares it required, and the GA type is billed tokens or
/// billed audio (live acceptance L3, §23). The event reports what the
/// cascade's ASR took as the committed segment's audio, so the variant is
/// `duration` — which `@openai/agents`' schema accepts too. Tokens a cloud
/// ASR reports go on its `request_logs` row, not into this event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptionUsage {
    /// The segment's length, pre-roll and trailing silence included.
    Duration { seconds: f64 },
}

/// The `error` of `…input_audio_transcription.failed` (§5.2). Unlike
/// [`ErrorObject`], its optional fields are **left out** when empty rather
/// than written as `null`: `@openai/agents`' schema for this event accepts a
/// missing `code` or `param` but rejects a null one, and an event that fails
/// the schema is dropped as a generic event (§2.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptionError {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}

impl From<ErrorObject> for TranscriptionError {
    fn from(e: ErrorObject) -> Self {
        Self {
            kind: e.kind,
            code: e.code,
            message: e.message,
            param: e.param,
        }
    }
}

/// The response object of `response.created` / `response.done`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseObject {
    pub id: String,
    /// `"realtime.response"`.
    pub object: String,
    pub status: ResponseStatus,
    pub status_details: Option<StatusDetails>,
    pub output: Vec<Item>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub output_modalities: Vec<Modality>,
    pub max_output_tokens: MaxOutputTokens,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<Value>,
    pub usage: Option<Usage>,
    /// The `response.create`'s own `metadata`, echoed.
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    InProgress,
    Completed,
    Cancelled,
    Failed,
    Incomplete,
}

/// Why a response ended the way it did — `{type: "cancelled", reason:
/// "turn_detected"}` for a barge-in (§2.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusDetails {
    #[serde(rename = "type")]
    pub kind: ResponseStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

/// `response.done.usage` (§11): the chat model's text tokens; a cascade has
/// no audio tokens, so their details are 0.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub input_token_details: TokenDetails,
    pub output_token_details: TokenDetails,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenDetails {
    pub text_tokens: u64,
    pub audio_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
}
