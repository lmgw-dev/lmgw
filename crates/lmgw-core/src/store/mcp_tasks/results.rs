//! A bound realtime session's read of its thread's result rows (MCP Tasks
//! design §3.4, `realtime::thread::tasks`): narrow and indexed, as it runs
//! at every wake of the thread.
//!
//! One statement, so one snapshot of the thread:
//! - **the newest reply** — the last of the thread's `assistant` rows: the
//!   result rows past it are the ones no reply answered;
//! - **the result rows** (role `tool`) past the smaller of that reply and
//!   the caller's watermark `since` — the unanswered ones and the ones that
//!   entered since the caller's last read — with only the columns the caller
//!   reads (id, the synthetic call pair, the task facts);
//! - **whether the thread holds any result row at all**;
//! - **the watermark**: `chat_messages`' `AUTOINCREMENT` sequence — the
//!   largest id ever handed out, deleted rows' included. Every row written
//!   before the snapshot has an id at most it, every row written after a
//!   larger one (one writer at a time: ids commit in order).
//!
//! Each part is a range of `idx_chat_messages_thread_role` (`thread_id,
//! role, id`, migration 0077).

use sqlx::{Row, SqlitePool};

use super::super::ChatMessageRow;
use super::super::DbResult;

/// The read (module doc): `?1` the thread, `?2` the watermark (`i64::MAX`
/// for none).
const READ: &str = "\
    WITH r AS (SELECT COALESCE((SELECT id FROM chat_messages \
                                 WHERE thread_id = ?1 AND role = 'assistant' \
                                 ORDER BY id DESC LIMIT 1), 0) AS last_reply), \
         s AS (SELECT COALESCE((SELECT seq FROM sqlite_sequence \
                                 WHERE name = 'chat_messages'), 0) AS watermark), \
         a AS (SELECT EXISTS(SELECT 1 FROM chat_messages \
                              WHERE thread_id = ?1 AND role = 'tool') AS any_result) \
    SELECT r.last_reply, s.watermark, a.any_result, m.id, m.ir_messages, m.task \
      FROM r CROSS JOIN s CROSS JOIN a \
      LEFT JOIN chat_messages m \
        ON m.thread_id = ?1 AND m.role = 'tool' AND m.id > MIN(r.last_reply, ?2) \
     ORDER BY m.id";

/// What [`results_read`] found (module doc).
#[derive(Debug, Clone, Default)]
pub struct ResultsRead {
    /// The thread's newest reply's id; 0 with none.
    pub last_reply: i64,
    /// The largest message id handed out when the read ran (module doc).
    pub watermark: i64,
    /// The thread holds a result row (any, answered or not).
    pub any: bool,
    /// The result rows past the smaller of `last_reply` and the read's
    /// `since`, in id order: `id`, `thread_id`, `role`, `ir_messages` and
    /// `task` set, the rest left empty.
    pub rows: Vec<ChatMessageRow>,
}

/// Thread `thread_id`'s results as of now (module doc); `since`: the
/// caller's watermark, `None` when it has none yet (only the unanswered
/// rows are read then).
pub async fn results_read(
    pool: &SqlitePool,
    thread_id: i64,
    since: Option<i64>,
) -> DbResult<ResultsRead> {
    let rows = sqlx::query(READ)
        .bind(thread_id)
        .bind(since.unwrap_or(i64::MAX))
        .fetch_all(pool)
        .await?;
    let mut read = ResultsRead::default();
    for row in &rows {
        read.last_reply = row.get("last_reply");
        read.watermark = row.get("watermark");
        read.any = row.get::<i64, _>("any_result") != 0;
        // The left join's one row with no result past the bound.
        let Some(id) = row.get::<Option<i64>, _>("id") else {
            continue;
        };
        read.rows.push(ChatMessageRow {
            id,
            thread_id,
            role: "tool".into(),
            ir_messages: row.get("ir_messages"),
            // A column this process wrote: one that does not parse reads as
            // none, as the full row's read does.
            task: row
                .get::<Option<String>, _>("task")
                .and_then(|t| serde_json::from_str(&t).ok()),
            ..Default::default()
        });
    }
    // Every id read was handed out before the read (module doc).
    read.watermark = read
        .rows
        .iter()
        .map(|m| m.id)
        .chain([read.last_reply])
        .fold(read.watermark, i64::max);
    Ok(read)
}

#[cfg(test)]
mod tests;
