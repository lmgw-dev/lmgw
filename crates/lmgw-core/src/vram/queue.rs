//! The admission queue: the gate, the decide/evict loop, and a start's entry
//! points into it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Snapshot;
use crate::error::GatewayError;
use crate::hf::fmt_bytes;
use crate::runtime::descriptor::{model_runtime, ModelRuntime};
use crate::runtime::registry::{Origin, RuntimeError, RuntimeState, RuntimeView};
use crate::runtime::Class;
use crate::state::SharedState;

use super::background::{self, Restart};
use super::ledger::Ledger;
use super::scheduler::{Waiter, CONTROL_TIMEOUT, MIB, POLL};
use super::{
    Admission, Decided, ExternalFallback, Fit, Gated, LocalHold, StartPermit, Target, VramScheduler,
};

impl VramScheduler {
    /// [`Self::admit_local`] for a caller without a fallback, which is only
    /// ever admitted or refused.
    pub(super) async fn admit_plain(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        runtime: &ModelRuntime,
        alias: &str,
    ) -> Result<Option<LocalHold>, GatewayError> {
        match self
            .admit_local(state, snap, target, runtime, alias, None)
            .await?
        {
            Admission::Admitted(hold) => Ok(hold),
            // Only a caller that offered a fallback is ever answered this way.
            Admission::External(short) => Err(GatewayError::Internal(format!(
                "admission answered with a fallback nobody offered: {}",
                short.describe()
            ))),
        }
    }

    /// `fallback`: the caller's, still usable — the wait takes the verdict
    /// again on every pass while it is ([`Self::recheck`]). `None` for every
    /// caller without one, which is then exactly the admission of old.
    pub(super) async fn admit_local(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        runtime: &ModelRuntime,
        alias: &str,
        fallback: Option<&mut ExternalFallback<'_>>,
    ) -> Result<Admission, GatewayError> {
        // The GPU hold, before anything else (gpu-hold design §4). Every
        // request-shaped caller has already been re-routed or refused by
        // `Snapshot::resolve_for_request`, so reaching here under a hold means
        // a caller that resolves its own route: `LocalHold::recover`'s restart
        // of a dead container, the two quickdoc batch runners, and whatever is
        // written next. This is the net under all of them — without it a hold
        // is only as good as the discipline of every future call site, and one
        // missed site puts a container back on the card the owner is gaming on.
        // A benchmark's lease is the same net (benchmark design §3.2): the run
        // has the card to itself, and nothing of any class may start on it.
        if let Some(block) = snap.gpu_block() {
            return Err(block.refusal(target.model_id.clone(), ""));
        }
        let mut fallback = fallback;
        // The snapshot and descriptor a retry after a failed climb re-read.
        let mut reread: Option<(Arc<Snapshot>, ModelRuntime)> = None;
        let guard =
            loop {
                let (snap, runtime) = match &reread {
                    Some((s, r)) => (&**s, r),
                    None => (snap, runtime),
                };
                // Arbitrate only when there is something to arbitrate: the switch
                // is on, and the model is not already up. A model the registry
                // already holds takes no gate, no measurement and no queue —
                // admission must not put a serialization point in front of
                // traffic that fits.
                let up = self.is_up(state, target);
                let reservation = if snap.settings.vram.enabled && !up {
                    match self
                        .arbitrate(state, snap, target, alias, fallback.as_deref_mut())
                        .await?
                    {
                        Decided::Go(reservation) => reservation,
                        Decided::External(short) => return Ok(Admission::External(short)),
                    }
                } else {
                    None
                };

                // Asked again, of the snapshot as it is now: the footprint and
                // ledger reads above are awaits, and the lease (or the hold)
                // may have come on during them — the check at the top is this
                // request's arrival, not its start. The registry refuses a
                // start under the lease anyway (`runtime/registry/lease.rs`);
                // this is the same refusal in the admission's own words.
                if let Some(block) = state.snapshot().gpu_block() {
                    return Err(block.refusal(target.model_id.clone(), ""));
                }
                // Always. Admission decided *whether and when*; this is what makes
                // the model reachable, and it is the same call on every path that
                // got here.
                let guard = state
                    .runtime()
                    .acquire(&lifecycle_spec(state, snap, runtime))
                    .await;
                drop(reservation);
                match guard {
                    // The climb this request parked on could not start its rung,
                    // and the model is gone (ladder design §12 entry 22): it wanted
                    // the model, not that rung, so admission starts over — which
                    // cold-starts the base through the decision above instead of
                    // the registry starting it unarbitrated.
                    //
                    // Starting over means reading again what a new admission reads
                    // (§12 entry 51): the climb ran start to finish while this
                    // request waited, and the owner may have switched the hold on,
                    // or edited or deleted the row, meanwhile.
                    Err(RuntimeError::ClimbFailed { message, .. }) => {
                        tracing::debug!("re-admitting '{alias}' after a failed climb: {message}");
                        let now = state.snapshot();
                        if let Some(block) = now.gpu_block() {
                            return Err(block.refusal(target.model_id.clone(), ""));
                        }
                        let runtime = model_runtime(&now, target.class, &target.model_id)
                            .ok_or_else(|| GatewayError::Upstream {
                                status: 502,
                                provider_type: None,
                                message: format!(
                                    "{} model '{}' is no longer configured, so it could not be \
                                 started again after its climb failed",
                                    target.class, target.model_id
                                ),
                            })?;
                        reread = Some((now, runtime));
                        continue;
                    }
                    guard => {
                        if !up && guard.is_ok() {
                            // It came up just now: learn its PID while it is known
                            // to be here.
                            self.cache_pids(state);
                        }
                        break guard.map_err(upstream_error);
                    }
                }
            };
        Ok(Admission::Admitted(Some(LocalHold {
            guard: std::sync::Mutex::new(guard?),
            state: state.clone(),
            target: target.clone(),
            alias: alias.to_string(),
            recovering: tokio::sync::Mutex::new(()),
            policy: None,
            origin: Origin::Owner,
            restart: Restart::Admit,
        })))
    }

    /// Pre-flight for a start nobody is waiting on: the boot warm starts
    /// (§3.4) and the operator surface's `start`/`apply` (§8).
    ///
    /// These are not requests. A request that does not fit is what eviction
    /// and the queue exist for — somebody is holding the line for an answer.
    /// A warm start is background warmth, and an operator start is a "make
    /// this resident" the operator can repeat; neither is worth stopping a
    /// model that is actually in use, and a boot that evicts its way through
    /// a warm-start list would end with the last model up and the rest gone.
    /// So this only ever *refuses*: it never evicts, never queues and never
    /// waits.
    ///
    /// It is the same ledger the request path decides on, read the same way
    /// (measured free, minus what is committed but not yet on the card, plus
    /// the headroom setting) — not a second admission with its own arithmetic.
    /// The gate is taken for the measure-and-claim, exactly as
    /// [`Self::decide`] does, so N concurrent warm starts cannot each be told
    /// the same free bytes; the returned permit carries the reservation and is
    /// dropped once the start has settled.
    ///
    /// [`Fit::Unchecked`] is the inactive case — admission disabled, no
    /// capacity to measure against, or no row to size the model from — and
    /// means exactly what it does everywhere else in this module: the start
    /// proceeds unarbitrated, as it would on a box without this feature.
    pub async fn check_background_start(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        class: Class,
        model_id: &str,
    ) -> Fit {
        // Ahead of the `vram.enabled` check on purpose: hold is orthogonal to
        // admission (gpu-hold design §1). An install with admission switched
        // off, or no NVML at all, would otherwise answer `Unchecked` and warm
        // every flagged model straight onto the card the owner just took back.
        if let Some(block) = snap.gpu_block() {
            return held(&block, model_id);
        }
        if !snap.settings.vram.enabled {
            return Fit::Unchecked;
        }
        let target = Target {
            class,
            model_id: model_id.to_string(),
        };
        // Already up (or coming up): the start will join it, and it is
        // already charged to the ledger. Nothing to decide.
        if self.is_up(state, &target) {
            return Fit::Go(StartPermit { _reservation: None });
        }
        let Some(fp) = self.footprint(snap, class, model_id).await else {
            return Fit::Unchecked;
        };
        let headroom = snap.settings.vram.headroom_mb.saturating_mul(MIB);
        let needs = fp.total_bytes.saturating_add(headroom);

        let _gate = self.gate.lock().await;
        let l = self.ledger(state, snap).await;
        // Again, after the last await before the claim: `snap` is the one
        // this start began with, and the lease (or the hold) may have come on
        // while it waited for the footprint, the gate or the ledger. Under
        // the gate, so a benchmark's drain — which takes the gate once before
        // it looks — never looks while a start it cannot see yet is decided.
        if let Some(block) = state.snapshot().gpu_block() {
            return held(&block, model_id);
        }
        let Some(cap) = l.capacity.as_ref() else {
            return Fit::Unchecked;
        };
        if needs > cap.total {
            return Fit::Full(format!(
                "'{model_id}' needs {} plus {} headroom and the GPU holds {} in total",
                fmt_bytes(fp.total_bytes),
                fmt_bytes(headroom),
                fmt_bytes(cap.total)
            ));
        }
        if cap.free < needs {
            return Fit::Full(format!(
                "'{model_id}' needs {} and only {} is free — {}",
                fmt_bytes(needs),
                fmt_bytes(cap.free),
                describe_holders(&l)
            ));
        }
        Fit::Go(StartPermit {
            _reservation: Some(self.reserve(state, &target, fp.total_bytes)),
        })
    }

    /// Decide whether this model's container may start now, evicting first if
    /// that is what it takes. `Go(None)` = nothing to arbitrate against, so the
    /// start proceeds unarbitrated. `External` = while it waited, the
    /// shortfall became VRAM outside lmgw's control and the caller's fallback
    /// answers instead ([`Self::recheck`]).
    async fn arbitrate(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        alias: &str,
        fallback: Option<&mut ExternalFallback<'_>>,
    ) -> Result<Decided, GatewayError> {
        let Some(fp) = self.footprint(snap, target.class, &target.model_id).await else {
            return Ok(Decided::Go(None));
        };
        let vs = &snap.settings.vram;
        let headroom = vs.headroom_mb.saturating_mul(MIB);
        let needs = fp.total_bytes.saturating_add(headroom);

        // Is there anything to decide against at all?
        {
            let l = self.ledger(state, snap).await;
            let Some(cap) = l.capacity.as_ref() else {
                tracing::debug!(
                    "VRAM admission inactive: {}",
                    l.inactive_reason.as_deref().unwrap_or("no capacity")
                );
                return Ok(Decided::Go(None));
            };
            // A model bigger than the GPU never becomes admissible, however
            // long it waits, so it is refused here rather than queued forever.
            if needs > cap.total {
                return Err(GatewayError::VramTooLarge {
                    model: target.model_id.clone(),
                    need: fmt_bytes(fp.total_bytes),
                    headroom: fmt_bytes(headroom),
                    capacity: fmt_bytes(cap.total),
                });
            }
        }

        let started = Instant::now();
        let queued = self.enqueue(
            state,
            Waiter {
                id: self.next_id(),
                alias: alias.to_string(),
                model: target.model_id.clone(),
                class: target.class,
                needs,
                since: started,
                stage: "waiting for the admission gate".into(),
                origin: Origin::Owner,
            },
        );
        // A queue that forms should be visible while it exists, not once it is
        // over, so the dashboard is told at both ends of the wait — the
        // leaving end by `queued`'s drop, however the wait ends.
        broadcast(state);
        let result = self
            .decide(
                state,
                snap,
                target,
                alias,
                (fp.total_bytes, needs),
                queued.id,
                started,
                fallback,
            )
            .await;
        drop(queued);
        result
    }

    /// The decision, from behind the queue: take the gate, then measure →
    /// evict → measure until the model fits, and **claim the memory** — all
    /// inside the gate. The gate is released when this function returns, which
    /// is *before* the container starts, so two models load at once (§4).
    ///
    /// The claim belongs here rather than at the caller. Between "it fits" and
    /// "the container has taken the memory" the model is, to every other
    /// decision, invisible: the driver has not seen it and the registry does
    /// not hold it yet, so the next request would be told the same free bytes
    /// twice. The reservation is what closes that window, and it cannot be
    /// created outside the gate without reopening it.
    ///
    /// With a `fallback` still usable, every pass of the wait — behind the
    /// gate, and for room once it holds it — takes §4.7's verdict again
    /// ([`Self::recheck`]); `External` leaves the queue (the caller drops the
    /// waiter) with nothing reserved. `(bytes, needs)`: the footprint, and it
    /// plus the headroom.
    #[allow(clippy::too_many_arguments)]
    async fn decide(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        alias: &str,
        (bytes, needs): (u64, u64),
        waiter_id: u64,
        started: Instant,
        mut fallback: Option<&mut ExternalFallback<'_>>,
    ) -> Result<Decided, GatewayError> {
        let vs = &snap.settings.vram;
        let budget =
            (vs.queue_timeout_seconds > 0).then(|| Duration::from_secs(vs.queue_timeout_seconds));
        let expired = |holding: String| GatewayError::VramQueueTimeout {
            model: target.model_id.clone(),
            waited_seconds: started.elapsed().as_secs(),
            holding,
        };

        // Held for the make-room decision only. Dropped on every return path by
        // leaving scope — including the success path, where the reservation
        // takes over from it.
        let mut gate = match self
            .take_gate(state, target, alias, None, started, budget, &mut fallback)
            .await
        {
            Gated::Held(gate) => gate,
            Gated::External(short) => return Ok(Decided::External(short)),
            Gated::Expired => {
                return Err(expired(
                    "another request was still being admitted to the GPU".into(),
                ))
            }
        };

        self.set_stage(waiter_id, "measuring GPU memory");
        // Held from the first look that does not fit until this returns,
        // however it returns (candidate-aliases §4.5, §12 entry 47).
        let mut drain = None;
        loop {
            // Switched on while this request waited — the hold, or a
            // benchmark's lease (benchmark design §3.2): nothing new goes on
            // the card, however long this request has queued for it.
            if let Some(block) = state.snapshot().gpu_block() {
                return Err(block.refusal(target.model_id.clone(), ""));
            }
            let l = self.ledger(state, snap).await;
            let Some(cap) = l.capacity.as_ref() else {
                // Telemetry vanished mid-wait (driver reset). Forwarding
                // unarbitrated is the same behaviour as a gateway without this
                // feature, and strictly better than a hang.
                return Ok(Decided::Go(None));
            };

            // Somebody else started it while this request waited: their
            // reservation (or their registry entry) already accounts for it,
            // and `acquire` will join their start rather than launch a second.
            if l.residents
                .iter()
                .any(|r| r.container == target.class && r.model == target.model_id)
            {
                return Ok(Decided::Go(None));
            }

            if cap.free >= needs {
                self.set_stage(waiter_id, "starting the container");
                return Ok(Decided::Go(Some(self.reserve(state, target, bytes))));
            }

            if budget.is_some_and(|d| started.elapsed() >= d) {
                return Err(expired(describe_holders(&l)));
            }

            // Short of room: every resident model drains for the owner —
            // background traffic takes no new work on it, so the ones this
            // wait evicts or waits for do not stay busy with guests (§4.5).
            // A guest's run that holds one claim for a whole tool loop keeps
            // its model for the run: a request that has started finishes.
            if drain.is_none() {
                drain = background::owner_drain(state, snap);
            }
            self.set_stage(
                waiter_id,
                &format!(
                    "needs {}, {} free — evicting",
                    fmt_bytes(needs),
                    fmt_bytes(cap.free)
                ),
            );
            if self
                .evict_one(state, target, &l, background::Victims::Any)
                .await
            {
                continue;
            }
            // Everything resident is either busy or the target itself. Waiting
            // is the only correct move: the alternative is killing somebody's
            // half-finished generation, which is the thing `/slots` exists to
            // prevent.
            self.set_stage(waiter_id, "waiting for a busy model to finish");
            tokio::time::sleep(POLL).await;
            if let Some(short) = self.recheck(state, target, alias, &mut fallback).await {
                return Ok(Decided::External(short));
            }
            if self.climb_needs_the_gate(state, target) {
                drop(gate);
                gate = match self
                    .take_gate(state, target, alias, None, started, budget, &mut fallback)
                    .await
                {
                    Gated::Held(gate) => gate,
                    Gated::External(short) => return Ok(Decided::External(short)),
                    // Out of time back in the queue: what this request was
                    // waiting for is still the busy models it yielded over,
                    // not "another request being admitted" (§12 entry 74).
                    Gated::Expired => return Err(expired(describe_holders(&l))),
                };
            }
        }
    }

    /// Take the admission gate for a request queued since `started`, within
    /// its `budget` (`None`: no limit from it).
    ///
    /// Waited for in slices of `POLL` while a fallback could still answer, so
    /// the verdict is taken again behind the gate too — §4.7's for a start
    /// (`needed: None`), the climb's on its new rung (`Some`, ladder design
    /// §12 entry 8) — with one lock future for the whole wait, so its place in
    /// the gate's FIFO is kept across the slices. Without a fallback: one
    /// wait, bounded by the budget.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn take_gate<'g>(
        &'g self,
        state: &SharedState,
        target: &Target,
        alias: &str,
        needed: Option<u64>,
        started: Instant,
        budget: Option<Duration>,
        fallback: &mut Option<&mut ExternalFallback<'_>>,
    ) -> Gated<'g> {
        let lock = self.gate.lock();
        tokio::pin!(lock);
        loop {
            let slice = fallback.as_ref().is_some_and(|f| f.live()).then_some(POLL);
            let left = budget.map(|d| d.saturating_sub(started.elapsed()));
            let wait = match (left, slice) {
                (Some(l), Some(p)) => Some(l.min(p)),
                (l, None) => l,
                (None, p) => p,
            };
            let got = match wait {
                Some(w) => tokio::time::timeout(w, &mut lock).await.ok(),
                None => Some((&mut lock).await),
            };
            if let Some(gate) = got {
                return Gated::Held(gate);
            }
            if budget.is_some_and(|d| started.elapsed() >= d) {
                return Gated::Expired;
            }
            let short = match needed {
                None => self.recheck(state, target, alias, fallback).await,
                Some(n) => self.climb_recheck(state, target, n, alias, fallback).await,
            };
            if let Some(short) = short {
                return Gated::External(short);
            }
        }
    }

    /// Is a ladder climb of another model queued for this gate right now
    /// (review finding 8 and S1, §12 entries 53 and 71)? Marked, not yet
    /// started, and past its drain: while it drains it cannot use the gate,
    /// and yielding then would only hand the gate to whoever queued behind
    /// this admission, for as long as the drain lasts.
    ///
    /// An admission that holds the gate while it waits for a busy model asks
    /// this after each wait, and if so gives the gate up and queues for it
    /// again: the climb's model is busy *because* its trigger waits on the
    /// climb, and the climb waits on the gate, so holding it would only end
    /// when one of the two budgets does (never, with no budget). The mutex is
    /// FIFO, so whoever queued meanwhile — the climb among them — decides
    /// first, and this admission measures again after them.
    pub(crate) fn climb_needs_the_gate(&self, state: &SharedState, target: &Target) -> bool {
        state.runtime().list().iter().any(|e| {
            e.state == RuntimeState::Ready
                && e.climbing.as_ref().is_some_and(|c| c.at_gate)
                && !(e.class == target.class && e.model_id == target.model_id)
        })
    }

    /// Stop the least recently used idle container. Returns false when every
    /// resident model is busy — the caller then waits rather than killing work.
    ///
    /// `victims`: any idle model, or only idle `Background`-owned ones — a
    /// guest's start, which never evicts the owner's (candidate-aliases
    /// §4.4).
    pub(super) async fn evict_one(
        &self,
        state: &SharedState,
        target: &Target,
        l: &Ledger,
        victims: background::Victims,
    ) -> bool {
        // Residency in the registry is what makes a model a candidate, never a
        // nonzero estimate (§4): a resident whose footprint reads zero would be
        // unevictable forever, which is how a GPU fills up with models nothing
        // can reclaim.
        let mut candidates: Vec<&RuntimeView> = l
            .entries
            .iter()
            .filter(|e| {
                // `starting` is excluded, and not because stopping it would
                // fail: it would *succeed*. A start in progress has a waiter
                // by construction — the request that claimed it is inside
                // `acquire`, with no in-flight count yet to protect it — so
                // evicting one kills a request that has already been admitted,
                // and does it in the window where the victim's own footprint
                // is not on the card yet either (the ledger charges it
                // whole). Waiting for it to finish is what the LRU is for.
                e.state == RuntimeState::Ready
                    && e.in_flight == 0
                    && !(e.class == target.class && e.model_id == target.model_id)
                    && (victims == background::Victims::Any || e.owner == Origin::Background)
            })
            .collect();
        // Least recently used first.
        candidates.sort_by_key(|e| std::cmp::Reverse(e.last_used_age_seconds));

        let registry = state.runtime();
        for c in candidates {
            // lmgw is not the sole ingress — the dashboard publishes these
            // ports and tells clients (llama-server's own web UI, live ASR) to
            // use them directly. `/slots` is what sees that traffic; the
            // in-flight count above cannot (§4, §10.7). An unanswerable
            // `/slots` is not read as "busy": the eviction proceeds on lmgw's
            // own count, exactly as it did before this probe existed.
            if c.port != 0 {
                match busy_slots(&state.http, c.port, CONTROL_TIMEOUT).await {
                    Some(0) | None => {}
                    Some(n) => {
                        tracing::debug!(
                            "not evicting {}/{}: {n} slot(s) still generating",
                            c.class.as_str(),
                            c.model_id
                        );
                        continue;
                    }
                }
            }
            // Never forced: `stop` refuses on its own in-flight count, so a
            // request that raced in between the candidate list and here still
            // wins, and this pass moves on to the next victim. And only the
            // container the list showed idle (ladder design §12 entry 20):
            // one restarted or climbed since is another container, which
            // nobody judged.
            let stopped = match victims {
                background::Victims::Any => {
                    registry
                        .stop_generation(c.class, &c.model_id, c.generation, false)
                        .await
                }
                // Only while it is still a guest's: the owner may have
                // claimed it during the `/slots` probe.
                background::Victims::IdleBackground => {
                    registry
                        .stop_idle_background(c.class, &c.model_id, c.generation)
                        .await
                }
            };
            match stopped {
                Ok(()) => {
                    let estimated = l
                        .residents
                        .iter()
                        .find(|r| r.container == c.class && r.model == c.model_id)
                        .map(|r| r.estimated_bytes)
                        .unwrap_or(0);
                    tracing::info!(
                        "evicted {}/{} ({}) to make room for {}",
                        c.class.as_str(),
                        c.model_id,
                        fmt_bytes(estimated),
                        target.model_id
                    );
                    return true;
                }
                Err(e) => {
                    tracing::debug!("not evicting {}/{}: {e}", c.class.as_str(), c.model_id);
                    continue;
                }
            }
        }
        false
    }
}

/// Where this model's container is placed — what `acquire` (start this
/// model's container, or join a start already in flight, and take the
/// in-flight claim, all of it the registry's job, §3.2) and a climb's start
/// are handed.
///
/// The class-dependent placement the registry deliberately does not know about
/// comes from [`crate::runtime::lifecycle::acquire_spec`], shared with boot
/// reconciliation (§3.4) rather than restated here: a start and a
/// reconciliation that resolved `models_dir` differently would compare a
/// running container against argv it was never started with, and adoption
/// would silently never happen.
pub(super) fn lifecycle_spec<'a>(
    state: &'a SharedState,
    snap: &'a Snapshot,
    runtime: &'a ModelRuntime,
) -> crate::runtime::registry::AcquireSpec<'a> {
    crate::runtime::lifecycle::acquire_spec(&state.data_dir, snap, runtime)
}

/// A container that would not start is an upstream failure, not a client one:
/// the request was valid and lmgw could not produce the backend for it.
/// [`VramScheduler::check_background_start`]'s refusal under the hold or a
/// benchmark's lease: a whole sentence naming which, so the caller shows it
/// as it is.
fn held(block: &crate::bench::lease::GpuBlock, model_id: &str) -> Fit {
    Fit::Held(format!(
        "'{model_id}' is not started while {} — {} and start it again",
        block.who(),
        block.how_it_ends()
    ))
}

pub(super) fn upstream_error(e: RuntimeError) -> GatewayError {
    // The registry's own net under a benchmark's lease: the same refusal the
    // admission sites give, not a 502.
    if let RuntimeError::GpuBenchmark {
        model_id, run_id, ..
    } = e
    {
        return GatewayError::GpuBenchmark {
            model: model_id,
            run_id,
            detail: String::new(),
        };
    }
    GatewayError::Upstream {
        status: 502,
        provider_type: None,
        message: e.to_string(),
    }
}

/// How many of a container's slots are generating right now, or `None` when it
/// did not answer.
///
/// Measured (§10.7): direct-mode llama-server serves `/slots` by default in
/// this build, with no `--slots` flag and no model parameter — one container,
/// one model, so the answer is about that model by construction.
pub(crate) async fn busy_slots(
    http: &reqwest::Client,
    port: u16,
    timeout: Duration,
) -> Option<usize> {
    let url = format!("http://127.0.0.1:{port}/slots");
    let resp = http.get(&url).timeout(timeout).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    let slots = body.as_array().cloned().or_else(|| {
        // Some builds wrap the array; accept both rather than report "unknown"
        // for a shape difference that says nothing about busyness.
        body["slots"].as_array().cloned()
    })?;
    Some(slots.iter().filter(|s| slot_is_busy(s)).count())
}

/// Is one `/slots` entry generating?
///
/// `is_processing` is authoritative whenever the build serializes it: it is
/// llama-server's own definition of busy (`state != SLOT_STATE_IDLE`), read
/// off the slot rather than inferred. `id_task` is **not** evidence on its
/// own — measured 2026-09-17 on `official-latest`: the server keeps the task
/// a slot last finished (`task_prev`) and `to_json` falls back to it, so an
/// idle slot that has ever served a request reports
/// `is_processing: false, id_task: <finished id>`. Only a slot that has never
/// run a task omits the key. The earlier reading of a present, non-negative
/// `id_task` as busy made every model that had answered once permanently
/// unevictable: an idle resident sat on the card through the whole queue
/// timeout while the waiter was refused, and the hold sweep listed it as
/// draining forever.
///
/// The fallback exists because the endpoint is a debug surface with no
/// compatibility promise. A build that drops `is_processing` must not turn
/// every victim into "idle" (which is how a generation gets killed), so
/// without the flag a task id is read as work — and `id_task == -1` is
/// llama.cpp's serialized "no task" on builds that always emit the field, so
/// -1 is idle there, not busy.
pub(super) fn slot_is_busy(slot: &serde_json::Value) -> bool {
    if let Some(processing) = slot["is_processing"].as_bool() {
        return processing;
    }
    match &slot["id_task"] {
        serde_json::Value::Null => false,
        v => v.as_i64().is_none_or(|id| id >= 0),
    }
}

/// Push the current ledger + queue onto the live bus, off the request's own
/// path — building the view re-probes the driver.
///
/// `pub(crate)` for the hold surfaces (gpu-hold design §5/§6): a sweep that
/// stops containers, and `ops::hold_set` itself, change what is resident
/// without any request having run, and the `vram` frame is event-driven — with
/// no push the dashboard keeps showing models that are already gone.
pub(crate) fn broadcast(state: &SharedState) {
    // `Queued` calls this from a destructor. A handler future dropped by hyper
    // is still inside the runtime, so the spawn goes through; a drop during
    // runtime teardown has no runtime to spawn on, and a panic there would
    // turn a shutdown into an abort. No runtime means no dashboard to tell.
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let st = state.clone();
    rt.spawn(async move {
        let view = st.vram.view(&st).await;
        st.telemetry.vram(view);
    });
}

/// What is holding the GPU, for a refusal message. A timeout that only says
/// "timed out" leaves the owner nothing to act on.
pub(super) fn describe_holders(l: &Ledger) -> String {
    if l.residents.is_empty() {
        return "nothing lmgw put there is resident — another process is using the GPU".into();
    }
    let mut parts: Vec<String> = l
        .residents
        .iter()
        .map(|r| {
            format!(
                "{}/{} ({}{}{})",
                r.container.as_str(),
                r.model,
                fmt_bytes(r.estimated_bytes),
                // Named separately rather than summed in: "6.2 GB" and
                // "6.2 GB + 6.6 GB peak" are different facts about what an
                // owner can reclaim, and the second one is the one that says
                // why the free figure is smaller than the card looks.
                match r.peak_extra_bytes {
                    Some(p) => format!(" + {} peak", fmt_bytes(p)),
                    None => String::new(),
                },
                if r.in_flight > 0 {
                    format!(", {} in flight", r.in_flight)
                } else {
                    String::new()
                }
            )
        })
        .collect();
    parts.sort();
    format!("held by {}", parts.join(", "))
}
