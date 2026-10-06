//! What extraction adds to a chat attachment (chat-complete design §8): the
//! derived text and its metadata, a PDF's mode, and the cache of rendered
//! page images.

use sqlx::{Row, SqlitePool};

use super::*;

/// An upload ready to store: the sniffed kind and whatever extraction made of
/// it at upload time.
#[derive(Debug, Clone)]
pub struct NewAttachment {
    pub kind: String,
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
    pub extracted: Option<String>,
    pub meta: serde_json::Value,
    pub mode: Option<String>,
}

impl NewAttachment {
    /// A kind with no extraction: images, and the tests' text files.
    pub fn plain(kind: &str, name: &str, mime: &str, data: &[u8]) -> Self {
        Self {
            kind: kind.into(),
            name: name.into(),
            mime: mime.into(),
            data: data.to_vec(),
            extracted: None,
            meta: serde_json::json!({}),
            mode: None,
        }
    }

    /// The metadata row this upload becomes under `id`.
    pub fn into_meta(&self, id: i64, thread_id: i64) -> ChatAttachmentMeta {
        ChatAttachmentMeta {
            id,
            thread_id,
            message_id: None,
            kind: self.kind.clone(),
            name: self.name.clone(),
            mime: self.mime.clone(),
            size: self.data.len() as i64,
            mode: self.mode.clone(),
            extracted_tokens: attachment_tokens(&self.meta),
            meta: self.meta.clone(),
            blockers: None,
            hints: None,
        }
    }
}

/// Upload with extraction results: always a draft.
pub async fn insert_chat_attachment_new(
    pool: &SqlitePool,
    thread_id: i64,
    a: &NewAttachment,
) -> DbResult<i64> {
    let res = sqlx::query(
        "INSERT INTO chat_attachments (thread_id, kind, name, mime, size, data, extracted, meta, mode)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(thread_id)
    .bind(&a.kind)
    .bind(&a.name)
    .bind(&a.mime)
    .bind(a.data.len() as i64)
    .bind(&a.data)
    .bind(&a.extracted)
    .bind(a.meta.to_string())
    .bind(&a.mode)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

/// Store text derived after the upload (an audio transcript made on first
/// need) together with the metadata that describes it.
pub async fn set_chat_attachment_extracted(
    pool: &SqlitePool,
    id: i64,
    extracted: &str,
    meta: &serde_json::Value,
) -> DbResult<()> {
    sqlx::query("UPDATE chat_attachments SET extracted = ?2, meta = ?3 WHERE id = ?1")
        .bind(id)
        .bind(extracted)
        .bind(meta.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// What [`set_chat_attachment_mode`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetModeOutcome {
    Set,
    NotFound,
    /// Already sent with a message: the choice is part of what was sent.
    Sent,
    /// Not a PDF whose every page has text — the others are automatic.
    NotTextPdf,
}

pub async fn set_chat_attachment_mode(
    pool: &SqlitePool,
    id: i64,
    mode: &str,
) -> DbResult<SetModeOutcome> {
    let Some(row) =
        sqlx::query("SELECT kind, message_id, meta FROM chat_attachments WHERE id = ?1")
            .bind(id)
            .fetch_optional(pool)
            .await?
    else {
        return Ok(SetModeOutcome::NotFound);
    };
    if row.get::<Option<i64>, _>("message_id").is_some() {
        return Ok(SetModeOutcome::Sent);
    }
    let meta = parse_attachment_meta(row.get::<Option<String>, _>("meta").as_deref());
    if !is_text_pdf(&row.get::<String, _>("kind"), &meta) {
        return Ok(SetModeOutcome::NotTextPdf);
    }
    sqlx::query("UPDATE chat_attachments SET mode = ?2 WHERE id = ?1 AND message_id IS NULL")
        .bind(id)
        .bind(mode)
        .execute(pool)
        .await?;
    Ok(SetModeOutcome::Set)
}

/// A PDF whose every page has text (`meta.class == "text"`).
pub fn is_text_pdf(kind: &str, meta: &serde_json::Value) -> bool {
    kind == "pdf" && meta.get("class").and_then(|c| c.as_str()) == Some("text")
}

/// A cached page image (1-based page).
pub async fn get_chat_attachment_page(
    pool: &SqlitePool,
    id: i64,
    page: u32,
) -> DbResult<Option<Vec<u8>>> {
    Ok(sqlx::query_scalar(
        "SELECT png FROM chat_attachment_pages WHERE attachment_id = ?1 AND page = ?2",
    )
    .bind(id)
    .bind(page as i64)
    .fetch_optional(pool)
    .await?)
}

pub async fn put_chat_attachment_page(
    pool: &SqlitePool,
    id: i64,
    page: u32,
    png: &[u8],
) -> DbResult<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO chat_attachment_pages (attachment_id, page, png)
         VALUES (?1, ?2, ?3)",
    )
    .bind(id)
    .bind(page as i64)
    .bind(png)
    .execute(pool)
    .await?;
    Ok(())
}
