//! Conversation items (realtime design §7.1): user, assistant and system
//! messages, function calls and their outputs, and server-side MCP calls and
//! listings (realtime-server-tools design §1.2, §2.2).
//!
//! Ids are optional on input — `conversation.item.create` normally arrives
//! without one, and the session mints `item_…` — and always set on the
//! server's echo. The approval items parse (client-apps design §6.4): a
//! session bound to a chat thread takes a client's `mcp_approval_response`,
//! and any other refuses both by name (`conversation`). `input_image`
//! content (§19) fails to parse, as an `error` naming the variant.

use serde::{Deserialize, Serialize};

use super::mcp::{McpApprovalRequestItem, McpApprovalResponseItem, McpCallItem, McpListToolsItem};

/// One conversation item, tagged on `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    Message(MessageItem),
    FunctionCall(FunctionCallItem),
    FunctionCallOutput(FunctionCallOutputItem),
    /// Boxed: its always-written fields make it twice any other item, and
    /// every event that carries an item would pay for it.
    McpCall(Box<McpCallItem>),
    McpListTools(McpListToolsItem),
    /// A bound session's gated call and a client's answer to it
    /// (client-apps design §6.4).
    McpApprovalRequest(McpApprovalRequestItem),
    McpApprovalResponse(McpApprovalResponseItem),
}

impl Item {
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Message(m) => m.id.as_deref(),
            Self::FunctionCall(c) => c.id.as_deref(),
            Self::FunctionCallOutput(o) => o.id.as_deref(),
            Self::McpCall(c) => c.id.as_deref(),
            Self::McpListTools(l) => l.id.as_deref(),
            Self::McpApprovalRequest(r) => r.id.as_deref(),
            Self::McpApprovalResponse(r) => r.id.as_deref(),
        }
    }

    /// Give an item that arrived without an id the one the session minted.
    pub fn set_id_if_missing(&mut self, mint: impl FnOnce() -> String) {
        let slot = match self {
            Self::Message(m) => &mut m.id,
            Self::FunctionCall(c) => &mut c.id,
            Self::FunctionCallOutput(o) => &mut o.id,
            Self::McpCall(c) => &mut c.id,
            Self::McpListTools(l) => &mut l.id,
            Self::McpApprovalRequest(r) => &mut r.id,
            Self::McpApprovalResponse(r) => &mut r.id,
        };
        if slot.is_none() {
            *slot = Some(mint());
        }
    }
}

/// `"realtime.item"`, the `object` every item the server echoes carries.
pub const ITEM_OBJECT: &str = "realtime.item";

/// A message: user (`input_text` / `input_audio`), assistant
/// (`output_text` / `output_audio`) or system (`input_text`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
    pub role: Role,
    pub content: Vec<ContentPart>,
}

/// A function call the model made (§7.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCallItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    /// `in_progress` on `output_item.added`, `completed` exactly once on
    /// `output_item.done` — the SDK runs the tool on every completed event
    /// (§2.3, §7.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
    /// Session-unique `call_…` (§7.4). Optional on input only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub name: String,
    /// The arguments as the model streamed them: a JSON *string*.
    #[serde(default)]
    pub arguments: String,
}

/// The client's answer to a function call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCallOutputItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
    pub call_id: String,
    pub output: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Completed,
    Incomplete,
    InProgress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

/// One part of a message's `content`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    InputText {
        text: String,
    },
    /// A committed segment (base64 PCM16, when sent) and its transcript.
    /// `transcript` is always written: `null` is "not transcribed yet" — the
    /// user item of a commit is announced before its ASR call returns
    /// (§2.3 steps 4–6) — which is what OpenAI sends there too.
    InputAudio {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<String>,
        #[serde(default)]
        transcript: Option<String>,
    },
    OutputText {
        text: String,
    },
    /// What the assistant said: the audio (when echoed) and its transcript —
    /// after a truncate, what was heard (§7.3).
    OutputAudio {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transcript: Option<String>,
    },
}

/// The `part` of `response.content_part.added` / `.done`. The GA reference
/// types it `"text"` / `"audio"` rather than an item's `output_*` names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsePart {
    Text {
        #[serde(default)]
        text: String,
    },
    Audio {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<String>,
        #[serde(default)]
        transcript: String,
    },
}
