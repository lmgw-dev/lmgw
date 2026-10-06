//! The starts and stops the registry owns (§3.2, §3.6).
//!
//! A start or a stop is a podman call and a wait, and the caller that asked
//! for it is often a request whose client can hang up at any await — hyper
//! drops the handler future when the connection closes. Run inside that
//! future, a start dropped between `podman run` and its readiness verdict
//! left the container running with no entry naming it: nothing reaped,
//! evicted or stopped it, and its memory read as somebody else's to every
//! admission after it (`docs/design/2026-10-06-registry-owns-start.md`). A
//! stop dropped between marking the entry `stopping` and removing it left the
//! entry `stopping` for good, with every later acquirer parked on it and
//! nothing left to wake them.
//!
//! So both run as tasks the registry spawns, and the caller only waits for
//! the verdict. A caller that goes away loses its receiver, nothing else: the
//! start still settles — `ready` and idle, owned by the reaper and eviction
//! like any other — and the stop still removes its entry.
//!
//! **The first claim is handed over, not taken.** The requester of a start
//! gets its in-flight claim from the task, taken in the same lock hold that
//! flips the entry `ready` ([`StartClaim::ready`]), so nothing can reap or
//! evict the model between "it is up" and "this request owns a slot on it" —
//! the §3.2 invariant the hold sweep and eviction rely on. A requester that is
//! already gone by then is not given one, and the model settles idle.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, watch};

use super::raii::StartClaim;
use super::*;
use crate::runtime::Class;

/// What a start's requester is sent: its claim on the started model, or the
/// start's own error.
pub(super) type Verdict = Result<AcquireGuard, RuntimeError>;

/// The requester's end of a spawned start ([`Registry::spawn_start`]).
pub(super) struct OwnedStart {
    class: Class,
    model_id: String,
    verdict: oneshot::Receiver<Verdict>,
}

impl OwnedStart {
    /// Wait for the start to settle. Dropping this instead is how a requester
    /// that went away lets the start finish without it.
    pub(super) async fn outcome(self) -> Verdict {
        match self.verdict.await {
            Ok(verdict) => verdict,
            // The task ended without a verdict — a panic, or the runtime
            // going down. Its claim's `Drop` already settled the entry and
            // told the waiters the same thing.
            Err(_) => Err(RuntimeError::Start {
                class: self.class,
                model_id: self.model_id,
                container_name: String::new(),
                message: UNSETTLED.into(),
                logs: Vec::new(),
            }),
        }
    }
}

/// Why a start's waiters are failed when the start task ended without a
/// verdict ([`StartClaim`]'s `Drop`).
pub(super) const UNSETTLED: &str = "the start ended before it settled";

impl Registry {
    /// What to do once a start has settled `ready`, whoever waits for it:
    /// learn the new container's PID and tell the dashboard (`state.rs`
    /// installs it). Set once — a later call is ignored.
    pub fn set_on_started(&self, hook: OnStarted) {
        let _ = self.on_started.set(hook);
    }

    /// Run `claim`'s start sequence as a task of the registry's, not of the
    /// caller's. The entry is already `starting` (the claim was taken under
    /// the map lock), so a reconciliation pass or a second acquire sees the
    /// start before `podman run` does anything.
    pub(super) fn spawn_start(self: &Arc<Self>, claim: StartClaim, spec: StartSpec) -> OwnedStart {
        let (tx, rx) = oneshot::channel();
        let out = OwnedStart {
            class: claim.key.0,
            model_id: claim.key.1.clone(),
            verdict: rx,
        };
        tokio::spawn(Arc::clone(self).run_start(claim, spec, tx));
        out
    }

    async fn run_start(
        self: Arc<Self>,
        claim: StartClaim,
        spec: StartSpec,
        requester: oneshot::Sender<Verdict>,
    ) {
        let (class, model_id, name) = (
            claim.key.0,
            claim.key.1.clone(),
            claim.container_name.clone(),
        );
        let verdict = match self.start_container(&spec.as_spec(), &name).await {
            Ok(started) => claim.ready(started, &requester).await,
            Err(err) => {
                claim.fail(&err);
                Err(err)
            }
        };
        // Up, with or without a requester left to tell: what the rest of lmgw
        // learns about a new container is learned here, not only on the
        // request path (`vram::queue`'s `cache_pids`).
        if verdict.is_ok() {
            if let Some(hook) = self.on_started.get() {
                hook();
            }
        }
        match verdict {
            // Ready, and nobody to hand a claim to: the request that asked for
            // it went away while it loaded. It stays up, idle, for the reaper
            // and eviction to judge like any other model.
            Ok(None) => tracing::info!(
                container = %name,
                "{class} model '{model_id}' is ready; the request that started it went away \
                 while it loaded, so it is idle"
            ),
            // A requester that went away between the claim and this send gets
            // the guard back in the `Err`, which drops it — the claim is
            // released as it would have been by the request itself.
            Ok(Some(guard)) => drop(requester.send(Ok(guard))),
            Err(e) => drop(requester.send(Err(e))),
        }
    }

    /// Take `name` down and settle the entry `phase` names as a task of the
    /// registry's (module doc): the entry is gone and `verdict` sent once the
    /// container is, however the caller fares. `Some` is what went wrong.
    ///
    /// The caller has marked the entry `stopping` under the map lock (or
    /// inserted it so, for a container it had none for, `unheld.rs`); this is
    /// everything after that.
    pub(super) async fn spawn_stop(
        self: &Arc<Self>,
        key: Key,
        name: String,
        down: Down,
        phase: Arc<watch::Sender<Phase>>,
        verdict: Phase,
    ) -> Option<String> {
        let reg = Arc::clone(self);
        let task = tokio::spawn(async move {
            // Settles on every way out of the task, a panic and the runtime
            // going down included: an entry left `stopping` has nothing that
            // would ever remove it.
            let _settle = StopSettle {
                reg: Arc::clone(&reg),
                key,
                phase,
                verdict: Some(verdict),
            };
            match down {
                Down::Stop(stop_timeout) => reg.stop_container(&name, stop_timeout).await,
                Down::Remove(stop_timeout) => reg.remove_bounded(&name, stop_timeout).await,
            }
        });
        match task.await {
            Ok(problem) => problem,
            Err(e) => Some(format!("the stop ended abnormally: {e}")),
        }
    }
}

impl Registry {
    /// [`Registry::remove_unmanaged`], bounded like a stop's `podman wait`:
    /// the grace `podman stop -t` gives, then `stop_timeout`
    /// (`vram.unload_timeout_seconds`). A podman that hangs must not keep a
    /// `stopping` entry — and every start of the model behind it — for good;
    /// the runner kills its child on drop. `Some` is what went wrong.
    pub(super) async fn remove_bounded(
        &self,
        name: &str,
        stop_timeout: Duration,
    ) -> Option<String> {
        let bound = Duration::from_secs(STOP_GRACE_SECONDS) + stop_timeout;
        match tokio::time::timeout(bound, self.remove_unmanaged(name)).await {
            Ok(removed) => removed.err(),
            Err(_) => Some(format!(
                "removing '{name}' did not finish within {bound:?} (the stop grace plus \
                 vram.unload_timeout_seconds)"
            )),
        }
    }
}

/// How [`Registry::spawn_stop`] takes a container down.
pub(super) enum Down {
    /// `podman stop`, then `podman wait` within this
    /// (`vram.unload_timeout_seconds`): the container stays, with its logs,
    /// for the next start's `--replace` (§3.6).
    Stop(Duration),
    /// `podman stop` and `rm -f` ([`Registry::remove_unmanaged`]), bounded by
    /// the stop grace plus this (`vram.unload_timeout_seconds`): a container
    /// no start of today's configuration would ever replace.
    Remove(Duration),
}

/// Removes a stopped entry and tells its waiters, when dropped.
///
/// The verdict goes out *after* the removal, never before: a terminal phase
/// on an entry that is still in the map would spin every waiter (it would
/// re-park on the same entry and re-read the same final value). Waiters that
/// wanted this container's start get told it was stopped, so nobody silently
/// brings back up what somebody just asked to be taken down; the starting
/// task learns the same fact its own way, from `ready()` finding the entry no
/// longer `starting`.
struct StopSettle {
    reg: Arc<Registry>,
    key: Key,
    phase: Arc<watch::Sender<Phase>>,
    verdict: Option<Phase>,
}

impl Drop for StopSettle {
    fn drop(&mut self) {
        // Drop the entry either way: after a failed stop lmgw's belief about
        // this container is worthless, and the next start replaces it by name.
        self.reg.forget(&self.key, &self.phase, false);
        if let Some(verdict) = self.verdict.take() {
            self.phase.send_replace(verdict);
        }
    }
}
