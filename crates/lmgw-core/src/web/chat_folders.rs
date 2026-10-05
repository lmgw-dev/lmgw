//! The Chat's folders (chat-complete design §5): `GET/POST
//! /chat/api/folders`, `POST /chat/api/folders/{id}` (patch),
//! `…/{id}/delete`, `POST /chat/api/threads/{id}/move`, and the folder-aware
//! half of `POST /chat/api/threads` ([`create_in_folder`]).
//!
//! Folders live in the DB only. A temporary thread (negative id) has no
//! folder and refuses a move; **Keep** writes it without one.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use super::chat::{err_json, thread_json};
use super::chat_extract::{ChatJson, ChatPath};
use super::chat_repo::ChatRepo;
use super::{chat_knowledge, chat_reasoning, chat_sampling};
use crate::state::{AppState, SharedState};
use crate::store::{self, ChatFolderPatch, ChatThread, ThreadDefaults};

/// Normalise and validate folder defaults with the checks a thread's own
/// settings go through (`chat_sampling::check`, `chat_reasoning::check`):
/// the defaults are laid over a blank thread and that thread is checked, so a
/// value a thread would refuse is refused here — by name, before it is stored
/// — rather than at the first new thread. What the checks normalise (blank
/// stop sequences, a trimmed effort) is written back.
///
/// A new field joins the checks by joining [`ThreadDefaults::apply`]; only a
/// field with its own validator needs a line here.
pub(super) fn check_defaults(d: &mut ThreadDefaults) -> Result<(), String> {
    d.model_alias = d.model_alias.take().filter(|a| !a.trim().is_empty());
    let mut t = ChatThread::default();
    d.apply(&mut t);
    chat_reasoning::check(&mut t).and_then(|()| chat_sampling::check(&mut t))?;
    d.stop = d.stop.take().map(|_| t.stop).filter(|s| !s.is_empty());
    d.reasoning_effort = d.reasoning_effort.take().and(t.reasoning_effort);
    // No tool servers is the global behaviour, not a default to record.
    d.mcp_tools = d.mcp_tools.take().filter(|m| !m.is_empty());
    // Voice (chat-voice design §2.2): normalised as a thread's own; one that
    // sets nothing is no default. No seed: each thread draws its own on
    // first use, and one copied into every new thread would give the whole
    // folder one voice behind a field the form never shows.
    if let Some(v) = d.voice.as_mut() {
        if v.seed.is_some() {
            return Err(
                "voice.seed cannot be a folder default: each thread draws its own seed on \
                 first use"
                    .into(),
            );
        }
        v.normalise()?;
    }
    d.voice = d.voice.take().filter(|v| !v.is_empty());
    chat_knowledge::check_defaults(d)
}

/// Parse a request's `defaults` strictly — an unknown field or a wrong type
/// is a 400 naming it, not the extractor's bare 422 — then check it.
fn parse_defaults(v: serde_json::Value) -> Result<ThreadDefaults, Response> {
    let mut d: ThreadDefaults = serde_json::from_value(v).map_err(|e| {
        err_json(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!("defaults: {e}"),
        )
    })?;
    check_defaults(&mut d).map_err(|msg| err_json(StatusCode::BAD_REQUEST, "bad_request", msg))?;
    Ok(d)
}

fn valid_name(name: &str) -> Result<String, Response> {
    let n = name.trim();
    if n.is_empty() {
        return Err(err_json(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "a folder needs a name",
        ));
    }
    Ok(n.to_string())
}

fn not_found() -> Response {
    err_json(StatusCode::NOT_FOUND, "not_found", "folder not found")
}

fn internal(e: impl std::fmt::Display) -> Response {
    err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
}

/// `GET /chat/api/folders` — `{folders: [...]}` in sidebar order, each with
/// its `defaults` and `threads_active` / `threads_archived`.
pub async fn list_folders(State(state): State<SharedState>) -> Response {
    match store::list_chat_folders(&state.db).await {
        Ok(f) => Json(json!({ "folders": f })).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateFolderReq {
    name: String,
    /// A [`ThreadDefaults`] object; parsed by [`parse_defaults`].
    #[serde(default)]
    defaults: Option<serde_json::Value>,
}

/// `POST /chat/api/folders` `{name, defaults?}` — the new folder, last in
/// the list.
pub async fn create_folder(
    State(state): State<SharedState>,
    ChatJson(req): ChatJson<CreateFolderReq>,
) -> Response {
    let name = match valid_name(&req.name) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let defaults = match req.defaults.map(parse_defaults).transpose() {
        Ok(d) => d.unwrap_or_default(),
        Err(r) => return r,
    };
    if let Err(msg) = chat_knowledge::check_default_kbs(&state, &defaults, &[]).await {
        return err_json(StatusCode::BAD_REQUEST, "bad_request", msg);
    }
    if let Some(v) = &defaults.voice {
        if let Err(msg) = super::chat_voice::check_voice_aliases(&state, v, None).await {
            return err_json(StatusCode::BAD_REQUEST, "bad_request", msg);
        }
    }
    let id = match store::create_chat_folder(&state.db, &name, &defaults).await {
        Ok(id) => id,
        Err(e) => return internal(e),
    };
    respond_with_folder(&state, id).await
}

async fn respond_with_folder(state: &AppState, id: i64) -> Response {
    match store::list_chat_folders(&state.db).await {
        Ok(all) => match all.into_iter().find(|f| f.folder.id == id) {
            Some(f) => Json(f).into_response(),
            None => not_found(),
        },
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateFolderReq {
    name: Option<String>,
    sort: Option<i64>,
    /// Replaces the folder's defaults whole (an absent field is unchanged;
    /// `{}` clears them).
    defaults: Option<serde_json::Value>,
}

/// `POST /chat/api/folders/{id}` `{name?, sort?, defaults?}` — patch. Threads
/// already in the folder are not touched.
pub async fn update_folder(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<UpdateFolderReq>,
) -> Response {
    let mut patch = ChatFolderPatch {
        sort: req.sort,
        ..Default::default()
    };
    if let Some(n) = &req.name {
        match valid_name(n) {
            Ok(n) => patch.name = Some(n),
            Err(r) => return r,
        }
    }
    match req.defaults.map(parse_defaults).transpose() {
        Ok(d) => patch.defaults = d,
        Err(r) => return r,
    }
    if let Some(d) = &patch.defaults {
        // Bases the folder already named are not checked again (a base
        // deleted since must not block renaming the folder).
        let stored = match store::get_chat_folder(&state.db, id).await {
            Ok(Some(f)) => f.defaults,
            Ok(None) => return not_found(),
            Err(e) => return internal(e),
        };
        let already = stored.kb_ids.clone().unwrap_or_default();
        if let Err(msg) = chat_knowledge::check_default_kbs(&state, d, &already).await {
            return err_json(StatusCode::BAD_REQUEST, "bad_request", msg);
        }
        // Voice aliases the folder already named are not checked again
        // either.
        if let Some(v) = &d.voice {
            let before = stored.voice.unwrap_or_default();
            if let Err(msg) = super::chat_voice::check_voice_aliases(&state, v, Some(&before)).await
            {
                return err_json(StatusCode::BAD_REQUEST, "bad_request", msg);
            }
        }
    }
    match store::update_chat_folder(&state.db, id, &patch).await {
        Ok(true) => respond_with_folder(&state, id).await,
        Ok(false) => not_found(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ThreadsFate {
    Keep,
    Delete,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteFolderReq {
    threads: ThreadsFate,
}

/// `POST /chat/api/folders/{id}/delete` `{threads: "keep"|"delete"}` — `keep`
/// leaves the threads without a folder, `delete` deletes them with it. The
/// choice is required: there is no default for destroying conversations.
pub async fn delete_folder(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<DeleteFolderReq>,
) -> Response {
    match store::delete_chat_folder(&state.db, id, req.threads == ThreadsFate::Delete).await {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => not_found(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct MoveReq {
    /// The target folder; `null` takes the thread out of any folder.
    folder_id: Option<i64>,
}

/// `POST /chat/api/threads/{id}/move` `{folder_id|null}` — the thread, moved.
/// A temporary thread has no folder until it is kept: 409 `temporary_thread`.
pub async fn move_thread(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<MoveReq>,
) -> Response {
    let repo = ChatRepo::of(id);
    if repo.thread(&state, id).await.ok().flatten().is_none() {
        return err_json(StatusCode::NOT_FOUND, "not_found", "thread not found");
    }
    if repo.is_temp() {
        return err_json(
            StatusCode::CONFLICT,
            "temporary_thread",
            "a temporary chat cannot be moved into a folder — Keep it first",
        );
    }
    if let Some(f) = req.folder_id {
        if !matches!(store::get_chat_folder(&state.db, f).await, Ok(Some(_))) {
            return not_found();
        }
    }
    if let Err(e) = store::set_chat_thread_folder(&state.db, id, req.folder_id).await {
        return internal(e);
    }
    match store::get_chat_thread(&state.db, id).await {
        Ok(Some(t)) => Json(thread_json(&state, &t).await).into_response(),
        _ => internal("the thread vanished immediately after being moved"),
    }
}

/// A new stored thread in folder `folder_id`: the global start (`model_alias`,
/// `prompt` — the default system prompt) with the folder's defaults over it,
/// as the thread's own copy. Admin Chat threads keep their built-in setup:
/// they join the folder but take none of its defaults. Errors are ready
/// responses.
pub(super) async fn create_in_folder(
    state: &AppState,
    folder_id: i64,
    model_alias: &str,
    kind: &str,
    prompt: &str,
) -> Result<ChatThread, Response> {
    let folder = match store::get_chat_folder(&state.db, folder_id).await {
        Ok(Some(f)) => f,
        Ok(None) => return Err(not_found()),
        Err(e) => return Err(internal(e)),
    };
    let mut t = ChatThread {
        model_alias: model_alias.to_string(),
        system_prompt: prompt.to_string(),
        kind: kind.to_string(),
        folder_id: Some(folder_id),
        ..Default::default()
    };
    if kind == "chat" {
        folder.defaults.apply(&mut t);
    }
    let id = store::create_chat_thread_from(&state.db, &t)
        .await
        .map_err(internal)?;
    match store::get_chat_thread(&state.db, id).await {
        Ok(Some(t)) => Ok(t),
        _ => Err(internal(
            "the thread vanished immediately after being created",
        )),
    }
}
