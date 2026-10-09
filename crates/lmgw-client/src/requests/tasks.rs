//! The requests of a thread's MCP tasks (MCP Tasks design §5.1): a tool
//! whose server runs it as a task answers the turn at once (`started, job
//! <task id>`, the `tool` frame carrying `task: {id, task_id,
//! server_label}`), and its result enters the thread later as a message of
//! role `tool` — the feed says so with `task.done`.
//!
//! - [`thread_tasks`]: `GET /chat/api/threads/{id}/tasks`, the thread's
//!   jobs without its history, read with [`read_thread_tasks`].
//! - [`answer`]: `POST /chat/api/threads/{id}/answer`, the model answers
//!   the results nothing answered yet; the answer is the turn's frames, as
//!   a send streams them (`text/event-stream`; read it with
//!   [`crate::feed::SseDecoder`]). With nothing to answer, `409
//!   nothing_to_answer`; while a turn of the thread runs, `409
//!   turn_running` ([`super::read_refusal`]).
//! - [`cancel_task`]: `POST /chat/api/threads/{id}/tasks/{task}/cancel`,
//!   read with [`read_task_cancelled`]; refusals `404 not_found` (the
//!   thread), `404 task_not_found`, `409
//!   task_ended`, `409 task_cancel_unsupported`, `502 task_cancel_refused`
//!   ([`task_code`]).

pub use lmgw_api_types::chat::{task_code, AnswerRequest, MessageTask, TaskCancelled, ThreadTask};

use super::{read_refusal, ApiError, Method, Request};

/// `GET /chat/api/threads/{id}/tasks`: the thread's tasks, read with
/// [`read_thread_tasks`]; `404 not_found` for a thread the caller does not
/// reach.
pub fn thread_tasks(thread_id: i64) -> Request {
    Request::new(Method::Get, format!("/chat/api/threads/{thread_id}/tasks"))
}

reader!(
    /// [`thread_tasks`]' answer.
    read_thread_tasks -> Vec<ThreadTask>
);

/// `POST /chat/api/threads/{id}/answer`; `speak` reads the answer aloud
/// as it streams.
pub fn answer(thread_id: i64, speak: bool) -> Request {
    Request::new(
        Method::Post,
        format!("/chat/api/threads/{thread_id}/answer"),
    )
    .header("Accept", "text/event-stream")
    .json(serde_json::to_string(&AnswerRequest { speak }).unwrap_or_default())
}

/// `POST /chat/api/threads/{id}/tasks/{task}/cancel`, `task` lmgw's id of
/// the task ([`ThreadTask::id`], the feed's `task.started` `id`).
pub fn cancel_task(thread_id: i64, task: i64) -> Request {
    Request::new(
        Method::Post,
        format!("/chat/api/threads/{thread_id}/tasks/{task}/cancel"),
    )
    .json("{}".to_string())
}

reader!(
    /// [`cancel_task`]'s answer: the task as the cancel left it, whether its
    /// result is in the thread now, and what happened.
    read_task_cancelled -> TaskCancelled
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_routes_build_and_read() {
        let r = thread_tasks(7);
        assert_eq!(
            (r.method, r.path.as_str()),
            (Method::Get, "/chat/api/threads/7/tasks")
        );
        let l =
            read_thread_tasks(200, r#"[{"id": 3, "task_id": "t", "status": "working"}]"#).unwrap();
        assert_eq!(l[0].id, 3);
        let r = answer(7, true);
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.path, "/chat/api/threads/7/answer");
        assert_eq!(r.body.as_deref(), Some(r#"{"speak":true}"#));
        let r = cancel_task(7, 41);
        assert_eq!(r.path, "/chat/api/threads/7/tasks/41/cancel");
        let done = read_task_cancelled(
            200,
            r#"{"task": {"id": 41, "task_id": "t1", "status": "cancelled"},
                "delivered": true, "note": "the server cancelled the job"}"#,
        )
        .unwrap();
        assert!(done.delivered);
        assert_eq!(done.task.status, "cancelled");
        let e = read_task_cancelled(409, r#"{"code": "task_ended", "message": "no"}"#).unwrap_err();
        assert_eq!(e.code, task_code::TASK_ENDED);
    }
}
