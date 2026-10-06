//! RAII: the start claim and the in-flight guard

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::watch;

use crate::gate::facts::GateFacts;

use super::*;
use crate::runtime::Class;

/// The right to start one model, held by exactly one task — the registry's
/// own ([`Registry::spawn_start`]), never the request that asked for it.
///
/// Exists so the `starting` entry can never outlive the task that inserted
/// it. Every exit — success, failure, or the task being torn down mid-start —
/// either settles the entry or takes it back out of the map and wakes the
/// waiters.
pub(super) struct StartClaim {
    pub(super) reg: Arc<Registry>,
    pub(super) key: Key,
    pub(super) container_name: String,
    pub(super) phase: Arc<watch::Sender<Phase>>,
    /// The container this claim is starting ([`Entry::generation`]).
    pub(super) generation: u64,
    pub(super) settled: bool,
}

impl StartClaim {
    /// Flip the entry to `ready` and take the requester's in-flight claim in
    /// the same lock hold (§3.2), so the model cannot be reaped between "it
    /// is up" and "this request owns a slot on it". `None` when the requester
    /// is gone already: no claim is taken, and the model settles idle.
    ///
    /// Async because of the abort path: when the entry is gone (or is no
    /// longer ours), the container this claim just started is healthy,
    /// unregistered and holding VRAM nothing will ever account for, so it is
    /// removed here rather than left for the next boot to find.
    pub(super) async fn ready<T>(
        mut self,
        started: Started,
        requester: &tokio::sync::oneshot::Sender<T>,
    ) -> Result<Option<AcquireGuard>, RuntimeError> {
        self.settled = true;
        let Started {
            port,
            warnings,
            capabilities,
            llama,
            gate,
        } = started;
        // Decided under the lock, acted on outside it — the map lock is never
        // held across an await, including on this path.
        let (ours, claimed) = {
            let mut map = self.reg.map();
            match map.get_mut(&self.key) {
                Some(e)
                    if Arc::ptr_eq(&e.phase, &self.phase)
                        && e.generation == self.generation
                        && e.state == RuntimeState::Starting =>
                {
                    // A requester that goes away after this answer still gets
                    // its claim, and gives it back by dropping the guard it is
                    // sent (`owned.rs`).
                    let claimed = !requester.is_closed();
                    e.state = RuntimeState::Ready;
                    e.host_port = port;
                    e.started_at = Instant::now();
                    if claimed {
                        e.in_flight += 1;
                    }
                    e.last_used = Instant::now();
                    e.warnings = warnings.clone();
                    e.capabilities = capabilities.clone();
                    e.llama = llama.clone();
                    e.gate = gate.clone();
                    (true, claimed)
                }
                _ => (false, false),
            }
        };
        // A stop claimed the entry while the container was coming up, or the
        // key was taken over by a later start. Either way the map is not ours
        // to touch — but the container we started is, and nothing else knows
        // it exists: no entry names it, so no reaper, eviction or shutdown
        // would ever stop it, and it is holding VRAM as of a second ago.
        //
        // `stop`'s own `podman stop` may or may not have raced ours: it runs
        // against the same *name*, and `podman run --replace` means the object
        // under that name at this instant is the one this claim created —
        // unless an entry holds the name again by now ([`Registry::holds_name`]).
        // Best effort, because the entry is already settled and the caller is
        // being told the truth (`Aborted`) regardless of what podman answers.
        if !ours {
            if self.reg.holds_name(&self.container_name) {
                return Err(RuntimeError::Aborted {
                    class: self.key.0,
                    model_id: self.key.1.clone(),
                });
            }
            if let Err(e) = self.reg.rm_force(&self.container_name).await {
                tracing::warn!(
                    container = %self.container_name,
                    "removing the container of an aborted start: {e}"
                );
            }
            return Err(RuntimeError::Aborted {
                class: self.key.0,
                model_id: self.key.1.clone(),
            });
        }
        self.phase.send_replace(Phase::Ready);
        Ok(claimed.then(|| AcquireGuard {
            reg: Arc::clone(&self.reg),
            key: self.key.clone(),
            port,
            phase: Arc::clone(&self.phase),
            generation: self.generation,
            failed: AtomicBool::new(false),
            gate,
        }))
    }

    /// Unclaim after a failed start and hand every waiter the same error, so
    /// N queued requests fail once instead of starting N doomed containers in
    /// sequence.
    pub(super) fn fail(mut self, err: &RuntimeError) {
        self.settled = true;
        self.reg.forget(&self.key, &self.phase, true);
        self.phase.send_replace(Phase::Gone(Some(err.to_string())));
    }
}

impl Drop for StartClaim {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        // The start task itself ended mid-start — a panic, or the runtime
        // going down: a request's future never holds this claim
        // (`owned.rs`). The container may well be coming up regardless; the
        // reconciliation pass on the reaper tick adopts it if it does
        // (`unheld.rs`). The waiters fail with the start rather than start
        // the model again: what ended this one would end theirs.
        tracing::warn!(
            container = %self.container_name,
            "the start of {} model '{}' ended before it settled; unclaiming — a container \
             that comes up regardless is adopted by the next reconciliation pass",
            self.key.0,
            self.key.1
        );
        self.reg.forget(&self.key, &self.phase, true);
        self.phase
            .send_replace(Phase::Gone(Some(super::owned::UNSETTLED.to_string())));
    }
}

/// Proof that a model was resident when this request claimed it, and that
/// nothing will stop it until the guard drops (§3.2).
///
/// Mirrors [`crate::vram::LocalHold`] one layer down, and the same rule
/// applies: a multi-turn runner holds **one** guard for the whole loop, not
/// one per turn. Admitting per turn would let the reaper stop the container
/// in a tool-call gap and make every later turn pay a cold start.
///
/// A guard is a counter, not a lock: holding one never blocks another
/// acquire, of this model or any other.
///
/// **A claim is on the entry, the port on a container.** A ladder climb
/// replaces the container under the same entry (ladder design §12 entry 10):
/// the claim stays counted — so an idle tool loop keeps its model resident
/// across somebody else's climb — while [`Self::port`], [`Self::gate_facts`]
/// and [`Self::generation`] still name the container the claim was taken on,
/// until [`crate::vram::LocalHold::sync`] moves them to the new one.
pub struct AcquireGuard {
    pub(super) reg: Arc<Registry>,
    pub(super) key: Key,
    pub(super) port: u16,
    /// Identity of the entry this claim was taken on — the same handle
    /// [`Registry::forget`] compares against, and for the same reason. A guard
    /// can outlive its entry (the container died, a forced stop replaced it,
    /// the model was restarted underneath a long request), and a release that
    /// matched on the key alone would then decrement — and re-stamp — a
    /// *successor* entry it never held a claim on.
    pub(super) phase: Arc<watch::Sender<Phase>>,
    /// The container [`Self::port`] is on ([`Entry::generation`]). Behind the
    /// entry's identity rather than beside it: a climb keeps the entry and
    /// replaces the container, so the claim is still counted while this goes
    /// stale.
    pub(super) generation: u64,
    /// Set by [`Self::mark_failed`] when the request holding this guard did
    /// not get served by the container (design §3.2's dead-endpoint path).
    pub(super) failed: AtomicBool,
    /// The facts of the container this claim is on, as it was started — see
    /// [`Entry::gate`]. Copied at claim time: the entry cannot change them
    /// (a restart is a new entry, and a new claim).
    pub(super) gate: Option<Arc<GateFacts>>,
}

impl AcquireGuard {
    /// Where to forward: always loopback. The container publishes on all
    /// interfaces (the dashboard links it by the browser's hostname), but
    /// lmgw itself is on the same host and has no reason to leave it.
    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn class(&self) -> Class {
        self.key.0
    }

    pub fn model_id(&self) -> &str {
        &self.key.1
    }

    /// What the request gate reads about this container — the row it was
    /// started with ([`GateFacts`]). `None` for every class but chat.
    pub fn gate_facts(&self) -> Option<&Arc<GateFacts>> {
        self.gate.as_ref()
    }

    /// The container this claim's port is on ([`Entry::generation`]).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Move the claim onto the container that replaced its own under the same
    /// entry — a climb's new rung ([`ClaimStatus::Moved`]). The claim itself
    /// was never released: the entry carried it across. Whether it was served
    /// is a question about the new container, so a [`Self::mark_failed`] made
    /// against the old one is cleared.
    pub fn retarget(&mut self, port: u16, gate: Option<Arc<GateFacts>>, generation: u64) {
        self.port = port;
        self.gate = gate;
        self.generation = generation;
        self.failed.store(false, Ordering::Relaxed);
    }

    /// Record that this claim ends in a failure the container is answerable
    /// for — a connect/transport error against its endpoint (§3.2). The
    /// release then decrements the claim **without** re-stamping `last_used`,
    /// so a container that is dead (or wedged) keeps ageing towards the idle
    /// reaper and the LRU instead of looking freshly used every time a client
    /// retries into it.
    ///
    /// Deliberately not called for every failed request: a 400 from a healthy
    /// model *is* the model doing work, and pretending otherwise would make
    /// the LRU rank a busy model as cold.
    pub fn mark_failed(&self) {
        self.failed.store(true, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for AcquireGuard {
    /// Hand-written because [`Registry`] is not `Debug` and never should be
    /// (printing it would take the map lock from wherever a `{:?}` happens to
    /// land). What a reader wants from a guard is which model it holds and
    /// where that model answers.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcquireGuard")
            .field("class", &self.key.0)
            .field("model_id", &self.key.1)
            .field("port", &self.port)
            .field("generation", &self.generation)
            .finish()
    }
}

impl Drop for AcquireGuard {
    fn drop(&mut self) {
        self.reg
            .release(&self.key, &self.phase, !self.failed.load(Ordering::Relaxed));
    }
}
