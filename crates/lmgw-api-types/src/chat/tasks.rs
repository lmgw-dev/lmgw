//! MCP Tasks in a Chat thread (MCP Tasks design §2.2, §3, §5.1): a tool
//! whose server runs it as a task answers the turn at once (`started, job
//! <task id>`), and its result enters the thread later as a message of role
//! `tool`.
//!
//! - **A result row** carries [`MessageTask`] as its `task`; its `content`
//!   is the result as text. The next turn replays it as a call of
//!   `lmgw__job_result` and its result, where it is stored among the
//!   thread's messages (chronological order).
//! - **The thread's open tasks** are `GET /chat/api/threads/{id}`'s `tasks`
//!   ([`ThreadTask`]): the tasks still running, and the results waiting to
//!   enter the thread.
//! - **`POST /chat/api/threads/{id}/answer`** ([`ANSWER_PATH`]) asks the
//!   model to answer the results nothing answered yet, streaming the frames
//!   a send streams; with none, `409 nothing_to_answer`, and while a turn
//!   of the thread runs, `409 turn_running`.
//! - **`POST /chat/api/threads/{id}/tasks/{task}/cancel`** ([`CANCEL_PATH`])
//!   cancels one of them; the answer is [`TaskCancelled`].
//! - **A realtime session bound to the thread** is told of each result that
//!   entered it ([`TASK_DONE_EVENT`], the fields of
//!   [`crate::chat_feed::TaskDone`]), and answers the results nothing
//!   answered yet with a `response.create` that carries no new words.

use serde::{Deserialize, Serialize};

/// `GET /chat/api/threads/{id}/tasks`: the thread's tasks as [`ThreadTask`]s,
/// what `GET /chat/api/threads/{id}` lists as `tasks`, without the history.
pub const TASKS_PATH: &str = "/chat/api/threads/{id}/tasks";

/// `POST /chat/api/threads/{id}/answer`.
pub const ANSWER_PATH: &str = "/chat/api/threads/{id}/answer";

/// `POST /chat/api/threads/{id}/tasks/{task}/cancel`, `{task}` lmgw's id of
/// the task ([`ThreadTask::id`]).
pub const CANCEL_PATH: &str = "/chat/api/threads/{id}/tasks/{task}/cancel";

/// The `type` of the event a realtime session bound to the thread gets
/// once a task's result entered the thread: `lmgw.task.done`, with the
/// fields of [`crate::chat_feed::TaskDone`] beside it (flat, as every
/// `lmgw.*` event). One that comes while a response runs is answered by
/// that response. Otherwise a `response.create` with no new words — and no
/// turn the client committed before it — answers the results nothing
/// answered yet (a continuation); with none such it is refused
/// `empty_turn`.
pub const TASK_DONE_EVENT: &str = "lmgw.task.done";

/// The refusal codes of the two routes.
pub mod task_code {
    /// `409`: no result in the thread waits for an answer.
    pub const NOTHING_TO_ANSWER: &str = "nothing_to_answer";
    /// `409`: a turn of the thread is running, and an answer would cancel
    /// it; the results that wait enter when it ends, and its reply may
    /// answer them.
    pub const TURN_RUNNING: &str = "turn_running";
    /// `404`: the thread the caller reaches has no such task — another
    /// thread's, or one whose result entered the thread already (it is gone
    /// then). A thread the caller does not reach is the `404 not_found` of
    /// every Chat route.
    pub const TASK_NOT_FOUND: &str = "task_not_found";
    /// `409`: the task already ended, and its result waits to enter the
    /// thread. Once it entered, the task is gone: `404 task_not_found`.
    pub const TASK_ENDED: &str = "task_ended";
    /// `409`: the task's server does not declare `tasks.cancel`, so lmgw
    /// cannot cancel its tasks; the task goes on.
    pub const CANCEL_UNSUPPORTED: &str = "task_cancel_unsupported";
    /// `502`: the task's server answered the cancel with an error (its
    /// message follows); the task goes on.
    pub const CANCEL_REFUSED: &str = "task_cancel_refused";
}

/// What a result row says of its task: `task` on a message of role `tool`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MessageTask {
    /// The server's id of the task.
    pub task_id: String,
    /// The label the tool was offered under when it was called.
    pub server_label: String,
    /// The name the model called (`<label>__<tool>`).
    pub tool: String,
    /// How it ended: `completed`, `failed`, `cancelled`, or `abandoned`
    /// (the server no longer knows it, or its server row was removed).
    #[cfg_attr(
        feature = "schema",
        schemars(extend("enum" = ["completed", "failed", "cancelled", "abandoned"]))
    )]
    pub status: String,
    /// Who cancelled it, for a cancel lmgw sent ("the dashboard", "device
    /// 'phone'"); `null` otherwise.
    pub ended_by: Option<String>,
    /// The result's MCP `structuredContent`, for a view of the call
    /// (client-apps design §7.5): the model reads the result's `content`,
    /// and this only when the content was empty. Absent when the result
    /// had none, and for a result stored by an earlier lmgw.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<serde_json::Value>,
}

/// One of a thread's tasks: still running, or ended with its result on its
/// way into the thread.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadTask {
    /// lmgw's id of the task: the cancel route's `{task}`.
    pub id: i64,
    /// The server's id of the task.
    pub task_id: String,
    pub server_label: String,
    /// The name the model called.
    pub tool: String,
    /// `working` or `input_required` while it runs; `completed`, `failed`,
    /// `cancelled` or `abandoned` once it ended.
    #[cfg_attr(
        feature = "schema",
        schemars(extend("enum" = ["working", "input_required", "completed", "failed",
                                  "cancelled", "abandoned"]))
    )]
    pub status: String,
    /// The server's last word on it, when it gave one.
    pub status_message: Option<String>,
    /// How long the server keeps it, as it stated; `null` when it stated
    /// none.
    pub ttl_ms: Option<i64>,
    /// When it started (UTC, `YYYY-MM-DD HH:MM:SS`).
    pub started_at: String,
    /// Who started it ("the dashboard", "device 'phone'").
    pub by: Option<String>,
    /// What it waits for, when it waits: a device that is not connected
    /// (its tasks are asked about again when it links), a turn of the
    /// thread that runs (a result enters the thread once it ended), or the
    /// decision on the calls the thread's last reply waits on.
    pub waiting_for: Option<String>,
}

/// `POST /chat/api/threads/{id}/answer`'s body, which may be empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AnswerRequest {
    /// Read the answer aloud as it streams, as a send's `speak`.
    pub speak: bool,
}

/// `POST /chat/api/threads/{id}/tasks/{task}/cancel`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskCancelled {
    /// The task as the cancel left it.
    pub task: ThreadTask,
    /// Its result is in the thread now (a message of role `tool`); `false`
    /// while it waits for a turn that runs, or while the server has not
    /// ended the task yet.
    pub delivered: bool,
    /// What happened, in a sentence.
    pub note: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_row_s_task_reads_back() {
        let t = MessageTask {
            task_id: "7f3a".into(),
            server_label: "desktop".into(),
            tool: "desktop__build".into(),
            status: "cancelled".into(),
            ended_by: Some("the dashboard".into()),
            structured_content: None,
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["ended_by"], "the dashboard");
        assert!(v.get("structured_content").is_none(), "{v}");
        assert_eq!(serde_json::from_value::<MessageTask>(v).unwrap(), t);
        let with = MessageTask {
            structured_content: Some(serde_json::json!({"temp": 21})),
            ..t
        };
        let v = serde_json::to_value(&with).unwrap();
        assert_eq!(serde_json::from_value::<MessageTask>(v).unwrap(), with);
        let a: AnswerRequest = serde_json::from_str("{}").unwrap();
        assert!(!a.speak);
    }
}
