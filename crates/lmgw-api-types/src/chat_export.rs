//! The Chat export: the query of the three export routes and the
//! `lmgw.chat.v1` JSON file a thread exports as (alone, or as an entry of a
//! zip).
//!
//! The file is written straight into its writer, with each attachment's
//! bytes encoded as they go, so the gateway does not build these types to
//! write it; they describe it, and the gateway's own test reads every
//! export into them and compares what they write back with what was sent,
//! key by key.

use serde::{Deserialize, Serialize};

use crate::chat::MessageTask;
use crate::chat::{KbMode, ThreadMcp, ThreadVoice};
use crate::chat_threads::{AttachmentMeta, MessageContext, MessageVoice};

/// The `format` marker of the JSON export.
pub const EXPORT_FORMAT: &str = "lmgw.chat.v1";

/// The query of `GET /chat/api/threads/{id}/export`,
/// `GET /chat/api/folders/{id}/export` and `GET /chat/api/export`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExportQuery {
    /// `md` (default; `markdown` is accepted too) for a readable
    /// transcript, `json` for the lossless `lmgw.chat.v1` file. A zip holds
    /// one file of this format per thread.
    #[cfg_attr(feature = "schema", schemars(extend("enum" = ["md", "markdown", "json"])))]
    pub format: String,
    /// Zips only: `0` active threads, `1` archived threads only, `all`
    /// (default) both.
    #[cfg_attr(feature = "schema", schemars(extend("enum" = ["0", "1", "all"])))]
    pub archived: String,
}

/// A folder or personality profile named in an export.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExportRef {
    pub id: i64,
    pub name: String,
}

/// The `lmgw.chat.v1` file of one thread: every thread column, every
/// message, and every attachment with its bytes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatExport {
    /// Always `lmgw.chat.v1`.
    pub format: String,
    /// When the file was written (RFC 3339, UTC).
    pub exported_at: String,
    pub thread: ExportThread,
    /// The thread's folder; absent when it is in none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<ExportRef>,
    /// The thread's personality profile; `null` for none.
    pub profile: Option<ExportRef>,
    /// Oldest first.
    pub messages: Vec<ExportMessage>,
    /// The attachments not yet sent with a message.
    pub draft_attachments: Vec<ExportAttachment>,
}

/// A thread as the export carries it: the columns of the thread list's row
/// (`ThreadRow`) except the two derived from other rows, `last_message_at`
/// and `purge_at`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExportThread {
    /// Negative for a temporary thread.
    pub id: i64,
    pub title: String,
    pub model_alias: String,
    pub system_prompt: String,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i64>,
    pub top_p: Option<f64>,
    pub top_k: Option<i64>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub seed: Option<i64>,
    pub stop: Vec<String>,
    /// `chat` or `admin`.
    pub kind: String,
    pub mcp_tools: Vec<ThreadMcp>,
    pub reasoning_enabled: Option<bool>,
    pub reasoning_effort: Option<String>,
    pub reasoning_budget: Option<i64>,
    pub agent_id: Option<String>,
    pub pinned: bool,
    pub archived_at: Option<String>,
    pub folder_id: Option<i64>,
    pub profile_id: Option<i64>,
    pub kb_ids: Vec<i64>,
    pub kb_mode: KbMode,
    pub kb_budget_tokens: Option<i64>,
    pub voice: ThreadVoice,
    pub created_at: String,
    pub updated_at: String,
    /// The thread was held in memory only (a temporary chat).
    pub temporary: bool,
}

/// A message as the export carries it: the columns of a stored message with
/// its `ir_messages` parsed, and the files sent with it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExportMessage {
    pub id: i64,
    pub thread_id: i64,
    /// `user`, `assistant` or `tool`.
    pub role: String,
    pub content: String,
    pub reasoning: String,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    /// An agentic turn's tool calls and results as the JSON value the
    /// record holds; `null` for a plain turn. When the record does not
    /// parse, its raw text and `ir_messages_unparsed: true`.
    pub ir_messages: Option<serde_json::Value>,
    /// Set (to `true`) only when `ir_messages` is the raw text of a record
    /// that did not parse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ir_messages_unparsed: Option<bool>,
    pub kb_refs: Vec<i64>,
    pub context: Option<MessageContext>,
    pub model: Option<String>,
    pub answered_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images_note: Option<String>,
    pub voice: Option<MessageVoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<MessageTask>,
    pub created_at: String,
    pub attachments: Vec<ExportAttachment>,
}

/// An attachment as the export carries it: its metadata, position and bytes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExportAttachment {
    #[serde(flatten)]
    pub meta: AttachmentMeta,
    /// Its position among its message's files.
    pub ord: i64,
    /// When it was uploaded.
    pub created_at: String,
    /// The text extracted from it (a PDF's pages, an office file, an audio
    /// file's transcript); `null` when there is none.
    pub extracted: Option<String>,
    /// The file's bytes, base64 (standard alphabet, padded).
    pub data: String,
}
