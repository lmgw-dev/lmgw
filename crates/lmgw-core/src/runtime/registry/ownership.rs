//! Who a container runs for, and the claim that never starts one
//! (candidate-aliases design §4.4–§4.5; §12 entries 46–48).
//!
//! Background traffic — a candidate alias with the `background` flag — is a
//! guest on the GPU: it may use what the owner is not using, and it never
//! makes them wait for something they did not ask for. Three pieces of registry
//! state make that decidable where the decision has to be exact, under the
//! map lock:
//!
//! - **Every entry records its owner** ([`Origin`], §4.4, entry 48). An entry
//!   is `Background` only when a background request's start created it (the
//!   alias's primary, or that request's own restart). Any claim of the owner's
//!   — a hit, or a park on a start or climb in flight, since joining a start
//!   is a claim too — makes it `Owner` in the same lock hold, until the
//!   container stops: removal forgets the entry, and the next start records
//!   the origin of whoever starts it. A climb keeps the entry, so it keeps
//!   its owner. Boot adoption, warm starts and operator starts are `Owner`,
//!   because they all go through [`Registry::acquire`] or adopt.
//! - **The join-only claim** ([`Registry::join`], entry 46). A candidate that
//!   is *loaded* is joined, never started, by the claim that uses it: absent
//!   or `stopping` answers `None`, and a start or climb it parked on that
//!   ended with the entry gone answers `None` too. The decision and the claim
//!   are one lock hold, so a stop that lands between the gate's pick and the
//!   claim can only ever turn into "not loaded" — never into a container the
//!   alias started unarbitrated (an alternate, which the alias must never
//!   start at all).
//! - **Draining for the owner** ([`Registry::drain_for_owner`], §4.5, entry
//!   47). While an owner admission waits for room, every resident model is
//!   draining: background claims refuse it (the gate moves on to a loaded
//!   alternate, or the fallback), and what is in flight finishes. A counter,
//!   not a lock: nothing ever waits for it, so it cannot take part in a
//!   deadlock with the admission gate, a climb's gate yield or a climb's
//!   drain. It lives here rather than in the VRAM scheduler because a
//!   background join has to read it under the same map lock it claims in,
//!   and [`Registry::list`] publishes it on every entry.
//!
//! The VRAM half — when a background start may happen and what it may evict —
//! is `crate::vram::background`'s; this module is the machinery it drives, the
//! same split as `acquire` and admission, or [`super::climb`] and
//! `vram::climb`.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use super::climb::ClimbStart;
use super::{
    AcquireGuard, ClimbTicket, Entry, Key, Phase, Registry, RuntimeError, RuntimeState, StartSpec,
};
use crate::runtime::Class;

/// Whose a claim is, and — on an entry — whose container it is
/// (candidate-aliases design §4.4).
///
/// Published on [`super::RuntimeView::owner`] only when it is `Background`,
/// so the runtime frame of an install without background traffic is byte for
/// byte what it was.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The owner's: every request that is not a background alias's, every
    /// warm, operator and boot start, every in-process caller.
    #[default]
    Owner,
    /// A background candidate alias's (§4.3): a guest on the GPU.
    Background,
}

impl Origin {
    /// The default — what [`super::RuntimeView`] leaves off the frame.
    pub fn is_owner(&self) -> bool {
        *self == Self::Owner
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Background => "background",
        }
    }
}

/// Record a claim of `origin` on `e` (§4.4, entry 48): the owner's claim —
/// a hit, or a park on a start or a climb in flight — makes the entry theirs for
/// as long as this container runs. A background claim changes nothing: it
/// neither takes a model from the owner nor gives one to them. A `stopping`
/// entry is not claimed at all (a park there waits for the key to be free),
/// so it is left as it is.
pub(super) fn claimed_by(e: &mut Entry, origin: Origin) {
    if origin == Origin::Owner && e.state != RuntimeState::Stopping {
        e.owner = Origin::Owner;
    }
}

/// `skip_serializing_if` for [`super::RuntimeView::draining_for_owner`].
pub(super) fn is_false(b: &bool) -> bool {
    !*b
}

/// One owner admission waiting for room (§4.5, entry 47). While any exists,
/// every resident model is draining for the owner: background claims refuse
/// it. Dropped however the wait ends — admitted, timed out, answered by a
/// fallback, or the waiting future dropped with its client — so the mark can
/// never outlive the last waiter.
#[must_use = "the drain lasts exactly as long as this value — keep it for the whole wait"]
pub struct OwnerDrain {
    reg: Arc<Registry>,
}

impl std::fmt::Debug for OwnerDrain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerDrain").finish()
    }
}

impl Drop for OwnerDrain {
    fn drop(&mut self) {
        self.reg.owner_waits.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What one pass of [`Registry::join`] decided under the lock.
enum Step {
    Claimed(AcquireGuard),
    Park(tokio::sync::watch::Receiver<Phase>),
}

impl Registry {
    /// Mark every resident model draining for the owner until the returned
    /// value drops (§4.5, entry 47) — for an owner admission's make-room
    /// wait. Nested and concurrent waits count: the mark clears when the
    /// last of them ends.
    ///
    /// Armed under the map lock, the lock every background claim decides in
    /// ([`Self::join`], a guest climb's start): a background claim either
    /// landed before the owner's wait began — it is in flight, and the wait
    /// sees it — or it sees the mark and refuses. None slips in between.
    pub fn drain_for_owner(self: &Arc<Self>) -> OwnerDrain {
        {
            let _map = self.map();
            self.owner_waits.fetch_add(1, Ordering::SeqCst);
        }
        OwnerDrain {
            reg: Arc::clone(self),
        }
    }

    /// Is an owner admission waiting for room right now? Background claims
    /// refuse every resident model while it is ([`Self::join`]), and a
    /// background start does not begin one (`vram::background`).
    pub fn draining_for_owner(&self) -> bool {
        self.owner_waits.load(Ordering::SeqCst) > 0
    }

    /// Whose container runs for this model — `None` when none does, or it is
    /// on its way out. A climb's permission check reads it (§9: background
    /// never climbs the owner's model).
    pub fn owner_of(&self, class: Class, model_id: &str) -> Option<Origin> {
        self.map()
            .get(&(class, model_id.to_string()))
            .filter(|e| e.state != RuntimeState::Stopping)
            .map(|e| e.owner)
    }

    /// Claim this model **only if it is loaded** — `ready`, or `starting`
    /// (joining a start in flight), or being climbed — and never start it
    /// (§4.1 "loaded", entry 46). `Ok(None)` means it is not loaded for this
    /// claim:
    /// - no entry, or one being stopped;
    /// - a background claim while an owner admission waits for room
    ///   ([`Self::draining_for_owner`], §4.5) — read under the same lock hold
    ///   as the claim, and the mark is armed under that lock too
    ///   ([`Self::drain_for_owner`]), so no background claim lands on a model
    ///   after the owner's wait began;
    /// - the climb it parked on could not start its rung
    ///   ([`RuntimeError::ClimbFailed`] is `None` here, not an error: the
    ///   model is simply not loaded any more, and no waiter is sent to start
    ///   it). A start it parked on outlives the request that started it
    ///   (`owned.rs`), so a client that went away is no reason to give up.
    ///
    /// **The race it closes.** The gate picks a candidate from
    /// [`Self::list`] and claims it a moment later; the reaper, an eviction
    /// or a stop can take the container in between. `acquire` would then
    /// start it — for an alternate, a start the alias must never make, and
    /// for any candidate a start nobody arbitrated. Here the absent case
    /// simply answers `None`, and every re-decision after a park asks again
    /// under the lock, so the only way to get a claim is to find the model
    /// up.
    ///
    /// A start it joined that *fails*, or that a stop lands on, is that
    /// start's error, exactly as for [`Self::acquire`]'s waiters: a second
    /// attempt at a model that just failed to load would pay the whole load
    /// timeout again for the same answer, and one somebody just asked to be
    /// down must not come back because a request moved on to "load the
    /// primary" (§4.2 step 3) a moment later.
    ///
    /// An owner claim makes the entry the owner's ([`claimed_by`]), exactly
    /// as an owner `acquire` does.
    pub async fn join(
        self: &Arc<Self>,
        class: Class,
        model_id: &str,
        origin: Origin,
    ) -> Result<Option<AcquireGuard>, RuntimeError> {
        let key: Key = (class, model_id.to_string());
        loop {
            let step = {
                let mut map = self.map();
                let refused = origin == Origin::Background && self.draining_for_owner();
                let Some(e) = map.get_mut(&key) else {
                    return Ok(None);
                };
                if e.state == RuntimeState::Stopping || refused {
                    return Ok(None);
                }
                claimed_by(e, origin);
                if e.state == RuntimeState::Ready && e.climb.is_none() {
                    e.in_flight += 1;
                    e.last_used = Instant::now();
                    Step::Claimed(AcquireGuard {
                        reg: Arc::clone(self),
                        key: key.clone(),
                        port: e.host_port,
                        phase: e.phase.clone(),
                        generation: e.generation,
                        failed: std::sync::atomic::AtomicBool::new(false),
                        gate: e.gate.clone(),
                    })
                } else {
                    // `starting`, or a climb bringing up another rung: parked
                    // on like `acquire` parks on a start.
                    Step::Park(e.phase.subscribe())
                }
            };
            match step {
                Step::Claimed(guard) => return Ok(Some(guard)),
                Step::Park(rx) => match self.await_phase(&key, RuntimeState::Starting, rx).await {
                    Ok(()) => continue,
                    Err(RuntimeError::ClimbFailed { .. }) => return Ok(None),
                    Err(e) => return Err(e),
                },
            }
        }
    }

    /// Stop this model's container only if it is still the one `generation`
    /// names, idle, and still `Background`-owned (§4.4: "background traffic
    /// may evict only idle `Background` entries").
    ///
    /// [`Self::stop_generation`] with one more identity check in the same lock
    /// hold that marks the entry `stopping`: a background eviction judges its
    /// victim from a [`Self::list`] and then asks the victim's `/slots`,
    /// which can take seconds. An owner request that claimed the model in
    /// between, and even released it again, made it the owner's — and the
    /// stop is then [`RuntimeError::Moved`], nothing stopped. Never forced: a
    /// claim in flight is [`RuntimeError::Busy`], as for every eviction.
    pub async fn stop_idle_background(
        self: &Arc<Self>,
        class: Class,
        model_id: &str,
        generation: u64,
    ) -> Result<(), RuntimeError> {
        self.stop_where(class, model_id, false, Some(generation), true)
            .await
    }
}

impl Registry {
    /// A guest's stop of a container that stopped answering it (§12 entry
    /// 90): forced — the only claims left on a dead container are the ones
    /// failing against it — and, like [`Self::stop_idle_background`], only
    /// while it is still the container `generation` names and still
    /// `Background`-owned, decided in the lock hold that marks it stopping.
    /// The owner's model is [`RuntimeError::claimed_by_owner`], nothing
    /// stopped.
    pub async fn stop_dead_background(
        self: &Arc<Self>,
        class: Class,
        model_id: &str,
        generation: u64,
    ) -> Result<(), RuntimeError> {
        self.stop_where(class, model_id, true, Some(generation), true)
            .await
    }
}

/// Why [`Registry::stop_idle_background`] (and a guest's dead-container stop)
/// stopped nothing: the owner claimed the model since it was judged.
/// [`RuntimeError::claimed_by_owner`] tells it from the other `Moved`s.
pub(super) const CLAIMED_BY_OWNER: &str = "has been claimed by the owner since it was judged";

impl RuntimeError {
    /// A background stop refused because the model is the owner's now
    /// ([`CLAIMED_BY_OWNER`]) — as opposed to one restarted or climbed since
    /// it was judged.
    pub fn claimed_by_owner(&self) -> bool {
        matches!(self, RuntimeError::Moved { why, .. } if *why == CLAIMED_BY_OWNER)
    }
}

impl Registry {
    /// Mark `g`'s model climbing for a **guest**, or join the climb running on
    /// it — only while, in the lock hold that marks or joins, the model is
    /// still `Background`-owned and no owner admission waits for room (§12
    /// entry 91). A `Background`-owned model has never had an owner claim in
    /// this container's life, so every climb on it is background traffic's
    /// own, which a guest may ride; the owner's climb, or a model that became
    /// theirs since the guest's earlier look, is [`Marked::Refused`] — nothing
    /// marked, nothing raised, nothing waited for.
    pub fn mark_climb_for_guest(
        self: &Arc<Self>,
        g: &AcquireGuard,
        to: crate::runtime::descriptor::RungPos,
        reason: &str,
    ) -> super::Marked {
        self.mark_climb_unless(g, to, reason, |reg, e| {
            if e.owner != Origin::Background {
                Some("is the owner's model, and background traffic never climbs it")
            } else if reg.draining_for_owner() {
                Some(
                    "cannot be climbed now: the owner is waiting for room, and background \
                     traffic takes nothing new while the owner does",
                )
            } else {
                None
            }
        })
    }
}

impl ClimbTicket {
    /// Start the new rung for a **guest's** climb (§9, §12 entry 89): only
    /// if, under the same lock hold that claims the start, the model is still
    /// `Background`-owned and no owner admission is waiting for room. An
    /// owner who parked on the climb while it drained made the model theirs, and
    /// background traffic never climbs the owner's model; checked any
    /// earlier, their claim could land between the check and the start.
    /// Refused, the ticket is dropped and the running rung serves on.
    pub fn start_for_guest(self, spec: StartSpec) -> ClimbStart {
        self.start_unless(spec, |reg, e| {
            if e.owner != Origin::Background {
                Some(
                    "became the owner's model while the climb drained, and background traffic \
                     never climbs it",
                )
            } else if reg.draining_for_owner() {
                Some(
                    "cannot be climbed now: the owner is waiting for room, and background \
                     traffic takes nothing new while the owner does",
                )
            } else {
                None
            }
        })
    }
}
