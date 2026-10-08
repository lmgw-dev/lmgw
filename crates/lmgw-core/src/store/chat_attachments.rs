//! Chat attachments (chat-archive-pin-attachments design §2)

use sqlx::{Row, SqlitePool};

use super::*;

/// One attachment's metadata — everything the Chat page's chips need, never
/// the bytes. `thread_id`/`message_id` are read (to group and validate) but
/// not published: the wire shape is `{id, kind, name, mime, size}`, nested
/// under the thread or the message that owns it, so those two would be
/// redundant on the wire.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatAttachmentMeta {
    pub id: i64,
    #[serde(skip)]
    pub thread_id: i64,
    #[serde(skip)]
    pub message_id: Option<i64>,
    pub kind: String,
    pub name: String,
    pub mime: String,
    pub size: i64,
    /// A text-class PDF's choice, `text` | `images`; `None` = not applicable
    /// or not chosen yet (chat-complete design §8).
    pub mode: Option<String>,
    /// What extraction found: pages, text-less pages, class, sheets,
    /// transcript alias / error, tokens. `{}` for an image.
    pub meta: serde_json::Value,
    /// `meta.tokens`, the chips' estimate ("~12k tokens").
    pub extracted_tokens: Option<i64>,
    /// Why this draft cannot be sent to the thread's current model; filled in
    /// by the handlers for drafts, absent on the store's own rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blockers: Option<Vec<String>>,
    /// What this draft becomes on the way, sent all the same: under a GPU
    /// block whose fallback cannot see, an image goes as a placeholder and
    /// a PDF's pages as its text (`chat_attach_gate::annotate_drafts`).
    /// Filled in for drafts only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hints: Option<Vec<String>>,
}

/// One attachment with its bytes — never serialized (the bytes go out as a
/// raw body, not JSON): the `GET .../attachments/{id}` route, and rendering
/// a thread's history into a model request.
#[derive(Debug, Clone)]
pub struct ChatAttachmentFull {
    pub id: i64,
    pub thread_id: i64,
    pub message_id: Option<i64>,
    pub kind: String,
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
    /// The derived text (PDF pages, office markdown, audio transcript).
    pub extracted: Option<String>,
    pub meta: serde_json::Value,
    pub mode: Option<String>,
}

/// The `meta` column as JSON (`{}` when empty or unreadable).
pub fn parse_attachment_meta(raw: Option<&str>) -> serde_json::Value {
    raw.filter(|r| !r.trim().is_empty())
        .and_then(|r| serde_json::from_str(r).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}))
}

/// `meta.tokens`.
pub fn attachment_tokens(meta: &serde_json::Value) -> Option<i64> {
    meta.get("tokens").and_then(serde_json::Value::as_i64)
}

fn chat_attachment_meta_from_row(row: &sqlx::sqlite::SqliteRow) -> ChatAttachmentMeta {
    let meta = parse_attachment_meta(row.get::<Option<String>, _>("meta").as_deref());
    ChatAttachmentMeta {
        extracted_tokens: attachment_tokens(&meta),
        meta,
        mode: row.get("mode"),
        blockers: None,
        hints: None,
        id: row.get("id"),
        thread_id: row.get("thread_id"),
        message_id: row.get("message_id"),
        kind: row.get("kind"),
        name: row.get("name"),
        mime: row.get("mime"),
        size: row.get("size"),
    }
}

fn chat_attachment_full_from_row(row: &sqlx::sqlite::SqliteRow) -> ChatAttachmentFull {
    ChatAttachmentFull {
        id: row.get("id"),
        thread_id: row.get("thread_id"),
        message_id: row.get("message_id"),
        kind: row.get("kind"),
        name: row.get("name"),
        mime: row.get("mime"),
        data: row.get("data"),
        extracted: row.get("extracted"),
        meta: parse_attachment_meta(row.get::<Option<String>, _>("meta").as_deref()),
        mode: row.get("mode"),
    }
}

/// Upload: always a draft (`message_id` NULL) until a send binds it.
#[allow(clippy::too_many_arguments)]
pub async fn insert_chat_attachment(
    pool: &SqlitePool,
    thread_id: i64,
    kind: &str,
    name: &str,
    mime: &str,
    size: i64,
    data: &[u8],
) -> DbResult<i64> {
    let res = sqlx::query(
        "INSERT INTO chat_attachments (thread_id, kind, name, mime, size, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(thread_id)
    .bind(kind)
    .bind(name)
    .bind(mime)
    .bind(size)
    .bind(data)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

/// Every attachment on a thread, metadata only — sent ones in the order their
/// message listed them, drafts in upload order. Both the draft chips and the
/// per-message chips on `GET /chat/api/threads/{id}` come out of one query,
/// split by `message_id` in the handler.
pub async fn list_chat_attachments_meta(
    pool: &SqlitePool,
    thread_id: i64,
) -> DbResult<Vec<ChatAttachmentMeta>> {
    let rows = sqlx::query(
        "SELECT id, thread_id, message_id, kind, name, mime, size, mode, meta
         FROM chat_attachments WHERE thread_id = ?1 ORDER BY ord, id",
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(chat_attachment_meta_from_row).collect())
}

/// Each attachment's position among its message's files and when it was
/// uploaded, by id — what the JSON export carries besides the metadata the
/// API shows (review R1 item b).
pub async fn chat_attachment_order(
    pool: &SqlitePool,
    thread_id: i64,
) -> DbResult<std::collections::HashMap<i64, (i64, String)>> {
    let rows: Vec<(i64, i64, String)> =
        sqlx::query_as("SELECT id, ord, created_at FROM chat_attachments WHERE thread_id = ?1")
            .bind(thread_id)
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(id, ord, at)| (id, (ord, at)))
        .collect())
}

/// Every **sent** attachment of a thread (never drafts — a draft was never
/// part of a past turn, so it has no business in a replayed request), bytes
/// included, in the order each message sent them — what `build_messages`
/// groups by `message_id` to replay a turn's files with it on every later send.
///
/// Review finding 10: this used to be `SELECT *` with no `message_id`
/// filter, loading (and copying out of SQLite) every still-draft
/// attachment's bytes on *every* send, only for `web::chat::send`'s
/// `group_by_message` to immediately discard them (a draft has no
/// `message_id` to group under). It also always loaded an image's bytes even
/// when `vision` is already known `Some(false)` — that image is about to
/// become a text placeholder (`chat_attach::render`) that never reads them.
/// The `CASE` below skips the `data` column read entirely for exactly that
/// case, which is the one case this function can know in advance is
/// pointless: SQLite only evaluates a branch it takes, so the untaken `data`
/// reference for a vision-blocked image row is never fetched.
pub async fn list_sent_chat_attachments(
    pool: &SqlitePool,
    thread_id: i64,
    vision: Option<bool>,
) -> DbResult<Vec<ChatAttachmentFull>> {
    let skip_image_bytes = vision == Some(false);
    let rows = sqlx::query(
        "SELECT id, thread_id, message_id, kind, name, mime, size, extracted, meta, mode,
                CASE WHEN kind = 'image' AND ?2 THEN X'' ELSE data END AS data
         FROM chat_attachments
         WHERE thread_id = ?1 AND message_id IS NOT NULL
         ORDER BY ord, id",
    )
    .bind(thread_id)
    .bind(skip_image_bytes)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(chat_attachment_full_from_row).collect())
}

/// Whether any bytes ever uploaded to this thread are an image — the gate on
/// paying for a vision-capability lookup at all (design §2): a thread that
/// has never carried one has nothing for the model's `vision` flag to change.
pub async fn chat_thread_has_image_attachment(pool: &SqlitePool, thread_id: i64) -> DbResult<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_attachments WHERE thread_id = ?1 AND kind = 'image'",
    )
    .bind(thread_id)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// One attachment's bytes, for `GET /chat/api/attachments/{id}`.
pub async fn get_chat_attachment_full(
    pool: &SqlitePool,
    id: i64,
) -> DbResult<Option<ChatAttachmentFull>> {
    let row = sqlx::query("SELECT * FROM chat_attachments WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(chat_attachment_full_from_row))
}

/// The thread attachment `id` belongs to — whose reach decides whether a
/// device may reach the attachment by its own id (client-apps design L3).
/// `None` when there is no such attachment.
pub async fn chat_attachment_thread_id(pool: &SqlitePool, id: i64) -> DbResult<Option<i64>> {
    let thread = sqlx::query_scalar("SELECT thread_id FROM chat_attachments WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(thread)
}

/// Resolve a `send` request's `attachments: [id…]` to draft rows of this
/// thread, upload order — the shape `web::chat::send` needs to both validate
/// ("ids must be drafts of this thread, else 400", design §2) and order the
/// parts it renders. Missing, foreign, or already-sent ids are simply absent
/// from the result; the caller compares the count back against what it asked
/// for to tell "all good" from "something didn't match".
pub async fn list_draft_chat_attachments_by_ids(
    pool: &SqlitePool,
    thread_id: i64,
    ids: &[i64],
) -> DbResult<Vec<ChatAttachmentMeta>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT id, thread_id, message_id, kind, name, mime, size, mode, meta FROM chat_attachments \
         WHERE thread_id = ",
    );
    qb.push_bind(thread_id);
    qb.push(" AND message_id IS NULL AND id IN (");
    let mut sep = qb.separated(", ");
    for id in ids {
        sep.push_bind(*id);
    }
    sep.push_unseparated(")");
    qb.push(" ORDER BY id");
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows.iter().map(chat_attachment_meta_from_row).collect())
}

/// What [`append_user_message_with_attachments`] found.
pub enum SendMessageOutcome {
    /// The new message's id.
    Sent(i64),
    /// A draft the caller named was no longer bindable — consumed by a
    /// concurrent send of the same draft ids between `web::chat::send`'s
    /// up-front validation and this call (review finding 5). The whole insert
    /// is rolled back: no half-sent message quietly missing the files it
    /// claimed to carry.
    AttachmentNotDraft,
}

/// Persist a user turn and bind its draft attachments to it **in one
/// transaction** (design §2; review finding 5).
///
/// Before this, the insert and the bind were two separate statements and the
/// bind's result was never checked: two concurrent sends validating the same
/// draft ids both passed (`list_draft_chat_attachments_by_ids` saw drafts
/// both times), but only the first `UPDATE … WHERE message_id IS NULL`
/// actually matched any rows — the second matched zero and its message went
/// out silently missing every file it claimed. The `UPDATE` here re-checks
/// `thread_id` and `message_id IS NULL` itself rather than trusting the
/// caller's earlier validation, and its affected-row count is compared
/// against what was asked for; a mismatch rolls back (message included) and
/// reports [`SendMessageOutcome::AttachmentNotDraft`] instead of committing a
/// message with only some of its attachments.
pub async fn append_user_message_with_attachments(
    pool: &SqlitePool,
    thread_id: i64,
    content: &str,
    attachment_ids: &[i64],
) -> DbResult<SendMessageOutcome> {
    append_user_message_with_kb_refs(pool, thread_id, content, attachment_ids, &[]).await
}

/// [`append_user_message_with_attachments`] for a message that also names
/// knowledge bases for itself (`kb_refs`, chat-complete design §9.3) — in
/// the same insert, so the message never exists without them.
pub async fn append_user_message_with_kb_refs(
    pool: &SqlitePool,
    thread_id: i64,
    content: &str,
    attachment_ids: &[i64],
    kb_refs: &[i64],
) -> DbResult<SendMessageOutcome> {
    append_user_message_with_voice(pool, thread_id, content, attachment_ids, kb_refs, None).await
}

/// [`append_user_message_with_kb_refs`] for a spoken turn: the message
/// carries how it was spoken (chat-voice design §3), written in the same
/// insert.
pub async fn append_user_message_with_voice(
    pool: &SqlitePool,
    thread_id: i64,
    content: &str,
    attachment_ids: &[i64],
    kb_refs: &[i64],
    voice: Option<&MessageVoice>,
) -> DbResult<SendMessageOutcome> {
    let mut tx = super::begin_write(pool).await?;

    let res = sqlx::query(
        "INSERT INTO chat_messages (thread_id, role, content, reasoning, kb_refs, voice) \
         VALUES (?1, 'user', ?2, '', ?3, ?4)",
    )
    .bind(thread_id)
    .bind(content)
    .bind(super::chat_knowledge::id_list_json(kb_refs))
    .bind(super::chat_voice::message_voice_json(voice))
    .execute(&mut *tx)
    .await?;
    let msg_id = res.last_insert_rowid();

    // One UPDATE per id so each gets its position in the send's list: the
    // chips' order is the order the model reads them in, whichever upload
    // finished first. A duplicate id matches nothing the second time round
    // and so rolls back like any other id that is no longer a draft.
    let mut bound = 0u64;
    for (ord, id) in attachment_ids.iter().enumerate() {
        bound += sqlx::query(
            "UPDATE chat_attachments SET message_id = ?1, ord = ?2 \
             WHERE id = ?3 AND thread_id = ?4 AND message_id IS NULL",
        )
        .bind(msg_id)
        .bind(ord as i64)
        .bind(*id)
        .bind(thread_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    if bound as usize != attachment_ids.len() {
        tx.rollback().await?;
        return Ok(SendMessageOutcome::AttachmentNotDraft);
    }

    // Same bump `append_chat_message` does, in the same transaction — a
    // thread must not float to the top of the sidebar for a turn that just
    // got rolled back.
    sqlx::query("UPDATE chat_threads SET updated_at=datetime('now') WHERE id=?1")
        .bind(thread_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(SendMessageOutcome::Sent(msg_id))
}

/// What [`delete_draft_chat_attachment`] found.
pub enum DeleteAttachmentOutcome {
    Deleted,
    NotFound,
    /// Drafts only; sent (`message_id` set) is a 409, not silently ignored
    /// (design §2) — the message that was sent already named this file.
    AlreadySent,
}

pub async fn delete_draft_chat_attachment(
    pool: &SqlitePool,
    id: i64,
) -> DbResult<DeleteAttachmentOutcome> {
    let row = sqlx::query("SELECT message_id FROM chat_attachments WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else {
        return Ok(DeleteAttachmentOutcome::NotFound);
    };
    if row.get::<Option<i64>, _>("message_id").is_some() {
        return Ok(DeleteAttachmentOutcome::AlreadySent);
    }
    sqlx::query("DELETE FROM chat_attachments WHERE id = ?1 AND message_id IS NULL")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(DeleteAttachmentOutcome::Deleted)
}
