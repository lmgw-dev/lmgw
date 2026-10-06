//! **Keep** for a temporary chat (chat-complete design §7): a thread that
//! lived only in memory written to the DB whole — settings (its voice
//! included), messages (each with its voice), attachments — as one ordinary
//! thread.

use std::collections::HashMap;

use sqlx::SqlitePool;

use super::*;

/// One attachment of a thread being kept. `message_id` is the id of its
/// message in the list handed to [`insert_kept_chat_thread`] (the temporary
/// id), `None` for a draft; the insert maps it to the new row's id.
#[derive(Debug, Clone)]
pub struct KeptAttachment {
    pub message_id: Option<i64>,
    pub kind: String,
    pub name: String,
    pub mime: String,
    pub size: i64,
    pub ord: i64,
    pub data: Vec<u8>,
    pub created_at: String,
    pub extracted: Option<String>,
    pub meta: serde_json::Value,
    pub mode: Option<String>,
    /// Cached page images `(page, png)`.
    pub pages: Vec<(u32, Vec<u8>)>,
}

/// Insert `thread` with its `messages` (in the order given, which is the
/// conversation's) and `attachments` in one transaction, and return the new
/// thread's id. The timestamps come along: a kept chat is as old as it is.
/// It starts unpinned and active, as every new thread does.
pub async fn insert_kept_chat_thread(
    pool: &SqlitePool,
    thread: &ChatThread,
    messages: &[ChatMessageRow],
    attachments: &[KeptAttachment],
) -> DbResult<i64> {
    let mcp = serde_json::to_string(&thread.mcp_tools).unwrap_or_else(|_| "[]".to_string());
    let mut tx = pool.begin().await?;
    let thread_id = sqlx::query(
        "INSERT INTO chat_threads
           (title, model_alias, system_prompt, temperature, max_tokens, kind, mcp_tools,
            reasoning_enabled, reasoning_effort, reasoning_budget, agent_id,
            created_at, updated_at,
            top_p, top_k, min_p, repeat_penalty, presence_penalty, frequency_penalty, seed, stop,
            kb_ids, kb_mode, kb_budget_tokens, voice)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)",
    )
    .bind(&thread.title)
    .bind(&thread.model_alias)
    .bind(&thread.system_prompt)
    .bind(thread.temperature)
    .bind(thread.max_tokens)
    .bind(&thread.kind)
    .bind(mcp)
    .bind(thread.reasoning_enabled.map(i64::from))
    .bind(&thread.reasoning_effort)
    .bind(thread.reasoning_budget)
    .bind(&thread.agent_id)
    .bind(&thread.created_at)
    .bind(&thread.updated_at)
    .bind(thread.top_p)
    .bind(thread.top_k)
    .bind(thread.min_p)
    .bind(thread.repeat_penalty)
    .bind(thread.presence_penalty)
    .bind(thread.frequency_penalty)
    .bind(thread.seed)
    .bind(serde_json::to_string(&thread.stop).unwrap_or_else(|_| "[]".to_string()))
    .bind(super::chat_knowledge::id_list_json(&thread.kb_ids))
    .bind(thread.kb_mode.as_str())
    .bind(thread.kb_budget_tokens)
    .bind(thread.voice.to_stored())
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();

    let mut ids: HashMap<i64, i64> = HashMap::with_capacity(messages.len());
    for m in messages {
        let new_id = sqlx::query(
            "INSERT INTO chat_messages
               (thread_id, role, content, reasoning, prompt_tokens, completion_tokens,
                ir_messages, created_at, kb_refs, context, model, answered_by, voice,
                images_note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        )
        .bind(thread_id)
        .bind(&m.role)
        .bind(&m.content)
        .bind(&m.reasoning)
        .bind(m.prompt_tokens)
        .bind(m.completion_tokens)
        .bind(&m.ir_messages)
        .bind(&m.created_at)
        .bind(super::chat_knowledge::id_list_json(&m.kb_refs))
        .bind(super::chat_knowledge::context_json(m.context.as_ref()))
        .bind(&m.model)
        .bind(&m.answered_by)
        .bind(super::chat_voice::message_voice_json(m.voice.as_ref()))
        .bind(&m.images_note)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        ids.insert(m.id, new_id);
    }

    for a in attachments {
        // A message id the list does not contain cannot happen for a thread
        // read out whole; were it to, the file stays with the thread as a
        // draft rather than failing the whole Keep.
        let message_id = a.message_id.and_then(|m| ids.get(&m).copied());
        let att_id = sqlx::query(
            "INSERT INTO chat_attachments
               (thread_id, message_id, kind, name, mime, size, data, ord, created_at,
                extracted, meta, mode)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )
        .bind(thread_id)
        .bind(message_id)
        .bind(&a.kind)
        .bind(&a.name)
        .bind(&a.mime)
        .bind(a.size)
        .bind(&a.data)
        .bind(a.ord)
        .bind(&a.created_at)
        .bind(&a.extracted)
        .bind(a.meta.to_string())
        .bind(&a.mode)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        for (page, png) in &a.pages {
            sqlx::query(
                "INSERT INTO chat_attachment_pages (attachment_id, page, png) VALUES (?1, ?2, ?3)",
            )
            .bind(att_id)
            .bind(*page as i64)
            .bind(png)
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;
    Ok(thread_id)
}
