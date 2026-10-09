//! MCP Tasks in the Chat (MCP Tasks design §3, §4.1, §5.1): what a task a
//! turn started becomes in its thread once it ended.
//!
//! - [`deliver`]: an ended task's result is written into its thread as a
//!   message of role `tool`, only while no turn of the thread runs — when
//!   the task ends with the thread idle, when a turn ends, and at a turn's
//!   start before it reads the history (§3.1);
//! - [`render`]: what that row holds, and where the next turn's request
//!   places it — a synthetic `lmgw__job_result` call and its result, where
//!   the row is stored among the thread's messages (§3.2);
//! - [`routes`]: `POST …/answer`, a continuation that answers the results
//!   nothing answered yet, and `POST …/tasks/{task}/cancel` (§5.1).
//!
//! Beside them, here: the thread's tasks as `GET /chat/api/threads/{id}`
//! lists them ([`thread_tasks`]), the `tool` frame's `task` for a call that
//! started one ([`FrameTasks`], §4.3), and what follows a write that took
//! tasks from their thread or server ([`threads_gone`], [`servers_gone`]).

pub(crate) mod deliver;
pub(crate) mod render;
mod routes;
pub(in crate::web) use routes::{answer, cancel, tasks};

use lmgw_api_types::chat::ThreadTask;
use serde_json::{json, Value};

use crate::state::{AppState, SharedState};
use crate::store::mcp_tasks::{self, McpTaskRow};

/// `409 nothing_to_answer`: no result in the thread waits for an answer
/// (`POST …/answer`, checked by the route and again under the turn's
/// ticket).
pub(crate) fn nothing_to_answer() -> axum::response::Response {
    super::chat::err_json(
        axum::http::StatusCode::CONFLICT,
        lmgw_api_types::chat::task_code::NOTHING_TO_ANSWER,
        "no job result in this thread waits for an answer",
    )
}

/// `409 turn_running`: a turn of the thread runs, so `POST …/answer` does
/// not start one (it would cancel it); the results that wait enter when it
/// ends, and its reply may answer them.
pub(crate) fn turn_running() -> axum::response::Response {
    super::chat::err_json(
        axum::http::StatusCode::CONFLICT,
        lmgw_api_types::chat::task_code::TURN_RUNNING,
        "a turn of this thread is running: answer once it ended, if its reply did not",
    )
}

/// Threads were deleted, their tasks' rows moved in that write
/// (`store::mcp_tasks::threads_gone`): the open ones' followers send the
/// owed cancels now. Not waited for.
pub(crate) fn threads_gone(state: &SharedState) {
    let s = state.clone();
    tokio::spawn(async move { s.mcp.resume_tasks().await });
}

/// Server rows were removed, their open tasks ended `abandoned` in that
/// write (`store::mcp_tasks::server_gone`): their followers stop, and the
/// results enter every thread that is idle now.
pub(crate) async fn servers_gone(state: &AppState) {
    state.mcp.resume_tasks().await;
    deliver::all(state).await;
}

/// Thread `thread_id`'s tasks (§5.1): every one still running, and every
/// ended one whose result waits to enter the thread, oldest first. Empty
/// for a temporary thread, and when the read fails (logged): the thread
/// reads without them.
pub(crate) async fn thread_tasks(state: &AppState, thread_id: i64) -> Vec<ThreadTask> {
    if thread_id <= 0 {
        return Vec::new();
    }
    let rows = match mcp_tasks::of_thread(&state.db, thread_id).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("chat: thread {thread_id}'s tasks could not be read: {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(thread_task(state, row).await);
    }
    out
}

/// One task row as the thread lists it, with what it waits for now.
pub(crate) async fn thread_task(state: &AppState, row: &McpTaskRow) -> ThreadTask {
    let waiting_for = if row.state == mcp_tasks::OPEN {
        offline_device(state, row.server_id).await
    } else if let Some(thread_id) = row.thread_id.filter(|_| row.result.is_some()) {
        deliver::held_by(state, thread_id).await
    } else {
        None
    };
    ThreadTask {
        id: row.id,
        task_id: row.task_id.clone(),
        server_label: row.server_label.clone(),
        tool: row.tool.clone(),
        status: row.status.clone(),
        status_message: row.status_message.clone(),
        ttl_ms: row.ttl_ms,
        started_at: row.created_at.clone(),
        by: Some(mcp_tasks::started_by(row.started_by.as_deref())),
        waiting_for,
    }
}

/// "device '<name>', which is not connected" for a device row whose link is
/// not up; `None` for any other server.
async fn offline_device(state: &AppState, server_id: i64) -> Option<String> {
    let server = state.snapshot().mcp_servers.get(&server_id).cloned()?;
    if !server.is_device() || state.mcp.is_ready(server_id).await {
        return None;
    }
    Some(format!(
        "device '{}', which is not connected (it is asked again when it links)",
        crate::devices::short_name(&server.name)
    ))
}

/// The `tool` frame's `task` (MCP Tasks design §4.3): for a call of this
/// turn that started an MCP task, `{id, task_id, server_label}`, read from
/// the row the call stored. A turn's calls are told apart by the model's
/// call ids, unique within one model turn; ids repeat across turns (a
/// template that numbers them), so only rows stored since the model turn
/// began are looked at ([`Self::turn_started`]).
pub(crate) struct FrameTasks {
    state: SharedState,
    thread_id: i64,
    /// The highest task row id when the model turn began.
    after: i64,
}

impl FrameTasks {
    /// For a turn of stored thread `thread_id`.
    pub(crate) async fn new(state: &SharedState, thread_id: i64) -> Self {
        let mut t = Self {
            state: state.clone(),
            thread_id,
            after: 0,
        };
        t.turn_started().await;
        t
    }

    /// A model turn began: its calls' rows are the ones stored from now on.
    pub(crate) async fn turn_started(&mut self) {
        match mcp_tasks::newest_id(&self.state.db).await {
            Ok(id) => self.after = id,
            Err(e) => tracing::warn!("chat: reading the newest MCP task failed: {e}"),
        }
    }

    /// The task call `call_id` of this model turn started, as the frame
    /// carries it; `None` for a call that started none.
    pub(crate) async fn of_call(&self, call_id: &str) -> Option<Value> {
        if call_id.is_empty() {
            return None;
        }
        let row = mcp_tasks::of_call(&self.state.db, self.thread_id, call_id, self.after)
            .await
            .ok()
            .flatten()?;
        Some(json!({"id": row.id, "task_id": row.task_id, "server_label": row.server_label}))
    }
}
