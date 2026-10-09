//! An ongoing folder's current thread (client-apps design §3.2, §3.3, L8):
//! `POST /chat/api/folders/{id}/current`, and the thread created in such a
//! folder by hand.
//!
//! **One per folder, under a lock.** Every write that reads a folder's
//! current thread and then moves it — this route, a thread created in the
//! folder, the folder's patch and its delete — holds the folder's lock
//! ([`FolderLocks`]) from the read to the write, so two clients asking at
//! once (two devices, a device and the dashboard) get one thread, not two.
//! The writes that only end a current thread (a delete, a move out, an
//! archive) take no lock: the rollover's write is a compare-and-set on the
//! pointer (`store::create_current_thread`), and one that lost to them is
//! decided again on what is there now.
//!
//! **L3.** A folder that does not exist for the caller (a device, and
//! defaults that attach the self-admin toolset) is a 404 as anywhere else.
//! A current thread out of a device's reach (the toolset attached to it
//! since) is `gone` for the device: it gets a new thread, made from the
//! folder's defaults, which do not attach the toolset — it could not see
//! the folder otherwise. The new thread is always `kind: "chat"`, and
//! attributed to whoever asked.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::chat_folders::{CurrentReason, CurrentRequest, CurrentThread};

use super::super::chat::err_json;
use super::super::chat_caller::Caller;
use super::super::chat_extract::{ChatOptJson, ChatPath};
use super::super::{chat_turn, chat_wire};
use super::{folder_as, internal};
use crate::state::{AppState, SharedState};
use crate::store::{self, ChatFolder, ChatThread, FolderOngoing};

/// One async lock per folder id, made on first use and dropped with its last
/// holder or waiter.
#[derive(Default)]
pub(crate) struct FolderLocks(Mutex<HashMap<i64, Weak<tokio::sync::Mutex<()>>>>);

impl FolderLocks {
    /// Folder `id`'s lock, held until the guard drops.
    pub(crate) async fn lock(&self, id: i64) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
            map.retain(|_, w| w.strong_count() > 0);
            match map.get(&id).and_then(Weak::upgrade) {
                Some(l) => l,
                None => {
                    let l = Arc::new(tokio::sync::Mutex::new(()));
                    map.insert(id, Arc::downgrade(&l));
                    l
                }
            }
        };
        lock.lock_owned().await
    }
}

/// What a `current` call does with the folder as it is now.
enum Decision {
    /// Answer this thread.
    Keep(Box<ChatThread>),
    /// Start a new one, for this reason.
    New(CurrentReason),
}

/// The thread a new conversation in `folder` starts as: the default system
/// prompt and profile with the folder's defaults over them, the folder's
/// model, a chat thread in the folder.
pub(super) fn thread_from(
    state: &AppState,
    folder: &ChatFolder,
    model_alias: &str,
    kind: &str,
) -> ChatThread {
    let mut t = new_thread(&state.snapshot(), &folder.defaults, kind, model_alias);
    t.folder_id = Some(folder.id);
    // The owner's floor for the folder's tools is the thread's to start
    // with (client-apps design §6.6), whoever makes it.
    t.approval_floor = folder.approval_floor.clone();
    t
}

/// What a new thread of `kind` starts with before any folder: a chat
/// thread Settings → Chat's default prompt and profile (`chat_profile`),
/// an Admin Chat thread neither (personality-profiles design §3.1: a
/// profile as the prompt). The create route, a folder's new thread and the
/// comparison a folder's change is applied to its current thread by
/// (`apply::delta`) all start here, so the three agree (profiles review
/// fix 3).
pub(in crate::web) fn thread_start(
    snap: &crate::config::Snapshot,
    kind: &str,
) -> (String, Option<i64>) {
    if kind == "chat" {
        (
            snap.settings.default_chat_prompt().to_string(),
            super::super::chat_profiles::default_profile(snap),
        )
    } else {
        (String::new(), None)
    }
}

/// A new thread of `kind` on `model_alias` in a folder with defaults `d`:
/// [`thread_start`], then the defaults over it (the folder's model wins) —
/// a chat thread's only; an Admin Chat thread takes none of them.
pub(super) fn new_thread(
    snap: &crate::config::Snapshot,
    d: &crate::store::ThreadDefaults,
    kind: &str,
    model_alias: &str,
) -> ChatThread {
    let (system_prompt, profile_id) = thread_start(snap, kind);
    let mut t = ChatThread {
        model_alias: model_alias.to_string(),
        system_prompt,
        profile_id,
        kind: kind.to_string(),
        ..Default::default()
    };
    if kind == "chat" {
        d.apply(&mut t);
    }
    t
}

/// §3.3's rules, on `folder`'s current thread as `caller` sees it.
async fn decide(
    state: &AppState,
    caller: &Caller,
    folder: &ChatFolder,
    ongoing: &FolderOngoing,
    new: bool,
) -> Result<Decision, Response> {
    let Some(id) = ongoing.current_thread_id else {
        return Ok(Decision::New(CurrentReason::First));
    };
    let current = match store::get_chat_thread(&state.db, id).await {
        Ok(t) => t,
        Err(e) => return Err(internal(e)),
    };
    // A current thread is always a chat thread in its folder, not archived
    // (the writes that change that clear it); the check is the backstop.
    // One out of the caller's reach is gone for it.
    let Some(t) = current.filter(|t| {
        t.kind == "chat"
            && t.folder_id == Some(folder.id)
            && t.archived_at.is_none()
            && caller.sees(&state.snapshot(), t)
    }) else {
        return Ok(Decision::New(CurrentReason::Gone));
    };
    // A thread a turn answers now is never idle, however long ago its
    // newest message was written (review W5-11): a long tool loop's reply
    // would otherwise land in a thread the conversation had left.
    let minutes = if state.chat_live.running(t.id) {
        0
    } else {
        ongoing.idle_minutes
    };
    let idle = store::thread_idle(&state.db, t.id, minutes)
        .await
        .map_err(internal)?;
    Ok(match (new, idle) {
        // An empty thread is reused, even when a new one is asked for.
        (true, None) => Decision::Keep(Box::new(t)),
        (true, Some(_)) => Decision::New(CurrentReason::Requested),
        (false, Some(true)) => Decision::New(CurrentReason::Idle),
        (false, _) => Decision::Keep(Box::new(t)),
    })
}

/// The sentence that says why a new thread started, naming the setting
/// behind it.
fn note(reason: CurrentReason, folder: &ChatFolder, idle_minutes: i64) -> String {
    match reason {
        CurrentReason::First => format!("folder '{}' had no current thread", folder.name),
        CurrentReason::Gone => format!(
            "the current thread of folder '{}' can no longer be continued in it",
            folder.name
        ),
        CurrentReason::Idle => format!(
            "the current thread had no message for {idle_minutes} minute(s): folder '{}' starts \
             a new thread after that (its settings: Ongoing conversation → idle minutes)",
            folder.name
        ),
        CurrentReason::Requested => "a new thread was asked for".to_string(),
        // The gateway decides only the reasons above; a client reads the
        // fallback.
        _ => format!("folder '{}' started a new thread", folder.name),
    }
}

/// The thread as `GET /chat/api/threads/{id}` answers its `thread`.
async fn thread_object(state: &SharedState, t: &ChatThread) -> lmgw_api_types::chat::Thread {
    let mut thread = chat_wire::thread(state, t).await;
    let last = store::last_chat_message(&state.db, t.id)
        .await
        .ok()
        .flatten();
    thread.continue_state = Some(chat_turn::continue_state(
        &state.snapshot(),
        t,
        last.as_ref(),
    ));
    thread
}

/// `POST /chat/api/folders/{id}/current` `{new?: bool}` — the folder's
/// current thread, `{thread, rolled_over, reason, note}`: a new one when it
/// has none, when its thread is out of the caller's reach, when its thread
/// was idle past the folder's idle minutes, or on `new: true` (unless the
/// current thread has no message yet). A folder that does not exist for
/// the caller is a 404, one that is not ongoing a `409 not_ongoing`, one
/// whose defaults name no model a `409 folder_no_model`.
pub async fn current(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
    ChatOptJson(req): ChatOptJson<CurrentRequest>,
) -> Response {
    // Held from the read to the write, and no longer (review W5-9): the
    // answer is built after it, so another client's `current` of the folder
    // does not wait for this one's voice resolution.
    let held = state.chat_folder_locks.lock(id).await;
    loop {
        let folder = match folder_as(&state, &caller, id).await {
            Ok(f) => f,
            Err(r) => return r,
        };
        let Some(ongoing) = folder.ongoing.clone() else {
            return err_json(
                StatusCode::CONFLICT,
                "not_ongoing",
                format!(
                    "folder '{}' is not an ongoing conversation — mark it one in its settings",
                    folder.name
                ),
            );
        };
        let Some(model) = folder.defaults.model_alias.clone() else {
            return err_json(
                StatusCode::CONFLICT,
                "folder_no_model",
                format!(
                    "folder '{}' names no model: set the folder's model in its defaults",
                    folder.name
                ),
            );
        };
        let reason = match decide(&state, &caller, &folder, &ongoing, req.new).await {
            Ok(Decision::Keep(t)) => {
                drop(held);
                return Json(CurrentThread {
                    thread: thread_object(&state, &t).await,
                    rolled_over: false,
                    reason: None,
                    note: None,
                })
                .into_response();
            }
            Ok(Decision::New(reason)) => reason,
            Err(r) => return r,
        };
        let t = thread_from(&state, &folder, &model, "chat");
        let by = caller.named();
        let created = store::create_current_thread(
            &state.db,
            id,
            &t,
            ongoing.current_thread_id,
            reason.as_str(),
            Some(&by),
        )
        .await;
        let new_id = match created {
            Ok(Some(new_id)) => new_id,
            // The current thread moved meanwhile (a delete, a move, an
            // archive took it): decide again on what is there now.
            Ok(None) => continue,
            Err(e) => return internal(e),
        };
        drop(held);
        state.chat_feed.wake();
        let Ok(Some(t)) = store::get_chat_thread(&state.db, new_id).await else {
            return internal("the thread vanished immediately after being created");
        };
        // A device is never told why its conversation's thread went out of
        // its reach (review W5-3): for it the folder had no current thread,
        // as its folder JSON already says. The record keeps `gone`.
        let told = match reason {
            CurrentReason::Gone if caller.is_device() => CurrentReason::First,
            r => r,
        };
        return Json(CurrentThread {
            thread: thread_object(&state, &t).await,
            rolled_over: true,
            reason: Some(told),
            note: Some(note(told, &folder, ongoing.idle_minutes)),
        })
        .into_response();
    }
}

/// A thread created by hand in folder `folder_id` (`POST /chat/api/threads
/// {folder_id}`), under the folder's lock: in an ongoing folder a chat
/// thread becomes its current thread (`folder.current`, reason
/// `requested`); an Admin Chat thread does not. The thread as created.
pub(super) async fn create_by_hand(
    state: &SharedState,
    folder_id: i64,
    model_alias: &str,
    kind: &str,
    by: &Caller,
) -> Result<ChatThread, Response> {
    let _held = state.chat_folder_locks.lock(folder_id).await;
    let named = by.named();
    let id = loop {
        let folder = folder_as(state, by, folder_id).await?;
        // A device's model is within its alias scope, unless the folder's
        // defaults name the model, which the thread then takes (review
        // W3-4) — decided under the folder's lock, from the folder as it is
        // (review W5-12), after its 404 for a folder the device cannot see
        // (review W4-16).
        if folder.defaults.model_alias.is_none() {
            super::super::chat_tool_write::check_aliases(state, by, &[Some(model_alias)], &[])?;
        }
        let t = thread_from(state, &folder, model_alias, kind);
        if kind != "chat" || folder.ongoing.is_none() {
            break store::create_chat_thread_from(&state.db, &t, Some(&named))
                .await
                .map_err(internal)?;
        }
        match store::create_current_thread(
            &state.db,
            folder_id,
            &t,
            folder.current_thread_id(),
            CurrentReason::Requested.as_str(),
            Some(&named),
        )
        .await
        {
            Ok(Some(id)) => break id,
            Ok(None) => continue,
            Err(e) => return Err(internal(e)),
        }
    };
    state.chat_feed.wake();
    match store::get_chat_thread(&state.db, id).await {
        Ok(Some(t)) => Ok(t),
        _ => Err(internal(
            "the thread vanished immediately after being created",
        )),
    }
}
