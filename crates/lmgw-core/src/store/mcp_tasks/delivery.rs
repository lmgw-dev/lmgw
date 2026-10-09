//! A task's result entering its thread (MCP Tasks design §2.2, §2.3, §3.1),
//! and a thread delete's half (§1.5).
//!
//! - **What waits**: a row whose result is set and whose thread still
//!   holds it — `ended`, or `cancel_owed` for a cancel made while its
//!   server was not connected ([`waiting`], [`waiting_threads`]).
//! - **Delivery** ([`deliver_result`]) is one transaction: the result row of role
//!   `tool` inserted, `task.done` recorded, the task row let go
//!   ([`delivered_in`](super::delivered_in)), the thread's `updated_at`
//!   moved. A send delivers what waits inside its own transaction, before
//!   its user message ([`deliver_waiting_in`]). The web layer decides when (only while no turn of the thread
//!   runs) and what the row holds (`web::chat_tasks`).
//! - **`task.started`** is recorded in the insert's own transaction
//!   ([`started_in`]).
//! - **A thread delete** ([`threads_gone`]): run in the delete's own
//!   transaction, after its `DELETE`. The foreign key's `ON DELETE SET NULL`
//!   has cleared the deleted threads' rows by then, and a row without a
//!   thread is otherwise only ever an owed cancel with no result: so every
//!   `open` row without one owes its cancel now, every `ended` one is
//!   dropped, and an owed cancel loses its result — [`thread_gone`]'s rule,
//!   for whichever threads the statement took (a delete by hand, a folder's,
//!   the sweep's purge).

use sqlx::{Row, SqliteConnection, SqlitePool};

use super::super::feed::{self, TaskFacts};
use super::super::DbResult;
use super::{row_of, McpTaskRow, NewMcpTask};

/// How the feed names the starting principal of a task (`by`), from the
/// key name its row keeps: a device's as `device '<name>'`, everyone else
/// (the owner's turns run with no key) as the dashboard.
pub fn started_by(key: Option<&str>) -> String {
    match key {
        Some(k) if k.starts_with(crate::devices::NAME_PREFIX) => {
            lmgw_api_types::chat_feed::by_device(crate::devices::short_name(k))
        }
        _ => feed::BY_OWNER.to_string(),
    }
}

/// `task.started` for row `id`, just inserted as `t`, on the insert's
/// transaction.
pub(super) async fn started_in(
    conn: &mut SqliteConnection,
    id: i64,
    t: &NewMcpTask<'_>,
) -> DbResult<()> {
    let facts = TaskFacts {
        id,
        task_id: t.task_id.to_string(),
        server_label: t.server_label.to_string(),
        tool: t.tool.to_string(),
        message_id: None,
        status: None,
    };
    let by = started_by(t.started_by);
    feed::record_task(
        conn,
        feed::kind::TASK_STARTED,
        t.thread_id,
        &facts,
        Some(&by),
    )
    .await
}

/// The condition of the rows whose result waits for delivery into their
/// thread, as a literal.
macro_rules! waiting {
    () => {
        "result IS NOT NULL AND thread_id IS NOT NULL AND state IN ('ended', 'cancel_owed')"
    };
}

/// Thread `thread_id`'s results waiting for delivery, in their order.
pub async fn waiting(pool: &SqlitePool, thread_id: i64) -> DbResult<Vec<McpTaskRow>> {
    let rows = sqlx::query(concat!(
        "SELECT * FROM mcp_tasks WHERE thread_id = ?1 AND ",
        waiting!(),
        " ORDER BY id"
    ))
    .bind(thread_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_of).collect())
}

/// Every thread with a result waiting for delivery.
pub async fn waiting_threads(pool: &SqlitePool) -> DbResult<Vec<i64>> {
    Ok(sqlx::query_scalar(concat!(
        "SELECT DISTINCT thread_id FROM mcp_tasks WHERE ",
        waiting!(),
        " ORDER BY thread_id"
    ))
    .fetch_all(pool)
    .await?)
}

/// The result row a delivery writes (MCP Tasks design §2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultRow {
    /// The result as text, for every reader that does not take IR.
    pub content: String,
    /// The synthetic call and its result, as IR messages (JSON).
    pub ir_messages: String,
    /// The task's facts (`chat_messages.task`, JSON).
    pub task: String,
}

/// What [`deliver_result`] wrote: the result row's id and the task row as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    pub message_id: i64,
    pub row: McpTaskRow,
}

/// Deliver task row `id`'s result into thread `thread_id`, in one
/// transaction: the result row `render` makes of it, `task.done` (`by` the
/// canceller of a cancel lmgw sent), the task row let go. `None` — nothing
/// written — when the row no longer waits for that thread (delivered,
/// its thread or server gone meanwhile).
pub async fn deliver_result(
    pool: &SqlitePool,
    id: i64,
    thread_id: i64,
    render: impl FnOnce(&McpTaskRow) -> ResultRow,
) -> DbResult<Option<Delivered>> {
    let mut tx = super::super::begin_write(pool).await?;
    let out = deliver_result_in(&mut tx, id, thread_id, render).await?;
    if out.is_some() {
        tx.commit().await?;
    }
    Ok(out)
}

/// Every result of thread `thread_id` waiting for delivery, delivered in
/// their order inside the caller's transaction (a send's, before its user
/// message: MCP Tasks design §3.1): what [`deliver_result`] writes for
/// each, nothing committed here.
pub async fn deliver_waiting_in(
    conn: &mut SqliteConnection,
    thread_id: i64,
    render: &(dyn Fn(&McpTaskRow) -> ResultRow + Sync),
) -> DbResult<Vec<Delivered>> {
    let ids: Vec<i64> = sqlx::query_scalar(concat!(
        "SELECT id FROM mcp_tasks WHERE thread_id = ?1 AND ",
        waiting!(),
        " ORDER BY id"
    ))
    .bind(thread_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(d) = deliver_result_in(&mut *conn, id, thread_id, render).await? {
            out.push(d);
        }
    }
    Ok(out)
}

/// [`deliver_result`]'s writes on `conn`, uncommitted.
async fn deliver_result_in(
    conn: &mut SqliteConnection,
    id: i64,
    thread_id: i64,
    render: impl FnOnce(&McpTaskRow) -> ResultRow,
) -> DbResult<Option<Delivered>> {
    let Some(row) = sqlx::query(concat!(
        "SELECT * FROM mcp_tasks WHERE id = ?1 AND thread_id = ?2 AND ",
        waiting!()
    ))
    .bind(id)
    .bind(thread_id)
    .fetch_optional(&mut *conn)
    .await?
    .as_ref()
    .map(row_of) else {
        return Ok(None);
    };
    let out = render(&row);
    let message_id = sqlx::query(
        "INSERT INTO chat_messages (thread_id, role, content, ir_messages, task)
         VALUES (?1, 'tool', ?2, ?3, ?4)",
    )
    .bind(thread_id)
    .bind(&out.content)
    .bind(&out.ir_messages)
    .bind(&out.task)
    .execute(&mut *conn)
    .await?
    .last_insert_rowid();
    let facts = TaskFacts {
        id: row.id,
        task_id: row.task_id.clone(),
        server_label: row.server_label.clone(),
        tool: row.tool.clone(),
        message_id: Some(message_id),
        status: Some(row.status.clone()),
    };
    let by = row.ended_by.as_deref();
    feed::record_task(&mut *conn, feed::kind::TASK_DONE, thread_id, &facts, by).await?;
    super::delivered_in(&mut *conn, row.id).await?;
    sqlx::query("UPDATE chat_threads SET updated_at = datetime('now') WHERE id = ?1")
        .bind(thread_id)
        .execute(&mut *conn)
        .await?;
    Ok(Some(Delivered { message_id, row }))
}

/// Threads were deleted by the statement just run on `conn` (module doc):
/// their open tasks owe a cancel, their ended results are dropped, an owed
/// cancel keeps waiting without its result. The ids of the rows that now
/// owe a cancel: the caller has the follower send them once it committed
/// (`McpManager::resume_tasks`).
pub async fn threads_gone(conn: &mut SqliteConnection) -> DbResult<Vec<i64>> {
    let owing: Vec<i64> = sqlx::query(
        "UPDATE mcp_tasks
            SET state = 'cancel_owed', result = NULL, updated_at = datetime('now')
          WHERE thread_id IS NULL AND state = 'open'
         RETURNING id",
    )
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| r.get("id"))
    .collect();
    sqlx::query("DELETE FROM mcp_tasks WHERE thread_id IS NULL AND state = 'ended'")
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE mcp_tasks SET result = NULL, updated_at = datetime('now')
          WHERE thread_id IS NULL AND state = 'cancel_owed' AND result IS NOT NULL",
    )
    .execute(&mut *conn)
    .await?;
    Ok(owing)
}

/// The highest row id stored so far (`0` for none): ids only grow
/// (`AUTOINCREMENT`), so every row stored later is above it.
pub async fn newest_id(pool: &SqlitePool) -> DbResult<i64> {
    Ok(
        sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM mcp_tasks")
            .fetch_one(pool)
            .await?,
    )
}

/// The row model call `call_id` of thread `thread_id` stored above row id
/// `after`: the task that call started (the Chat's `tool` frame, MCP Tasks
/// design §4.3).
pub async fn of_call(
    pool: &SqlitePool,
    thread_id: i64,
    call_id: &str,
    after: i64,
) -> DbResult<Option<McpTaskRow>> {
    let row = sqlx::query(
        "SELECT * FROM mcp_tasks WHERE thread_id = ?1 AND call_id = ?2 AND id > ?3
          ORDER BY id DESC LIMIT 1",
    )
    .bind(thread_id)
    .bind(call_id)
    .bind(after)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_of))
}
