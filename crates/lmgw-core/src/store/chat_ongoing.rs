//! Ongoing-conversation folders (client-apps design §3): a folder's current
//! thread, the rollover that starts a new one, the writes that end one, and
//! a folder's own retention (§11 Q2).
//!
//! **What a current thread is.** A chat thread in its folder, not archived,
//! of a folder that is ongoing. Every write that would break that clears it
//! in its own transaction: a delete, a move out of the folder and an archive
//! by hand ([`clear_current`]), and a folder that stops being ongoing
//! (`update_chat_folder`). The sweep never archives or purges it. The
//! column's `ON DELETE SET NULL` is only the backstop.
//!
//! **Every change of a folder's current thread records `folder.current`**
//! in the change feed, in the write's own transaction
//! ([`feed::record_folder_current`]).

use std::collections::HashMap;

use sqlx::{SqliteConnection, SqlitePool};

use super::*;

/// The reasons `folder.current` carries. A new thread: [`FIRST`], [`GONE`],
/// [`IDLE`], [`REQUESTED`] (`lmgw_api_types::chat_folders::CurrentReason`
/// spells the same four). None any more: [`GONE`] (deleted, moved out,
/// archived) or [`NOT_ONGOING`].
///
/// [`FIRST`]: reason::FIRST
/// [`GONE`]: reason::GONE
/// [`IDLE`]: reason::IDLE
/// [`REQUESTED`]: reason::REQUESTED
/// [`NOT_ONGOING`]: reason::NOT_ONGOING
pub mod reason {
    pub const FIRST: &str = "first";
    pub const GONE: &str = "gone";
    pub const IDLE: &str = "idle";
    pub const REQUESTED: &str = "requested";
    pub const NOT_ONGOING: &str = "not_ongoing";
}

/// Thread `thread_id` is the current thread of no folder but `keep` any more
/// (a move into `keep` leaves it current there): each folder that had it
/// records `folder.current` with no thread, reason `gone`, as `by`'s — on
/// the transaction of the write that ends it, before a delete takes the row.
pub async fn clear_current(
    conn: &mut SqliteConnection,
    thread_id: i64,
    keep: Option<i64>,
    by: feed::By<'_>,
) -> DbResult<()> {
    let folders: Vec<i64> = sqlx::query_scalar(
        "UPDATE chat_folders SET current_thread_id = NULL
         WHERE current_thread_id = ?1 AND (?2 IS NULL OR id != ?2) RETURNING id",
    )
    .bind(thread_id)
    .bind(keep)
    .fetch_all(&mut *conn)
    .await?;
    for folder in folders {
        feed::record_folder_current(&mut *conn, folder, None, Some(thread_id), reason::GONE, by)
            .await?;
    }
    Ok(())
}

/// Insert `t` (a chat thread in folder `folder_id`) and make it the folder's
/// current thread in place of `previous`, for `reason`: one transaction,
/// recording `thread.created` and `folder.current` as `by`'s. The new id;
/// `None` — and nothing written — when the folder's current thread is not
/// `previous` any more, or the folder is no longer ongoing: another write
/// moved it meanwhile, and the caller decides again on what is there now.
pub async fn create_current_thread(
    pool: &SqlitePool,
    folder_id: i64,
    t: &ChatThread,
    previous: Option<i64>,
    reason: &str,
    by: feed::By<'_>,
) -> DbResult<Option<i64>> {
    // The lock up front (`begin_write`): this is the write a client makes the
    // moment its bound session closed with the 4004, beside the session's own
    // last writes, and a deferred transaction is at times refused `database
    // is locked` at once when another write meets it, instead of waiting.
    let mut tx = super::begin_write(pool).await?;
    let id = super::chat_folders::insert_chat_thread(&mut tx, t, by).await?;
    let moved = sqlx::query(
        "UPDATE chat_folders SET current_thread_id = ?2
         WHERE id = ?1 AND ongoing_idle_minutes IS NOT NULL AND current_thread_id IS ?3",
    )
    .bind(folder_id)
    .bind(id)
    .bind(previous)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if moved == 0 {
        tx.rollback().await?;
        return Ok(None);
    }
    feed::record_folder_current(&mut tx, folder_id, Some(id), previous, reason, by).await?;
    tx.commit().await?;
    Ok(Some(id))
}

/// Thread `thread_id`'s messages, for the rollover: `None` when it has none
/// (an empty thread is never idle, and is reused for a new conversation);
/// otherwise whether its newest message is older than `idle_minutes`
/// (`0`: never). Measured from the message's `created_at`, not the
/// thread's `updated_at`, which a settings change moves too. A count of
/// minutes past what `datetime()` can subtract is never reached.
pub async fn thread_idle(
    pool: &SqlitePool,
    thread_id: i64,
    idle_minutes: i64,
) -> DbResult<Option<bool>> {
    let (any, idle): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) > 0,
                ?2 > 0 AND IFNULL(MAX(created_at) < datetime('now', '-' || ?2 || ' minutes'), 0)
         FROM chat_messages WHERE thread_id = ?1",
    )
    .bind(thread_id)
    .bind(idle_minutes)
    .fetch_one(pool)
    .await?;
    Ok((any != 0).then_some(idle != 0))
}

/// Every folder a reader that reaches as far as `admin` cannot see
/// (client-apps design L3, reviews W3-1, W6-1), and so names to it as the
/// folder of none of its threads (review W4-7).
pub async fn self_admin_folder_ids(
    pool: &SqlitePool,
    admin: super::AdminThreads,
) -> DbResult<std::collections::HashSet<i64>> {
    let ids: Vec<i64> = sqlx::query_scalar(concat!(
        "SELECT id FROM chat_folders WHERE ",
        self_admin_folder!(""),
        " >= ?1"
    ))
    .bind(admin.reach())
    .fetch_all(pool)
    .await?;
    Ok(ids.into_iter().collect())
}

/// Every folder with its own purge days (§11 Q2): what a thread's
/// `purge_at` is computed with when it is in one of them.
pub async fn folder_purge_days(pool: &SqlitePool) -> DbResult<HashMap<i64, i64>> {
    let rows: Vec<(i64, i64)> =
        sqlx::query_as("SELECT id, purge_days FROM chat_folders WHERE purge_days IS NOT NULL")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}
