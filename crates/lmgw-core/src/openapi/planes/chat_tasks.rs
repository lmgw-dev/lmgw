//! A thread's MCP tasks' documented routes (MCP Tasks design §5.1, §5.4),
//! under the "Chat" tag and merged into the Chat plane (`planes::chat`):
//! answering the results nothing answered yet, and cancelling a task. Every
//! shape is `lmgw-api-types`' (`chat`'s task types). No op and no `x-lmgw`
//! header.

use lmgw_api_types::chat::{self, AnswerRequest, TaskCancelled, ThreadTask};

use super::super::registry::{DocRoute, Req, Resp};
use super::chat::chat_route;
use super::chat_approvals::{done_frame, error_frame, text_frame, tool_frame};

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<Vec<ThreadTask>>()),
            ..chat_route(
                "GET",
                chat::TASKS_PATH,
                "List a thread's jobs",
                "The thread's MCP tasks still running or waiting to enter it, oldest first, \
                 as GET /chat/api/threads/{id} lists them as `tasks` (ThreadTask), without \
                 the history: the Chat page's strip re-reads them at mcp.task_poll_interval_s. \
                 A thread the caller does not reach is 404 not_found, as everywhere in Chat; \
                 a temporary thread has none (an empty list).",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<AnswerRequest>()),
            response: Resp::Sse(&[
                ("delta", text_frame),
                ("reasoning", text_frame),
                ("tool", tool_frame),
                ("error", error_frame),
                ("done", done_frame),
            ]),
            ..chat_route(
                "POST",
                chat::ANSWER_PATH,
                "Answer a thread's job results",
                "A tool whose MCP server runs it as a task (its execution.taskSupport is \
                 required) answers the turn at once: its result frame's output is `started, \
                 job <task_id>` and carries task {id, task_id, server_label}, and the turn goes \
                 on. lmgw follows the task; when it ends, its result enters the thread as a \
                 message of role `tool` with task {task_id, server_label, tool, status: \
                 completed | failed | cancelled | abandoned, ended_by}, its content the result \
                 as text — only while no turn of the thread runs (a turn that runs takes it in \
                 when it ends; one that starts, before it reads the history), and not while the \
                 thread's last reply waits for approvals or for the turn that runs the calls \
                 decided on it. The feed records task.started and \
                 task.done. Every later turn sees it as a call of lmgw__job_result and its \
                 result, in chronological order: where it is stored among the thread's \
                 messages, so a message sent after it follows it; an edit or a regenerate \
                 keeps it, and the reply after it answers it again. lmgw starts no \
                 turn for a result: this route asks for one, a continuation with no new user \
                 message, streaming the frames a send streams; a send answers it too. It \
                 needs a result nothing answered yet (a result row with no reply after it, or \
                 one waiting to enter the thread), else 409 nothing_to_answer; while a turn of \
                 the thread runs it is 409 turn_running (that turn's end lets the results in, \
                 and its reply may answer them). It runs on the thread's alias with its \
                 configured fallbacks, as every turn does. The body may be empty; speak reads \
                 the answer aloud as a send's does. \
                 GET /chat/api/threads/{id} lists the thread's tasks still running or waiting \
                 to enter it as `tasks` (ThreadTask: id, task_id, server_label, tool, status, \
                 status_message, ttl_ms, started_at, by, waiting_for). A temporary thread's \
                 calls wait for their task instead, within the server's timeout_ms.",
            )
        },
        DocRoute {
            path_ints: &["id", "task"],
            response: Resp::Json(|g| g.root_schema_for::<TaskCancelled>()),
            ..chat_route(
                "POST",
                chat::CANCEL_PATH,
                "Cancel a thread's job",
                "Cancels task {task} (ThreadTask.id, the feed's task.started id) of the thread: \
                 lmgw sends tasks/cancel. The answer is the task as the cancel left it, whether \
                 its result is in the thread now, and what happened: the server cancelled it \
                 (its result says `cancelled by <who>`), it finished before the cancel (its real \
                 result), or the server was not connected (it ends cancelled at once and the \
                 server is told when it connects), or the server was asked and has not ended it \
                 yet (it is followed until it says it ended). The body is ignored. A thread \
                 the caller does not reach is 404 not_found; a task of another thread, or one \
                 whose result entered the thread already (it is gone then), 404 \
                 task_not_found; one that \
                 ended and whose result waits to enter the thread 409 task_ended; a server \
                 that does not declare \
                 tasks.cancel 409 task_cancel_unsupported, one that answers the cancel with an \
                 error 502 task_cancel_refused — the task goes on in both. Deleting the thread \
                 cancels its running tasks too.",
            )
        },
    ]
}
