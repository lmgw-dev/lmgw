//! A defaults change on an ongoing folder reaches its current thread
//! (client-apps design L9, §3.4; the answer to §11 Q3: by default, with an
//! opt-out per save).
//!
//! Folder defaults stay a copy taken at creation. For an ongoing folder the
//! current thread *is* the conversation, so a patch of its defaults also
//! applies the **changed** fields to that thread: each field whose value a
//! new thread in the folder would start with differs between the old
//! defaults and the new ones takes the new value — a field set back to
//! unset takes what a new thread starts with without it. A field the
//! defaults did not change keeps what the thread holds, a hand-made change
//! included. The voice is compared field by field, and the thread keeps its
//! seed.
//!
//! The changes go through the thread settings route's own checks
//! ([`apply_settings_patch`]), so the current thread refuses what that route
//! refuses, a device's L5 and alias checks included, and the change of a
//! current thread with lmgw's admin tools by a device below `full`
//! (`chat_steer`, the branch review's verification V-3); a refusal fails the
//! whole patch.

use axum::response::Response;
use lmgw_api_types::chat_folders::AppliedToCurrent;
use serde_json::{Map, Value};

use super::super::chat::{apply_settings_patch, SettingsReq};
use super::super::chat_caller::Caller;
use super::super::chat_steer::Change;
use super::internal;
use crate::state::SharedState;
use crate::store::{self, ChatFolder, ChatThread, SeedWrite, ThreadDefaults};

/// The current thread with the changes laid over it, ready to be written
/// with the folder's patch.
pub(super) struct Applied {
    pub thread: ChatThread,
    pub seed: SeedWrite,
    pub fields: Vec<String>,
    /// The thread as it was read, before the patch: what a read under the
    /// lock is compared with ([`to_current_held`]).
    pub read: ChatThread,
}

impl Applied {
    pub(super) fn report(&self) -> AppliedToCurrent {
        AppliedToCurrent {
            thread_id: self.thread.id,
            fields: self.fields.clone(),
        }
    }
}

/// What a new thread of `kind` in a folder with `d` starts as, for the
/// comparison: the very start a folder's new thread takes
/// (`current::new_thread`), so the current thread is brought where a new
/// one of its kind would be — an Admin Chat thread never to Settings →
/// Chat's profile, which its create never gives it (profiles review fix 3).
fn fresh(state: &SharedState, d: &ThreadDefaults, kind: &str) -> ChatThread {
    super::current::new_thread(&state.snapshot(), d, kind, "")
}

/// The settings patch that brings `current` from `old`'s defaults to
/// `new`'s, and the fields it names (`voice.<field>` for the voice).
pub(super) fn delta(
    state: &SharedState,
    old: &ThreadDefaults,
    new: &ThreadDefaults,
    current: &ChatThread,
) -> (Map<String, Value>, Vec<String>) {
    let as_map = |t: &ChatThread| match serde_json::to_value(t) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let (was, now) = (
        as_map(&fresh(state, old, &current.kind)),
        as_map(&fresh(state, new, &current.kind)),
    );
    let mut patch = Map::new();
    let mut fields = Vec::new();
    // Every default is a thread setting of the same name; the defaults'
    // own keys are the list, so a field added there is compared here too.
    let keys: Vec<String> = match serde_json::to_value(ThreadDefaults::default()) {
        Ok(Value::Object(m)) => m.into_iter().map(|(k, _)| k).collect(),
        _ => Vec::new(),
    };
    for key in keys.iter().filter(|k| *k != "voice") {
        let (a, b) = (was.get(key), now.get(key));
        // A folder whose defaults stop naming a model leaves the thread's.
        if a == b || (key == "model_alias" && b.and_then(Value::as_str) == Some("")) {
            continue;
        }
        patch.insert(key.clone(), b.cloned().unwrap_or(Value::Null));
        fields.push(key.clone());
    }
    let voice_of =
        |d: &ThreadDefaults| match serde_json::to_value(d.voice.clone().unwrap_or_default()) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        };
    let (vwas, vnow) = (voice_of(old), voice_of(new));
    let mut voice = match serde_json::to_value(&current.voice) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    // Without `seed` the route keeps the thread's own.
    voice.remove("seed");
    let mut voice_changed = false;
    let mut names: Vec<&String> = vwas.keys().chain(vnow.keys()).collect();
    names.sort();
    names.dedup();
    for name in names.into_iter().filter(|n| *n != "seed") {
        if vwas.get(name) == vnow.get(name) {
            continue;
        }
        match vnow.get(name) {
            Some(v) => voice.insert(name.clone(), v.clone()),
            None => voice.remove(name),
        };
        fields.push(format!("voice.{name}"));
        voice_changed = true;
    }
    if voice_changed {
        patch.insert("voice".into(), Value::Object(voice));
    }
    (patch, fields)
}

/// [`to_current`] as the folder patch writes it (reviews W5-2, W6-13):
/// checked first without the thread's lock, then the thread read again
/// under it — and the patch laid again when the thread changed in
/// between. The lock comes back with what to write, for the caller to hold
/// across its write; none when there is nothing to apply.
pub(super) async fn to_current_held(
    state: &SharedState,
    caller: &Caller,
    folder: &ChatFolder,
    new: &ThreadDefaults,
) -> Result<(Option<Applied>, Option<crate::web::chat_live::HistoryWrite>), Response> {
    let Some(first) = to_current(state, caller, folder, new).await? else {
        return Ok((None, None));
    };
    let hold = state.chat_live.hold(first.thread.id).await;
    let now = store::get_chat_thread(&state.db, first.thread.id)
        .await
        .map_err(internal)?;
    if now
        .as_ref()
        .is_some_and(|now| super::super::chat::same_thread(&first.read, now))
    {
        return Ok((Some(first), Some(hold)));
    }
    let again = to_current(state, caller, folder, new).await?;
    Ok((again, Some(hold)))
}

/// The changes `new` makes to `folder`'s defaults, laid over its current
/// thread with the settings route's checks: `Ok(None)` when there is
/// nothing to apply — no current thread, one out of `caller`'s reach (a
/// device's patch never touches it), or no field changed. A refusal is the
/// route's response, and fails the folder's patch.
pub(super) async fn to_current(
    state: &SharedState,
    caller: &Caller,
    folder: &ChatFolder,
    new: &ThreadDefaults,
) -> Result<Option<Applied>, Response> {
    let Some(id) = folder.current_thread_id() else {
        return Ok(None);
    };
    let Some(current) = store::get_chat_thread(&state.db, id)
        .await
        .map_err(internal)?
        .filter(|t| caller.sees(&state.snapshot(), t))
    else {
        return Ok(None);
    };
    let (patch, fields) = delta(state, &folder.defaults, new, &current);
    if fields.is_empty() {
        return Ok(None);
    }
    let req: SettingsReq = serde_json::from_value(Value::Object(patch)).map_err(internal)?;
    let read = current.clone();
    let mut thread = current;
    let seed = apply_settings_patch(state, caller, &mut thread, req, Change::CurrentThread).await?;
    Ok(Some(Applied {
        thread,
        seed,
        fields,
        read,
    }))
}
