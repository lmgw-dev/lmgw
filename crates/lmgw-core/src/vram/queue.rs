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
use super::download::Engine;
use super::ledger::Ledger;
use super::place::{others_hold_on, Mode, Placement};
use super::scheduler::{Waiter, CONTROL_TIMEOUT, POLL};
use super::stage_watch::WaitPhase;
use super::stall::{evicting_stage, waiting_stage};
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
        // Per model, by the descriptor this admission starts: an audio row
        // on the CPU is not held (`gpu_block_at`) — unless the container it
        // would join still runs on the GPU.
        if let Some(block) = snap.gpu_block_at(runtime.placement()) {
            return Err(block.refusal(target.model_id.clone(), ""));
        }
        if let Some(refusal) = super::on_cpu::gpu_container_under_hold(state, snap, target) {
            return Err(refusal);
        }
        let mut fallback = fallback;
        // The snapshot and descriptor a retry after a failed climb re-read.
        let mut reread: Option<(Arc<Snapshot>, ModelRuntime)> = None;
        let guard = loop {
            let (snap, runtime) = match &reread {
                Some((s, r)) => (&**s, r),
                None => (snap, runtime),
            };
            // The engine image first, when the model is not up and its
            // image is not on this machine (machine-check design §5):
            // downloaded before anything below takes the gate, evicts or
            // reserves, so the queue's budget starts after it. On every
            // pass: a start over after a failed climb renders the row as
            // it is now, which may name another image.
            if !self.is_up(state, target) {
                match self
                    .engine_download(state, target, runtime, alias, &mut fallback)
                    .await?
                {
                    Engine::Here => {}
                    Engine::Blocked(block) => {
                        return Err(block.refusal(target.model_id.clone(), ""))
                    }
                    Engine::External(short) => return Ok(Admission::External(short)),
                    // The download can take minutes: the owner may have
                    // switched the hold on, or edited or deleted the row,
                    // while it ran. A new admission reads both again, and
                    // so does this one (as after a failed climb, below);
                    // the next pass finds the image here.
                    Engine::Downloaded => {
                        reread = Some(read_again(
                            state,
                            target,
                            "after its engine image downloaded",
                        )?);
                        continue;
                    }
                }
            }
            // A CPU start takes nothing from the card: no gate, no queue,
            // no reservation, and no `vram_too_large`.
            let on_gpu = runtime.placement().is_gpu();
            // Arbitrate only when there is something to arbitrate: the switch
            // is on, and the model is not already up. A model the registry
            // already holds takes no gate, no measurement and no queue —
            // admission must not put a serialization point in front of
            // traffic that fits.
            let up = self.is_up(state, target);
            let reservation = if snap.settings.vram.enabled && !up && on_gpu {
                // Boxed: the placing and the queue's futures are large in a
                // debug build, and a test thread's 2 MiB is shared by the
                // whole chain from the route to here.
                match Box::pin(self.arbitrate(state, snap, target, alias, fallback.as_deref_mut()))
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
            // By the placement of the descriptor `acquire` starts below,
            // not the row's: a row switched to the CPU while this request
            // queued still has its GPU descriptor started here.
            if let Some(block) = state.snapshot().gpu_block_at(runtime.placement()) {
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
                    reread = Some(read_again(state, target, "again after its climb failed")?);
                    continue;
                }
                Ok(guard) => {
                    if !up {
                        // It came up just now: learn its PID while it is known
                        // to be here — and, when the last machine check said
                        // this computer could not start it, that check is
                        // out of date (machine-check design §2.5).
                        self.cache_pids(state);
                        crate::machine::after_start(state, runtime);
                    }
                    break Ok(guard);
                }
                // The machine check runs again for a start that failed on
                // the graphics card, and names the cause when it is the
                // computer's (machine-check design §2.5, WP5).
                Err(e) => break Err(super::start_error(state, runtime, e).await),
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
            turns: Default::default(),
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
    /// waits for room. The one wait is the engine download of a model whose
    /// image is not on this machine (machine-check design §5), before
    /// anything is measured, exactly as a request's admission has it: the
    /// hold or a benchmark's lease coming on during it is [`Fit::Held`], a
    /// download that fails is [`Fit::Failed`], and one that ran to its end is
    /// [`Fit::Downloaded`] — the caller reads its row again and asks again.
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
        // Per model: an audio row on the CPU is not held, and takes nothing
        // from the card, so it goes ahead without a reservation.
        if let Some(block) = snap.gpu_block_for(class, model_id) {
            return held(&block, model_id);
        }
        let target = Target {
            class,
            model_id: model_id.to_string(),
        };
        if !self.is_up(state, &target) {
            if let Some(runtime) = model_runtime(snap, class, model_id) {
                match self
                    .engine_download(state, &target, &runtime, model_id, &mut None)
                    .await
                {
                    Ok(Engine::Here) => {}
                    Ok(Engine::Blocked(block)) => return held(&block, model_id),
                    // Only ever answered to a caller with a fallback, and a
                    // start nobody waits on has none; were it, the room is
                    // short outside lmgw's control, which is what `Full`
                    // tells.
                    Ok(Engine::External(short)) => return Fit::Full(short.describe()),
                    // The download can take minutes: the snapshot this check
                    // was given is from before it. The hold as it is now,
                    // so a hold switched on at the very end is said at once;
                    // then the caller reads the row again and asks again
                    // (`Fit::Downloaded`), and that check starts from the
                    // snapshot the start renders.
                    Ok(Engine::Downloaded) => {
                        return match state.snapshot().gpu_block_for(class, model_id) {
                            Some(block) => held(&block, model_id),
                            None => Fit::Downloaded,
                        };
                    }
                    Err(e) => {
                        return Fit::Failed(match e {
                            GatewayError::EngineDownload { message } => message,
                            other => other.to_string(),
                        })
                    }
                }
            }
        }
        if !snap.placement(class, model_id).is_gpu() {
            return Fit::Go(StartPermit { _reservation: None });
        }
        if !snap.settings.vram.enabled {
            return Fit::Unchecked;
        }
        // Already up (or coming up): the start will join it, and it is
        // already charged to the ledger. Nothing to decide.
        if self.is_up(state, &target) {
            return Fit::Go(StartPermit { _reservation: None });
        }
        self.read_fit_vocab(state, snap, &target).await;
        let Some(fp) = self.footprint(snap, class, model_id).await else {
            return Fit::Unchecked;
        };
        let _gate = self.ask_gate().await;
        let l = Box::pin(self.ledger(state, snap)).await;
        // Again, after the last await before the claim: `snap` is the one
        // this start began with, and the lease (or the hold) may have come on
        // while it waited for the footprint, the gate or the ledger. Under
        // the gate, so a benchmark's drain — which takes the gate once before
        // it looks — never looks while a start it cannot see yet is decided.
        // The global block, not the row's: past the CPU exit above this is a
        // GPU start, whatever the row has been switched to since.
        if let Some(block) = state.snapshot().gpu_block() {
            return held(&block, model_id);
        }
        if l.capacity.is_none() {
            return Fit::Unchecked;
        }
        if let Some(problem) = self.pin_gone(snap, &target, &l.devices) {
            return Fit::Failed(format!("'{model_id}' {problem}"));
        }
        // The card it goes on (device-ledger design §5): this start never
        // evicts or waits, so only a card with room now takes it, and one
        // with nothing of lmgw's on it before an idle model elsewhere that
        // it would not evict (`Mode::Still`).
        let p = self.placement(snap, &target, &fp, &l, Mode::Still, None);
        self.background_on(state, snap, &target, fp, &l, p)
    }

    /// [`Self::check_background_start`]'s decision on the cards `p` names,
    /// on ledger `l`, under the gate: a reservation where the start fits
    /// there now, else why not. A start that never evicts or waits; also
    /// a model moved to another card to make room (device-ledger design
    /// §6.2).
    pub(super) fn background_on(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        fp: super::Footprint,
        l: &Ledger,
        p: Placement,
    ) -> Fit {
        let model_id = target.model_id.as_str();
        let Some(cap) = l.capacity_on(&p.cards, None) else {
            return Fit::Unchecked;
        };
        let (headroom, margin, floor) = (p.headroom, p.margin, p.floor);
        // A row llama.cpp's `--fit` would shrink, fitted to what this
        // reading's card can give it (`partial::llama`).
        let fp = self.fit_in_on(target, fp, l, &p, None);
        let needs = fp
            .total_bytes
            .saturating_add(p.engine)
            .saturating_add(headroom);
        // A pipeline that can run with less on the card is never too large
        // for it (`partial`). Too large is judged without the learned room,
        // which only decides whether there is room now (`headroom`), against
        // every card it may use (§5.8).
        if let Some(t) = self.too_large(snap, target, &fp, l) {
            return Fit::Full(format!(
                "'{model_id}' needs {} plus {} headroom and {} {} in total{}",
                fmt_bytes(t.bytes),
                fmt_bytes(t.structural),
                t.holder,
                fmt_bytes(t.capacity),
                super::partial::too_large_hint(snap, target, &fp)
                    + &self.no_split_hint(snap, target, &fp)
            ));
        }
        // A split (device-ledger design §4.3): where each of its cards holds
        // its part now, else not now.
        if p.split.is_some() {
            return match self.split_short(l, target, &p, None) {
                None => match super::place::split_grant(&p) {
                    Some(grant) => Fit::Go(StartPermit {
                        _reservation: Some(self.reserve_as(state, target, &p, grant)),
                    }),
                    None => Fit::Unchecked,
                },
                Some((card, short, free)) => Fit::Full(format!(
                    "'{model_id}' is split over {} cards and needs {} on {} where only {} is \
                     free — {}",
                    p.cards.len(),
                    fmt_bytes(short),
                    super::place::card_label(&l.devices, &card),
                    fmt_bytes(free),
                    describe_holders(l, &super::stall::wait_cards(l, std::slice::from_ref(&card))),
                )),
            };
        }
        // The whole where it fits, else what `partial` gives it. This start
        // never evicts, so anything else of lmgw's on the card is something
        // it could wait for; with nothing of lmgw's there, the learned room
        // and, where nothing smaller fits, its margin are no reason not to
        // start (`room_on`, `partial::decide`).
        let room = self.room_on(l, target, &p, &cap, false);
        let Some(grant) = super::partial::decide(target, &fp, room) else {
            return Fit::Full(format!(
                "'{model_id}' needs {} and only {} is free{} — {}{}",
                fmt_bytes(needs),
                fmt_bytes(cap.free),
                self.on_words(l, &p),
                describe_holders(l, &super::stall::wait_cards(l, &p.cards)),
                super::partial::why_not(&fp, room)
                    .map(|w| format!("; {w}"))
                    .unwrap_or_default()
            ));
        };
        let grant = self.note_growth(
            target,
            grant,
            (headroom + p.engine, margin + p.engine, floor + p.engine),
            cap.free,
        );
        // A context lowered to one card of several, said as the editor
        // says it (device-ledger design §8.2).
        let grant = self.one_card_grant(snap, target, &fp, &p, l, grant);
        Fit::Go(StartPermit {
            _reservation: Some(self.reserve_as(state, target, &p, grant)),
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
        self.read_fit_vocab(state, snap, target).await;
        let Some(fp) = self.footprint(snap, target.class, &target.model_id).await else {
            return Ok(Decided::Go(None));
        };

        // Is there anything to decide against at all?
        let (fp, needs) = {
            let l = Box::pin(self.ledger(state, snap)).await;
            if l.capacity.is_none() {
                tracing::debug!(
                    "VRAM admission inactive: {}",
                    l.inactive_reason.as_deref().unwrap_or("no capacity")
                );
                return Ok(Decided::Go(None));
            }
            // A pinned card that is gone: no other card (device-ledger
            // design §8), and the fallback answers where there is one.
            if let Some(problem) = self.pin_gone(snap, target, &l.devices) {
                return Err(GatewayError::GpuSetup {
                    model: target.model_id.clone(),
                    problem,
                    detail: String::new(),
                });
            }
            // The card it would go on now (device-ledger design §5); the
            // wait asks again on every reading until it commits to one.
            let p = self.placement(snap, target, &fp, &l, Mode::Owner, None);
            // A row llama.cpp's `--fit` would shrink, fitted to what this
            // reading's card can give it (`partial::llama`); fitted again
            // on every reading of the wait (`Self::decide`).
            let fp = self.fit_in_on(target, fp, &l, &p, None);
            let needs = fp
                .total_bytes
                .saturating_add(p.engine)
                .saturating_add(p.headroom);
            // A model bigger than every card it may use never becomes
            // admissible, however long it waits, so it is refused here
            // rather than queued forever — unless it can run with less on
            // the card, which admission then gives it (`partial`). Judged
            // without the learned room, which only decides whether there
            // is room now (`headroom`), and against each card it may use,
            // or their sum for a chat model that goes over them (§5.8).
            if let Some(t) = self.too_large(snap, target, &fp, &l) {
                return Err(GatewayError::VramTooLarge {
                    model: target.model_id.clone(),
                    need: GatewayError::vram_need(t.bytes, t.structural),
                    holds: GatewayError::vram_holds(&t.holder, t.capacity),
                    hint: super::partial::too_large_hint(snap, target, &fp)
                        + &self.no_split_hint(snap, target, &fp),
                });
            }
            (fp, super::CardCharge::over(p.cards.clone(), needs))
        };

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
                phase: Some(WaitPhase::Gate),
                // The caller's own, when it follows its wait
                // (chat-media-tools design §5.3).
                watch: super::stage_watch::current(),
                origin: Origin::Owner,
            },
        );
        // A queue that forms should be visible while it exists, not once it is
        // over, so the dashboard is told at both ends of the wait — the
        // leaving end by `queued`'s drop, however the wait ends.
        broadcast(state);
        let result = self
            .decide(
                state, snap, target, alias, &fp, queued.id, started, fallback,
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
    /// waiter) with nothing reserved. `base`: the footprint, fitted again on
    /// every reading of the card where llama.cpp's `--fit` would shrink it
    /// ([`Self::fit_in`]).
    ///
    /// **The learned room decides waiting** (`headroom`, device-ledger
    /// design WP13): a start waits for, and evicts, lmgw's own models until
    /// its footprint and its headroom — the card's floor, its learned room
    /// and the start's margin — are free; `place` has already preferred a
    /// card where that holds, now or after evicting there. Once nothing of
    /// lmgw's is left to wait for or evict, it goes ahead wherever it fits
    /// with the floor and its margin, with a warning that names the room
    /// it took ([`Self::note_growth`]). The floor is never given up.
    #[allow(clippy::too_many_arguments)]
    async fn decide(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        alias: &str,
        base: &super::Footprint,
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

        self.set_phase(waiter_id, WaitPhase::Measuring, None);
        // Held from the first look that does not fit until this returns,
        // however it returns (candidate-aliases §4.5, §12 entry 47).
        let mut drain = None;
        // One reconciliation pass per wait, at its first dead end (`stall`).
        let mut looked = false;
        // Why a pipeline that can run with less still waits, logged once.
        let mut said_why = false;
        // The cards this start committed to, once a reading could not start
        // it at once (device-ledger design §5.5, D15): asked of `place` no
        // more while it holds them.
        let mut claim: Option<(super::scheduler::Claimed, Placement)> = None;
        loop {
            // Switched on while this request waited — the hold, or a
            // benchmark's lease (benchmark design §3.2): nothing new goes on
            // the card, however long this request has queued for it. The
            // global block, not the row's: only a GPU start is arbitrated,
            // and a row switched to the CPU meanwhile does not change the
            // descriptor this request will start.
            if let Some(block) = state.snapshot().gpu_block() {
                return Err(block.refusal(target.model_id.clone(), ""));
            }
            let l = Box::pin(self.ledger(state, snap)).await;
            if l.capacity.is_none() {
                // Telemetry vanished mid-wait (driver reset). Forwarding
                // unarbitrated is the same behaviour as a gateway without this
                // feature, and strictly better than a hang.
                return Ok(Decided::Go(None));
            }

            // Somebody else started it while this request waited: their
            // reservation (or their registry entry) already accounts for it,
            // and `acquire` will join their start rather than launch a second.
            if l.residents
                .iter()
                .any(|r| r.container == target.class && r.model == target.model_id)
            {
                return Ok(Decided::Go(None));
            }

            // The cards: the claim's, else where `place` puts it on this
            // reading (§5).
            let own = claim.as_ref().map(|(c, _)| c.id);
            let p = match &claim {
                Some((_, p)) => p.clone(),
                None => self.placement(snap, target, base, &l, Mode::Owner, None),
            };
            let Some(cap) = l.capacity_on(&p.cards, own) else {
                return Ok(Decided::Go(None));
            };
            let (headroom, margin, floor) = (p.headroom, p.margin, p.floor);
            // Only a split holds it, and none fits now (owner, 2026-11-18):
            // the cards that split waits on. It commits to no card and
            // evicts nothing on the roomiest one, which can never hold it.
            let split_wait = match &claim {
                Some(_) => None,
                None => self.split_wait(snap, target, base, &l, &p),
            };

            // Fitted to what this reading's card can give it: lmgw's own
            // models counted as room, so evicting them changes nothing,
            // and with none left the target is what is free beside its
            // margin (`partial::llama`).
            let fitted = self.fit_in_on(target, base.clone(), &l, &p, own);
            let fp = &fitted;
            // A split needs its part on each of its cards (§4.3): the card
            // short of it is the one an eviction or a wait is on.
            let parts = self.split_needs(&l, target, &p);
            let short = self.split_short(&l, target, &p, own);
            let needs = match parts.is_empty() {
                true => fp
                    .total_bytes
                    .saturating_add(p.engine)
                    .saturating_add(headroom),
                false => parts.iter().map(|(_, b)| *b).fold(0, u64::saturating_add),
            };
            self.set_needs(
                waiter_id,
                match parts.is_empty() {
                    true => super::CardCharge::over(p.cards.clone(), needs),
                    false => super::CardCharge::per_card(parts.clone()),
                },
            );
            if let (Some(grant), None) = (super::place::split_grant(&p), &short) {
                self.set_phase(waiter_id, WaitPhase::Starting, None);
                return Ok(Decided::Go(Some(self.reserve_as(state, target, &p, grant))));
            }
            if p.split.is_none() && cap.free >= needs {
                self.set_phase(waiter_id, WaitPhase::Starting, None);
                let room = super::partial::Room {
                    free: cap.free,
                    headroom: headroom.saturating_add(p.engine),
                    floor: headroom.saturating_add(p.engine),
                    others_hold: others_hold_on(&l, target, &p.cards),
                    fallback: fallback.as_ref().is_some_and(|f| f.live()),
                    cards: p.count(),
                };
                let grant = super::partial::whole(target, fp, room);
                // A context lowered to one card of several, said as the
                // editor says it (device-ledger design §8.2).
                let grant = self.one_card_grant(snap, target, fp, &p, &l, grant);
                return Ok(Decided::Go(Some(self.reserve_as(state, target, &p, grant))));
            }

            // The cards an eviction or a wait is on: a split's short card,
            // else all of the placement's. On a computer with several cards
            // the phases and words name them, with what is needed and free
            // there (device-ledger design §10).
            let evict_on = match (&short, &split_wait) {
                (Some((card, _, _)), _) => vec![card.clone()],
                (None, Some(on)) => on.clone(),
                (None, None) => p.cards.clone(),
            };
            let (wait_needs, wait_free) = match (&short, &split_wait) {
                (Some((_, n, f)), _) => (*n, *f),
                (None, Some(on)) => (needs, l.capacity_on(on, own).map_or(cap.free, |c| c.free)),
                (None, None) => (needs, cap.free),
            };
            let cards = super::stall::wait_cards(&l, &evict_on);
            // What it waited for, said once it is out of time: the models
            // of lmgw's on the cards a split needs where only a split holds
            // it.
            let holding = |l: &Ledger| match (&split_wait, &p.split) {
                (Some(_), _) => describe_split_holders(l, &cards),
                // A split's wait, a row's own on its pin among them, names
                // the budget its parts share.
                (None, Some(_)) => describe_holders(l, &cards) + &split_budget_note(l),
                (None, None) => describe_holders(l, &cards),
            };

            if budget.is_some_and(|d| started.elapsed() >= d) {
                return Err(expired(holding(&l)));
            }

            // Not now: it commits to these cards, where one card or a split
            // was answered (§5.5); a split claims its part on each card. A
            // group is the pooled wait of before.
            let waits = super::place::waits_on(&l, target, &p);
            if claim.is_none() && !parts.is_empty() && l.devices.len() > 1 {
                let c = self.claim_parts(state, waiter_id, parts.clone(), waits);
                claim = Some((c, p.clone()));
            } else if claim.is_none()
                && split_wait.is_none()
                && p.cards.len() == 1
                && l.devices.len() > 1
            {
                let c = self.claim(state, waiter_id, p.cards.clone(), needs, waits);
                claim = Some((c, p.clone()));
            } else if let Some((c, _)) = &claim {
                match parts.is_empty() {
                    true => self.reclaim(c.id, needs, waits),
                    false => self.reclaim_parts(c.id, &parts, waits),
                }
            }

            // Short of room: every resident model drains for the owner —
            // background traffic takes no new work on it, so the ones this
            // wait evicts or waits for do not stay busy with guests (§4.5).
            // A guest's run that holds one claim for a whole tool loop keeps
            // its model for the run: a request that has started finishes.
            // Not where only a split holds it: the roomiest card it was
            // answered can never hold it, so nothing there is evicted or
            // drained for it.
            if drain.is_none() && split_wait.is_none() {
                drain = Some(background::owner_drain(state));
            }
            if split_wait.is_none() {
                self.set_stage(
                    waiter_id,
                    &evicting_stage(wait_needs, Some(wait_free), None, &cards, false),
                );
            }
            if split_wait.is_none()
                && self
                    .evict_one(
                        state,
                        target,
                        &l,
                        background::Victims::Any,
                        Some(ForWait {
                            id: waiter_id,
                            needs: wait_needs,
                            free: wait_free,
                            cards: &cards,
                        }),
                        &evict_on,
                    )
                    .await
            {
                continue;
            }
            // Everything resident is either busy or the target itself — or
            // nothing of lmgw's is there, as far as the registry knows: a
            // reconciliation pass looks once, before the wait, with the gate
            // let go for it (`stall`). Waiting is the only correct move: the
            // alternative is killing somebody's half-finished generation,
            // which is the thing `/slots` exists to prevent.
            if !looked {
                looked = true;
                drop(gate);
                self.look_for_unheld(state, waiter_id, started, budget)
                    .await;
                gate = match self
                    .take_gate(state, target, alias, None, started, budget, &mut fallback)
                    .await
                {
                    Gated::Held(gate) => gate,
                    Gated::External(short) => return Ok(Decided::External(short)),
                    Gated::Expired => return Err(expired(holding(&l))),
                };
                // Measured again whatever the pass found: the gate was let go.
                continue;
            }
            // Nothing more can be made free now. With nothing of lmgw's
            // left on the card, the learned room is no reason to wait any
            // longer: a start that fits beside what other programs hold now
            // with the floor and its margin goes ahead, warned (`room_on`);
            // and where nothing smaller of it fits beside the margin, one
            // that fits beside the floor alone, its margin given up last
            // (`partial::decide`), warned too. Else a
            // pipeline that can run with less on the card goes ahead with
            // what `partial` gives it — its on-card part, or what is free
            // where that leaves the working budget it has rendered with;
            // short of that, only when nothing else of lmgw's could be
            // waited for and no fallback would answer instead.
            let room = self.room_on(
                &l,
                target,
                &p,
                &cap,
                fallback.as_ref().is_some_and(|f| f.live()),
            );
            // A split goes ahead only card by card, above (§4.3): never
            // with a grant that would leave its split to the engine.
            let go = match p.split {
                Some(_) => None,
                None => super::partial::decide(target, fp, room),
            };
            if let Some(grant) = go {
                let grant = self.note_growth(
                    target,
                    grant,
                    (headroom + p.engine, margin + p.engine, floor + p.engine),
                    cap.free,
                );
                self.set_phase(waiter_id, WaitPhase::Starting, None);
                return Ok(Decided::Go(Some(self.reserve_as(state, target, &p, grant))));
            }
            if !said_why {
                let why = super::partial::why_not(fp, room)
                    .map(|w| w + &self.no_split_hint(snap, target, fp))
                    .or_else(|| {
                        Some(self.no_split_hint(snap, target, fp))
                            .filter(|h| !h.is_empty())
                            .map(|h| h.trim_start_matches(". ").to_string())
                    });
                if let Some(why) = why {
                    said_why = true;
                    tracing::info!(
                        "{}/{} waits for room{}: {why}",
                        target.class.as_str(),
                        target.model_id,
                        match cards.is_empty() {
                            true => String::new(),
                            false => format!(" on {}", lmgw_api_types::WaitCard::list(&cards)),
                        }
                    );
                }
            }
            self.set_phase(
                waiter_id,
                WaitPhase::WaitingForRoom {
                    needs_bytes: wait_needs,
                    free_bytes: wait_free,
                    cards: cards.clone(),
                },
                Some(&waiting_stage(
                    &l, target, wait_needs, wait_free, &evict_on, &cards,
                )),
            );
            // A start that committed to its cards gives the gate back while
            // it waits (§5.5): a start that fits another card is not held
            // up by it, and one on its card sees what it claimed there. So
            // does one only a split holds, which holds nothing meanwhile: a
            // later, smaller start may take the room first (owner,
            // 2026-11-18).
            if claim.is_some() || split_wait.is_some() {
                drop(gate);
                tokio::time::sleep(POLL).await;
                if let Some(short) = self.recheck(state, target, alias, &mut fallback).await {
                    return Ok(Decided::External(short));
                }
                gate = match self
                    .take_gate(state, target, alias, None, started, budget, &mut fallback)
                    .await
                {
                    Gated::Held(gate) => gate,
                    Gated::External(short) => return Ok(Decided::External(short)),
                    Gated::Expired => return Err(expired(holding(&l))),
                };
                continue;
            }
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
                    Gated::Expired => return Err(expired(holding(&l))),
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
        let lock = self.ask_gate();
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
    /// `on`: the cards that are short (device-ledger design §6.1); only a
    /// model charged there is a victim. Empty: every card, the pooled
    /// capacity.
    ///
    /// `victims`: any idle model, or only idle `Background`-owned ones — a
    /// guest's start, which never evicts the owner's (candidate-aliases
    /// §4.4). `waiter`: the wait this eviction makes room for and the bytes
    /// it needs; its phase names each victim before it is stopped
    /// (chat-media-tools design §5.3), and its stage keeps what it needs and
    /// what is free beside the victim ([`evicting_stage`]).
    pub(super) async fn evict_one(
        &self,
        state: &SharedState,
        target: &Target,
        l: &Ledger,
        victims: background::Victims,
        waiter: Option<ForWait<'_>>,
        on: &[super::nvml::DeviceId],
    ) -> bool {
        // Residency in the registry is what makes a model a candidate, never a
        // nonzero estimate (§4): a resident whose footprint reads zero would be
        // unevictable forever, which is how a GPU fills up with models nothing
        // can reclaim.
        //
        // A request on its way to, or served by, its own fallback still
        // holds its claim on each model it left; such a claim alone keeps
        // nothing busy for this admission (`leaving`, openai egress design
        // §14.3).
        let own = super::leaving_now();
        let idle = |e: &RuntimeView| {
            e.in_flight == 0
                || own
                    .iter()
                    .any(|o| o.frees(e.class, &e.model_id, e.generation, e.in_flight, e.sends))
        };
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
                    && idle(e)
                    && !(e.class == target.class && e.model_id == target.model_id)
                    && (victims == background::Victims::Any || e.owner == Origin::Background)
                    // Only a model on the cards that are short (device-ledger
                    // design §6.1): an idle model on another card is left
                    // alone.
                    && l.residents.iter().any(|r| {
                        r.generation == Some(e.generation)
                            && super::ledger::Ledger::is_on(r, on)
                    })
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
            if let Some(w) = &waiter {
                let victim = format!("{}/{}", c.class.as_str(), c.model_id);
                // On one card the machine's free, as it always was; on
                // several, what is free on the cards the wait is on.
                let free = match w.cards.is_empty() {
                    true => l.capacity.as_ref().map(|c| c.free),
                    false => Some(w.free),
                };
                // Started again on another card after the stop (§6.2): the
                // same question `repack` asks below, on the same reading.
                let moves =
                    !w.cards.is_empty() && !self.elsewhere(&state.snapshot(), l, c, on).is_empty();
                let stage = evicting_stage(w.needs, free, Some(&victim), w.cards, moves);
                self.set_phase(
                    w.id,
                    WaitPhase::Evicting {
                        victim,
                        cards: w.cards.to_vec(),
                        moves,
                    },
                    Some(&stage),
                );
            }
            // Never forced: `stop` refuses on its own in-flight count, so a
            // request that raced in between the candidate list and here still
            // wins, and this pass moves on to the next victim. And only the
            // container the list showed idle (ladder design §12 entry 20):
            // one restarted or climbed since is another container, which
            // nobody judged.
            // A guest's eviction stops only while the victim is still a
            // guest's: the owner may have claimed it during the `/slots`
            // probe.
            let only_background = victims == background::Victims::IdleBackground;
            let stopped = registry
                .stop_for_eviction(c.class, &c.model_id, c.generation, only_background, &own)
                .await;
            match stopped {
                Ok(()) => {
                    let estimated = l
                        .residents
                        .iter()
                        .find(|r| r.container == c.class && r.model == c.model_id)
                        .map(|r| r.estimated_bytes)
                        .unwrap_or(0);
                    let from = super::stall::wait_cards(l, on);
                    tracing::info!(
                        "evicted {}/{} ({}){} to make room for {}",
                        c.class.as_str(),
                        c.model_id,
                        fmt_bytes(estimated),
                        match from.is_empty() {
                            true => String::new(),
                            false => format!(" from {}", lmgw_api_types::WaitCard::list(&from)),
                        },
                        target.model_id
                    );
                    // Started again on another card that holds it now
                    // (device-ledger design §6.2), by a task of its own:
                    // this start does not wait for it.
                    self.repack(state, l, c, on, target);
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

/// The wait an eviction makes room for ([`VramScheduler::evict_one`]): its
/// waiter, what it needs and what is free on the cards it waits for, and
/// those cards named on a computer with several (empty on one card), so
/// the phase names the victim's card and whether it moves (device-ledger
/// design §6.2, §10).
pub(super) struct ForWait<'a> {
    pub id: u64,
    pub needs: u64,
    pub free: u64,
    pub cards: &'a [lmgw_api_types::WaitCard],
}

/// What a start over reads again (ladder design §12 entry 51): the snapshot
/// as it is now — the hold, or a benchmark's lease, may have come on — and
/// the model's row, which may have been edited or deleted. `after` finishes
/// the 502's sentence when the row is gone: "… could not be started
/// <after>".
fn read_again(
    state: &SharedState,
    target: &Target,
    after: &str,
) -> Result<(Arc<Snapshot>, ModelRuntime), GatewayError> {
    let now = state.snapshot();
    if let Some(block) = now.gpu_block_for(target.class, &target.model_id) {
        return Err(block.refusal(target.model_id.clone(), ""));
    }
    let runtime = model_runtime(&now, target.class, &target.model_id).ok_or_else(|| {
        GatewayError::Upstream {
            status: 502,
            provider_type: None,
            message: format!(
                "{} model '{}' is no longer configured, so it could not be started {after}",
                target.class, target.model_id
            ),
        }
    })?;
    Ok((now, runtime))
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
    crate::runtime::lifecycle::acquire_spec(state, snap, runtime)
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

impl VramScheduler {
    /// The room a start decides in once nothing more can be made free, on
    /// the cards of placement `p` as ledger `l` reads them, with `cap` their
    /// capacity (per-model containers §4, "Headroom"; device-ledger design
    /// §5.6, WP13): kept free beside it is its headroom — the floor, the
    /// learned room and its margin — while anything of lmgw's is on those
    /// cards that a wait could free; with nothing of lmgw's left there, the
    /// floor and its margin, so a start that fits beside what other
    /// programs hold now goes ahead ([`Self::note_growth`] says so), and
    /// the floor alone where nothing smaller fits beside the margin
    /// (`partial::Room::floor`, given up last). The engine's cost there
    /// rides on each.
    pub(super) fn room_on(
        &self,
        l: &Ledger,
        target: &Target,
        p: &Placement,
        cap: &super::ledger::Capacity,
        fallback: bool,
    ) -> super::partial::Room {
        let others = others_hold_on(l, target, &p.cards);
        super::partial::Room {
            free: cap.free,
            headroom: if others { p.headroom } else { p.margin }.saturating_add(p.engine),
            floor: if others { p.headroom } else { p.floor }.saturating_add(p.engine),
            others_hold: others,
            fallback,
            cards: p.count(),
        }
    }

    /// " on <card>" for a refusal where the placement is one card of
    /// several: by name and index, never by id; empty otherwise.
    pub(super) fn on_words(&self, l: &Ledger, p: &Placement) -> String {
        match p.cards.as_slice() {
            [one] if l.devices.len() > 1 => l
                .devices
                .iter()
                .find(|d| &d.id == one)
                .map(|d| format!(" on {} (card {})", d.name, d.index))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// `grant`, with the warning of a start that goes ahead without the
    /// learned room or its margin kept free beside it where it does
    /// ([`Self::growth_let_go`]). `kept`: what it keeps while it can make
    /// room another way, once the room is given up, and once the margin is
    /// too. An image pipeline's partial grant sets its context aside inside
    /// itself (`partial::Room`), the image margin's stand-in: the margin is
    /// in the grant, never given up, and what it keeps beside it is less
    /// by it.
    pub(super) fn note_growth(
        &self,
        target: &Target,
        grant: super::partial::Grant,
        (headroom, margin, floor): (u64, u64, u64),
        free: u64,
    ) -> super::partial::Grant {
        use super::partial::Grant;
        let (bytes, kept) = match &grant {
            Grant::Whole(b) | Grant::Noted(b, _) => (*b, (headroom, margin, floor)),
            Grant::Partial(g) if g.llama.is_none() => {
                let set_aside = margin.saturating_sub(floor);
                (g.bytes, (headroom.saturating_sub(set_aside), floor, floor))
            }
            Grant::Partial(g) => (g.bytes, (headroom, margin, floor)),
        };
        let Some(note) = self.growth_let_go(target, bytes, kept, free) else {
            return grant;
        };
        match grant {
            Grant::Whole(b) => Grant::Noted(b, note),
            Grant::Noted(b, n) => Grant::Noted(b, format!("{n}. {note}")),
            Grant::Partial(mut g) => {
                g.warning = match g.warning.is_empty() {
                    true => note,
                    false => format!("{}. {note}", g.warning),
                };
                Grant::Partial(g)
            }
        }
    }
}

/// What is holding the GPU, for a refusal message. A timeout that only says
/// "timed out" leaves the owner nothing to act on.
///
/// `cards`: on a computer with several, the cards the start waited for
/// (device-ledger design §10): the message names them and lists lmgw's
/// models there. Empty on one card, and the message is as it always was.
pub(super) fn describe_holders(l: &Ledger, cards: &[lmgw_api_types::WaitCard]) -> String {
    let on = lmgw_api_types::WaitCard::list(cards);
    let parts = holders_on(l, cards);
    if parts.is_empty() {
        return match cards {
            [] => "nothing lmgw put there is resident — another process is using the GPU".into(),
            [_] => {
                format!("nothing lmgw put on {on} is resident — another process is using that card")
            }
            _ => format!(
                "nothing lmgw put on {on} is resident — other processes are using those cards"
            ),
        };
    }
    match cards {
        [] => format!("held by {}", parts.join(", ")),
        _ => format!("{on} is held by {}", parts.join(", ")),
    }
}

/// What a start only a split holds waited for (owner, 2026-11-18): no card
/// holds it whole, and the split it needs over `cards` waited for the busy
/// or starting models of lmgw's there — not "another process is using that
/// card", which [`describe_holders`] would say of the roomiest card it was
/// answered.
pub(super) fn describe_split_holders(l: &Ledger, cards: &[lmgw_api_types::WaitCard]) -> String {
    let on = lmgw_api_types::WaitCard::list(cards);
    let parts = holders_on(l, cards);
    let said = match parts.is_empty() {
        true => format!(
            "no card holds it whole, and a split over {on} found no room there although \
             nothing lmgw put there is resident — other processes are using those cards"
        ),
        false => format!(
            "no card holds it whole, and the split it needs over {on} waited for {} to leave \
             room there",
            parts.join(", ")
        ),
    };
    said + &split_budget_note(l)
}

/// What a split's wait or refusal adds under a declared budget over several
/// cards (owner, 2026-11-18): its parts together fit within what the budget
/// leaves too; empty without one.
pub(super) fn split_budget_note(l: &Ledger) -> String {
    match l.budget_total() {
        Some(b) => format!(
            " (under the GPU memory budget of {}, a split's parts on its cards together, \
             their engine's cost included, fit within what it leaves)",
            fmt_bytes(b)
        ),
        None => String::new(),
    }
}

/// lmgw's models with bytes on `cards` (every card for none), each as
/// "class/model (size…)", sorted.
fn holders_on(l: &Ledger, cards: &[lmgw_api_types::WaitCard]) -> Vec<String> {
    let ids: Vec<super::nvml::DeviceId> = cards.iter().map(|c| c.id.clone()).collect();
    let there: Vec<&super::ResidentView> = l
        .residents
        .iter()
        .filter(|r| Ledger::is_on(r, &ids))
        .collect();
    let mut parts: Vec<String> = there
        .iter()
        .map(|r| {
            format!(
                "{}/{} ({}{}{}{})",
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
                // The part of an audio model's estimate it has not loaded
                // yet: not on the card, so the driver does not show it, and
                // kept free for it — why the free figure is smaller than the
                // driver says (realtime design §9.4).
                match r.pending_bytes {
                    Some(p) => format!(", {} of it until loaded", fmt_bytes(p)),
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
    parts
}
