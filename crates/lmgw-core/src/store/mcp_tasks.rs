//! MCP Tasks (MCP Tasks design §2.1): the `mcp_tasks` table and every query
//! on it. Addressed as `store::mcp_tasks::…`, like [`feed`](super::feed),
//! since its names (`insert`, `end`, `thread_gone`) only make sense under it.
//!
//! A row is one task lmgw started for a stored Chat thread (the late path):
//! - `open`: followed (`mcp::tasks`);
//! - `ended`: its result waits for delivery into the thread, which deletes
//!   it;
//! - `cancel_owed`: a `tasks/cancel` waits for the server's next
//!   connection. A cancel made while the server was not connected ends the
//!   task at once **and** owes the cancel: such a row keeps its `result` and
//!   `thread_id` until the result is delivered ([`owed_delivered`]), and the
//!   cancel sent ([`owed_cancel_sent`]) leaves it `ended` while the result
//!   still waits.
//!
//! A receiver may reuse a task id once lmgw thinks the task ended, or even
//! while lmgw still follows it: at most one row per `(server, task id)` is
//! not `ended` (a partial unique index), and a new task whose id an older
//! such row holds ends that row first ([`insert_reusing`], [`reused`]).
//!
//! Every write is one [`begin_write`](super::begin_write) transaction; the
//! `*_in` forms take the caller's, for the writes that must join another
//! (the thread delete's [`thread_gone`], a server removal's
//! [`server_gone`]). Nothing else removes a row (design T9).
//!
//! A result's delivery into its thread, `task.started`'s record and a thread
//! delete's half are [`delivery`]'s (MCP Tasks WP2); a bound realtime
//! session's read of a thread's result rows is [`results`]' (WP3).

use sqlx::{Row, SqliteConnection, SqlitePool};

use super::DbResult;

mod delivery;
mod results;
pub use delivery::{
    deliver_result, deliver_waiting_in, newest_id, of_call, started_by, threads_gone, waiting,
    waiting_threads, Delivered, ResultRow,
};
pub use results::{results_read, ResultsRead};

/// `state`: followed.
pub const OPEN: &str = "open";
/// `state`: ended, its result waiting for delivery.
pub const ENDED: &str = "ended";
/// `state`: a `tasks/cancel` owed to the server.
pub const CANCEL_OWED: &str = "cancel_owed";

/// One row, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpTaskRow {
    /// lmgw's own id (the route's `{task}`, `lmgw_task_<id>`).
    pub id: i64,
    pub server_id: i64,
    /// The label the tool was offered under, at the call.
    pub server_label: String,
    /// The receiver's id.
    pub task_id: String,
    pub thread_id: Option<i64>,
    /// The exposed name the model called.
    pub tool: String,
    /// The model's id of the call that started it.
    pub call_id: String,
    /// The starting principal's key name; `None` for the gateway's own.
    pub started_by: Option<String>,
    pub state: String,
    /// `working`, `input_required`, `completed`, `failed`, `cancelled` or
    /// `abandoned`.
    pub status: String,
    pub status_message: Option<String>,
    /// The receiver's `pollInterval`, when it gave one.
    pub poll_interval_ms: Option<i64>,
    /// The receiver's stated `ttl`; `None`: unlimited or not given.
    pub ttl_ms: Option<i64>,
    /// Who cancelled, for a cancel lmgw sent.
    pub ended_by: Option<String>,
    /// The result as IR tool-result blocks (JSON), once ended.
    pub result: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A row to insert: the task lmgw was just answered with.
#[derive(Debug, Clone)]
pub struct NewMcpTask<'a> {
    pub server_id: i64,
    pub server_label: &'a str,
    pub task_id: &'a str,
    pub thread_id: i64,
    pub tool: &'a str,
    pub call_id: &'a str,
    pub started_by: Option<&'a str>,
    pub status: &'a str,
    pub status_message: Option<&'a str>,
    pub poll_interval_ms: Option<i64>,
    pub ttl_ms: Option<i64>,
}

/// `SELECT <every column> FROM mcp_tasks <rest>`, as a literal.
macro_rules! select {
    ($rest:literal) => {
        concat!(
            "SELECT id, server_id, server_label, task_id, thread_id, tool, call_id, started_by, ",
            "state, status, status_message, poll_interval_ms, ttl_ms, ended_by, result, ",
            "created_at, updated_at FROM mcp_tasks ",
            $rest
        )
    };
}

fn row_of(r: &sqlx::sqlite::SqliteRow) -> McpTaskRow {
    McpTaskRow {
        id: r.get("id"),
        server_id: r.get("server_id"),
        server_label: r.get("server_label"),
        task_id: r.get("task_id"),
        thread_id: r.get("thread_id"),
        tool: r.get("tool"),
        call_id: r.get("call_id"),
        started_by: r.get("started_by"),
        state: r.get("state"),
        status: r.get("status"),
        status_message: r.get("status_message"),
        poll_interval_ms: r.get("poll_interval_ms"),
        ttl_ms: r.get("ttl_ms"),
        ended_by: r.get("ended_by"),
        result: r.get("result"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

/// Insert `t` as an `open` row on the caller's transaction: its id.
pub async fn insert_in(conn: &mut SqliteConnection, t: &NewMcpTask<'_>) -> DbResult<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO mcp_tasks
                (server_id, server_label, task_id, thread_id, tool, call_id, started_by,
                 state, status, status_message, poll_interval_ms, ttl_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'open', ?8, ?9, ?10, ?11)
         RETURNING id",
    )
    .bind(t.server_id)
    .bind(t.server_label)
    .bind(t.task_id)
    .bind(t.thread_id)
    .bind(t.tool)
    .bind(t.call_id)
    .bind(t.started_by)
    .bind(t.status)
    .bind(t.status_message)
    .bind(t.poll_interval_ms)
    .bind(t.ttl_ms)
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

/// [`insert_in`] in a transaction of its own.
pub async fn insert(pool: &SqlitePool, t: &NewMcpTask<'_>) -> DbResult<i64> {
    let mut tx = super::begin_write(pool).await?;
    let id = insert_in(&mut tx, t).await?;
    tx.commit().await?;
    Ok(id)
}

/// What [`insert_reusing`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inserted {
    /// The new row's id.
    pub id: i64,
    /// The open rows that ended because they held its task id, as they
    /// were.
    pub reused: Vec<McpTaskRow>,
    /// Its server row was gone by the insert (removed while the call was
    /// in flight): the row ended `abandoned` in the insert's own
    /// transaction, and nothing follows it.
    pub server_gone: bool,
}

/// Insert `t`, ending first any older row that still holds its id (a
/// receiver that reused it, [`reused_in`]), in one transaction with the
/// feed's `task.started` (MCP Tasks design §1.2). A server row removed
/// while the call was in flight ([`server_gone`] ran in its delete, before
/// this row existed) ends the new row at once, `abandoned` with the result
/// `removed_of` writes for it, as the delete would have: its result enters
/// the thread, and no follower waits for a server that is gone.
pub async fn insert_reusing(
    pool: &SqlitePool,
    t: &NewMcpTask<'_>,
    result_of: impl Fn(&McpTaskRow) -> String,
    removed_of: impl FnOnce(&McpTaskRow) -> String,
) -> DbResult<Inserted> {
    let mut tx = super::begin_write(pool).await?;
    let reused = reused_in(&mut tx, t.server_id, t.task_id, result_of).await?;
    let id = insert_in(&mut tx, t).await?;
    delivery::started_in(&mut tx, id, t).await?;
    let server_gone = !server_exists(&mut tx, t.server_id).await?;
    if server_gone {
        let row = sqlx::query(select!("WHERE id = ?1"))
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        let e = Ended {
            status: "abandoned",
            status_message: None,
            result: &removed_of(&row_of(&row)),
            ended_by: None,
        };
        end_in(&mut tx, id, OPEN, ENDED, &e).await?;
    }
    tx.commit().await?;
    Ok(Inserted {
        id,
        reused,
        server_gone,
    })
}

/// Whether server row `server_id` is still stored (a registered server's
/// or a device's row alike: both are `mcp_servers` rows).
async fn server_exists(conn: &mut SqliteConnection, server_id: i64) -> DbResult<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM mcp_servers WHERE id = ?1)")
            .bind(server_id)
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// Whether server row `server_id` is still stored: a follower whose
/// server's row is gone ends its task (`mcp::tasks`), where one whose
/// server only is not connected waits.
pub async fn server_stored(pool: &SqlitePool, server_id: i64) -> DbResult<bool> {
    let mut conn = pool.acquire().await?;
    server_exists(&mut conn, server_id).await
}

/// [`reused_in`] in a transaction of its own, taken only when a row holds
/// the id (a bridged task's check, made for every bridged task).
pub async fn reused(
    pool: &SqlitePool,
    server_id: i64,
    task_id: &str,
    result_of: impl Fn(&McpTaskRow) -> String,
) -> DbResult<Vec<McpTaskRow>> {
    let held: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM mcp_tasks WHERE server_id = ?1 AND task_id = ?2 AND state <> 'ended'",
    )
    .bind(server_id)
    .bind(task_id)
    .fetch_optional(pool)
    .await?;
    if held.is_none() {
        return Ok(Vec::new());
    }
    let mut tx = super::begin_write(pool).await?;
    let ended = reused_in(&mut tx, server_id, task_id, result_of).await?;
    tx.commit().await?;
    Ok(ended)
}

/// Server `server_id` answered a new task with `task_id`, which an older
/// row not yet `ended` holds: an `open` row ends `abandoned` with the result
/// `result_of` writes for it; an owed cancel is dropped, since it would
/// cancel the new task (the row goes, or stays `ended` while its result
/// waits). The open rows that ended, as they were.
pub async fn reused_in(
    conn: &mut SqliteConnection,
    server_id: i64,
    task_id: &str,
    result_of: impl Fn(&McpTaskRow) -> String,
) -> DbResult<Vec<McpTaskRow>> {
    let held: Vec<McpTaskRow> = sqlx::query(select!(
        "WHERE server_id = ?1 AND task_id = ?2 AND state <> 'ended' ORDER BY id"
    ))
    .bind(server_id)
    .bind(task_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(row_of)
    .collect();
    let mut ended = Vec::new();
    for row in held {
        if row.state == OPEN {
            let e = Ended {
                status: "abandoned",
                status_message: None,
                result: &result_of(&row),
                ended_by: None,
            };
            if end_in(conn, row.id, OPEN, ENDED, &e).await? {
                ended.push(row);
            }
        } else {
            owed_cancel_sent_in(conn, row.id).await?;
        }
    }
    Ok(ended)
}

/// Row `id`.
pub async fn get(pool: &SqlitePool, id: i64) -> DbResult<Option<McpTaskRow>> {
    let row = sqlx::query(select!("WHERE id = ?1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_of))
}

/// Every row in state `open` or `cancel_owed`: what the follower resumes.
pub async fn followed(pool: &SqlitePool) -> DbResult<Vec<McpTaskRow>> {
    let rows = sqlx::query(select!(
        "WHERE state IN ('open', 'cancel_owed') ORDER BY id"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_of).collect())
}

/// Thread `thread_id`'s rows, in their order.
pub async fn of_thread(pool: &SqlitePool, thread_id: i64) -> DbResult<Vec<McpTaskRow>> {
    let rows = sqlx::query(select!("WHERE thread_id = ?1 ORDER BY id"))
        .bind(thread_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(row_of).collect())
}

/// How many tasks each server runs for lmgw now (rows in state `open`), by
/// server id; a server with none is not in the map. The MCP page's "N open
/// tasks" (MCP Tasks design §6): open tasks have no cap (T9), so the page
/// counts them.
pub async fn open_counts(pool: &SqlitePool) -> DbResult<std::collections::HashMap<i64, i64>> {
    let rows = sqlx::query(
        "SELECT server_id, COUNT(*) AS n FROM mcp_tasks WHERE state = 'open' GROUP BY server_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<i64, _>("server_id"), r.get::<i64, _>("n")))
        .collect())
}

/// Every row, in their order (for tests and the inventory).
pub async fn all(pool: &SqlitePool) -> DbResult<Vec<McpTaskRow>> {
    let rows = sqlx::query(select!("ORDER BY id")).fetch_all(pool).await?;
    Ok(rows.iter().map(row_of).collect())
}

/// An open row's status moved (a poll, a notification): `false` when the
/// row is no longer open.
pub async fn set_status(
    pool: &SqlitePool,
    id: i64,
    status: &str,
    status_message: Option<&str>,
    poll_interval_ms: Option<i64>,
) -> DbResult<bool> {
    let mut tx = super::begin_write(pool).await?;
    let moved = sqlx::query(
        "UPDATE mcp_tasks
            SET status = ?2, status_message = ?3,
                poll_interval_ms = COALESCE(?4, poll_interval_ms),
                updated_at = datetime('now')
          WHERE id = ?1 AND state = 'open'",
    )
    .bind(id)
    .bind(status)
    .bind(status_message)
    .bind(poll_interval_ms)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(moved > 0)
}

/// How a task ended, as its row stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended<'a> {
    pub status: &'a str,
    pub status_message: Option<&'a str>,
    /// The result as IR tool-result blocks (JSON).
    pub result: &'a str,
    pub ended_by: Option<&'a str>,
}

/// An open row ended: `ended` with its result. `false` when the row was no
/// longer open (ended meanwhile, its thread or server gone).
pub async fn end(pool: &SqlitePool, id: i64, e: &Ended<'_>) -> DbResult<bool> {
    let mut tx = super::begin_write(pool).await?;
    let moved = end_in(&mut tx, id, OPEN, ENDED, e).await?;
    tx.commit().await?;
    Ok(moved)
}

/// An open row was cancelled while its server was not connected: ended at
/// once, its result waiting for delivery, **and** the cancel owed.
pub async fn end_owing_cancel(pool: &SqlitePool, id: i64, e: &Ended<'_>) -> DbResult<bool> {
    let mut tx = super::begin_write(pool).await?;
    let moved = end_in(&mut tx, id, OPEN, CANCEL_OWED, e).await?;
    tx.commit().await?;
    Ok(moved)
}

async fn end_in(
    conn: &mut SqliteConnection,
    id: i64,
    from: &str,
    to: &str,
    e: &Ended<'_>,
) -> DbResult<bool> {
    let moved = sqlx::query(
        "UPDATE mcp_tasks
            SET state = ?3, status = ?4, status_message = COALESCE(?5, status_message),
                result = ?6, ended_by = ?7, updated_at = datetime('now')
          WHERE id = ?1 AND state = ?2",
    )
    .bind(id)
    .bind(from)
    .bind(to)
    .bind(e.status)
    .bind(e.status_message)
    .bind(e.result)
    .bind(e.ended_by)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(moved > 0)
}

/// The owed cancel of row `id` was sent (whatever the server answered): the
/// row goes, unless its result still waits for delivery, which keeps it as
/// `ended`.
pub async fn owed_cancel_sent(pool: &SqlitePool, id: i64) -> DbResult<()> {
    let mut tx = super::begin_write(pool).await?;
    owed_cancel_sent_in(&mut tx, id).await?;
    tx.commit().await?;
    Ok(())
}

async fn owed_cancel_sent_in(conn: &mut SqliteConnection, id: i64) -> DbResult<()> {
    sqlx::query(
        "DELETE FROM mcp_tasks
          WHERE id = ?1 AND state = 'cancel_owed' AND (result IS NULL OR thread_id IS NULL)",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE mcp_tasks SET state = 'ended', updated_at = datetime('now')
          WHERE id = ?1 AND state = 'cancel_owed'",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Row `id`'s result was delivered into its thread, on the delivery's own
/// transaction: an `ended` row goes; a `cancel_owed` one keeps waiting for
/// its server, its result and thread cleared.
pub async fn delivered_in(conn: &mut SqliteConnection, id: i64) -> DbResult<()> {
    sqlx::query("DELETE FROM mcp_tasks WHERE id = ?1 AND state = 'ended'")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    owed_delivered(conn, id).await
}

/// A `cancel_owed` row's result left it: delivered, or its thread gone.
async fn owed_delivered(conn: &mut SqliteConnection, id: i64) -> DbResult<()> {
    sqlx::query(
        "UPDATE mcp_tasks SET result = NULL, thread_id = NULL, updated_at = datetime('now')
          WHERE id = ?1 AND state = 'cancel_owed'",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Thread `thread_id` is being deleted, on the delete's own transaction
/// (design §1.5): its open tasks owe a cancel, its ended results are
/// dropped, and an owed cancel keeps waiting without its result. The ids of
/// the rows that now owe a cancel.
pub async fn thread_gone(conn: &mut SqliteConnection, thread_id: i64) -> DbResult<Vec<i64>> {
    let owing: Vec<i64> = sqlx::query_scalar(
        "UPDATE mcp_tasks
            SET state = 'cancel_owed', thread_id = NULL, result = NULL,
                updated_at = datetime('now')
          WHERE thread_id = ?1 AND state IN ('open', 'cancel_owed')
         RETURNING id",
    )
    .bind(thread_id)
    .fetch_all(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM mcp_tasks WHERE thread_id = ?1 AND state = 'ended'")
        .bind(thread_id)
        .execute(&mut *conn)
        .await?;
    Ok(owing)
}

/// Server row `server_id` is being removed, on the removal's own
/// transaction (design §1.6): its open tasks end `abandoned`, each with the
/// result `result_of` writes for it; its owed cancels go (nobody is left to
/// tell), unless their result still waits for delivery.
pub async fn server_gone(
    conn: &mut SqliteConnection,
    server_id: i64,
    result_of: impl Fn(&McpTaskRow) -> String,
) -> DbResult<()> {
    let open: Vec<McpTaskRow> = sqlx::query(select!("WHERE server_id = ?1 AND state = 'open'"))
        .bind(server_id)
        .fetch_all(&mut *conn)
        .await?
        .iter()
        .map(row_of)
        .collect();
    for row in &open {
        let ended = Ended {
            status: "abandoned",
            status_message: None,
            result: &result_of(row),
            ended_by: None,
        };
        end_in(conn, row.id, OPEN, ENDED, &ended).await?;
    }
    sqlx::query(
        "DELETE FROM mcp_tasks
          WHERE server_id = ?1 AND state = 'cancel_owed'
            AND (result IS NULL OR thread_id IS NULL)",
    )
    .bind(server_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE mcp_tasks SET state = 'ended', updated_at = datetime('now')
          WHERE server_id = ?1 AND state = 'cancel_owed'",
    )
    .bind(server_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
