//! Containers podman runs under this instance's label that the registry does
//! not hold (§3.4, after boot).
//!
//! Boot reconciliation used to be the only moment lmgw compared its map with
//! podman's, so a container that lost its entry while lmgw ran — a start
//! abandoned between `podman run` and its readiness verdict, a stop that
//! failed and dropped the entry anyway, a `podman start` from a shell — held
//! its memory as "outside" until the next restart: nothing reaped or evicted
//! it, and an admission waiting for room never learned it could have had it.
//! [`Registry::readopt`] is the same comparison for a running lmgw, and
//! [`Registry::stop_unheld`] is `container stop` for such a container.
//!
//! **Only this lmgw's own.** Two instances can share a `container_prefix` —
//! every dev copy is `lmgw-dev` — and with it the instance label, and a pass
//! every tick acting on the other's containers would make them fight every
//! 15 s. So a pass acts only on containers whose
//! [`OWNER_LABEL`](crate::runtime::argv::OWNER_LABEL) names this registry's
//! owner ([`Registry::set_owner`]); another owner's, or a container with
//! none (an older lmgw's), is logged once and left alone. Boot reconciliation
//! stays prefix-based: it is what takes back a container an older build left
//! behind.
//!
//! **What a pass never touches** beyond that: a name the registry holds in
//! any state — `starting` included, which a start makes before its `podman
//! run` (`owned.rs`) — a container that is not running (it holds no memory,
//! and its logs are the record of a failed start that `--replace` collects
//! later), the benchmark's and the agents' containers, as boot skips them, and
//! a container younger than `vram.load_timeout_seconds` that does not answer
//! yet: it may be loading. A container it removes is taken down under a
//! `stopping` entry for its key, so a start of the same model waits for the
//! name instead of racing `podman run --replace` against the removal, and one
//! it keeps failing on is retried with a backoff (`memory.rs`).
//!
//! **One pass at a time**, whoever asks ([`PassWait`]), and nothing waits on
//! one it does not have to: the caller bounds it, and every step it may be
//! cut short at is safe to drop — an adoption inserts last, under the map
//! lock, and a removal is the registry's own task.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::owned::Down;
use super::reconcile::{PsRow, Refused};
use super::*;
use crate::runtime::argv::OWNER_LABEL;
use crate::runtime::{container_name, Class};

mod memory;
mod remove;

pub(super) use memory::PassMemory;

/// What one [`Registry::readopt`] pass did. Names are container names.
#[derive(Debug, Default)]
pub struct ReadoptReport {
    /// Running containers taken over as `ready`, idle entries.
    pub adopted: Vec<String>,
    /// Running containers that could not be adopted and were stopped and
    /// removed, each with the reason. A failed removal is in `errors` too.
    pub removed: Vec<(String, String)>,
    /// True when `podman ps` answered. False means the pass learned nothing,
    /// and its one error says why — on a box without podman, every pass.
    pub listed: bool,
    /// What went wrong along the way, rendered. A pass never fails as a
    /// whole: one unreadable container must not stop the others.
    pub errors: Vec<String>,
    /// `Some` when this call found a pass running and waited for it instead
    /// of running its own ([`PassWait::Join`]): whether that pass changed what
    /// the registry holds. Its adoptions and removals are that pass's to
    /// report, so they are not repeated here.
    pub joined: Option<bool>,
}

impl ReadoptReport {
    /// Did the pass change what the registry holds?
    pub fn changed(&self) -> bool {
        self.joined.unwrap_or(false) || !self.adopted.is_empty() || !self.removed.is_empty()
    }
}

/// What a pass needs from the settings.
#[derive(Debug, Clone, Copy)]
pub struct PassLimits {
    /// `vram.load_timeout_seconds`: a container younger than this that does
    /// not answer may still be loading, and is left alone.
    pub load_timeout: Duration,
    /// `vram.unload_timeout_seconds`: what a removal's `stopping` entry
    /// records, and what bounds the removal after the stop grace.
    pub stop_timeout: Duration,
    /// The reaper tick: the backoff's unit (`memory.rs`).
    pub tick: Duration,
}

/// What a caller does when a pass is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassWait {
    /// Nothing: the reaper tick's next pass comes soon enough.
    Skip,
    /// Wait for it, and take its verdict instead of running another — the
    /// caller bounds the wait.
    Join,
}

impl Registry {
    /// The lmgw instance this registry starts containers for — stamped on
    /// every start as [`OWNER_LABEL`], and the only owner a reconciliation
    /// pass acts on (module doc). Set once at startup; a later call is
    /// ignored. A registry with no owner stamps nothing and leaves every
    /// container alone.
    pub fn set_owner(&self, owner: String) {
        let _ = self.owner.set(owner);
    }

    pub(super) fn owner(&self) -> Option<&str> {
        self.owner.get().map(String::as_str)
    }

    /// Compare the running containers labelled for `container_prefix` with
    /// the map, and adopt — or remove — every one of this lmgw's it does not
    /// hold (module doc). `candidates` are boot's: one [`AcquireSpec`] per
    /// enabled model and ladder rung, so "would lmgw start this container the
    /// same way today?" is asked exactly as boot asks it — [`Self::reconcile`]'s
    /// own adoption.
    ///
    /// `None`: a pass was running and `wait` said skip.
    pub async fn readopt(
        self: &Arc<Self>,
        container_prefix: &str,
        candidates: &[AcquireSpec<'_>],
        limits: PassLimits,
        wait: PassWait,
    ) -> Option<ReadoptReport> {
        let mut memory = match self.pass.try_lock() {
            Ok(memory) => memory,
            Err(_) if wait == PassWait::Skip => return None,
            Err(_) => {
                let memory = self.pass.lock().await;
                return Some(ReadoptReport {
                    joined: Some(memory.last_changed),
                    ..Default::default()
                });
            }
        };
        let report = self
            .pass(container_prefix, candidates, limits, &mut memory)
            .await;
        memory.last_changed = report.changed();
        Some(report)
    }

    async fn pass(
        self: &Arc<Self>,
        container_prefix: &str,
        candidates: &[AcquireSpec<'_>],
        limits: PassLimits,
        memory: &mut PassMemory,
    ) -> ReadoptReport {
        let mut report = ReadoptReport::default();
        let rows = match self.running(container_prefix).await {
            Ok(rows) => rows,
            Err(e) => {
                report.errors.push(e);
                return report;
            }
        };
        report.listed = true;
        memory.prune(
            &rows
                .iter()
                .filter_map(|r| r.names.first().map(String::as_str))
                .collect::<HashSet<_>>(),
        );
        for row in &rows {
            let Some(name) = row.names.first().cloned() else {
                continue;
            };
            if row.state != "running" || not_a_model(&row.labels) || self.holds(row, &name) {
                continue;
            }
            if !self.is_ours(&row.labels) {
                if memory.first_foreign(&name) {
                    tracing::warn!(
                        container = %name,
                        "a running container under this instance's prefix ({container_prefix}) \
                         {} — the reconciliation pass leaves it alone",
                        whose(&row.labels)
                    );
                }
                continue;
            }
            let now = Instant::now();
            if memory.backing_off(&name, row.created, now) {
                tracing::debug!(container = %name, "reconciliation pass: backing off");
                continue;
            }
            match self.adopt(container_prefix, candidates, row, &name).await {
                Ok(true) => {
                    // Adopted before, and unheld again: whatever took its
                    // entry left it running — a stop that failed.
                    if memory.again(&name, row.created) {
                        let first = memory.failed(&name, row.created, now, limits.tick);
                        self.log_failure(
                            &name,
                            first,
                            "it lost its entry again since it was last adopted (a stop that \
                             failed, or a restart outside lmgw)",
                        );
                    } else {
                        memory.adopted(&name, row.created, now);
                    }
                    report.adopted.push(name);
                }
                // A start claimed the key meanwhile: the name is its now.
                Ok(false) => {}
                Err(Refused { why, silent }) => {
                    if silent && young(row.created, limits.load_timeout) {
                        tracing::debug!(
                            container = %name,
                            "reconciliation pass: not answering yet, and younger than \
                             vram.load_timeout_seconds — may still be loading"
                        );
                        continue;
                    }
                    match self
                        .remove_unheld(container_prefix, row, &name, limits)
                        .await
                    {
                        Ok(true) => {
                            memory.forget(&name);
                            report.removed.push((name, why));
                        }
                        // Claimed meanwhile, or no longer running: not ours to
                        // remove any more.
                        Ok(false) => {}
                        Err(e) => {
                            let first = memory.failed(&name, row.created, now, limits.tick);
                            self.log_failure(&name, first, &format!("removing it failed: {e}"));
                            report.errors.push(e);
                        }
                    }
                }
            }
        }
        report
    }

    /// A container the pass keeps failing on: at WARN the first time, at
    /// DEBUG after that (`memory.rs`).
    fn log_failure(&self, name: &str, first: bool, why: &str) {
        if first {
            tracing::warn!(
                container = %name,
                "reconciliation pass: {why} — retrying on later reaper ticks, backing off while \
                 it keeps failing"
            );
        } else {
            tracing::debug!(container = %name, "reconciliation pass: {why} (again)");
        }
    }

    /// `container stop` for a model the registry holds no entry for: stop
    /// the running container its name renders, if there is one and it is
    /// this lmgw's (module doc). Stopped, not removed, like any `stop`
    /// (§3.6): its logs stay for `container logs`, and the next start's
    /// `--replace` collects it. A model the registry does hold is
    /// [`Self::stop`]'s, and is left alone here.
    pub async fn stop_unheld(
        self: &Arc<Self>,
        container_prefix: &str,
        class: Class,
        model_id: &str,
        stop_timeout: Duration,
    ) -> Result<Unheld, RuntimeError> {
        let name = container_name(container_prefix, class, model_id);
        let Some(found) = self.inspect_unheld(&name).await.filter(|i| i.running) else {
            return Ok(Unheld::NotRunning);
        };
        if !self.is_ours(&found.labels) {
            return Ok(Unheld::NotOurs {
                whose: whose(&found.labels),
                name,
            });
        }
        let key: Key = (class, model_id.to_string());
        let Some(phase) = self.claim_for_removal(&key, &name, stop_timeout) else {
            return Ok(Unheld::NotRunning);
        };
        match self
            .spawn_stop(
                key,
                name.clone(),
                Down::Stop(stop_timeout),
                phase,
                Phase::Gone(None),
            )
            .await
        {
            None => Ok(Unheld::Stopped(name)),
            Some(message) => Err(RuntimeError::Stop {
                class,
                model_id: model_id.to_string(),
                message,
            }),
        }
    }

    /// `podman ps` of this instance's running containers — no `-a`: an
    /// exited one holds no memory, and is none of this pass's business.
    async fn running(&self, container_prefix: &str) -> Result<Vec<PsRow>, String> {
        let filter = format!("label=lmgw.instance={container_prefix}");
        let out = self
            .podman(&["ps", "--format", "json", "--filter", &filter])
            .await
            .map_err(|e| format!("podman ps could not be run: {e}"))?;
        if !out.ok() {
            return Err(format!(
                "podman ps failed (exit {}): {}",
                out.status,
                out.stderr.trim()
            ));
        }
        if out.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        serde_json::from_str(&out.stdout)
            .map_err(|e| format!("podman ps returned unreadable JSON: {e}"))
    }

    /// Does the registry hold this container — by its name, or by the model
    /// its labels name — in any state?
    fn holds(&self, row: &PsRow, name: &str) -> bool {
        self.holds_name(name)
            || labelled_key(&row.labels).is_some_and(|k| self.map().contains_key(&k))
    }

    /// Do `labels` name this registry's owner?
    fn is_ours(&self, labels: &HashMap<String, String>) -> bool {
        self.owner()
            .is_some_and(|me| labels.get(OWNER_LABEL).map(String::as_str) == Some(me))
    }
}

/// What [`Registry::stop_unheld`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unheld {
    /// This lmgw's container of that name was running, and is stopped.
    Stopped(String),
    /// No container of that name is running.
    NotRunning,
    /// One is running, but another lmgw started it, or one with no owner
    /// label: not stopped. `whose` says which, as a clause.
    NotOurs { name: String, whose: String },
}

/// A benchmark run's or an agent's container: labelled for this instance,
/// and no model's — as [`Registry::reconcile`] skips them.
fn not_a_model(labels: &HashMap<String, String>) -> bool {
    labels.contains_key(crate::bench::BENCH_LABEL)
        || labels
            .get(crate::agents::container::LABEL_KIND)
            .map(String::as_str)
            == Some(crate::agents::container::KIND_AGENT)
}

/// The registry key a container's `lmgw.class` / `lmgw.model` labels name.
fn labelled_key(labels: &HashMap<String, String>) -> Option<Key> {
    let class = labels.get("lmgw.class").and_then(|c| Class::parse(c))?;
    Some((class, labels.get("lmgw.model")?.clone()))
}

/// Whose a container that is not this lmgw's is, as a clause.
fn whose(labels: &HashMap<String, String>) -> String {
    match labels.get(OWNER_LABEL) {
        Some(owner) => format!("belongs to another lmgw (owner {owner})"),
        None => "carries no owner label (an older lmgw's, or started outside lmgw)".into(),
    }
}

/// Created less than `load_timeout` ago — `created` in unix seconds, `0`
/// (unknown) reading as long ago.
fn young(created: i64, load_timeout: Duration) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    created > 0 && now.saturating_sub(created) < load_timeout.as_secs() as i64
}
