//! The `task.*` records (MCP Tasks design §4.1): a turn's tool call became
//! an MCP task (`task.started`, in the transaction that stores the task),
//! and its result entered the thread (`task.done`, in the delivery's).
//!
//! Their facts are in `detail` — lmgw's id of the task, the server's, the
//! label and the tool, and for `task.done` the result row and the status —
//! so a record renders after its task row is gone, as `profile.*` records
//! keep their profile's id and name. The level is the thread's at the write
//! (L3): a device never hears of an Admin Chat thread's tasks.

use serde_json::{json, Value};
use sqlx::SqliteConnection;

use super::super::DbResult;
use super::{By, Record};

/// What a `task.*` record names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFacts {
    /// lmgw's id of the task (`mcp_tasks.id`).
    pub id: i64,
    pub task_id: String,
    pub server_label: String,
    pub tool: String,
    /// `task.done`'s result row; `None` for `task.started`.
    pub message_id: Option<i64>,
    /// `task.done`'s status; `None` for `task.started`.
    pub status: Option<String>,
}

impl TaskFacts {
    fn detail(&self) -> String {
        let mut v = json!({
            "id": self.id,
            "task_id": self.task_id,
            "server_label": self.server_label,
            "tool": self.tool,
        });
        if let Some(m) = self.message_id {
            v["message_id"] = json!(m);
        }
        if let Some(s) = &self.status {
            v["status"] = json!(s);
        }
        v.to_string()
    }
}

/// Record `kind` (`task.started` or `task.done`) for thread `thread_id` as
/// the row stands in this transaction: its folder and its level are read
/// from it, as [`record_thread`](super::record_thread) reads them. A thread
/// that is not there records nothing.
pub async fn record_task(
    conn: &mut SqliteConnection,
    kind: &str,
    thread_id: i64,
    facts: &TaskFacts,
    by: By<'_>,
) -> DbResult<()> {
    sqlx::query(concat!(
        "INSERT INTO chat_feed (type, thread_id, folder_id, admin, by, detail)
         SELECT ?1, id, folder_id, ",
        self_admin_thread!(""),
        ", ?2, ?3 FROM chat_threads WHERE id = ?4"
    ))
    .bind(kind)
    .bind(by)
    .bind(facts.detail())
    .bind(thread_id)
    .execute(conn)
    .await?;
    Ok(())
}

impl Record {
    /// A `task.*` record's facts ([`record_task`]); `None` for any other
    /// record, or one whose `detail` does not read.
    pub fn task(&self) -> Option<TaskFacts> {
        if !self.kind.starts_with("task.") {
            return None;
        }
        let v: Value = serde_json::from_str(self.detail.as_deref()?).ok()?;
        let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(TaskFacts {
            id: v.get("id")?.as_i64()?,
            task_id: text("task_id")?,
            server_label: text("server_label")?,
            tool: text("tool")?,
            message_id: v.get("message_id").and_then(Value::as_i64),
            status: text("status"),
        })
    }
}
