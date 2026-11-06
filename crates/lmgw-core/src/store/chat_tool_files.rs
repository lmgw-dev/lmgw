//! The files a turn's image and audio tools made (chat-media-tools design
//! §6.3): stored when each call ends, so its `att:N` ref exists for the next
//! call of the same turn, and bound to the reply in the reply's own
//! transaction — or dropped when no reply takes them.

use sqlx::SqlitePool;

use super::*;

/// The id list as the `json_each` the statements below read.
fn id_list(ids: &[i64]) -> String {
    serde_json::to_string(ids).unwrap_or_else(|_| "[]".into())
}

/// Bind the turn's tool files `ids` to reply `message_id`, inside the
/// reply's own write. A file another reply already took, or an upload, is
/// left as it is.
pub(super) async fn bind_tool_files(
    tx: &mut sqlx::SqliteConnection,
    thread_id: i64,
    message_id: i64,
    ids: &[i64],
) -> DbResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE chat_attachments SET message_id = ?1
         WHERE thread_id = ?2 AND message_id IS NULL AND source <> 'upload'
           AND id IN (SELECT value FROM json_each(?3))",
    )
    .bind(message_id)
    .bind(thread_id)
    .bind(id_list(ids))
    .execute(tx)
    .await?;
    Ok(())
}

/// Bind card action file `id` (chat-media-tools design §8.3) to message
/// `message_id`, the message of the picture it was made from: `false` when
/// nothing was bound (the message went meanwhile).
pub async fn bind_action_file(
    pool: &SqlitePool,
    thread_id: i64,
    message_id: i64,
    id: i64,
) -> DbResult<bool> {
    let done = sqlx::query(
        "UPDATE chat_attachments SET message_id = ?1
         WHERE thread_id = ?2 AND id = ?3 AND message_id IS NULL AND source = 'action'
           AND EXISTS (SELECT 1 FROM chat_messages WHERE id = ?1 AND thread_id = ?2)",
    )
    .bind(message_id)
    .bind(thread_id)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Delete the tool files `ids` no reply took: the turn's reply was not
/// saved, so nothing names them.
pub async fn drop_tool_files(pool: &SqlitePool, thread_id: i64, ids: &[i64]) -> DbResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "DELETE FROM chat_attachments
         WHERE thread_id = ?1 AND message_id IS NULL AND source <> 'upload'
           AND id IN (SELECT value FROM json_each(?2))",
    )
    .bind(thread_id)
    .bind(id_list(ids))
    .execute(pool)
    .await?;
    Ok(())
}

/// Attachments `ids` of thread `thread_id` with their bytes, whatever their
/// source: what a turn record's `lmgw:attachment/N` refs and a tool call's
/// `att:N` resolve to. Another thread's id is simply not there.
pub async fn chat_attachments_by_ids(
    pool: &SqlitePool,
    thread_id: i64,
    ids: &[i64],
) -> DbResult<Vec<ChatAttachmentFull>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT * FROM chat_attachments
         WHERE thread_id = ?1 AND id IN (SELECT value FROM json_each(?2)) ORDER BY id",
    )
    .bind(thread_id)
    .bind(id_list(ids))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(super::chat_attachments::chat_attachment_full_from_row)
        .collect())
}

/// The ids of thread `thread_id`'s image and audio attachments a model may
/// name by ref, newest last: what a refused ref lists. A card action's
/// picture is not one (chat-media-tools design §8.2): the user gives it to
/// the model by attaching it, as an upload of its own.
pub async fn chat_media_attachment_ids(
    pool: &SqlitePool,
    thread_id: i64,
) -> DbResult<Vec<(i64, String)>> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, kind FROM chat_attachments
         WHERE thread_id = ?1 AND kind IN ('image', 'audio') AND source <> 'action'
         ORDER BY id",
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
