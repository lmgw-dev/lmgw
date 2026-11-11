//! Which entry a start sequence runs for, and the start's line back to it
//! while the launcher prepares ([`StartWatch`], Windows design §4.2).
//!
//! A stop, the GPU hold's sweep or an eviction takes a `starting` entry by
//! marking it `stopping`; the launcher's stop then finds nothing to stop
//! while nothing is launched yet. So the start sequence asks before every
//! launch whether the entry is still its own, and again right after one —
//! a stop that marked the entry before the launch and asked the launcher
//! before the process was there is caught then — and ends `Aborted` without
//! loading anything outside the ledger. A prepare that waits or downloads
//! asks [`StartWatch::wanted`] as it goes.

use std::sync::Arc;

use tokio::sync::watch;

use super::*;
use crate::runtime::launcher::StartWatch;

/// The entry a start sequence runs for: its key, its lineage and the
/// generation its claim (or climb) took.
#[derive(Clone, Copy)]
pub(super) struct StartOf<'a> {
    pub(super) key: &'a Key,
    pub(super) phase: &'a Arc<watch::Sender<Phase>>,
    pub(super) generation: u64,
}

impl StartOf<'_> {
    /// `e` is this start's entry, still `starting`: not `stopping`, which a
    /// stop sets as soon as it takes the entry, well before it removes it.
    pub(super) fn owns(&self, e: &Entry) -> bool {
        Arc::ptr_eq(&e.phase, self.phase)
            && e.generation == self.generation
            && e.state == RuntimeState::Starting
    }
}

/// [`StartWatch`] over the registry's map for one start.
pub(super) struct EntryWatch<'a> {
    pub(super) reg: &'a Registry,
    pub(super) of: StartOf<'a>,
}

impl StartWatch for EntryWatch<'_> {
    fn wanted(&self) -> bool {
        self.reg.still_starting(self.of)
    }

    fn note(&self, note: String) {
        let mut map = self.reg.map();
        if let Some(e) = map.get_mut(self.of.key).filter(|e| self.of.owns(e)) {
            if !e.warnings.contains(&note) {
                e.warnings.push(note);
            }
        }
    }
}

impl Registry {
    /// The entry `of` names is still that start's (module doc).
    pub(super) fn still_starting(&self, of: StartOf<'_>) -> bool {
        self.map().get(of.key).is_some_and(|e| of.owns(e))
    }

    /// A start whose entry was taken after it launched `name`: the process
    /// is removed unless another entry holds the name by now (a later
    /// start's, an adoption), and the start is `Aborted`.
    pub(super) async fn abort_launched(&self, of: StartOf<'_>, name: &str) -> RuntimeError {
        let held_by_another = self
            .map()
            .values()
            .any(|e| e.container_name == name && !Arc::ptr_eq(&e.phase, of.phase));
        if !held_by_another {
            if let Err(e) = self.launcher.remove(name).await {
                tracing::warn!(container = %name, "removing what an aborted start launched: {e}");
            }
        }
        RuntimeError::Aborted {
            class: of.key.0,
            model_id: of.key.1.clone(),
        }
    }
}
