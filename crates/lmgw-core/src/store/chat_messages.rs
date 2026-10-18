//! Chat messages rewritten in place (chat-complete design §3): the message
//! actions' half of the store — one message read, edited, deleted, or a
//! thread cut back to a point. Appending stays in [`super::chat`] and
//! [`super::chat_attachments`].
//!
//! Every write here takes the thread id as well as the message id and matches
//! both, so a message id from another thread is simply not found; and every
//! write that changed something bumps the thread's `updated_at`, the idle
//! clock the sidebar sorts by and the archive sweep reads. Each records its
//! `message.*` change in its own transaction, `by` its author
//! (`feed::record_messages`, client-apps design §2.2).

use sqlx::SqlitePool;

use super::chat::chat_message_from_row;
use super::*;

/// A message's editable columns, written together by
/// [`update_chat_message`]. The caller states every one of them: editing an
/// assistant reply clears its reasoning, token counts and tool record (they
/// no longer describe the text) and keeps its voice less what the edit made
/// stale, a continued reply gets the longer text and the last call's counts,
/// and a user message keeps its (empty) others.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatMessageUpdate {
    pub content: String,
    pub reasoning: String,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub ir_messages: Option<String>,
    /// How the turn was spoken (chat-voice design §3): an edited reply keeps
    /// its voice without the unheard rest and the timing
    /// ([`MessageVoice::edited`]).
    pub voice: Option<MessageVoice>,
}

/// A turn's reply as it is saved ([`append_chat_reply`],
/// [`continue_chat_reply`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatReply {
    pub content: String,
    pub reasoning: String,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub ir_messages: Option<String>,
    /// The alias the turn asked for ([`ChatMessageRow::model`]).
    pub model: Option<String>,
    /// The alias that answered instead ([`ChatMessageRow::answered_by`]).
    pub answered_by: Option<String>,
    /// What a fallback that cannot see was sent instead of the images
    /// ([`ChatMessageRow::images_note`]).
    pub images_note: Option<String>,
    /// A spoken reply's voice ([`ChatMessageRow::voice`]). A continue never
    /// writes it: the continued row keeps its own, less the unheard rest
    /// ([`continue_chat_reply`]).
    pub voice: Option<MessageVoice>,
    /// A gated turn's calls (client-apps design §6.2): `Some` when the turn
    /// stopped on one, and on a reply a resumed turn saves.
    pub pending_approvals: Option<super::PendingApprovals>,
}

/// What [`continue_chat_reply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinueSave {
    Saved,
    /// The row's text is no longer the one the continue was shown (it was
    /// edited meanwhile): nothing was written.
    Changed,
    /// No such message in this thread.
    Gone,
}

/// Append a turn's reply as a new assistant row and bump the thread's
/// `updated_at`; its id.
pub async fn append_chat_reply(pool: &SqlitePool, thread_id: i64, r: &ChatReply) -> DbResult<i64> {
    append_chat_reply_by(pool, thread_id, r, None).await
}

/// [`append_chat_reply`], `by` naming the turn's starter in the feed's
/// `message.added` record and the `approval.requested` records of a gated
/// turn's calls.
pub async fn append_chat_reply_by(
    pool: &SqlitePool,
    thread_id: i64,
    r: &ChatReply,
    by: Option<&str>,
) -> DbResult<i64> {
    let mut tx = super::begin_write(pool).await?;
    let id = sqlx::query(
        "INSERT INTO chat_messages
           (thread_id, role, content, reasoning, prompt_tokens, completion_tokens, ir_messages,
            model, answered_by, voice, images_note, pending_approvals)
         VALUES (?1, 'assistant', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )
    .bind(thread_id)
    .bind(&r.content)
    .bind(&r.reasoning)
    .bind(r.prompt_tokens)
    .bind(r.completion_tokens)
    .bind(&r.ir_messages)
    .bind(&r.model)
    .bind(&r.answered_by)
    .bind(super::chat_voice::message_voice_json(r.voice.as_ref()))
    .bind(&r.images_note)
    .bind(
        r.pending_approvals
            .as_ref()
            .map(super::PendingApprovals::to_json),
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    feed::record_added(&mut tx, thread_id, id, by).await?;
    super::chat_approvals::record_requested(
        &mut tx,
        thread_id,
        id,
        r.pending_approvals.as_ref(),
        by,
    )
    .await?;
    touch_thread(&mut tx, thread_id).await?;
    tx.commit().await?;
    Ok(id)
}

/// Save a continue: reply `id`'s columns become `r` (its whole new text —
/// the prefix the model was shown plus what it wrote), but only while the
/// row's text, trailing whitespace aside, is still `prefix`. Read and written
/// in one transaction, so an edit cannot land between the check and the
/// write (review R1 finding 1). A spoken reply keeps its voice, less the
/// unheard rest: the continuation follows the heard text (chat-voice design
/// §3). `by`: the continue's caller, in the feed's `message.updated`.
pub async fn continue_chat_reply(
    pool: &SqlitePool,
    thread_id: i64,
    id: i64,
    prefix: &str,
    r: &ChatReply,
    by: feed::By<'_>,
) -> DbResult<ContinueSave> {
    let mut tx = super::begin_write(pool).await?;
    let now: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT content, voice FROM chat_messages
         WHERE id = ?1 AND thread_id = ?2 AND role = 'assistant'",
    )
    .bind(id)
    .bind(thread_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((now, voice)) = now else {
        return Ok(ContinueSave::Gone);
    };
    if now.trim_end() != prefix {
        return Ok(ContinueSave::Changed);
    }
    let voice = MessageVoice::from_stored(voice).map(MessageVoice::continued);
    sqlx::query(
        "UPDATE chat_messages SET content=?3, reasoning=?4, prompt_tokens=?5,
           completion_tokens=?6, ir_messages=?7, model=?8, answered_by=?9, voice=?10,
           images_note=?11
         WHERE id=?1 AND thread_id=?2",
    )
    .bind(id)
    .bind(thread_id)
    .bind(&r.content)
    .bind(&r.reasoning)
    .bind(r.prompt_tokens)
    .bind(r.completion_tokens)
    .bind(&r.ir_messages)
    .bind(&r.model)
    .bind(&r.answered_by)
    .bind(super::chat_voice::message_voice_json(voice.as_ref()))
    .bind(&r.images_note)
    .execute(&mut *tx)
    .await?;
    feed::record_updated(&mut tx, thread_id, id, by).await?;
    touch_thread(&mut tx, thread_id).await?;
    tx.commit().await?;
    Ok(ContinueSave::Saved)
}

/// Rewrite user message `id` for a resend, in one transaction (review R1
/// finding 9): its text becomes `content`, its knowledge picks `kb_refs`, its
/// stored retrieval goes (it described the old text), and every later message
/// is deleted with its attachments — but a late MCP task result (role
/// `tool`), which stays and is answered again (MCP Tasks design T13). Its own attachments stay bound. A
/// dictated message whose text changed loses its `voice`: the text is no
/// longer what was spoken (chat-voice design §3). `false` — and nothing
/// written — when no such user message is in this thread. The feed hears
/// `message.updated` for it and `message.deleted` for what went, `by` the
/// editor.
pub async fn rewrite_chat_user_message(
    pool: &SqlitePool,
    thread_id: i64,
    id: i64,
    content: &str,
    kb_refs: &[i64],
    by: feed::By<'_>,
) -> DbResult<bool> {
    let mut tx = super::begin_write(pool).await?;
    let n = sqlx::query(
        "UPDATE chat_messages SET content = ?3, kb_refs = ?4, context = NULL,
           voice = CASE WHEN content = ?3 THEN voice ELSE NULL END
         WHERE id = ?1 AND thread_id = ?2 AND role = 'user'",
    )
    .bind(id)
    .bind(thread_id)
    .bind(content)
    .bind(super::chat_knowledge::id_list_json(kb_refs))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Ok(false);
    }
    feed::record_updated(&mut tx, thread_id, id, by).await?;
    let gone: Vec<i64> = sqlx::query_scalar(
        "DELETE FROM chat_messages WHERE thread_id = ?1 AND id > ?2 AND role <> 'tool'
         RETURNING id",
    )
    .bind(thread_id)
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    feed::record_deleted(&mut tx, thread_id, &gone, by).await?;
    touch_thread(&mut tx, thread_id).await?;
    tx.commit().await?;
    Ok(true)
}

/// One message of `thread_id`, or `None` — also when `id` exists but belongs
/// to another thread.
pub async fn get_chat_message(
    pool: &SqlitePool,
    thread_id: i64,
    id: i64,
) -> DbResult<Option<ChatMessageRow>> {
    let row = sqlx::query("SELECT * FROM chat_messages WHERE id = ?1 AND thread_id = ?2")
        .bind(id)
        .bind(thread_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(chat_message_from_row))
}

/// Messages `ids` of thread `thread_id` as stored now, oldest first, with
/// the attachments sent with them, in their order: what the feed's
/// `message.added` and `message.updated` render (client-apps design §2.3).
/// A message gone, or another thread's, is not there. Read in one
/// transaction, so the attachments are the messages'.
pub async fn chat_messages_by_ids(
    pool: &SqlitePool,
    thread_id: i64,
    ids: &[i64],
) -> DbResult<(Vec<ChatMessageRow>, Vec<ChatAttachmentMeta>)> {
    if ids.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let list = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
    // Reads only, for one snapshot: deferred, as no write lock is needed
    // (`begin_write` is for a transaction that writes).
    #[allow(clippy::disallowed_methods)]
    let mut tx = pool.begin().await?;
    let rows = sqlx::query(
        "SELECT * FROM chat_messages
         WHERE thread_id = ?1 AND id IN (SELECT value FROM json_each(?2)) ORDER BY id",
    )
    .bind(thread_id)
    .bind(&list)
    .fetch_all(&mut *tx)
    .await?;
    let atts = sqlx::query(
        "SELECT id, thread_id, message_id, kind, name, mime, size, mode, meta
         FROM chat_attachments
         WHERE thread_id = ?1 AND message_id IN (SELECT value FROM json_each(?2))
         ORDER BY ord, id",
    )
    .bind(thread_id)
    .bind(&list)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((
        rows.iter().map(chat_message_from_row).collect(),
        atts.iter()
            .map(super::chat_attachments::chat_attachment_meta_from_row)
            .collect(),
    ))
}

/// The thread's newest message, `None` when it has none.
pub async fn last_chat_message(
    pool: &SqlitePool,
    thread_id: i64,
) -> DbResult<Option<ChatMessageRow>> {
    let row =
        sqlx::query("SELECT * FROM chat_messages WHERE thread_id = ?1 ORDER BY id DESC LIMIT 1")
            .bind(thread_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.as_ref().map(chat_message_from_row))
}

/// Rewrite one message's columns in place — the row, its id, its position
/// and its attachments stay (an edited user message keeps its files). An
/// edited reply's tool record goes, and so does what it waited on.
/// `false` when no such message is in this thread. `by`: the editor.
pub async fn update_chat_message(
    pool: &SqlitePool,
    thread_id: i64,
    id: i64,
    m: &ChatMessageUpdate,
    by: feed::By<'_>,
) -> DbResult<bool> {
    let mut tx = super::begin_write(pool).await?;
    let n = sqlx::query(
        "UPDATE chat_messages SET content=?3, reasoning=?4, prompt_tokens=?5,
           completion_tokens=?6, ir_messages=?7, voice=?8, pending_approvals=NULL
         WHERE id=?1 AND thread_id=?2",
    )
    .bind(id)
    .bind(thread_id)
    .bind(&m.content)
    .bind(&m.reasoning)
    .bind(m.prompt_tokens)
    .bind(m.completion_tokens)
    .bind(&m.ir_messages)
    .bind(super::chat_voice::message_voice_json(m.voice.as_ref()))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n > 0 {
        feed::record_updated(&mut tx, thread_id, id, by).await?;
        touch_thread(&mut tx, thread_id).await?;
    }
    tx.commit().await?;
    Ok(n > 0)
}

/// Delete one message; its attachments go with it (the `message_id` foreign
/// key cascades, migration 0038). `false` when no such message is in this
/// thread. `by`: who deleted it.
pub async fn delete_chat_message(
    pool: &SqlitePool,
    thread_id: i64,
    id: i64,
    by: feed::By<'_>,
) -> DbResult<bool> {
    let mut tx = super::begin_write(pool).await?;
    let n = sqlx::query("DELETE FROM chat_messages WHERE id=?1 AND thread_id=?2")
        .bind(id)
        .bind(thread_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n > 0 {
        feed::record_deleted(&mut tx, thread_id, &[id], by).await?;
        touch_thread(&mut tx, thread_id).await?;
    }
    tx.commit().await?;
    Ok(n > 0)
}

/// Cut the thread back to message `id`: delete every message after it, and
/// `id` itself too when `inclusive`. Their attachments cascade. Returns how
/// many messages went. "After" is id order, which is the order
/// [`list_chat_messages`] reads a thread in. A late MCP task result (role
/// `tool`) is no reply and stays: the work happened, and the turn that
/// follows answers it again (MCP Tasks design T13). Deleting one by itself
/// is [`delete_chat_message`]'s. The feed hears `message.deleted` for what
/// went, `by` who cut it.
pub async fn truncate_chat_messages(
    pool: &SqlitePool,
    thread_id: i64,
    id: i64,
    inclusive: bool,
    by: feed::By<'_>,
) -> DbResult<u64> {
    let sql = if inclusive {
        "DELETE FROM chat_messages WHERE thread_id=?1 AND id >= ?2 AND role <> 'tool' RETURNING id"
    } else {
        "DELETE FROM chat_messages WHERE thread_id=?1 AND id > ?2 AND role <> 'tool' RETURNING id"
    };
    let mut tx = super::begin_write(pool).await?;
    let gone: Vec<i64> = sqlx::query_scalar(sql)
        .bind(thread_id)
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
    if !gone.is_empty() {
        feed::record_deleted(&mut tx, thread_id, &gone, by).await?;
        touch_thread(&mut tx, thread_id).await?;
    }
    tx.commit().await?;
    Ok(gone.len() as u64)
}

async fn touch_thread(tx: &mut sqlx::SqliteConnection, thread_id: i64) -> DbResult<()> {
    sqlx::query("UPDATE chat_threads SET updated_at=datetime('now') WHERE id=?1")
        .bind(thread_id)
        .execute(tx)
        .await?;
    Ok(())
}

/// When the newest message of each of `ids` was written, in unix seconds:
/// a thread without messages is not in the map. One query for a whole list
/// (the thread list's `last_message_at`; client-apps design §3.3, where an
/// ongoing folder's idleness is measured from it).
pub async fn chat_last_message_at(
    pool: &SqlitePool,
    ids: &[i64],
) -> DbResult<std::collections::HashMap<i64, i64>> {
    if ids.is_empty() {
        return Ok(Default::default());
    }
    let ids = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT thread_id, CAST(strftime('%s', MAX(created_at)) AS INTEGER)
         FROM chat_messages WHERE thread_id IN (SELECT value FROM json_each(?1))
         GROUP BY thread_id",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}
