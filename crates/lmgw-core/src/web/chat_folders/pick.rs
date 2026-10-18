//! An ongoing folder's current thread, decided under the folder's lock
//! ([`FolderLocks`]) by §3.3's rules (client-apps design), typed: the one
//! path `POST /chat/api/folders/{id}/current` answers by and a realtime
//! session binds a folder by (`?chat_folder=`, realtime calls design §2.2).
//!
//! [`pick`] takes the lock and decides: keep the current thread, or start a
//! new one from the folder's defaults — when there is none (`first`), when it
//! is out of the caller's reach (`gone`), when it idled past the folder's idle
//! minutes (`idle`; a thread a turn answers now is never idle), or on `new`
//! unless it has no message (`requested`). Nothing is written by the
//! decision: a new thread is made only by [`Picked::create`], which records
//! `folder.current` on the feed. The lock is held until the [`Picked`]
//! drops, so a caller that binds the thread it picked does so before any
//! other client's `current` moves the folder.
//!
//! [`FolderLocks`]: super::FolderLocks

use lmgw_api_types::chat_folders::CurrentReason;
use tokio::sync::OwnedMutexGuard;

use super::super::chat_caller::Caller;
use super::current::thread_from;
use crate::state::AppState;
use crate::store::{self, ChatFolder, ChatThread, FolderOngoing};

/// Why a folder gives no current thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A folder the caller cannot see (404).
    NotFound,
    /// Not an ongoing conversation (409 `not_ongoing`): the sentence.
    NotOngoing(String),
    /// Its defaults name no model (409 `folder_no_model`): the sentence.
    NoModel(String),
    /// The store failed: what it said.
    Internal(String),
}

/// What the decision picked.
#[derive(Debug)]
pub(crate) enum Pick {
    /// The current thread, kept.
    Keep(Box<ChatThread>),
    /// A new thread, for `reason`: `thread` is what the folder's defaults
    /// make, not created yet ([`Picked::create`]).
    New {
        reason: CurrentReason,
        thread: Box<ChatThread>,
    },
}

/// A folder's decision, its lock held until this drops.
pub(crate) struct Picked {
    held: OwnedMutexGuard<()>,
    id: i64,
    new: bool,
    folder: ChatFolder,
    ongoing: FolderOngoing,
    pick: Pick,
}

/// Take folder `id`'s lock and decide its current thread for `caller`
/// (module doc); `new`: a new conversation is asked for.
pub(crate) async fn pick(
    state: &AppState,
    caller: &Caller,
    id: i64,
    new: bool,
) -> Result<Picked, Refusal> {
    let held = state.chat_folder_locks.lock(id).await;
    decide_held(state, caller, held, id, new).await
}

impl Picked {
    /// What was picked.
    pub(crate) fn pick(&self) -> &Pick {
        &self.pick
    }

    /// The folder as the decision read it.
    pub(crate) fn folder(&self) -> &ChatFolder {
        &self.folder
    }

    /// The folder's idle minutes, as the decision read them.
    pub(crate) fn idle_minutes(&self) -> i64 {
        self.ongoing.idle_minutes
    }

    /// Act on the decision, still under the lock: the thread the folder's
    /// current thread is now, by id. [`Pick::New`] makes its thread, which
    /// becomes the folder's current thread, `folder.current` recording the
    /// reason and who asked, and the feed woken. [`Pick::Keep`] makes
    /// nothing: the folder is read again, and its thread is still current.
    /// `Ok(None)` when the folder's current thread moved since the decision
    /// — a delete, a move or an archive, which take no lock and clear the
    /// pointer — for either: decide [`again`](Self::again).
    pub(crate) async fn create(
        &self,
        state: &AppState,
        caller: &Caller,
    ) -> Result<Option<i64>, Refusal> {
        let (reason, thread) = match &self.pick {
            Pick::Keep(t) => {
                let now = store::get_chat_folder(&state.db, self.id)
                    .await
                    .map_err(|e| Refusal::Internal(e.to_string()))?;
                let current = now.and_then(|f| f.current_thread_id());
                return Ok((current == Some(t.id)).then_some(t.id));
            }
            Pick::New { reason, thread } => (reason, thread),
        };
        let by = caller.named();
        let created = store::create_current_thread(
            &state.db,
            self.id,
            thread,
            self.ongoing.current_thread_id,
            reason.as_str(),
            Some(&by),
        )
        .await
        .map_err(|e| Refusal::Internal(e.to_string()))?;
        if created.is_some() {
            state.chat_feed.wake();
        }
        Ok(created)
    }

    /// Decide again on the folder as it is now, under the same lock.
    pub(crate) async fn again(self, state: &AppState, caller: &Caller) -> Result<Self, Refusal> {
        decide_held(state, caller, self.held, self.id, self.new).await
    }
}

/// [`pick`] under the lock already held.
async fn decide_held(
    state: &AppState,
    caller: &Caller,
    held: OwnedMutexGuard<()>,
    id: i64,
    new: bool,
) -> Result<Picked, Refusal> {
    let folder = match store::get_chat_folder(&state.db, id).await {
        Ok(Some(f)) if caller.sees_folder(&state.snapshot(), &f) => f,
        Ok(_) => return Err(Refusal::NotFound),
        Err(e) => return Err(Refusal::Internal(e.to_string())),
    };
    let Some(ongoing) = folder.ongoing.clone() else {
        return Err(Refusal::NotOngoing(format!(
            "folder '{}' is not an ongoing conversation — mark it one in its settings",
            folder.name
        )));
    };
    let Some(model) = folder.defaults.model_alias.clone() else {
        return Err(Refusal::NoModel(format!(
            "folder '{}' names no model: set the folder's model in its defaults",
            folder.name
        )));
    };
    let pick = match decide(state, caller, &folder, &ongoing, new).await? {
        Decision::Keep(t) => Pick::Keep(t),
        Decision::New(reason) => Pick::New {
            reason,
            thread: Box::new(thread_from(state, &folder, &model, "chat")),
        },
    };
    Ok(Picked {
        held,
        id,
        new,
        folder,
        ongoing,
        pick,
    })
}

/// What the rules make of the folder's current thread.
enum Decision {
    /// Answer this thread.
    Keep(Box<ChatThread>),
    /// Start a new one, for this reason.
    New(CurrentReason),
}

/// §3.3's rules, on `folder`'s current thread as `caller` sees it.
async fn decide(
    state: &AppState,
    caller: &Caller,
    folder: &ChatFolder,
    ongoing: &FolderOngoing,
    new: bool,
) -> Result<Decision, Refusal> {
    let internal = |e: crate::error::GatewayError| Refusal::Internal(e.to_string());
    let Some(id) = ongoing.current_thread_id else {
        return Ok(Decision::New(CurrentReason::First));
    };
    let current = store::get_chat_thread(&state.db, id)
        .await
        .map_err(internal)?;
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
