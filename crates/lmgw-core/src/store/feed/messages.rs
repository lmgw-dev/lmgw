//! The `message.*` records (client-apps design §2.2, WP10): every write of
//! a stored thread's messages records which messages it added, changed in
//! place or deleted, **inside its own transaction**, as the thread and
//! folder writes do (L6). A write that fails, or matches no message,
//! records nothing.
//!
//! A record names the messages by id (`chat_feed.message_ids`, a JSON
//! array) and the thread they are in; delivery renders them as they are
//! then (`web::chat_feed`), so a catch-up never replays a stale copy. The
//! level is the thread's at the write (L3): a device never hears of an
//! Admin Chat thread's messages.
//!
//! Which write records what:
//! - **`message.added`**: a user message (`send_user_message_by`), a saved
//!   reply (`append_chat_reply_by`), a late MCP task result entering the
//!   thread (`mcp_tasks::deliver_result`, and inside a send), and the
//!   plain append (`append_chat_message`).
//! - **`message.updated`**: an edit (`update_chat_message`), a continue,
//!   a resumed turn's save, the user message of a resend
//!   (`rewrite_chat_user_message`), a stored retrieval, a spoken reply's
//!   annotation or cut, and every write of a reply's pending approvals (a
//!   decision, a decline by a new message, a close as not run).
//! - **`message.deleted`**: a delete, a truncation (an edit's resend, a
//!   regenerate) with the ids its `DELETE … RETURNING id` named.
//!
//! A kept temporary chat records none: its `thread.created` names a thread
//! a client reads whole. A thread deleted whole records `thread.deleted`
//! alone.

use sqlx::SqliteConnection;

use super::super::DbResult;
use super::{kind, By, Record};

/// Record `kind` (one of the `message.*` kinds) for messages `ids` of thread
/// `thread_id`, as the thread stands in this transaction: its folder and its
/// level are read from it, as [`record_thread`](super::record_thread) reads
/// them. Nothing for an empty list, or for a thread that is not there.
pub async fn record_messages(
    conn: &mut SqliteConnection,
    kind: &str,
    thread_id: i64,
    ids: &[i64],
    by: By<'_>,
) -> DbResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let list = serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into());
    sqlx::query(concat!(
        "INSERT INTO chat_feed (type, thread_id, folder_id, admin, by, message_ids)
         SELECT ?1, id, folder_id, ",
        self_admin_thread!(""),
        ", ?2, ?3 FROM chat_threads WHERE id = ?4"
    ))
    .bind(kind)
    .bind(by)
    .bind(list)
    .bind(thread_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// [`kind::MESSAGE_ADDED`] for message `id` of thread `thread_id`.
pub async fn record_added(
    conn: &mut SqliteConnection,
    thread_id: i64,
    id: i64,
    by: By<'_>,
) -> DbResult<()> {
    record_messages(conn, kind::MESSAGE_ADDED, thread_id, &[id], by).await
}

/// [`kind::MESSAGE_UPDATED`] for message `id` of thread `thread_id`.
pub async fn record_updated(
    conn: &mut SqliteConnection,
    thread_id: i64,
    id: i64,
    by: By<'_>,
) -> DbResult<()> {
    record_messages(conn, kind::MESSAGE_UPDATED, thread_id, &[id], by).await
}

/// [`kind::MESSAGE_DELETED`] for messages `ids` of thread `thread_id`
/// (nothing when none went).
pub async fn record_deleted(
    conn: &mut SqliteConnection,
    thread_id: i64,
    ids: &[i64],
    by: By<'_>,
) -> DbResult<()> {
    record_messages(conn, kind::MESSAGE_DELETED, thread_id, ids, by).await
}

impl Record {
    /// A `message.*` record's messages, ascending ([`record_messages`]);
    /// `None` for any other record, or one whose list does not read.
    pub fn message_ids(&self) -> Option<Vec<i64>> {
        if !self.kind.starts_with("message.") {
            return None;
        }
        serde_json::from_str(self.message_ids.as_deref()?).ok()
    }
}
