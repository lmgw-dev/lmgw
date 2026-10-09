//! The three routes of a thread's MCP tasks (MCP Tasks design §5.1,
//! `Cap::Chat`), resolving the thread as every Chat route does: one the
//! caller does not reach is the `404` of a thread that is not there (L3).
//!
//! - **`GET /chat/api/threads/{id}/tasks`**: the thread's `ThreadTask`s, the
//!   `tasks` of `GET …/threads/{id}` without the history; the same reach
//!   rules (a thread out of reach is `404 not_found`).
//! - **`POST /chat/api/threads/{id}/answer`** (T12): a continuation, the
//!   turn a send starts but with no new user message, streaming the same
//!   frames. It needs a result nothing answered yet — a result row with no
//!   reply stored after it, or one waiting to enter the thread (the turn's
//!   start writes it in) — else `409 nothing_to_answer`, asked again once
//!   the turn took the thread (`TurnMode::Answer`). While a turn of the
//!   thread runs it is `409 turn_running`: it would cancel that turn, whose
//!   end lets the results in and whose reply may answer them; the start
//!   itself refuses one that began meanwhile, cancelling nothing. It runs
//!   on the thread's alias with its configured fallbacks, as every turn
//!   does.
//! - **`POST /chat/api/threads/{id}/tasks/{task}/cancel`** (T16):
//!   `McpManager::cancel_task` with the thread the caller reaches, so a
//!   task of another thread is `404 task_not_found`, and so is one whose
//!   result entered the thread already (its row is gone) — the thread's own
//!   `404 not_found` is for a thread the caller does not reach; one that
//!   ended and waits to enter is `409 task_ended`. A server that does not declare `tasks.cancel` is `409
//!   task_cancel_unsupported`, one that answers the cancel with an error
//!   `502 task_cancel_refused` (design decision of WP2: both are said, and
//!   the task goes on). Otherwise the answer says what happened, and the
//!   result enters the thread at once when no turn runs.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::chat::{task_code, AnswerRequest, TaskCancelled, ThreadTask};

use super::super::chat::err_json;
use super::super::chat_attach_gate;
use super::super::chat_caller::Caller;
use super::super::chat_extract::{ChatOptJson, ChatPath};
use super::super::chat_repo::ChatRepo;
use super::super::chat_turn::{self, TurnMode};
use super::super::chat_voice::ReadAloud;
use crate::mcp::tasks::{CancelOutcome, CancelRefusal};
use crate::state::SharedState;
use crate::store::mcp_tasks;

fn not_found() -> Response {
    err_json(StatusCode::NOT_FOUND, "not_found", "thread not found")
}

fn internal(e: impl std::fmt::Display) -> Response {
    err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
}

/// `GET /chat/api/threads/{id}/tasks` — the thread's tasks as
/// `GET /chat/api/threads/{id}` lists them, without reading the history
/// (the Chat page's strip re-reads them at the poll interval). A thread the
/// caller does not reach is the same `404 not_found`.
pub(in crate::web) async fn tasks(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
) -> Response {
    match ChatRepo::of(id).thread_as(&state, &caller, id).await {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    }
    Json(super::thread_tasks(&state, id).await).into_response()
}

/// `POST /chat/api/threads/{id}/answer` (module doc).
pub(in crate::web) async fn answer(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
    ChatOptJson(req): ChatOptJson<AnswerRequest>,
) -> Response {
    let repo = ChatRepo::of(id);
    let thread = match repo.thread_as(&state, &caller, id).await {
        Ok(Some(t)) => t,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    let history = match repo.messages(&state, id).await {
        Ok(h) => h,
        Err(e) => return internal(e),
    };
    if state.chat_live.running(id) {
        return super::turn_running();
    }
    let waiting = !repo.is_temp()
        && mcp_tasks::waiting(&state.db, id)
            .await
            .is_ok_and(|w| !w.is_empty())
        && super::deliver::held_by(&state, id).await.is_none();
    if !super::render::unanswered(&history) && !waiting {
        return super::nothing_to_answer();
    }
    let caps = chat_attach_gate::thread_caps(&state, repo, &thread).await;
    let mode = TurnMode::Answer;
    let speak = req.speak.then(ReadAloud::default);
    chat_turn::start_turn(&state, &caller, repo, &thread, mode, caps, speak).await
}

/// `POST /chat/api/threads/{id}/tasks/{task}/cancel` (module doc).
pub(in crate::web) async fn cancel(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath((id, task)): ChatPath<(i64, i64)>,
) -> Response {
    let repo = ChatRepo::of(id);
    match repo.thread_as(&state, &caller, id).await {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    }
    let by = caller.named();
    let outcome = match state.mcp.cancel_task(id, task, &by).await {
        Ok(o) => o,
        Err(CancelRefusal::NotFound) => {
            return err_json(
                StatusCode::NOT_FOUND,
                task_code::TASK_NOT_FOUND,
                format!(
                    "thread {id} has no job {task} (a job whose result is in the thread is \
                     gone)"
                ),
            )
        }
        Err(CancelRefusal::Ended) => {
            return err_json(
                StatusCode::CONFLICT,
                task_code::TASK_ENDED,
                format!("job {task} already ended: its result is in the thread or on its way"),
            )
        }
        Err(CancelRefusal::Unsupported(why)) => {
            return err_json(StatusCode::CONFLICT, task_code::CANCEL_UNSUPPORTED, why)
        }
        Err(CancelRefusal::Server(message)) => {
            return err_json(
                StatusCode::BAD_GATEWAY,
                task_code::CANCEL_REFUSED,
                format!("the server refused to cancel job {task}: {message}; it goes on"),
            )
        }
    };
    // The row as the cancel left it, before its result may enter the
    // thread and let it go.
    let row = match mcp_tasks::get(&state.db, task).await {
        Ok(row) => row,
        Err(e) => return internal(e),
    };
    let ended = row.as_ref().is_some_and(|r| r.result.is_some());
    if ended {
        super::deliver::when_idle(&state, id).await;
    }
    let delivered = ended
        && !mcp_tasks::get(&state.db, task)
            .await
            .ok()
            .flatten()
            .is_some_and(|r| r.result.is_some() && r.thread_id == Some(id));
    let task = match &row {
        Some(r) => super::thread_task(&state, r).await,
        None => ThreadTask {
            id: task,
            ..ThreadTask::default()
        },
    };
    let held = match delivered {
        true => None,
        false => super::deliver::held_by(&state, id).await,
    };
    let note = note(&outcome, delivered, held);
    Json(TaskCancelled {
        task,
        delivered,
        note,
    })
    .into_response()
}

/// What a cancel did, in a sentence.
fn note(outcome: &CancelOutcome, delivered: bool, held: Option<String>) -> String {
    let result = match (delivered, held) {
        (true, _) => "its result is in the thread".to_string(),
        (false, Some(by)) => format!("its result enters the thread after {by}"),
        (false, None) => "its result enters the thread at once".to_string(),
    };
    match outcome {
        CancelOutcome::Cancelled => format!("the server cancelled the job; {result}"),
        CancelOutcome::FinishedFirst => {
            format!("the job finished before the cancel reached it; {result}")
        }
        CancelOutcome::Owed => format!(
            "the server was not connected: the job is cancelled here, and the server is told \
             when it connects; {result}"
        ),
        CancelOutcome::Requested => "the server was asked to cancel the job and has not ended \
                                     it yet; it is followed until it says it ended"
            .to_string(),
    }
}
