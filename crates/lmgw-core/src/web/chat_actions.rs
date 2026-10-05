//! Thread-scoped actions that change what a conversation is, rather than add
//! to it (chat-complete design §3, §7): **Keep** a temporary chat, and the
//! message actions.
//!
//! Every route here takes the thread in its path and dispatches on the id's
//! sign ([`ChatRepo::of`]), so a temporary thread gets the same actions an
//! ordinary one does.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::chat::{err_json, thread_json};
use super::chat_extract::{ChatJson, ChatOptJson, ChatPath};
use super::chat_knowledge;
use super::chat_repo::{ChatRepo, KeepOutcome};
use super::chat_turn::{self, TurnMode};
use super::chat_voice::ReadAloud;
use crate::state::SharedState;
use crate::store::{ChatMessageRow, ChatMessageUpdate, ChatThread, MessageVoice};

/// `POST /chat/api/threads/{id}/persist` — **Keep** a temporary chat: it is
/// written to the DB as an ordinary thread (messages, attachments, settings)
/// and leaves memory. Answers `{id, thread}` with the new, positive id. A
/// stored thread is refused (`409 not_temporary`), and so is a Keep while
/// another Keep of the same thread runs (`409 keep_in_progress` — it is
/// stored once), and so is a Keep while a realtime session is bound to it
/// (`409 voice_session_active`, chat-voice design §8.1); an unknown
/// temporary one is a 404.
pub async fn persist_thread(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
) -> Response {
    if !ChatRepo::of(id).is_temp() {
        return err_json(
            StatusCode::CONFLICT,
            "not_temporary",
            "this chat is already saved",
        );
    }
    let new_id = match ChatRepo::keep(&state, id).await {
        Ok(KeepOutcome::Kept(new_id)) => new_id,
        Ok(KeepOutcome::Busy) => {
            return err_json(
                StatusCode::CONFLICT,
                "keep_in_progress",
                "this chat is being kept right now",
            )
        }
        Ok(KeepOutcome::VoiceActive) => {
            return err_json(
                StatusCode::CONFLICT,
                "voice_session_active",
                "this chat is in voice mode; leave voice mode to keep it",
            )
        }
        Ok(KeepOutcome::NotFound) => {
            return err_json(StatusCode::NOT_FOUND, "not_found", "thread not found")
        }
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    match ChatRepo::Db.thread(&state, new_id).await {
        Ok(Some(t)) => {
            Json(json!({ "id": new_id, "thread": thread_json(&state, &t).await })).into_response()
        }
        _ => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "the kept thread vanished immediately after being written",
        ),
    }
}

// ---------------------------------------------------------------------------
// Message actions (design §3)
// ---------------------------------------------------------------------------
//
// The conversation stays one linear list, and an action that rewrites it is
// final: whatever it cuts is deleted, with its attachments. The thread's
// *current* settings answer — a regenerate after switching models is how a
// reply is retried on another one.

fn thread_not_found() -> Response {
    err_json(StatusCode::NOT_FOUND, "not_found", "thread not found")
}

fn message_not_found() -> Response {
    err_json(
        StatusCode::NOT_FOUND,
        "not_found",
        "message not found in this thread",
    )
}

fn internal(e: impl std::fmt::Display) -> Response {
    err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
}

/// The thread in the path and its message `mid` — the message must belong
/// to that thread, or it is a 404 like an unknown one.
async fn thread_and_message(
    state: &SharedState,
    repo: ChatRepo,
    id: i64,
    mid: i64,
) -> Result<(ChatThread, ChatMessageRow), Response> {
    let thread = match repo.thread(state, id).await {
        Ok(Some(t)) => t,
        Ok(None) => return Err(thread_not_found()),
        Err(e) => return Err(internal(e)),
    };
    match repo.message(state, id, mid).await {
        Ok(Some(m)) => Ok((thread, m)),
        Ok(None) => Err(message_not_found()),
        Err(e) => Err(internal(e)),
    }
}

/// A message as `GET /chat/api/threads/{id}` lists it: the row plus its
/// `attachments`.
async fn message_json(state: &SharedState, repo: ChatRepo, m: &ChatMessageRow) -> Value {
    let atts: Vec<_> = repo
        .attachments_meta(state, m.thread_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.message_id == Some(m.id))
        .collect();
    let mut v = serde_json::to_value(m).expect("ChatMessageRow always serializes");
    v["attachments"] = json!(atts);
    v
}

/// `POST /chat/api/threads/{id}/messages/{mid}/delete` — delete that one
/// message (its attachments go with it). Nothing after it moves.
pub async fn delete_message(
    State(state): State<SharedState>,
    ChatPath((id, mid)): ChatPath<(i64, i64)>,
) -> Response {
    match ChatRepo::of(id).delete_message(&state, id, mid).await {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => message_not_found(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct EditReq {
    content: String,
    /// A user message's knowledge bases for itself (`#`), replacing the ones
    /// it had; absent keeps them. Ignored for a reply.
    #[serde(default)]
    kb_refs: Option<Vec<i64>>,
    /// Read the new answer aloud as it streams (chat-voice design §6.4).
    /// Ignored for a reply, which is not answered again.
    #[serde(default)]
    speak: bool,
}

/// The optional body of a regenerate or a continue (chat-voice design
/// §6.4): an empty request is still accepted.
#[derive(Debug, Default, Deserialize)]
pub struct TurnReq {
    /// Read the answer aloud as it streams.
    #[serde(default)]
    speak: bool,
}

/// `POST /chat/api/threads/{id}/messages/{mid}/edit` `{content}`.
///
/// - **A reply** is rewritten in place and nothing is sent: its reasoning,
///   token counts and tool record are cleared, since they no longer describe
///   the text, and so are a spoken reply's unheard rest and timing.
///   Answers `{ok, message}` (the row as the thread lists it).
/// - **A user message** is rewritten in place (its attachments stay bound),
///   every later message is deleted, and it is answered again: the answer is
///   the send's SSE stream, opening with `turn {user_message_id}`. Empty text
///   is refused unless the message carries files, as a send refuses it. Its
///   stored knowledge retrieval goes with the old text: auto mode searches
///   again for the new one (`kb_refs`, when sent, replaces its `#` picks).
///   A dictated message whose text changed becomes a typed turn: its
///   `voice` goes, since the text is no longer what was spoken
///   (chat-voice §3).
///   Everything that can refuse or take time — the checks, the model's
///   capabilities, which may probe a catalog — happens before anything is
///   written, and the rewrite (text, picks, the cut) is one write, so a
///   dropped request never leaves half of it (review R1 finding 9).
pub async fn edit_message(
    State(state): State<SharedState>,
    ChatPath((id, mid)): ChatPath<(i64, i64)>,
    ChatJson(req): ChatJson<EditReq>,
) -> Response {
    let repo = ChatRepo::of(id);
    let (thread, msg) = match thread_and_message(&state, repo, id, mid).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match msg.role.as_str() {
        "assistant" => {
            // A spoken reply stays spoken, but what was not heard and the
            // turn's timing no longer describe the text (chat-voice §3).
            let update = ChatMessageUpdate {
                content: req.content,
                voice: msg.voice.clone().map(MessageVoice::edited),
                ..Default::default()
            };
            match repo.update_message(&state, id, mid, &update).await {
                Ok(true) => {}
                Ok(false) => return message_not_found(),
                Err(e) => return internal(e),
            }
            match repo.message(&state, id, mid).await {
                Ok(Some(m)) => {
                    let message = message_json(&state, repo, &m).await;
                    Json(json!({ "ok": true, "message": message })).into_response()
                }
                Ok(None) => message_not_found(),
                Err(e) => internal(e),
            }
        }
        "user" => {
            let content = req.content.trim().to_string();
            if content.is_empty() {
                let has_files = repo
                    .attachments_meta(&state, id)
                    .await
                    .unwrap_or_default()
                    .iter()
                    .any(|a| a.message_id == Some(mid));
                if !has_files {
                    return err_json(
                        StatusCode::BAD_REQUEST,
                        "empty_message",
                        "the message is empty",
                    );
                }
            }
            let kb_refs = match &req.kb_refs {
                Some(ids) => match chat_knowledge::check_kb_ids(&state, ids, &msg.kb_refs).await {
                    Ok(ids) => ids,
                    Err(m) => return err_json(StatusCode::BAD_REQUEST, "bad_request", m),
                },
                None => msg.kb_refs.clone(),
            };
            let caps = super::chat_attach_gate::thread_caps(&state, repo, &thread).await;
            match repo
                .rewrite_user_message(&state, id, mid, &content, &kb_refs)
                .await
            {
                Ok(true) => {}
                Ok(false) => return message_not_found(),
                Err(e) => return internal(e),
            }
            answer(&state, repo, &thread, Some(mid), caps, req.speak).await
        }
        other => err_json(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!("a '{other}' message cannot be edited — only user messages and replies"),
        ),
    }
}

/// `POST /chat/api/threads/{id}/messages/{mid}/regenerate` — answer again,
/// as SSE. The model's capabilities are read before anything is cut (review
/// R1 finding 9), and the cut is one write.
///
/// - **On a reply**: it and everything after it are deleted, and the model
///   answers the history before it. That history has to end with a user
///   message (`409 nothing_to_answer` otherwise — nothing is deleted then).
/// - **On a user message**: everything after it is deleted and it is answered
///   again; the stream opens with `turn {user_message_id}`.
///
/// The body is optional (`{speak}`, [`TurnReq`]); an empty one is accepted.
pub async fn regenerate_message(
    State(state): State<SharedState>,
    ChatPath((id, mid)): ChatPath<(i64, i64)>,
    ChatOptJson(req): ChatOptJson<TurnReq>,
) -> Response {
    let repo = ChatRepo::of(id);
    let (thread, msg) = match thread_and_message(&state, repo, id, mid).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match msg.role.as_str() {
        "assistant" => {
            let history = match repo.messages(&state, id).await {
                Ok(h) => h,
                Err(e) => return internal(e),
            };
            let before = history
                .iter()
                .position(|m| m.id == mid)
                .and_then(|p| p.checked_sub(1))
                .map(|p| &history[p]);
            if !before.is_some_and(|m| m.role == "user") {
                return err_json(
                    StatusCode::CONFLICT,
                    "nothing_to_answer",
                    "there is no user message right before this reply to answer again",
                );
            }
            let caps = super::chat_attach_gate::thread_caps(&state, repo, &thread).await;
            if let Err(e) = repo.truncate(&state, id, mid, true).await {
                return internal(e);
            }
            answer(&state, repo, &thread, None, caps, req.speak).await
        }
        "user" => {
            let caps = super::chat_attach_gate::thread_caps(&state, repo, &thread).await;
            if let Err(e) = repo.truncate(&state, id, mid, false).await {
                return internal(e);
            }
            answer(&state, repo, &thread, Some(mid), caps, req.speak).await
        }
        other => err_json(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!("a '{other}' message cannot be regenerated — only user messages and replies"),
        ),
    }
}

/// A fresh turn over the thread as it now stands, with the `caps` read
/// before it was rewritten; `speak` reads it aloud as it streams.
async fn answer(
    state: &SharedState,
    repo: ChatRepo,
    thread: &ChatThread,
    user_message_id: Option<i64>,
    caps: super::chat_attach_gate::Caps,
    speak: bool,
) -> Response {
    let mode = TurnMode::Fresh { user_message_id };
    let speak = speak.then(ReadAloud::default);
    chat_turn::start_turn(state, repo, thread, mode, caps, speak).await
}

/// `POST /chat/api/threads/{id}/continue` — the model continues the
/// thread's last reply (assistant prefill), as SSE: the `delta`s are the
/// continuation only, `done.message_id` is the continued row, and the row's
/// text and reasoning grow by what came (its token counts become this
/// call's). Refused with `409 continue_unavailable` and the reason the
/// thread JSON's `continue` gives when that says no; a route the send is
/// re-routed to that cannot take a prefill is refused in the stream. The
/// body is optional (`{speak}`, [`TurnReq`]): with `speak`, the
/// continuation is read aloud as it streams.
pub async fn continue_reply(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatOptJson(req): ChatOptJson<TurnReq>,
) -> Response {
    let repo = ChatRepo::of(id);
    let thread = match repo.thread(&state, id).await {
        Ok(Some(t)) => t,
        Ok(None) => return thread_not_found(),
        Err(e) => return internal(e),
    };
    let last = match repo.last_message(&state, id).await {
        Ok(m) => m,
        Err(e) => return internal(e),
    };
    let verdict = chat_turn::continue_state(&state.snapshot(), &thread, last.as_ref());
    let Some(last) = last.filter(|_| verdict.ok) else {
        return err_json(
            StatusCode::CONFLICT,
            "continue_unavailable",
            verdict.reason.unwrap_or_default(),
        );
    };
    let caps = super::chat_attach_gate::thread_caps(&state, repo, &thread).await;
    let mode = TurnMode::Continue {
        message_id: last.id,
    };
    // Read from the clause the stored reply broke off in (§6.4).
    let speak = req.speak.then(|| ReadAloud::continuing(&last.content));
    chat_turn::start_turn(&state, repo, &thread, mode, caps, speak).await
}
