//! Personality profiles' routes (personality-profiles design §3.1): list,
//! create, read, change, reset a built-in and delete, all
//! [`Cap::Chat`](crate::principal::Cap::Chat).
//! The logic is `ops::chat_profiles`, which the self-admin tools share
//! (D12); these handlers add who the caller is — the feed's author, how far
//! `used_by` counts, and a device's alias scope (D13).
//!
//! A device may list, create, edit, reset and delete profiles: the desktop
//! client's settings window hosts the editor. A `voice.tts_alias` it writes
//! passes its key's alias scope (`403 key_scope`); one the profile already
//! had is not checked again, as for a thread's aliases. A profile that Admin
//! Chat or the self-admin toolset uses is the exception: a device changes,
//! resets or deletes it only while it may use lmgw's admin tools with
//! writes (`403 profile_in_admin_use`, `ops::chat_profiles`).
//!
//! The editor's Preview, Test and Speak (`/chat/api/profiles/preview`,
//! `…/test`, `…/speak`) try an unsaved draft (the `try` module).

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::chat_profiles::{ProfileCreate, ProfilePatch};

use super::chat::err_json;
use super::chat_caller::Caller;
use super::chat_extract::{ChatJson, ChatPath};
use crate::ops::{self, ProfileError};
use crate::state::SharedState;

/// Preview, Test and Speak on an unsaved draft (design §3.1, D17).
mod r#try;
pub use r#try::{preview_profile, speak_profile, test_profile};

fn refused(e: ProfileError) -> Response {
    let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    err_json(status, e.code(), e.to_string())
}

/// `GET /chat/api/profiles` — `{profiles, default_profile_id}`: every
/// profile in name order, each with `used_by` as far as the caller reaches,
/// and the profile new threads start with (Settings → Chat, `null` for
/// none).
pub async fn list_profiles(State(state): State<SharedState>, caller: Caller) -> Response {
    let admin = caller.reach(&state.snapshot());
    match ops::profiles_list(&state, admin).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => refused(e),
    }
}

/// `GET /chat/api/profiles/{id}` — the profile.
pub async fn get_profile(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
) -> Response {
    let admin = caller.reach(&state.snapshot());
    match ops::profile_get(&state, id, admin).await {
        Ok(p) => Json(p).into_response(),
        Err(e) => refused(e),
    }
}

/// `POST /chat/api/profiles` — `{name, …fields}`, or `{builtin}` to create
/// a built-in profile again after it was deleted: the profile. `200`, as
/// every create of the Chat API answers (a folder's), and as the API
/// document can say it.
pub async fn create_profile(
    State(state): State<SharedState>,
    caller: Caller,
    ChatJson(body): ChatJson<ProfileCreate>,
) -> Response {
    let admin = caller.reach(&state.snapshot());
    let by = caller.named();
    match ops::profile_create(&state, &body, admin, Some(&by), caller.device_id()).await {
        Ok(p) => Json(p).into_response(),
        Err(e) => refused(e),
    }
}

/// `POST /chat/api/profiles/{id}` — the fields to change (absent
/// unchanged, `null` unset): the profile.
pub async fn update_profile(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
    ChatJson(patch): ChatJson<ProfilePatch>,
) -> Response {
    let admin = caller.reach(&state.snapshot());
    let by = caller.named();
    match ops::profile_update(&state, id, patch, admin, Some(&by), caller.device_id()).await {
        Ok(p) => Json(p).into_response(),
        Err(e) => refused(e),
    }
}

/// `POST /chat/api/profiles/{id}/reset` — the built-in profile reset to its
/// built-in texts, voice unset, name kept: the profile. `400
/// profile_not_builtin` for one of the owner's own.
pub async fn reset_profile(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
) -> Response {
    let admin = caller.reach(&state.snapshot());
    let by = caller.named();
    match ops::profile_reset(&state, id, admin, Some(&by), caller.device_id()).await {
        Ok(p) => Json(p).into_response(),
        Err(e) => refused(e),
    }
}

/// `POST /chat/api/profiles/{id}/delete` — `{deleted, threads_cleared,
/// folders_cleared, default_cleared}`: what the delete cleared, in the one
/// transaction it ran in (D16), counted as far as the caller reaches.
pub async fn delete_profile(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
) -> Response {
    let admin = caller.reach(&state.snapshot());
    let by = caller.named();
    match ops::profile_delete(&state, id, admin, Some(&by), caller.device_id()).await {
        Ok(d) => Json(d).into_response(),
        Err(e) => refused(e),
    }
}

/// `profile_id` in a thread's settings or a folder's defaults names a
/// profile that exists (design §3.1): `400 unknown_profile` otherwise.
pub(super) fn check_profile(state: &SharedState, profile_id: Option<i64>) -> Result<(), Response> {
    ops::profile_known(&state.snapshot(), profile_id)
        .map_err(|msg| err_json(StatusCode::BAD_REQUEST, "unknown_profile", msg))
}

/// The profile a new chat thread starts with when its folder's defaults
/// name none (Settings → Chat's `chat_profile`, design D9), as the default
/// prompt is the one it starts with; `None` when that names no profile now.
pub(super) fn default_profile(snap: &crate::config::Snapshot) -> Option<i64> {
    snap.settings
        .chat_profile
        .filter(|id| snap.chat_profile(*id).is_some())
}
