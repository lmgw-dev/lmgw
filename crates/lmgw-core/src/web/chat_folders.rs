//! The Chat's folders (chat-complete design §5): `GET/POST
//! /chat/api/folders`, `POST /chat/api/folders/{id}` (patch),
//! `…/{id}/delete`, `POST /chat/api/threads/{id}/move`, and the folder-aware
//! half of `POST /chat/api/threads` ([`create_in_folder`]).
//!
//! Folders live in the DB only. A temporary thread (negative id) has no
//! folder and refuses a move; **Keep** writes it without one.
//!
//! A folder may be one **ongoing conversation** (client-apps design §3):
//! `ongoing: {idle_minutes}` on create or patch, its current thread answered
//! by `POST /chat/api/folders/{id}/current` ([`current`]), and a defaults
//! change applied to that thread ([`apply`]). Any folder may carry its own
//! retention, `archive_days` and `purge_days` (§11 Q2; empty is the global
//! setting).

mod apply;
mod current;
pub(super) use current::thread_start;
pub(crate) mod retention;

pub use current::current;
pub(crate) use current::FolderLocks;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::chat::FolderList;
use lmgw_api_types::chat_folders::OngoingInput;
use serde::Deserialize;
use serde_json::json;

use super::chat::{err_json, present};
use super::chat_caller::Caller;
use super::chat_extract::{ChatJson, ChatPath};
use super::chat_repo::ChatRepo;
use super::chat_steer::Change;
use super::{chat_knowledge, chat_reasoning, chat_sampling, chat_tool_write, chat_wire};
use crate::state::{AppState, SharedState};
use crate::store::{
    self, ChatFolder, ChatFolderPatch, ChatThread, CurrentSettings, FolderOptions, ThreadDefaults,
};

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

/// `fields` (a folder patch's `defaults_patch`) laid over `stored`: each
/// field given replaces the stored one, `null` unsetting it; `voice` field
/// by field, `voice: null` unsetting it whole. The whole defaults, for
/// [`parse_defaults`] — which refuses a field it does not know.
fn laid_over(
    stored: &ThreadDefaults,
    fields: serde_json::Value,
) -> Result<serde_json::Value, Response> {
    use serde_json::Value;
    let Value::Object(fields) = fields else {
        return Err(err_json(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "defaults_patch: an object of the defaults' fields to change",
        ));
    };
    let mut base = serde_json::to_value(stored).map_err(internal)?;
    for (field, value) in fields {
        if let (Value::Object(voice), "voice") = (&value, field.as_str()) {
            let mut now = base["voice"].as_object().cloned().unwrap_or_default();
            for (k, v) in voice {
                if v.is_null() {
                    now.remove(k);
                } else {
                    now.insert(k.clone(), v.clone());
                }
            }
            // A voice that sets nothing is no default.
            base["voice"] = if now.is_empty() {
                Value::Null
            } else {
                Value::Object(now)
            };
            continue;
        }
        base[field] = value;
    }
    Ok(base)
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

fn bad_request(msg: impl Into<String>) -> Response {
    err_json(StatusCode::BAD_REQUEST, "bad_request", msg)
}

/// A count of days or minutes a folder setting takes: a whole number of
/// zero or more. No upper bound: a count past what the clock can reach
/// means "never".
fn not_negative(field: &str, v: Option<i64>, what: &str) -> Result<(), Response> {
    match v {
        Some(n) if n < 0 => Err(bad_request(format!(
            "{field} must be 0 or more ({what}), not {n}"
        ))),
        _ => Ok(()),
    }
}

/// A folder's own retention is the owner's to set (review W5-1, decided by
/// the owner): the sweep applies it to every thread in the folder, the
/// Admin Chat and self-admin threads a device cannot see included, so a
/// device that could shorten it would have them deleted. `written` names
/// the fields the request changes; a value equal to the stored one is no
/// write (as for L5's labels).
fn retention_is_owners(caller: &Caller, written: &[&str]) -> Result<(), Response> {
    if !caller.is_device() || written.is_empty() {
        return Ok(());
    }
    Err(err_json(
        StatusCode::FORBIDDEN,
        "forbidden",
        format!(
            "a folder's own retention ({}) deletes threads {} may not see, so only the \
             dashboard or an admin key sets it",
            written.join(", "),
            caller.named()
        ),
    ))
}

/// A device that writes `devices_hidden` (review F-7): the owner's to change,
/// as a folder's own retention is.
fn devices_hidden_is_owners(caller: &Caller) -> Response {
    err_json(
        StatusCode::FORBIDDEN,
        "forbidden",
        format!(
            "devices_hidden is set by the dashboard or an admin key, not by {}",
            caller.named()
        ),
    )
}

/// The folder's own retention as given: each a number of days, 0 or more.
fn check_retention(archive_days: Option<i64>, purge_days: Option<i64>) -> Result<(), Response> {
    not_negative(
        "archive_days",
        archive_days,
        "days without activity before a thread is archived; 0 never archives, empty is the \
         global setting",
    )?;
    not_negative(
        "purge_days",
        purge_days,
        "days after archiving before a thread is deleted; 0 never deletes, empty is the \
         global setting",
    )
}

/// An ongoing conversation needs a model in its defaults (client-apps
/// design §3.1): lmgw has no default chat model, and two clients starting
/// the next thread with models of their own would disagree.
fn check_ongoing(idle_minutes: Option<i64>, defaults: &ThreadDefaults) -> Result<(), Response> {
    let Some(minutes) = idle_minutes else {
        return Ok(());
    };
    not_negative(
        "ongoing.idle_minutes",
        Some(minutes),
        "minutes without a message before a new thread starts; 0 starts one only when asked",
    )?;
    if defaults.model_alias.is_none() {
        return Err(bad_request(
            "an ongoing conversation needs a model: set defaults.model_alias",
        ));
    }
    Ok(())
}

pub(super) fn internal(e: impl std::fmt::Display) -> Response {
    err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
}

/// A folder write's error as the route answers it: a refusal made on the
/// write's transaction keeps its status and code (`400 unknown_profile`
/// for a profile deleted since the route's check); anything else is the
/// store's 500.
fn store_refusal(e: &crate::error::GatewayError) -> Response {
    err_json(e.http_status(), e.code(), e.to_string())
}

/// Folder `id` as `caller` may reach it (client-apps design L3, review
/// W3-1): for a device, a folder whose defaults attach the self-admin
/// toolset is not found, exactly as one that is not there. The refusal is
/// a ready response.
pub(super) async fn folder_as(
    state: &AppState,
    caller: &Caller,
    id: i64,
) -> Result<ChatFolder, Response> {
    match store::get_chat_folder(&state.db, id).await {
        Ok(Some(f)) if caller.sees_folder(&state.snapshot(), &f) => Ok(f),
        Ok(_) => Err(not_found()),
        Err(e) => Err(internal(e)),
    }
}

/// `GET /chat/api/folders` — `{folders: [...]}` in sidebar order, each with
/// its `defaults` and `threads_active` / `threads_archived` (a device's
/// counts leave out what it does not see, client-apps design L3).
pub async fn list_folders(State(state): State<SharedState>, caller: Caller) -> Response {
    match store::list_chat_folders(&state.db, caller.reach(&state.snapshot())).await {
        Ok(f) => Json(chat_wire::wire(&FolderList {
            folders: f.iter().map(chat_wire::folder).collect(),
        }))
        .into_response(),
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
    /// `{idle_minutes}`: the folder is one ongoing conversation.
    #[serde(default)]
    ongoing: Option<OngoingInput>,
    /// The folder's own retention; absent or `null` is the global setting.
    #[serde(default)]
    archive_days: Option<i64>,
    #[serde(default)]
    purge_days: Option<i64>,
}

/// `POST /chat/api/folders` `{name, defaults?, ongoing?, archive_days?,
/// purge_days?}` — the new folder, last in the list. A device's default
/// `mcp_tools` pass its tool scope (client-apps design L5).
pub async fn create_folder(
    State(state): State<SharedState>,
    caller: Caller,
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
    let opts = FolderOptions {
        ongoing_idle_minutes: req.ongoing.map(|o| o.idle_minutes),
        archive_days: req.archive_days,
        purge_days: req.purge_days,
    };
    let written: Vec<&str> = [
        ("archive_days", opts.archive_days),
        ("purge_days", opts.purge_days),
    ]
    .into_iter()
    .filter_map(|(field, v)| v.map(|_| field))
    .collect();
    if let Err(r) = retention_is_owners(&caller, &written)
        .and_then(|()| check_ongoing(opts.ongoing_idle_minutes, &defaults))
        .and_then(|()| check_retention(opts.archive_days, opts.purge_days))
        .and_then(|()| super::chat_profiles::check_profile(&state, defaults.profile_id))
    {
        return r;
    }
    // The device's reach before the bases' existence (review W3-5).
    if let Some(ids) = &defaults.kb_ids {
        if let Err(refused) = chat_tool_write::check_kbs(&state, &caller, ids, &[]).await {
            return refused;
        }
    }
    if let Err(msg) = chat_knowledge::check_default_kbs(&state, &defaults, &[]).await {
        return err_json(StatusCode::BAD_REQUEST, "bad_request", msg);
    }
    if let Some(written) = &defaults.mcp_tools {
        if let Err(refused) = chat_tool_write::check(&state, &caller, written, &[], &[], None).await
        {
            return refused;
        }
    }
    // The owner's defaults are the new folder's approval floor; a device's
    // folder has none (client-apps design §6.6).
    let floor = match &defaults.mcp_tools {
        Some(written) if !caller.is_device() => written.clone(),
        _ => Vec::new(),
    };
    if let Err(refused) = chat_tool_write::check_aliases(
        &state,
        &caller,
        &chat_tool_write::default_aliases(&defaults),
        &[],
    ) {
        return refused;
    }
    if let Some(v) = &defaults.voice {
        if let Err(msg) = super::chat_voice::check_voice_aliases(&state, v, None).await {
            return err_json(StatusCode::BAD_REQUEST, "bad_request", msg);
        }
    }
    let id = match store::create_chat_folder_with(
        &state.db,
        &name,
        &defaults,
        &opts,
        &floor,
        Some(&caller.named()),
    )
    .await
    {
        Ok(id) => id,
        Err(e) => return store_refusal(&e),
    };
    state.chat_feed.wake();
    match folder_json(&state, &caller, id).await {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

/// The folder as the list carries it, for `caller`.
async fn folder_json(
    state: &AppState,
    caller: &Caller,
    id: i64,
) -> Result<serde_json::Value, Response> {
    let snap = state.snapshot();
    match store::get_chat_folder_listed(&state.db, id, caller.reach(&snap)).await {
        Ok(Some(f)) if caller.sees_folder(&snap, &f.folder) => {
            Ok(chat_wire::wire(&chat_wire::folder(&f)))
        }
        Ok(_) => Err(not_found()),
        Err(e) => Err(internal(e)),
    }
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateFolderReq {
    name: Option<String>,
    sort: Option<i64>,
    /// Replaces the folder's defaults whole (an absent field is unchanged;
    /// `{}` clears them).
    defaults: Option<serde_json::Value>,
    /// Changes the defaults field by field instead (review W6-10): each
    /// field given replaces the stored one (`null` unsets it), the others
    /// stay as stored — so a save of the fields one client changed never
    /// writes back, and an ongoing folder never re-applies to its current
    /// thread, what another changed meanwhile. `voice` is laid field by
    /// field the same way (`voice: null` unsets the whole voice). Not beside
    /// `defaults`.
    defaults_patch: Option<serde_json::Value>,
    /// `{idle_minutes}` marks the folder as one ongoing conversation (or
    /// changes its idle minutes); `null` ends that. Absent: unchanged.
    #[serde(default, deserialize_with = "present")]
    ongoing: Option<Option<OngoingInput>>,
    /// The folder's own retention; `null` goes back to the global setting.
    /// Absent: unchanged.
    #[serde(default, deserialize_with = "present")]
    archive_days: Option<Option<i64>>,
    #[serde(default, deserialize_with = "present")]
    purge_days: Option<Option<i64>>,
    /// `false` shows a folder a device's delete hid from devices to them
    /// again (review F-7). The owner's alone, as the retention is; `true` is
    /// no request anyone makes — a device's delete is what hides a folder.
    devices_hidden: Option<bool>,
    /// For an ongoing folder: also apply the defaults' changes to its
    /// current thread (client-apps design L9). Default `true`.
    #[serde(default = "yes")]
    apply_to_current: bool,
}

/// `POST /chat/api/folders/{id}` `{name?, sort?, defaults?, ongoing?,
/// archive_days?, purge_days?, devices_hidden?, apply_to_current?}` — patch. The folder as
/// listed, with `applied`: what a defaults change applied to an ongoing
/// folder's current thread (`{thread_id, fields}`), or `null`. Other threads
/// in the folder are not touched. A device's default `mcp_tools` pass its
/// tool scope (client-apps design L5), and so does what reaches the current
/// thread; a field the thread refuses fails the whole patch.
pub async fn update_folder(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<UpdateFolderReq>,
) -> Response {
    // Read to write under the folder's lock: the folder as read (its
    // defaults, its current thread) does not change under the patch, and a
    // rollover beside it starts its thread from the defaults before or
    // after it (both of the patch's writes share one transaction; review
    // W5-19).
    let folder_lock = state.chat_folder_locks.lock(id).await;
    let folder = match folder_as(&state, &caller, id).await {
        Ok(f) => f,
        Err(r) => return r,
    };
    let stored = folder.defaults.clone();
    let written: Vec<&str> = [
        ("archive_days", req.archive_days, folder.archive_days),
        ("purge_days", req.purge_days, folder.purge_days),
    ]
    .into_iter()
    .filter_map(|(field, new, old)| new.filter(|n| *n != old).map(|_| field))
    .collect();
    if let Err(r) = retention_is_owners(&caller, &written) {
        return r;
    }
    // Showing a folder to devices again is the owner's, as its retention
    // is (review F-7): a device never sees such a folder, and is refused
    // for trying, never for restating `false`.
    let show_to_devices = match req.devices_hidden {
        Some(true) if !folder.devices_hidden => {
            if caller.is_device() {
                return devices_hidden_is_owners(&caller);
            }
            return err_json(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "devices_hidden: only false is taken, which shows the folder to devices again; \
                 a folder is hidden from devices by a device's delete",
            );
        }
        Some(false) if folder.devices_hidden => {
            if caller.is_device() {
                return devices_hidden_is_owners(&caller);
            }
            true
        }
        _ => false,
    };
    let mut patch = ChatFolderPatch {
        show_to_devices,
        sort: req.sort,
        ongoing_idle_minutes: req.ongoing.clone().map(|o| o.map(|o| o.idle_minutes)),
        archive_days: req.archive_days,
        purge_days: req.purge_days,
        ..Default::default()
    };
    if let Some(n) = &req.name {
        match valid_name(n) {
            Ok(n) => patch.name = Some(n),
            Err(r) => return r,
        }
    }
    let defaults = match (req.defaults, req.defaults_patch) {
        (Some(_), Some(_)) => {
            return err_json(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "defaults and defaults_patch cannot both be given: defaults replaces them \
                 whole, defaults_patch changes the fields it names",
            )
        }
        (Some(whole), None) => Some(whole),
        (None, Some(fields)) => match laid_over(&stored, fields) {
            Ok(merged) => Some(merged),
            Err(r) => return r,
        },
        (None, None) => None,
    };
    match defaults.map(parse_defaults).transpose() {
        Ok(d) => patch.defaults = d,
        Err(r) => return r,
    }
    // A folder whose defaults carry lmgw's admin tools: they start the
    // owner's threads there, so a device below `full` does not change them
    // (L5's note, 2026-10-07); the same defaults sent back are no change
    // (the branch review's verification, V-12).
    if patch.defaults.as_ref().is_some_and(|d| *d != stored) {
        if let Some(refused) =
            super::chat_steer::refusal(&state, &caller, folder.reach_level(), Change::Defaults)
                .await
        {
            return refused;
        }
    }
    if let Some(d) = &patch.defaults {
        // The profile, when it changed (a delete strips a gone one from
        // every folder's defaults).
        if d.profile_id != stored.profile_id {
            if let Err(r) = super::chat_profiles::check_profile(&state, d.profile_id) {
                return r;
            }
        }
        // Bases the folder already named are not checked again (a base
        // deleted since must not block renaming the folder).
        let already = stored.kb_ids.clone().unwrap_or_default();
        // The device's reach before the bases' existence (review W3-5).
        if let Some(ids) = &d.kb_ids {
            if let Err(refused) = chat_tool_write::check_kbs(&state, &caller, ids, &already).await {
                return refused;
            }
        }
        if let Err(msg) = chat_knowledge::check_default_kbs(&state, d, &already).await {
            return err_json(StatusCode::BAD_REQUEST, "bad_request", msg);
        }
        let before = stored.mcp_tools.as_deref().unwrap_or_default();
        if let Some(written) = &d.mcp_tools {
            if let Err(refused) = chat_tool_write::check(
                &state,
                &caller,
                written,
                before,
                &folder.approval_floor,
                None,
            )
            .await
            {
                return refused;
            }
        }
        // The owner's defaults are the folder's approval floor from now on
        // (client-apps design §6.6); a device's never move it.
        if !caller.is_device() {
            patch.approval_floor = Some(chat_tool_write::owner_floor(
                &state,
                d.mcp_tools.as_deref().unwrap_or_default(),
                before,
                &folder.approval_floor,
            ));
        }
        if let Err(refused) = chat_tool_write::check_aliases(
            &state,
            &caller,
            &chat_tool_write::default_aliases(d),
            &chat_tool_write::default_aliases(&stored),
        ) {
            return refused;
        }
        // Voice aliases the folder already named are not checked again
        // either.
        if let Some(v) = &d.voice {
            let before = stored.voice.clone().unwrap_or_default();
            if let Err(msg) = super::chat_voice::check_voice_aliases(&state, v, Some(&before)).await
            {
                return err_json(StatusCode::BAD_REQUEST, "bad_request", msg);
            }
        }
    }
    let ongoing_after = match patch.ongoing_idle_minutes {
        Some(v) => v,
        None => folder.ongoing.as_ref().map(|o| o.idle_minutes),
    };
    if let Err(r) = check_ongoing(ongoing_after, patch.defaults.as_ref().unwrap_or(&stored))
        .and_then(|()| check_retention(patch.archive_days.flatten(), patch.purge_days.flatten()))
    {
        return r;
    }
    // L9: the changed defaults reach the current thread, checked as its
    // settings route checks them — first without the thread's lock (review
    // W6-13: a device's checks may take the lazy-list budget), then read
    // again and written under it (review W5-2), taken after the folder's
    // (the order everywhere: folder, thread, database).
    let applies = req.apply_to_current && ongoing_after.is_some() && patch.defaults.is_some();
    let (applied, hold) = match &patch.defaults {
        Some(new) if applies => match apply::to_current_held(&state, &caller, &folder, new).await {
            Ok(a) => a,
            Err(refused) => return refused,
        },
        _ => (None, None),
    };
    // The current thread's settings change as the settings route changes
    // them (`chat::write_flipping`): the toolset attached stops the thread's
    // live events reaching devices before the commit and takes the thread
    // from them after it — the close follows the commit — and taken off
    // gives it back after it, decided from the store's before and after
    // (reviews W4-3, W4-8); the thread's lock is held from the read across
    // the write. From the first step to the last on a task of its own, with
    // the locks (`chat_live::to_its_end`, the branch review's N-3).
    let admin = caller.reach(&state.snapshot());
    let by = caller.named();
    let task_state = state.clone();
    let done = super::chat_live::to_its_end(async move {
        let state = task_state;
        let _folder = folder_lock;
        let attaching = applied
            .as_ref()
            .filter(|a| a.thread.drives_self_admin())
            .map(|a| super::chat::Attaching::start(&state, ChatRepo::Db, &a.thread));
        let current = applied.as_ref().map(|a| CurrentSettings {
            thread: &a.thread,
            seed: a.seed,
            admin,
        });
        let written = store::update_chat_folder(&state.db, id, &patch, current, Some(&by)).await;
        // A write that did not reach the thread after the pre-commit step
        // gives it back its real flag (review W5-7); both under the
        // thread's lock (review W6-14), so another attaching write's
        // pre-commit step, made once it is free, is never undone by this
        // one's.
        let reached = matches!(&written, Ok(w) if w.current.is_some());
        if let Some(a) = applied.as_ref().filter(|_| attaching.is_some() && !reached) {
            super::chat::restore_flag(&state, ChatRepo::Db, a.thread.id).await;
        }
        if let Ok(w) = &written {
            if let Some(c) = &w.current {
                if attaching.is_some() || c.level_before != c.level_after {
                    state.chat_live.self_admin_changed(
                        &state.snapshot(),
                        c.thread_id,
                        c.level_after,
                    );
                }
            }
        }
        if let Some(a) = attaching {
            a.done();
        }
        drop(hold);
        if matches!(&written, Ok(w) if w.found) {
            state.chat_feed.wake();
        }
        (written, applied)
    })
    .await;
    let (written, applied) = match done {
        Ok(done) => done,
        Err(e) => return internal(e),
    };
    let written = match written {
        Ok(w) if w.found => w,
        Ok(_) => return not_found(),
        Err(e) => return store_refusal(&e),
    };
    if show_to_devices {
        tracing::info!("chat: folder {id} is shown to devices again");
    }
    let applied = applied
        .filter(|_| written.current.is_some())
        .map(|a| a.report());
    match folder_json(&state, &caller, id).await {
        Ok(mut v) => {
            v["applied"] = json!(applied);
            Json(v).into_response()
        }
        Err(r) => r,
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
/// `{ok: true}`.
///
/// A device's delete touches only the threads it sees (client-apps design
/// L3). A folder that also holds the owner's Admin Chat or self-admin
/// threads stays for them, its retention unchanged, and is gone for every
/// device from then on; the device's answer is the same `{ok: true}`, and
/// says nothing of them (review W6-1, decided by the owner).
pub async fn delete_folder(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<DeleteFolderReq>,
) -> Response {
    // No thread starts in it while it goes.
    let folder_lock = state.chat_folder_locks.lock(id).await;
    if let Err(r) = folder_as(&state, &caller, id).await {
        return r;
    }
    let delete = req.threads == ThreadsFate::Delete;
    let by = caller.named();
    // The threads going with it go as a thread's own delete makes them go
    // (review W4-22): their locks are held across the delete, and only their
    // live events leave every device's reach before it; once it committed a
    // turn still running in one is cancelled, a session bound to one says
    // its thread went (`voice.ended`, `thread_gone`), and a device's closes.
    // The threads are the ones the delete took (a move beside it may have
    // taken one in or out since the read): one it did not take goes back,
    // one taken without a lock of ours goes as a purged one does. From the
    // first step to the last on a task of its own, with the locks
    // (`chat_live::to_its_end`, the branch review's N-3).
    let reach = caller.reach(&state.snapshot());
    let deleted = super::chat_live::to_its_end(async move {
        let _folder = folder_lock;
        let mut going = Vec::new();
        if delete {
            let ids = store::chat_folder_thread_ids(&state.db, id, reach).await?;
            for thread in ids {
                going.push((thread, state.chat_live.discard(thread).await));
            }
        }
        let deleted = store::delete_chat_folder_ids(&state.db, id, delete, reach, Some(&by)).await;
        let taken: Vec<i64> = match &deleted {
            Ok(Some((_, taken))) => taken.clone(),
            _ => Vec::new(),
        };
        for (thread, held) in &mut going {
            if taken.contains(thread) {
                state.chat_live.discarded(held);
            } else {
                state.chat_live.delete_failed(held);
            }
        }
        state.chat_feed.threads_deleted(&taken);
        if !taken.is_empty() {
            super::chat_tasks::threads_gone(&state);
        }
        let unlocked: Vec<i64> = taken
            .into_iter()
            .filter(|t| !going.iter().any(|(g, _)| g == t))
            .collect();
        drop(going);
        state.chat_live.purged(&unlocked).await;
        let deleted = deleted.map(|d| d.map(|(how, _)| how));
        if let Ok(Some(how)) = &deleted {
            state.chat_feed.wake();
            if *how == store::FolderDeleted::HiddenFromDevices {
                tracing::info!(
                    "chat folder {id}: {by} deleted it with its own threads; it stays for the \
                     threads out of the device's reach, hidden from devices"
                );
            }
        }
        deleted
    })
    .await;
    match deleted.and_then(|d| d) {
        Ok(Some(_)) => Json(json!({ "ok": true })).into_response(),
        Ok(None) => not_found(),
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
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<MoveReq>,
) -> Response {
    let repo = ChatRepo::of(id);
    // Reach checked, then again under the thread's lock held across the
    // write (review W6-6).
    let held = match super::chat::reach_held(&state, &caller, id).await {
        Ok(held) => held,
        Err(r) => return r,
    };
    if repo.is_temp() {
        return err_json(
            StatusCode::CONFLICT,
            "temporary_thread",
            "a temporary chat cannot be moved into a folder — Keep it first",
        );
    }
    if let Some(f) = req.folder_id {
        if let Err(r) = folder_as(&state, &caller, f).await {
            return r;
        }
    }
    if let Err(e) =
        store::set_chat_thread_folder(&state.db, id, req.folder_id, Some(&caller.named())).await
    {
        return internal(e);
    }
    drop(held);
    state.chat_feed.wake();
    // As the caller reads it, like pin and archive (reviews W5-5, W6-6).
    super::chat::respond_with_thread(&state, &caller, id).await
}

/// A new stored thread in folder `folder_id`: the global start (`model_alias`,
/// the default system prompt) with the folder's defaults over it, as the
/// thread's own copy. Admin Chat threads keep their built-in setup: they
/// join the folder but take none of its defaults. In an ongoing folder a
/// chat thread becomes the current thread (client-apps design §3.3). Errors
/// are ready responses.
pub(super) async fn create_in_folder(
    state: &SharedState,
    folder_id: i64,
    model_alias: &str,
    kind: &str,
    by: &Caller,
) -> Result<ChatThread, Response> {
    current::create_by_hand(state, folder_id, model_alias, kind, by).await
}
