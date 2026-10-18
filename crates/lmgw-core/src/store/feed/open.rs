//! What a feed opens at (device link design §7.3): the head and the
//! reader's levels, as [`bounds_and_switch`](super::bounds_and_switch)
//! reads them, and the calls waiting for a decision then, in one
//! transaction, so `hello.open_approvals` is the open set at the head
//! `hello.cursor` names: every `approval.*` record up to it is reflected
//! in the set, and every one after it is a change the stream reads after.
//!
//! **A waiting call** is one of a thread's last message: a reply saved with
//! a gated call that has no decision yet (`PendingApprovals::open`). Only
//! that message waits: a new message declines what its predecessor held
//! (`chat_approvals::decline_waiting`), and a decision on an older reply is
//! refused as moved on (`claim_approvals`). Each thread's last message is
//! found through the `(thread_id, id)` index, so the read costs one message
//! row per thread, not one per message. Temporary threads are never in the
//! database, and have no feed (L7).

use sqlx::{Row, SqliteConnection, SqlitePool};

use super::{head_and_levels, Bounds, Levels};
use crate::store::{DbResult, PendingApprovals};

/// A thread's last message: a reply whose calls wait for a decision.
#[derive(Debug, Clone)]
pub struct WaitingReply {
    pub thread_id: i64,
    /// The thread's title at the head.
    pub thread_title: String,
    /// The thread's level (L3), by the rule a record's is written with
    /// (`self_admin_thread!`).
    pub level: i64,
    pub message_id: i64,
    /// Its calls and their decisions; at least one call is open.
    pub pending: PendingApprovals,
}

/// Where a feed opens: the head, the reader's levels and the replies that
/// wait there, oldest first.
#[derive(Debug, Clone)]
pub struct Opening {
    pub bounds: Bounds,
    pub levels: Levels,
    pub waiting: Vec<WaitingReply>,
}

/// [`Opening`] for device key `key_id` (`None`: the owner), read in one
/// transaction (module doc).
pub async fn opening(pool: &SqlitePool, key_id: Option<i64>) -> DbResult<Opening> {
    // Reads only, for one snapshot: deferred, as no write lock is needed
    // (`begin_write` is for a transaction that writes).
    #[allow(clippy::disallowed_methods)]
    let mut tx = pool.begin().await?;
    let (bounds, levels) = head_and_levels(&mut tx, key_id).await?;
    let waiting = waiting_on(&mut tx).await?;
    tx.commit().await?;
    Ok(Opening {
        bounds,
        levels,
        waiting,
    })
}

/// The replies waiting now, without a head: what a `state` sends again
/// after the reader's reach moved or a `resync` passed records (device
/// link design §7.3, the owner's ruling of 2026-10-15). A client matches
/// them by id, so a read a little past the stream's cursor is safe.
pub async fn waiting(pool: &SqlitePool) -> DbResult<Vec<WaitingReply>> {
    let mut conn = pool.acquire().await?;
    waiting_on(&mut conn).await
}

/// Every thread's last message whose calls wait, oldest first. A
/// `pending_approvals` that does not read waits for nothing, as the
/// thread's own read takes it.
async fn waiting_on(conn: &mut SqliteConnection) -> DbResult<Vec<WaitingReply>> {
    let rows = sqlx::query(concat!(
        "SELECT t.id AS thread_id, t.title AS title, ",
        self_admin_thread!("t."),
        " AS level, m.id AS message_id, m.pending_approvals AS pending
         FROM chat_threads t
         JOIN chat_messages m
           ON m.id = (SELECT MAX(id) FROM chat_messages WHERE thread_id = t.id)
         WHERE m.pending_approvals IS NOT NULL
         ORDER BY m.id"
    ))
    .fetch_all(conn)
    .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let pending =
                PendingApprovals::parse(r.get::<Option<String>, _>("pending").as_deref())?;
            pending.is_open().then(|| WaitingReply {
                thread_id: r.get("thread_id"),
                thread_title: r.get("title"),
                level: r.get("level"),
                message_id: r.get("message_id"),
                pending,
            })
        })
        .collect())
}
