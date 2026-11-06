//! A spoken reply's writes after it was saved (chat-voice design §8.3): a
//! bound realtime session's journal finalizes each reply with what was
//! heard of it.
//!
//! - **Annotate**: a reply heard whole gets its `voice` (the TTS, the voice,
//!   the timing) and nothing else changes.
//! - **Cut**: a reply heard in part keeps the heard text as `content` — what
//!   the model sees and search finds — and the rest in `voice.unheard`, in
//!   one write. Its reasoning seals answered past the cut go with the
//!   unheard rest (`ReplySeal::heard`, WP8 verify V-F2); its trace stays. The caller holds the thread's conditional write
//!   (`web::chat_live::LiveTurns::write_if`).
//!
//! A delete is the ordinary one (`delete_chat_message`), under the same
//! conditional write. None of them is activity: the thread's `updated_at`
//! stays. Each records `message.updated` in its own transaction, `by` the
//! session's binder (client-apps design §2.2).

use sqlx::SqlitePool;

use super::super::feed;
use super::super::DbResult;
use super::MessageVoice;

/// Set message `id`'s `voice` alone; `false` when it is not in the thread.
pub async fn set_chat_message_voice(
    pool: &SqlitePool,
    thread_id: i64,
    id: i64,
    voice: &MessageVoice,
    by: feed::By<'_>,
) -> DbResult<bool> {
    let mut tx = super::super::begin_write(pool).await?;
    let n = sqlx::query("UPDATE chat_messages SET voice = ?3 WHERE id = ?1 AND thread_id = ?2")
        .bind(id)
        .bind(thread_id)
        .bind(voice.to_stored())
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n > 0 {
        feed::record_updated(&mut tx, thread_id, id, by).await?;
    }
    tx.commit().await?;
    Ok(n > 0)
}

/// Cut reply `id` to `content`, its `voice` (the unheard rest in it) with
/// it, and its reasoning seals to the heard ones (module doc); `false` when
/// it is not a reply of the thread.
pub async fn cut_chat_reply(
    pool: &SqlitePool,
    thread_id: i64,
    id: i64,
    content: &str,
    voice: &MessageVoice,
    by: feed::By<'_>,
) -> DbResult<bool> {
    let mut tx = super::super::begin_write(pool).await?;
    let seals: Option<Option<String>> = sqlx::query_scalar(
        "SELECT reasoning_seals FROM chat_messages
         WHERE id = ?1 AND thread_id = ?2 AND role = 'assistant'",
    )
    .bind(id)
    .bind(thread_id)
    .fetch_optional(&mut *tx)
    .await?;
    let heard = super::super::ReplySeal::heard(
        super::super::chat_seals::from_column(seals.flatten()),
        content.len(),
    );
    let n = sqlx::query(
        "UPDATE chat_messages SET content = ?3, voice = ?4, reasoning_seals = ?5
         WHERE id = ?1 AND thread_id = ?2 AND role = 'assistant'",
    )
    .bind(id)
    .bind(thread_id)
    .bind(content)
    .bind(voice.to_stored())
    .bind(super::super::chat_seals::to_column(&heard))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n > 0 {
        feed::record_updated(&mut tx, thread_id, id, by).await?;
    }
    tx.commit().await?;
    Ok(n > 0)
}
