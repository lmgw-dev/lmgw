//! Client → server events: the whole GA union, eleven events (realtime design
//! §2.3).
//!
//! Every event may carry an `event_id`; clients differ (the Python SDK sends
//! none, `@openai/agents` names its `response.create`s), and an `error` about
//! an event echoes the id when there was one so the client can match it.
//! Unknown fields inside a known event are ignored, as everywhere on input.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::item::Item;
use super::session::{MaxOutputTokens, Modality};
use super::tools::{Tool, ToolChoice};

/// One client event, tagged on `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientEvent {
    /// A **partial** session: merged into the current one (`merge`), and
    /// answered with the full session. Kept as a JSON object rather than a
    /// typed [`super::Session`] because a merge has to tell an absent key from
    /// an explicit `null` (`turn_detection: null` switches to manual turns).
    #[serde(rename = "session.update")]
    SessionUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_id: Option<String>,
        session: Map<String, Value>,
    },
    /// Base64 PCM16 at the session's input rate.
    #[serde(rename = "input_audio_buffer.append")]
    InputAudioBufferAppend {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_id: Option<String>,
        audio: String,
    },
    #[serde(rename = "input_audio_buffer.commit")]
    InputAudioBufferCommit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_id: Option<String>,
    },
    #[serde(rename = "input_audio_buffer.clear")]
    InputAudioBufferClear {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_id: Option<String>,
    },
    #[serde(rename = "conversation.item.create")]
    ConversationItemCreate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_id: Option<String>,
        /// Insert after this item; absent = append (§7.1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous_item_id: Option<String>,
        item: Item,
    },
    #[serde(rename = "conversation.item.retrieve")]
    ConversationItemRetrieve {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_id: Option<String>,
        item_id: String,
    },
    /// Cut an assistant audio item at what the listener heard (§7.3).
    #[serde(rename = "conversation.item.truncate")]
    ConversationItemTruncate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_id: Option<String>,
        item_id: String,
        content_index: u32,
        audio_end_ms: u64,
    },
    #[serde(rename = "conversation.item.delete")]
    ConversationItemDelete {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_id: Option<String>,
        item_id: String,
    },
    #[serde(rename = "response.create")]
    ResponseCreate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response: Option<ResponseCreateParams>,
    },
    #[serde(rename = "response.cancel")]
    ResponseCancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
    },
    /// WebRTC only in OpenAI's API (§2.3); parsed so it can be answered.
    #[serde(rename = "output_audio_buffer.clear")]
    OutputAudioBufferClear {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_id: Option<String>,
    },
}

impl ClientEvent {
    /// The client's own id for this event, echoed by an `error` about it.
    pub fn event_id(&self) -> Option<&str> {
        match self {
            Self::SessionUpdate { event_id, .. }
            | Self::InputAudioBufferAppend { event_id, .. }
            | Self::InputAudioBufferCommit { event_id }
            | Self::InputAudioBufferClear { event_id }
            | Self::ConversationItemCreate { event_id, .. }
            | Self::ConversationItemRetrieve { event_id, .. }
            | Self::ConversationItemTruncate { event_id, .. }
            | Self::ConversationItemDelete { event_id, .. }
            | Self::ResponseCreate { event_id, .. }
            | Self::ResponseCancel { event_id, .. }
            | Self::OutputAudioBufferClear { event_id } => event_id.as_deref(),
        }
    }

    /// The wire `type`, for log lines and errors.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::SessionUpdate { .. } => "session.update",
            Self::InputAudioBufferAppend { .. } => "input_audio_buffer.append",
            Self::InputAudioBufferCommit { .. } => "input_audio_buffer.commit",
            Self::InputAudioBufferClear { .. } => "input_audio_buffer.clear",
            Self::ConversationItemCreate { .. } => "conversation.item.create",
            Self::ConversationItemRetrieve { .. } => "conversation.item.retrieve",
            Self::ConversationItemTruncate { .. } => "conversation.item.truncate",
            Self::ConversationItemDelete { .. } => "conversation.item.delete",
            Self::ResponseCreate { .. } => "response.create",
            Self::ResponseCancel { .. } => "response.cancel",
            Self::OutputAudioBufferClear { .. } => "output_audio_buffer.clear",
        }
    }
}

/// `response.create.response`: one response's overrides (§7.2).
///
/// `conversation: "none"` and `input` (out-of-band responses) parse but are
/// §19 — they imply concurrent responses, which §4.3 does not allow; the
/// responder refuses them by name.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ResponseCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_modalities: Option<Vec<Modality>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<MaxOutputTokens>,
    /// Echoed on the response object (`response.created` / `.done`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<Value>,
    /// lmgw's own knobs for this one response (WP10) — strict, like
    /// `session.lmgw`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lmgw: Option<ResponseLmgw>,
}

/// `response.create.response.lmgw` (WP10 D2): **strict** — an unknown key is
/// an `error`, since a misspelt knob would otherwise be ignored silently.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseLmgw {
    /// This response's speech instructions, over the session's: `""` =
    /// none for this response; absent or `null` = the session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_instructions: Option<String>,
}
