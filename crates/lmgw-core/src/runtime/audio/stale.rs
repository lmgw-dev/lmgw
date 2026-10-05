//! A running audio container whose row now renders another `server.json`
//! than the one it mounts.
//!
//! `server.json` is a function of the row, the class settings and — since
//! the direct-file pick (`audio::files::direct`) — of how many GGUFs sit at
//! the top of the row's directory. An edit stops the container for apply;
//! a download into that directory, or a delete from it, changes the render
//! with no edit, and the container would go on mounting the old file until
//! its next start (its next model load then fails on the directory it was
//! handed, while every lmgw surface shows the fresh render). So after an
//! audio download and an audio delete, each such container that is up and
//! idle is stopped for apply as an edit would stop it — the comparison is
//! boot adoption's — and the next request starts it on the fresh render.
//!
//! A container that is still **starting** or is **serving** is not cut: a
//! stop would fail the request that started it, or the ones it serves. It is
//! left on the render it was started with, said in the log, and remembered
//! ([`LeftStale`]); the idle reaper's tick compares it again
//! ([`recheck_left`]) and stops it once it is up and idle — or forgets it,
//! when it went away or mounts the fresh render by then.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::config::Snapshot;
use crate::runtime::descriptor::model_runtime;
use crate::runtime::registry::{RuntimeError, RuntimeState, RuntimeView};
use crate::runtime::Class;
use crate::state::SharedState;

/// The audio containers [`stop_stale`] left running on a render that is no
/// longer their row's — model id → what changed the files, for the log.
/// Per gateway, on its state, like the rest of the runtime's bookkeeping.
#[derive(Debug, Default)]
pub struct LeftStale(Mutex<HashMap<String, String>>);

impl LeftStale {
    fn remember(&self, model_id: &str, why: &str) {
        self.lock().insert(model_id.to_string(), why.to_string());
    }

    fn take(&self) -> HashMap<String, String> {
        std::mem::take(&mut *self.lock())
    }

    /// The model ids waiting for their re-check.
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.lock().keys().cloned().collect();
        ids.sort();
        ids
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What [`settle`] did to one stale container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// Stopped: the next request starts it on the fresh render.
    Stopped,
    /// Left running on its old render: starting, or serving (the text says
    /// which, for the log). Remembered for the reaper's re-check.
    Left(String),
    /// Gone, or on its way out, by the time it was looked at.
    Gone,
    /// The stop failed; the registry has forgotten it either way.
    Failed(String),
}

/// After a download into or a delete from the audio models dir: stop every
/// running audio container whose mounted `server.json` is not what its row
/// renders now, if it is up and idle; leave and remember it otherwise (module
/// doc). `why` names what changed the files, for the log. Returns the model
/// ids it found stale, with what it did.
pub async fn stop_stale(state: &SharedState, why: &str) -> Vec<(String, Settled)> {
    let snap = state.snapshot();
    let mut out = Vec::new();
    for view in state
        .runtime()
        .list()
        .into_iter()
        .filter(|e| e.class == Class::Audio)
    {
        let id = view.model_id.clone();
        if !renders_otherwise(state, &snap, &id) {
            continue;
        }
        let settled = settle(state, &view).await;
        match &settled {
            Settled::Stopped => tracing::info!(
                "audio model '{id}': {why} changed the server.json it renders — its container \
                 was stopped, and the next request starts it on the fresh one"
            ),
            Settled::Left(doing) => {
                tracing::info!(
                    "audio model '{id}': {why} changed the server.json it renders while its \
                     container was {doing} — it keeps the one it was started with for now, and \
                     is stopped once it is idle"
                );
                state.audio_stale.remember(&id, why);
            }
            Settled::Gone => tracing::debug!(
                "audio model '{id}': {why} changed the server.json it renders; it stopped \
                 meanwhile"
            ),
            Settled::Failed(e) => tracing::warn!(
                "audio model '{id}': {why} changed the server.json it renders, and stopping its \
                 container failed: {e}"
            ),
        }
        out.push((id, settled));
    }
    out
}

/// The idle reaper's re-check of the containers [`stop_stale`] left: each
/// one still up on its old render is stopped once it is idle, and is left
/// for the next tick while it is still starting or serving. One that went
/// away, or that mounts what its row renders by now (a start since rendered
/// it fresh), is forgotten.
pub async fn recheck_left(state: &SharedState) {
    let left = state.audio_stale.take();
    if left.is_empty() {
        return;
    }
    let snap = state.snapshot();
    let views = state.runtime().list();
    for (id, why) in left {
        let Some(view) = views
            .iter()
            .find(|v| v.class == Class::Audio && v.model_id == id)
        else {
            continue;
        };
        if !renders_otherwise(state, &snap, &id) {
            continue;
        }
        match settle(state, view).await {
            Settled::Stopped => tracing::info!(
                "audio model '{id}': its container, now idle, still ran the server.json from \
                 before {why} — stopped, and the next request starts it on the fresh one"
            ),
            Settled::Left(_) => state.audio_stale.remember(&id, &why),
            Settled::Gone => {}
            Settled::Failed(e) => tracing::warn!(
                "audio model '{id}': stopping its container, which still ran the server.json \
                 from before {why}, failed: {e}"
            ),
        }
    }
}

/// The row of this running audio container renders another `server.json`
/// than the one the container mounts. `false` for a container no row
/// describes any more: deleting the row stops it by name.
fn renders_otherwise(state: &SharedState, snap: &Snapshot, model_id: &str) -> bool {
    let Some(rt) = model_runtime(snap, Class::Audio, model_id) else {
        return false;
    };
    let (Some(model), Some(settings)) = (&rt.audio, &rt.audio_settings) else {
        return false;
    };
    let fresh = super::render_single_model_config(settings, model);
    let mounted = std::fs::read_to_string(
        super::config_dir(&state.data_dir, model_id).join(super::CONFIG_FILE_NAME),
    )
    .ok();
    mounted.as_deref() != Some(fresh.as_str())
}

/// Stop the container `view` shows if it is up and idle — only that one: a
/// restart since is a container nobody judged.
async fn settle(state: &SharedState, view: &RuntimeView) -> Settled {
    match view.state {
        RuntimeState::Stopping => return Settled::Gone,
        RuntimeState::Starting => return Settled::Left("starting".into()),
        RuntimeState::Ready => {}
    }
    if view.in_flight > 0 {
        return Settled::Left(format!("serving {} request(s)", view.in_flight));
    }
    match state
        .runtime()
        .stop_generation(Class::Audio, &view.model_id, view.generation, false)
        .await
    {
        Ok(()) => Settled::Stopped,
        Err(RuntimeError::Busy { in_flight, .. }) => {
            Settled::Left(format!("serving {in_flight} request(s)"))
        }
        // Restarted since `view` was read: the new container may have
        // rendered the fresh file, which the re-check compares.
        Err(RuntimeError::Moved { .. }) => Settled::Left("being restarted".into()),
        Err(e) => Settled::Failed(e.to_string()),
    }
}
