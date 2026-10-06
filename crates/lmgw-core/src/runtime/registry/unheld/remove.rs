//! Taking an unheld container down: under a `stopping` entry for the name a
//! start of its model would use, as a task of the registry's (`owned.rs`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::watch;

use super::super::owned::Down;
use super::super::reconcile::PsRow;
use super::super::*;
use super::{labelled_key, PassLimits};
use crate::runtime::{container_name, Placement};

/// The part of `podman inspect <name>` the pass reads: whether the container
/// runs, and whose it is.
#[derive(Debug, Deserialize)]
struct InspectRow {
    #[serde(default, rename = "Config")]
    config: InspectConfig,
    #[serde(default, rename = "State")]
    state: Option<InspectState>,
}

#[derive(Debug, Default, Deserialize)]
struct InspectConfig {
    #[serde(default, rename = "Labels")]
    labels: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct InspectState {
    #[serde(default, rename = "Running")]
    running: bool,
}

/// What podman says about one name, as far as the pass needs it.
pub(super) struct Inspected {
    pub(super) running: bool,
    pub(super) labels: HashMap<String, String>,
}

impl Registry {
    /// `podman inspect` one name; `None` when podman cannot say (no such
    /// container, an unreadable answer).
    pub(super) async fn inspect_unheld(&self, name: &str) -> Option<Inspected> {
        let out = self
            .podman(&["inspect", "--format", "json", name])
            .await
            .ok()?;
        if !out.ok() {
            return None;
        }
        let rows: Vec<InspectRow> = serde_json::from_str(&out.stdout).ok()?;
        let row = rows.into_iter().next()?;
        Some(Inspected {
            running: row.state?.running,
            labels: row.config.labels.unwrap_or_default(),
        })
    }

    /// Remove one running container [`Registry::adopt`] refused. `Ok(false)`
    /// when it was left alone: its key was claimed meanwhile, or it is no
    /// longer running.
    pub(super) async fn remove_unheld(
        self: &Arc<Self>,
        container_prefix: &str,
        row: &PsRow,
        name: &str,
        limits: PassLimits,
    ) -> Result<bool, String> {
        // The listing is several awaits old by now (the adoption's inspect and
        // probe): a container that stopped meanwhile — a stop, a failed start
        // tidying up — keeps its logs.
        if !self.inspect_unheld(name).await.is_some_and(|i| i.running) {
            return Ok(false);
        }
        // Under a `stopping` entry for the key whose start would use this very
        // name. Any other name no start renders, so nothing can race its
        // removal, and it needs no entry.
        let Some(key) = labelled_key(&row.labels)
            .filter(|(c, m)| container_name(container_prefix, *c, m) == name)
        else {
            if self.holds_name(name) {
                return Ok(false);
            }
            return match self.remove_bounded(name, limits.stop_timeout).await {
                None => Ok(true),
                Some(e) => Err(e),
            };
        };
        let Some(phase) = self.claim_for_removal(&key, name, limits.stop_timeout) else {
            return Ok(false);
        };
        match self
            .spawn_stop(
                key,
                name.to_string(),
                Down::Remove(limits.stop_timeout),
                phase,
                Phase::Gone(None),
            )
            .await
        {
            None => Ok(true),
            Some(e) => Err(e),
        }
    }

    /// Insert a `stopping` entry for `key` while its container goes, so an
    /// acquire of the same model parks until the name is free (`acquire`'s
    /// `stopping` arm). `None` when the key or the name is taken — a start
    /// claimed it. `stop_timeout` is `vram.unload_timeout_seconds`, as on
    /// every entry: a `stop` that lands on this one reads it.
    pub(super) fn claim_for_removal(
        &self,
        key: &Key,
        name: &str,
        stop_timeout: Duration,
    ) -> Option<Arc<watch::Sender<Phase>>> {
        let mut map = self.map();
        if map.contains_key(key) || map.values().any(|e| e.container_name == name) {
            return None;
        }
        let (tx, _rx) = watch::channel(Phase::Starting);
        let phase = Arc::new(tx);
        let now = Instant::now();
        map.insert(
            key.clone(),
            Entry {
                generation: next_generation(),
                container_name: name.to_string(),
                host_port: 0,
                state: RuntimeState::Stopping,
                started_at: now,
                in_flight: 0,
                last_used: now,
                stop_timeout,
                phase: phase.clone(),
                warnings: Vec::new(),
                capabilities: None,
                llama: None,
                gate: None,
                charge: None,
                resident_key: None,
                placement: Placement::default(),
                sends: watch::channel(0).0,
                climb: None,
                owner: Origin::Owner,
            },
        );
        Some(phase)
    }
}
