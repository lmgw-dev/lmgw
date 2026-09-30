//! Knowledge bases in Chat threads (chat-complete design §9.3), the store's
//! half: how a thread uses its bases ([`KbMode`]), what a retrieval left on a
//! user message ([`ChatContext`]), and the one write that sets a message's
//! `kb_refs` and `context` together.
//!
//! The stored context is its own type rather than the retrieval's
//! [`Excerpt`](crate::knowledge::retrieve::Excerpt): it is a persisted format
//! (replayed into every later turn, exported with the thread), so it must
//! keep reading when the search's own result type grows or changes. Every
//! field defaults, so a context written by another build still loads.

use sqlx::SqlitePool;

use super::*;

/// How a thread's knowledge bases reach the model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KbMode {
    /// Search before every turn and send the excerpts with the user message.
    #[default]
    Auto,
    /// Attach the `kb__*` tools, restricted to the thread's bases; the model
    /// searches when it decides to.
    Tool,
}

impl KbMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Tool => "tool",
        }
    }

    /// The column's text; anything else is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }
}

/// One excerpt as a user message stores it: enough to number it in the
/// `<context>` block, cite it as `[n]` and open it in the source viewer.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ContextExcerpt {
    pub kb_id: i64,
    /// The base's name when it was retrieved.
    pub kb: String,
    pub file_id: i64,
    /// The file's name when it was retrieved.
    pub file: String,
    /// 1-based PDF page; `None` for other files.
    pub page: Option<i64>,
    pub chunk_id: String,
    pub heading_path: String,
    /// The chunk's text, verbatim.
    pub text: String,
    pub score: f32,
    pub tokens: usize,
    /// Byte range in the file's extracted text (the source viewer's
    /// highlight).
    pub span_start: i64,
    pub span_end: i64,
    /// The sha256 of the file version the chunk was cut from, so the source
    /// viewer can say the document changed since. Empty for a context stored
    /// before it was kept.
    pub file_sha: String,
}

impl From<&crate::knowledge::retrieve::Excerpt> for ContextExcerpt {
    fn from(e: &crate::knowledge::retrieve::Excerpt) -> Self {
        Self {
            kb_id: e.kb_id,
            kb: e.kb.clone(),
            file_id: e.file_id,
            file: e.file.clone(),
            page: e.page,
            chunk_id: e.chunk_id.clone(),
            heading_path: e.heading_path.clone(),
            text: e.text.clone(),
            score: e.score,
            tokens: e.tokens,
            span_start: e.span_start,
            span_end: e.span_end,
            file_sha: e.file_sha.clone(),
        }
    }
}

/// The retrieval that ran for one user message — `chat_messages.context`.
/// Excerpt `n` in the `<context>` block (and `[n]` in the answer) is
/// `excerpts[n - 1]`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ChatContext {
    pub excerpts: Vec<ContextExcerpt>,
    /// Tokens the excerpts use (heading paths included).
    pub tokens: usize,
    /// Excerpts that ranked but did not fit the budget.
    pub dropped: usize,
    /// The budget the retrieval ran with.
    pub budget_tokens: usize,
    /// Why something was not searched, or not fully — one sentence each.
    pub notes: Vec<String>,
    /// The bases that were searched, by name.
    pub searched: Vec<String>,
    /// The bases asked for: the thread's `kb_ids` and the message's
    /// `kb_refs`, as they were then.
    pub kb_ids: Vec<i64>,
    /// What was searched for.
    pub query: String,
    pub ms: f64,
}

/// A JSON id list column (`kb_ids`, `kb_refs`); a hand-edited value that
/// does not parse reads as none rather than failing the whole thread.
pub(super) fn id_list(text: &str) -> Vec<i64> {
    serde_json::from_str(text).unwrap_or_default()
}

pub(super) fn id_list_json(ids: &[i64]) -> String {
    serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_string())
}

/// `chat_messages.context` as stored; unreadable text reads as none.
pub(super) fn context_from_column(text: Option<String>) -> Option<ChatContext> {
    text.and_then(|t| serde_json::from_str(&t).ok())
}

pub(super) fn context_json(c: Option<&ChatContext>) -> Option<String> {
    c.and_then(|c| serde_json::to_string(c).ok())
}

/// Set one message's `kb_refs` and `context` together. `false` when no such
/// message is in this thread. Not activity: the thread's `updated_at` is
/// left alone (the turn that wrote it already moved it).
pub async fn set_chat_message_knowledge(
    pool: &SqlitePool,
    thread_id: i64,
    id: i64,
    kb_refs: &[i64],
    context: Option<&ChatContext>,
) -> DbResult<bool> {
    let n = sqlx::query(
        "UPDATE chat_messages SET kb_refs = ?3, context = ?4 WHERE id = ?1 AND thread_id = ?2",
    )
    .bind(id)
    .bind(thread_id)
    .bind(id_list_json(kb_refs))
    .bind(context_json(context))
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}
