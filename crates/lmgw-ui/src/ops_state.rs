//! Per-target in-flight podman-op tracking (per-model-containers §8).
//!
//! Router mode had three containers, so one global `busy: Option<&'static
//! str>` slot (naming the one target — `chat`/`aux`/`audio` — currently under
//! a podman command) was enough, and it doubled as a *global* lock: any
//! button anywhere checked `busy.is_some()` before firing, so one class's
//! apply froze every other class's buttons too. Per-model containers make a
//! class N containers, and the Overview runtime table (§8) puts many models'
//! Start/Stop/Restart/Apply buttons on screen at once — a single slot would
//! mean one model's cold start disables every other model's row.
//!
//! This replaces it with a set of busy *keys*, so unrelated ops never block
//! each other and the same key blocks its own re-entry. `key` is either a
//! model id (a per-model op, see `model_key`) or `class:<name>` (a
//! whole-class group op, see `class_key`) — two different vocabularies that
//! by convention never collide, never by parsing.

use std::collections::HashSet;

use leptos::prelude::*;

#[derive(Clone, Copy)]
pub struct OpsState(RwSignal<HashSet<String>>);

pub fn provide_ops_state() {
    provide_context(OpsState(RwSignal::new(HashSet::new())));
}

pub fn use_ops() -> OpsState {
    expect_context::<OpsState>()
}

impl OpsState {
    /// Is `key` currently under an op? Reactive — call from a `move ||`.
    pub fn busy(&self, key: &str) -> bool {
        self.0.with(|s| s.contains(key))
    }

    /// Claim `key` for an in-flight op. `true` = claimed, go ahead; `false` =
    /// already busy, the caller must not fire a second command against it.
    pub fn start(&self, key: &str) -> bool {
        let mut claimed = false;
        self.0.update(|s| claimed = s.insert(key.to_string()));
        claimed
    }

    /// Release `key` once the op settles (success or failure alike).
    pub fn finish(&self, key: &str) {
        self.0.update(|s| {
            s.remove(key);
        });
    }
}

/// Busy key for a per-model op — a model id is unique only *within* its
/// class (chat and aux tables are independent), so the class comes along.
pub fn model_key(class: &str, model_id: &str) -> String {
    format!("{class}:{model_id}")
}

/// Busy key for a whole-class group op (Settings' "apply class settings").
pub fn class_key(class: &str) -> String {
    format!("class:{class}")
}
