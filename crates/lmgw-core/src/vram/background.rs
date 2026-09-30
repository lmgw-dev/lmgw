//! Background traffic on the GPU — the admission half (candidate-aliases
//! design §4.1–§4.5, §9; §12 entries 45–48).
//!
//! A candidate alias with the `background` flag is a **guest**: it may use
//! what the owner is not using, and it never evicts, waits on or climbs
//! anything of theirs. The registry half — who owns a container, the claim that
//! never starts one, the draining mark — is
//! [`crate::runtime::registry::Origin`] and its module; this is what the
//! request gate calls, and what decides whether a guest may start or climb.
//!
//! - **[`join`]** claims a candidate only when it is loaded (`ready`, or a
//!   start or climb in flight), and never starts it (entry 46). Both modes use
//!   it for a loaded candidate, so an alias never starts an alternate. A
//!   guest's join refuses every resident model while the owner waits for
//!   room (§4.5, entry 47).
//! - **[`start_background`]** is §4.3 step 2, "the primary can be loaded
//!   without disturbing the owner". It never waits for the owner and never
//!   queues for the gate: it takes the admission gate with a **try-lock** (a
//!   gate somebody holds is an admission in progress, the owner's perhaps,
//!   and counts as "cannot start without disturbing them"), measures like
//!   [`VramScheduler::check_background_start`], and when short evicts only
//!   **idle `Background`-owned** models, LRU-first, re-measuring after each.
//!   Anything else in the way is [`BackgroundStart::Blocked`], naming it. The
//!   one thing it waits for is **another guest's start of the same model**
//!   ([`GuestTurns`], §12 entry 91): parallel requests of a background job
//!   ride the one start their primary gets, instead of losing the try-lock
//!   to it and going to the fallback while it comes up.
//! - **[`Restart`]** is the rule a hold's model is brought back by when its
//!   container goes away under it (a dead container, a stop, a failed climb):
//!   admission as for any start, the guest's start rule, or never — an
//!   alternate the alias only joined. The last two end in
//!   [`GatewayError::CandidateLost`] when they cannot, which the gate answers
//!   by picking again.
//! - **The guest's climb** ([`climb_denied`], [`admit_climb_background`], §9):
//!   only a `Background`-owned model, only its alias's primary, and only into
//!   VRAM that is free now — never an eviction, never a wait at the gate.
//!   Denied, it is [`super::Climbed::Denied`], and the gate picks again
//!   (entry 45).
//! - **Draining** is taken by owner admissions ([`owner_drain`]): `decide`'s
//!   make-room wait and a climb's, but only on an install that has a
//!   background alias at all, so nothing changes where none exists.
//!
//! **No deadlock by construction.** Nothing here ever waits for the admission
//! gate, for the draining mark or for an owner's admission: the gate is only
//! ever *tried*, the mark is a counter nobody awaits, and a join parks only on
//! a start or a climb in flight, each bounded by its own load timeout and
//! budget. A guest holding the gate evicts at most one idle guest per look,
//! and gives the gate up as soon as an owner admission queues behind it. The
//! one lock a guest awaits is its model's [`GuestTurn`]: only guest starts
//! take one, one at a time and never with the gate or a claim held while
//! waiting, and it is held across one decision only — which tries the gate
//! and stops idle guests, and waits for nothing else.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{Route, Snapshot};
use crate::error::GatewayError;
use crate::hf::fmt_bytes;
use crate::runtime::descriptor::{model_runtime, ModelRuntime};
use crate::runtime::registry::{
    ClimbRun, ClimbStart, ClimbTicket, Origin, OwnerDrain, RuntimeError, StartSpec,
};
use crate::runtime::Class;
use crate::state::SharedState;

use super::{
    classify, lifecycle_spec, upstream_error, Ledger, LocalHold, Target, VramScheduler, MIB,
};

/// How a hold's model may be brought back when its container is gone under
/// the hold — [`LocalHold::recover`]'s dead container, or a
/// [`LocalHold::sync`] that finds the entry gone after a stop or a failed
/// climb. Set when the claim is taken, for the life of the hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// Admission, exactly as for a new request: evict, queue, start. Every
    /// hold [`super::admit`] and [`super::admit_or_external`] give — every
    /// caller there was before candidate aliases, unchanged — and an owner
    /// request's primary.
    Admit,
    /// The guest's start rule ([`start_background`]): only into room it can
    /// make without disturbing the owner, else
    /// [`GatewayError::CandidateLost`]. A background request's primary.
    Background,
    /// Never: the model was joined because it was already loaded — an
    /// alternate, which its alias never starts (§4.1, entry 46).
    /// [`GatewayError::CandidateLost`].
    No,
}

/// What [`start_background`] came to.
#[derive(Debug)]
pub enum BackgroundStart {
    /// The model is up and claimed for a guest: it joined a container that
    /// was already there, or started one into room it could make without
    /// disturbing the owner. `Background`-owned if this call started it.
    Started(LocalHold),
    /// It cannot be started without disturbing the owner, and this says what
    /// is in the way. Written to read after "GPU in use by " — the gate's
    /// deferral message (`GatewayError::GpuDeferred`) — e.g. "chat/qwen
    /// (18.0 GiB, 1 in flight) — chat/p needs 12.0 GiB and 3.0 GiB is free".
    /// Nothing was stopped but idle guests, and nothing waited.
    Blocked(String),
}

/// Claim the model `route` names **only if it is loaded** — `ready`, or a
/// start or climb in flight — and never start it (§4.1 "loaded", §12 entry
/// 46). `Ok(None)` is "not loaded for this claim": no container, one being
/// stopped, a guest's claim while the owner waits for room (§4.5), a start it
/// joined abandoned by its client, or a local id no row describes. The gate
/// then moves on — to the next candidate, or to its fallback.
///
/// `alias` is the name the client asked for (the candidate alias): the hold
/// carries it for the queue view, the logs and the request's fallback
/// ([`Snapshot::request_fallback`]). `origin` is whose claim it is — an owner
/// claim makes the model theirs ([`Origin`]). `restart` is how the model may be
/// brought back if its container goes away under the hold ([`Restart`]): the
/// gate passes [`Restart::No`] for an alternate.
///
/// Under the GPU hold this is `gpu_hold`, like every admission: nothing local
/// takes new work then, loaded or not (gpu-hold design §2). A start it joined
/// that failed, or that a stop landed on, is that start's error (502).
pub async fn join(
    state: &SharedState,
    route: &Route,
    alias: &str,
    origin: Origin,
    restart: Restart,
) -> Result<Option<LocalHold>, GatewayError> {
    let Some(target) = classify(route) else {
        return Ok(None);
    };
    let snap = state.snapshot();
    state
        .vram
        .join_at(state, &snap, &target, alias, origin, restart)
        .await
}

/// §4.3 step 2: start the model `route` names for a **guest**, if that is
/// possible without disturbing the owner — or join it, when it is loaded
/// already. Never waits for the owner, never queues for the gate, never
/// evicts anything of the owner's (module doc). Another guest's start of the
/// same model is waited for — its turn ([`GuestTurns`]), within
/// `vram.queue_timeout_seconds` — and then joined (§12 entry 91). The hold it
/// returns is a guest's ([`Origin::Background`]) and restarts by this same
/// rule ([`Restart::Background`]).
///
/// [`BackgroundStart::Blocked`] names what is in the way, for the gate's
/// deferral message — every one of these is blocked:
/// - the model is loaded but draining: the owner waits for room (§4.5);
/// - another admission holds the admission gate, or an owner admission is
///   queued for it;
/// - another guest's start of the same model did not decide within
///   `vram.queue_timeout_seconds`;
/// - lmgw cannot measure the card (VRAM admission off, or no telemetry and no
///   `vram.budget_mb`): a guest cannot promise not to disturb the owner on a
///   card nothing measures, so it only uses models that are already loaded
///   (§12 phase-4 decision; the owner's own requests start unarbitrated then,
///   as they always have);
/// - it does not fit, and evicting every idle guest did not make it fit.
///
/// Errors: `gpu_hold` under the GPU hold; `vram_too_large` for a model that
/// cannot fit on an empty card (a configuration error, never a deferral —
/// §4.7's rule); a start that fails (502).
pub async fn start_background(
    state: &SharedState,
    route: &Route,
    alias: &str,
) -> Result<BackgroundStart, GatewayError> {
    let Some(target) = classify(route) else {
        return Err(GatewayError::Internal(format!(
            "'{alias}' routes to '{}', which is not a local model — only a local model can be \
             started for background traffic",
            route.upstream_model
        )));
    };
    let snap = state.snapshot();
    let Some(runtime) = model_runtime(&snap, target.class, &target.model_id) else {
        return Ok(BackgroundStart::Blocked(format!(
            "nothing lmgw can start — '{}' is no longer configured",
            target.model_id
        )));
    };
    state
        .vram
        .start_background_at(state, &snap, &target, &runtime, alias)
        .await
}

/// Mark every resident model draining for the owner (§4.5, entry 47), for
/// as long as the returned value lives — an owner admission's make-room wait.
/// `None` on an install without a background alias: there is no guest to
/// drain for, and the runtime frame stays exactly what it was.
///
/// A guest's in-process run — an agent run, a `/v1/responses` tool loop —
/// holds one claim for the whole run, so a wait for its model lasts the run:
/// a request that has started always finishes (§4.5).
pub(super) fn owner_drain(state: &SharedState, snap: &Snapshot) -> Option<OwnerDrain> {
    snap.any_background_alias()
        .then(|| state.runtime().drain_for_owner())
}

/// Which residents an eviction may stop ([`VramScheduler::evict_one`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Victims {
    /// Any idle one — the owner's admissions, as always (§4.4).
    Any,
    /// Only idle `Background`-owned ones, stopped only while they still are
    /// ([`crate::runtime::registry::Registry::stop_idle_background`]) — a
    /// guest's start (§4.4).
    IdleBackground,
}

impl VramScheduler {
    /// [`join`] on a resolved target.
    pub(super) async fn join_at(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        alias: &str,
        origin: Origin,
        restart: Restart,
    ) -> Result<Option<LocalHold>, GatewayError> {
        if let Some(block) = snap.gpu_block() {
            return Err(block.refusal(target.model_id.clone(), ""));
        }
        // No row: nothing a hold could ever restart, and nothing this
        // gateway treats as its model.
        if model_runtime(snap, target.class, &target.model_id).is_none() {
            return Ok(None);
        }
        match state
            .runtime()
            .join(target.class, &target.model_id, origin)
            .await
        {
            Ok(Some(guard)) => Ok(Some(LocalHold::claimed(
                state, target, alias, guard, origin, restart,
            ))),
            Ok(None) => Ok(None),
            Err(e) => Err(upstream_error(e)),
        }
    }

    /// [`start_background`] on a resolved target and descriptor — also the
    /// restart of a guest's hold whose container went away
    /// ([`Restart::Background`]).
    ///
    /// **The loop, and why it ends.** Each pass either answers or goes round
    /// because another actor changed the model under it: it was not loaded
    /// when joined, and was up (or reserved) by the time the gate was held —
    /// then the next pass joins it — or a climb this start parked on failed.
    /// The only wait between passes is for this model's turn, bounded by
    /// `vram.queue_timeout_seconds` from this call's start (0: as long as the
    /// guest before it decides, which waits for nothing). No number of passes
    /// is right that is not invented.
    pub(super) async fn start_background_at(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        runtime: &ModelRuntime,
        alias: &str,
    ) -> Result<BackgroundStart, GatewayError> {
        let label = format!("{}/{}", target.class.as_str(), target.model_id);
        let queue_seconds = snap.settings.vram.queue_timeout_seconds;
        let deadline =
            (queue_seconds > 0).then(|| Instant::now() + Duration::from_secs(queue_seconds));
        loop {
            // The hold or a benchmark's lease, as it is now: `snap` is this
            // call's entry, and a pass after the first comes back from a wait
            // (its turn, a start it went round for).
            if let Some(block) = state.snapshot().gpu_block() {
                return Err(block.refusal(target.model_id.clone(), ""));
            }
            // Loaded already: a guest joins it like any candidate (§4.3
            // step 1) — refused while the owner waits for room.
            if let Some(hold) = self
                .join_at(
                    state,
                    snap,
                    target,
                    alias,
                    Origin::Background,
                    Restart::Background,
                )
                .await?
            {
                return Ok(BackgroundStart::Started(hold));
            }
            if let Some(why) = self.owner_waiting(state) {
                return Ok(BackgroundStart::Blocked(why));
            }
            if !snap.settings.vram.enabled {
                return Ok(BackgroundStart::Blocked(unmeasured(
                    "VRAM admission is off (vram.enabled)",
                )));
            }
            let Some(fp) = self.footprint(snap, target.class, &target.model_id).await else {
                return Ok(BackgroundStart::Blocked(format!(
                    "nothing lmgw can start — no row describes {label}"
                )));
            };
            let headroom = snap.settings.vram.headroom_mb.saturating_mul(MIB);
            let needs = fp.total_bytes.saturating_add(headroom);

            // Another guest deciding a start of this very model is waited
            // for, not raced for the gate (§12 entry 91): the gate it holds
            // is background traffic's own, and the start it decides is the
            // one this request wants. After it, the gate is tried as ever,
            // and a start it made is up or reserved — the next look joins it.
            let Some(turn) = self.guest_turns.take(target, deadline).await else {
                return Ok(BackgroundStart::Blocked(format!(
                    "another background start of {label}, which had not decided within \
                     vram.queue_timeout_seconds ({queue_seconds}s)"
                )));
            };
            // Never waited for: a gate somebody holds is an admission in
            // progress — the owner's, perhaps, waiting for room — and a guest
            // does not queue behind it (entry 46). tokio's mutex hands a
            // released gate to whoever is queued for it, so this fails while
            // anyone is.
            let Ok(gate) = self.gate.try_lock() else {
                return Ok(BackgroundStart::Blocked(
                    "another admission, which holds the admission gate right now".into(),
                ));
            };
            // `Some(reservation)` = start it; `None` = it came up meanwhile,
            // join it on the next pass.
            let decided = loop {
                if let Some(why) = self.owner_waiting(state) {
                    return Ok(BackgroundStart::Blocked(why));
                }
                if self.is_up(state, target) {
                    break None;
                }
                let l = self.ledger(state, snap).await;
                // Again after the ledger read, the last await before the
                // claim, and under the gate — which a benchmark's drain takes
                // once before it looks, so it never looks while a start it
                // cannot see yet is being decided (benchmark design §3.2).
                if let Some(block) = state.snapshot().gpu_block() {
                    return Err(block.refusal(target.model_id.clone(), ""));
                }
                let Some(cap) = l.capacity.as_ref() else {
                    return Ok(BackgroundStart::Blocked(unmeasured(
                        l.inactive_reason
                            .as_deref()
                            .unwrap_or("no capacity to measure against"),
                    )));
                };
                if needs > cap.total {
                    return Err(GatewayError::VramTooLarge {
                        model: target.model_id.clone(),
                        need: fmt_bytes(fp.total_bytes),
                        headroom: fmt_bytes(headroom),
                        capacity: fmt_bytes(cap.total),
                    });
                }
                // A start already decided for it (another admission's
                // reservation) accounts for its memory; the `acquire` below
                // joins that start or makes it, like `decide`'s own case.
                if l.residents
                    .iter()
                    .any(|r| r.container == target.class && r.model == target.model_id)
                {
                    break Some(None);
                }
                if cap.free >= needs {
                    break Some(Some(self.reserve(state, target, fp.total_bytes)));
                }
                if !self
                    .evict_one(state, target, &l, Victims::IdleBackground)
                    .await
                {
                    return Ok(BackgroundStart::Blocked(in_use(
                        &l, &label, needs, cap.free,
                    )));
                }
            };
            drop(gate);
            drop(turn);
            let Some(reservation) = decided else {
                continue;
            };
            let guard = state
                .runtime()
                .acquire_as(&lifecycle_spec(state, snap, runtime), Origin::Background)
                .await;
            drop(reservation);
            match guard {
                Ok(guard) => {
                    self.cache_pids(state);
                    tracing::info!(
                        alias = %alias,
                        "{label} is up for background traffic ('{alias}')"
                    );
                    return Ok(BackgroundStart::Started(LocalHold::claimed(
                        state,
                        target,
                        alias,
                        guard,
                        Origin::Background,
                        Restart::Background,
                    )));
                }
                // It parked on a climb that could not start its rung, and the
                // model is gone: look again from the top.
                Err(RuntimeError::ClimbFailed { .. }) => continue,
                Err(e) => return Err(upstream_error(e)),
            }
        }
    }

    /// Why a guest must not start or climb anything right now because of the
    /// owner, if it must not: an owner admission waits for room (the
    /// draining mark, §4.5), or one is queued for the gate — a start's, or
    /// an owner climb's. A guest's own queue rows (a guest climb shows in the
    /// queue while it drains) are never a reason: background traffic gives
    /// way to the owner, not to itself (§12 entry 91).
    pub(super) fn owner_waiting(&self, state: &SharedState) -> Option<String> {
        let draining = state.runtime().draining_for_owner();
        let live = self.live.lock().unwrap();
        let first = live.queue.iter().find(|w| w.origin == Origin::Owner);
        match (draining, first) {
            (true, Some(w)) => Some(format!(
                "the owner — '{}' is waiting for room for {}/{}",
                w.alias,
                w.class.as_str(),
                w.model
            )),
            (true, None) => Some("the owner, who is waiting for room".into()),
            (false, Some(w)) => Some(format!(
                "'{}' ({}/{}), queued for room",
                w.alias,
                w.class.as_str(),
                w.model
            )),
            (false, None) => None,
        }
    }
}

/// Background starts, one at a time **per model** (§12 entry 91).
///
/// A background job's parallel requests to a primary that is not loaded all
/// want the same start. The first takes the admission gate to decide it —
/// measure, evict idle guests, reserve — and before this the rest failed the
/// gate's try-lock on it and went to the fallback while their primary came
/// up. A turn per model makes them wait for that decision instead; after it
/// the model is up or reserved, and the next look joins its start. Only guest
/// starts take a turn, so it never stands between the owner and the gate,
/// and a start of another model is not waited for (it may still hold the
/// gate a moment, and a guest does not queue behind that).
#[derive(Default)]
pub(super) struct GuestTurns {
    turns: std::sync::Mutex<HashMap<TurnKey, Arc<tokio::sync::Mutex<()>>>>,
}

/// Which model a turn is for.
type TurnKey = (Class, String);

/// One guest start's turn at deciding one model's start; the next waiting
/// guest gets it on drop.
pub(super) struct GuestTurn<'a> {
    turns: &'a GuestTurns,
    key: TurnKey,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl GuestTurns {
    /// Wait for `target`'s turn until `deadline` (`None`: for as long as the
    /// guest before decides, which waits for nothing). `None` when the
    /// deadline came first. FIFO, like the admission gate.
    async fn take(&self, target: &Target, deadline: Option<Instant>) -> Option<GuestTurn<'_>> {
        let key = (target.class, target.model_id.clone());
        let turn = Arc::clone(self.turns.lock().unwrap().entry(key.clone()).or_default());
        let locked = turn.lock_owned();
        let guard = match deadline {
            Some(d) => tokio::time::timeout_at(tokio::time::Instant::from_std(d), locked)
                .await
                .ok()?,
            None => locked.await,
        };
        Some(GuestTurn {
            turns: self,
            key,
            guard: Some(guard),
        })
    }
}

impl Drop for GuestTurn<'_> {
    fn drop(&mut self) {
        let mut turns = self.turns.turns.lock().unwrap();
        drop(self.guard.take());
        // Nobody holds or waits for it any more (a waiter clones it under
        // this lock): the map keeps only models a guest start is deciding.
        if turns
            .get(&self.key)
            .is_some_and(|t| Arc::strong_count(t) == 1)
        {
            turns.remove(&self.key);
        }
    }
}

impl LocalHold {
    /// A hold on a claim [`join`] or [`start_background`] took.
    pub(super) fn claimed(
        state: &SharedState,
        target: &Target,
        alias: &str,
        guard: crate::runtime::registry::AcquireGuard,
        origin: Origin,
        restart: Restart,
    ) -> Self {
        Self {
            guard: std::sync::Mutex::new(guard),
            state: state.clone(),
            target: target.clone(),
            alias: alias.to_string(),
            recovering: tokio::sync::Mutex::new(()),
            policy: None,
            origin,
            restart,
        }
    }

    /// [`Self::recover`]'s stop of the container that stopped answering on
    /// `port`: forced, and only the `generation` that failed. The owner's
    /// holds stop it as they always have. A **guest's** hold stops it only
    /// while it is still a guest's, decided in the lock hold that marks it
    /// stopping ([`crate::runtime::registry::Registry::stop_dead_background`]),
    /// and never the owner's model (review R, finding 11; §12 entry 90): what
    /// looked dead to background traffic may be one connection that failed,
    /// a forced stop would kill the owner's own requests on it, and if it
    /// really is down their own next request brings it back. The guest's
    /// request is then [`GatewayError::CandidateLost`], and the gate picks
    /// again. `Ok` carries the stop's own outcome, for the caller to log.
    pub(super) async fn stop_dead(
        &self,
        port: u16,
        generation: u64,
    ) -> Result<Result<(), RuntimeError>, GatewayError> {
        let (class, model_id) = (self.target.class, self.target.model_id.as_str());
        let registry = self.state.runtime();
        if self.origin != Origin::Background {
            tracing::warn!(
                "{class} model '{model_id}' stopped answering on port {port} — stopping its \
                 container and acquiring a fresh one"
            );
            return Ok(registry
                .stop_generation(class, model_id, generation, true)
                .await);
        }
        match registry
            .stop_dead_background(class, model_id, generation)
            .await
        {
            Err(e) if e.claimed_by_owner() => {
                tracing::warn!(
                    alias = %self.alias,
                    "{class} model '{model_id}' stopped answering background traffic for '{}' on \
                     port {port} — it is the owner's model, so a guest does not stop it; picking \
                     again",
                    self.alias
                );
                Err(GatewayError::CandidateLost {
                    model: model_id.to_string(),
                    detail: format!(
                        "it stopped answering '{}', and it is the owner's model, which background \
                         traffic never stops — their own next request brings it back if it is \
                         really down",
                        self.alias
                    ),
                })
            }
            stopped => {
                tracing::warn!(
                    alias = %self.alias,
                    "{class} model '{model_id}' stopped answering background traffic for '{}' on \
                     port {port} — stopping the guest's container",
                    self.alias
                );
                Ok(stopped)
            }
        }
    }

    /// The fresh claim for a hold whose model is gone, by this hold's
    /// [`Restart`] rule when it is not [`Restart::Admit`] (that one is
    /// `readmit_base`'s own admission). [`GatewayError::CandidateLost`] when
    /// the rule says the request may not bring it back: the gate answers it
    /// at the send by picking again (entry 45), never by a normal admission.
    pub(super) async fn restart_candidate(&self) -> Result<LocalHold, GatewayError> {
        let model_id = self.target.model_id.clone();
        let lost = |detail: String| GatewayError::CandidateLost {
            model: model_id.clone(),
            detail,
        };
        match self.restart {
            Restart::Admit => Err(GatewayError::Internal(
                "restart_candidate is for candidate holds; an Admit hold re-admits".into(),
            )),
            Restart::No => Err(lost(format!(
                "'{}' used it because it was already loaded, and a candidate alias never \
                 starts a model it only uses while loaded",
                self.alias
            ))),
            Restart::Background => {
                let snap = self.state.snapshot();
                let Some(runtime) = model_runtime(&snap, self.target.class, &model_id) else {
                    return Err(lost("it is no longer configured".into()));
                };
                match self
                    .state
                    .vram
                    .start_background_at(&self.state, &snap, &self.target, &runtime, &self.alias)
                    .await?
                {
                    BackgroundStart::Started(hold) => Ok(hold),
                    BackgroundStart::Blocked(why) => Err(lost(format!(
                        "background traffic restarts it only without disturbing the owner, and \
                         the GPU is in use by {why}"
                    ))),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The guest's climb (§4.3 "Ladders", §9, ladder design §12 entry 9)
// ---------------------------------------------------------------------------

/// May a guest's hold climb its model to a rung that `needs` bytes (its
/// footprint plus the headroom)? `None` = yes; `Some` says why not. Asked
/// before anything is marked, so a denied climb never touches the running
/// rung. Only [`Origin::Background`] holds come here.
///
/// A guest climbs only (§9, §4.3):
/// - a model it owns — `Background`-owned: never one the owner uses;
/// - while the owner is not waiting for room, and no owner admission is
///   queued (another guest's climb is not a reason, §12 entry 91);
/// - its alias's **primary** — a model that is some other background alias's
///   primary, used here as an alternate, is not this alias's to reload;
/// - into VRAM that is free now, with the running rung's own footprint
///   counted as freed (it is what the climb stops) — no eviction, no wait. A
///   card lmgw cannot measure is not free VRAM.
pub(super) async fn climb_denied(
    state: &SharedState,
    snap: &Snapshot,
    hold: &LocalHold,
    needs: u64,
    rung: &str,
) -> Option<String> {
    let model = hold.model_id();
    if state.runtime().owner_of(hold.class(), model) == Some(Origin::Owner) {
        return Some(format!(
            "'{model}' is the owner's model now, and background traffic never climbs it"
        ));
    }
    if let Some(why) = state.vram.owner_waiting(state) {
        return Some(format!("the GPU is in use by {why}"));
    }
    if let Some(ca) = snap.candidate_alias(&hold.alias) {
        if ca.primary() != Some(model) {
            return Some(format!(
                "'{model}' is an alternate of '{}', and background traffic climbs only its \
                 alias's primary",
                ca.alias
            ));
        }
    }
    let running = running_bytes(state, snap, &hold.target).await;
    match free_for_climb(state, snap, running).await {
        Err(why) => Some(why),
        Ok(free) if free >= needs => None,
        Ok(free) => Some(too_small(rung, needs, free, running)),
    }
}

/// What a guest's climb came to at its admission ([`admit_climb_background`]).
pub(super) enum GuestClimb {
    /// The new rung's start is claimed and running.
    Started(ClimbRun),
    /// Not now, and why. The ticket was dropped: the mark is cleared and the
    /// running rung serves on, untouched.
    Denied(String),
    /// A stop took the model before the start could be claimed.
    Gone,
}

/// Step 4 of a guest's climb, after its drain: the replacement admission
/// with every rule of [`climb_denied`] asked again — the drain can have taken
/// a while — and **no eviction and no wait**: the admission gate is only
/// tried, and a rung that does not fit the VRAM free now (the running rung
/// counted as freed) is denied. When it fits, the start is claimed while the
/// gate is held, exactly as the owner's climb claims it, so nothing else is
/// told the memory the running rung frees — and the owner check is made in
/// the registry's lock hold that claims it ([`ClimbTicket::start_for_guest`]).
#[allow(clippy::too_many_arguments)]
pub(super) async fn admit_climb_background(
    state: &SharedState,
    snap: &Snapshot,
    target: &Target,
    ticket: ClimbTicket,
    spec: StartSpec,
    needs: u64,
    running: u64,
    rung: &str,
) -> GuestClimb {
    let Ok(_gate) = state.vram.gate.try_lock() else {
        return GuestClimb::Denied(
            "the GPU is in use by another admission, which holds the admission gate right now"
                .into(),
        );
    };
    if let Some(why) = state.vram.owner_waiting(state) {
        return GuestClimb::Denied(format!("the GPU is in use by {why}"));
    }
    let free = match free_for_climb(state, snap, running).await {
        Ok(free) => free,
        Err(why) => return GuestClimb::Denied(why),
    };
    if free < needs {
        return GuestClimb::Denied(too_small(rung, needs, free, running));
    }
    // Whose model it is, and whether the owner waits for room, are asked
    // again in the lock hold that claims the start (§12 entry 89): an owner
    // claim landing after an earlier look would otherwise be climbed over.
    match ticket.start_for_guest(spec) {
        ClimbStart::Started(run) => GuestClimb::Started(run),
        ClimbStart::Refused(why) => GuestClimb::Denied(format!("'{}' {why}", target.model_id)),
        ClimbStart::Gone => GuestClimb::Gone,
    }
}

/// The running rung's footprint — what a climb frees by stopping it.
async fn running_bytes(state: &SharedState, snap: &Snapshot, target: &Target) -> u64 {
    let charge = state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.class == target.class && v.model_id == target.model_id)
        .and_then(|v| v.charge);
    state
        .vram
        .footprint_at(snap, target.class, &target.model_id, charge.as_ref())
        .await
        .map_or(0, |f| f.total_bytes)
}

/// Free VRAM for a climb, the running rung's `running` bytes counted as
/// freed — or why a guest cannot know it.
async fn free_for_climb(state: &SharedState, snap: &Snapshot, running: u64) -> Result<u64, String> {
    if !snap.settings.vram.enabled {
        return Err(format!(
            "the GPU is in use by {}",
            unmeasured("VRAM admission is off (vram.enabled)")
        ));
    }
    let l = state.vram.ledger(state, snap).await;
    match l.capacity.as_ref() {
        Some(cap) => Ok(cap.free.saturating_add(running)),
        None => Err(format!(
            "the GPU is in use by {}",
            unmeasured(
                l.inactive_reason
                    .as_deref()
                    .unwrap_or("no capacity to measure against")
            )
        )),
    }
}

fn too_small(rung: &str, needs: u64, free: u64, running: u64) -> String {
    format!(
        "{rung} needs {} and only {} is free with the running rung's {} counted — background \
         traffic climbs only into free VRAM",
        fmt_bytes(needs),
        fmt_bytes(free),
        fmt_bytes(running)
    )
}

// ---------------------------------------------------------------------------
// Words
// ---------------------------------------------------------------------------

/// A card lmgw cannot measure, said to read after "GPU in use by ".
fn unmeasured(reason: &str) -> String {
    format!(
        "whatever runs on it — lmgw cannot measure this card ({reason}), so background traffic \
         cannot promise to leave the owner's models alone and only uses models that are already \
         loaded; declare vram.budget_mb, or turn vram.enabled on"
    )
}

/// What holds the memory a guest's start could not get, to read after "GPU
/// in use by ": every resident (none of them an idle guest any more — those
/// were evicted), or the applications outside lmgw when nothing of lmgw's is
/// on the card.
fn in_use(l: &Ledger, label: &str, needs: u64, free: u64) -> String {
    let who = if l.residents.is_empty() {
        "applications outside lmgw".to_string()
    } else {
        let mut parts: Vec<String> = l
            .residents
            .iter()
            .map(|r| {
                format!(
                    "{}/{} ({}{})",
                    r.container.as_str(),
                    r.model,
                    fmt_bytes(r.estimated_bytes),
                    if r.in_flight > 0 {
                        format!(", {} in flight", r.in_flight)
                    } else {
                        String::new()
                    }
                )
            })
            .collect();
        parts.sort();
        parts.join(", ")
    };
    format!(
        "{who} — {label} needs {} and {} is free",
        fmt_bytes(needs),
        fmt_bytes(free)
    )
}
