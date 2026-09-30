//! A ladder climb's replacement admission (ladder design §3.4, §5; §12
//! entries 8–10 and 19–30).
//!
//! [`climb`] is the one way a ladder model moves to a higher rung: the request
//! path calls it when a request does not fit the rung that runs, and a
//! per-rung test can call it directly. It drives the registry's climb
//! primitive ([`crate::runtime::registry::ClimbTicket`]) and decides, like
//! admission decides for a cold start, whether the new rung may go on the
//! card now and what has to make room:
//!
//! 1. **Checks that cost nothing come first, before the mark**, so a climb
//!    that cannot happen never blocks the model: the GPU hold (no climb under
//!    it — the request's fallback answers, or `gpu_hold`); a rung larger than
//!    the card (`vram_too_large`, naming the rung); the phase-4 permission
//!    hook ([`may_climb`]: a guest never climbs the owner's model, and only
//!    into free VRAM — candidate-aliases §9; denied, [`Climbed::Denied`]);
//!    and, when the request may fall back, §4.7's verdict (entry 8): VRAM
//!    outside lmgw's control is short → the fallback answers with
//!    `external_vram`, the running rung untouched.
//!
//!    A guest's climb (a hold of `Origin::Background`) then drains like any
//!    other, but its step 4 is `vram::background`'s: the gate only tried,
//!    no eviction, no wait, its own rung only. It marks, or joins another
//!    guest's climb of the same model, only while the model is still the
//!    guests' and the owner is not waiting for room — never the owner's
//!    climb (candidate-aliases §12 entry 91).
//! 2. **Mark** the model climbing: new claims and new sends wait. A second
//!    trigger joins (raising the target, so one reload serves both) and waits.
//! 3. **Drain** lmgw's sends on the running rung, then its `/slots` until
//!    idle — what eviction asks a victim — within one budget from the mark,
//!    `vram.queue_timeout_seconds` (0 = no limit from it, as for admission).
//! 4. **Admission as a replacement**, behind the admission gate: the running
//!    rung's footprint counts as freed; lmgw's other idle models are evicted
//!    LRU-first (never this one); busy ones are waited for within the same
//!    budget. When it fits, the start is claimed while the gate is still
//!    held, so nothing else is told the memory the running rung frees.
//! 5. **Start** the new rung in a task of its own and wait for it: stop the
//!    running rung, start the new one, settle.
//!
//! Any refusal before step 5 leaves the running rung serving exactly as it
//! was, with the mark cleared. Every wait is bounded by an existing setting
//! (the queue timeout, the load timeout) — nothing here invents a limit.

use std::time::{Duration, Instant};

use super::background::{self, GuestClimb};
use super::{
    broadcast, busy_slots, describe_holders, lifecycle_spec, log_external, ExternalFallback,
    ExternalShortfall, ExternalVerdict, Fill, Gated, Ledger, LocalHold, Target, VramScheduler,
    Waiter, CONTROL_TIMEOUT, MIB, POLL,
};
use crate::config::{FallbackRoute, Route, Snapshot};
use crate::error::GatewayError;
use crate::gate::open::fallback_serves;
use crate::gate::FallbackReason;
use crate::hf::fmt_bytes;
use crate::runtime::descriptor::{model_runtime_at, ModelRuntime, RungPos};
use crate::runtime::registry::{
    ClimbRun, ClimbTicket, DrainEnd, Marked, Origin, RuntimeError, RuntimeState, Settled, StartSpec,
};
use crate::runtime::Class;
use crate::state::SharedState;

/// What [`climb`] did.
///
/// `#[allow(large_enum_variant)]` like [`FallbackRoute`]: the route is the
/// point of the fallback answer, and one of these exists per climb.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Climbed {
    /// The model runs a higher rung now — this climb's, or one another
    /// trigger ran while this one waited — or there was nothing to climb
    /// (the claim was stale, the row changed). Either way: [`LocalHold::sync`]
    /// and judge the request again on whatever runs.
    Done,
    /// The climb cannot happen now, and the request's fallback answers it
    /// instead: the GPU hold is on ([`FallbackReason::Hold`]), or the VRAM
    /// that is short is outside lmgw's control
    /// ([`FallbackReason::ExternalVram`], §12 entry 8). The running rung was
    /// not touched. The caller sends to `route` and stamps `x-lmgw-fallback:
    /// <alias>` with the reason, as the gate does at admission.
    Fallback {
        alias: String,
        route: Route,
        reason: FallbackReason,
    },
    /// A guest may not make this climb (candidate-aliases §9, §12 entry 45):
    /// the model is the owner's, or not its alias's primary, or the rung does
    /// not fit the VRAM free now, or the owner is waiting for room. `why`
    /// says which. The running rung was not touched — denied before the mark,
    /// or after the drain with the mark cleared — and nothing reached the
    /// client: the request gate answers it, by picking another loaded
    /// candidate or with the alias's fallback. vram never computes that
    /// answer itself. Only a hold of [`Origin::Background`] is ever denied.
    Denied { why: String },
}

/// The answer of [`may_climb`].
enum ClimbPermission {
    Allowed,
    Denied(String),
}

/// **Phase 4 hook — may this request climb this model?** (ladder design §12
/// entry 9; candidate-aliases design §9: "background may climb only the
/// primary, and only when it owns it", and only into free VRAM). Every climb
/// passes here, after the GPU hold and before anything is marked.
///
/// The owner's holds ([`LocalHold::origin`]) may make any climb, as before.
/// A guest's is judged by [`background::climb_denied`]: its own model, its
/// alias's primary, no owner waiting, and a rung that fits the VRAM free now
/// with the running rung counted as freed. Denied, the climb is
/// [`Climbed::Denied`] and the gate answers — never a climb of the owner's
/// ladder, never an eviction for a guest.
async fn may_climb(
    state: &SharedState,
    snap: &Snapshot,
    hold: &LocalHold,
    plan: &Plan,
) -> ClimbPermission {
    if hold.origin() == Origin::Owner {
        return ClimbPermission::Allowed;
    }
    match background::climb_denied(state, snap, hold, plan.needs, &plan.label_of(hold)).await {
        None => ClimbPermission::Allowed,
        Some(why) => ClimbPermission::Denied(why),
    }
}

/// Climb the model `hold` is on to rung `to` (0-based, like every rung in
/// code) because of `reason` — the numbers of the request that did not fit,
/// "prompt 41,210 + 8,192 > 30,000", shown on the status surfaces while the
/// climb runs.
///
/// `hold` is the triggering request's own claim: it keeps the model busy for
/// the length of the climb, and it is where the request's fallback policy is
/// read. On [`Climbed::Done`] the caller syncs it before sending again.
///
/// Errors, each leaving the running rung serving unless it says otherwise:
/// `gpu_hold` (no fallback may answer); `vram_too_large` and
/// `vram_queue_timeout` naming the rung; [`GatewayError::LadderDrainTimeout`]
/// when the running rung never went quiet; a 502 when a stop won against the
/// climb (the model is then down, as that stop wanted) or when the new rung
/// would not start (the model is then down too, and its next request starts
/// the base).
pub async fn climb(
    state: &SharedState,
    hold: &LocalHold,
    to: usize,
    reason: &str,
) -> Result<Climbed, GatewayError> {
    climb_with(state, hold, to, None, reason).await
}

/// [`climb`] on behalf of a request that needs `need` tokens in one slot —
/// its prompt plus its max output: the send path's climb. `to` is the
/// smallest rung that holds `need` on the row the caller judged. If the row
/// is edited while the climb drains, the rung is picked again by `need` on
/// the row as it is then (review S6, §12 entry 73): the index alone could now
/// name a smaller slot, and loading it would be a reload for nothing.
pub async fn climb_for(
    state: &SharedState,
    hold: &LocalHold,
    to: usize,
    need: u64,
    reason: &str,
) -> Result<Climbed, GatewayError> {
    climb_with(state, hold, to, Some(need), reason).await
}

async fn climb_with(
    state: &SharedState,
    hold: &LocalHold,
    to: usize,
    need: Option<u64>,
    reason: &str,
) -> Result<Climbed, GatewayError> {
    let target = hold.target.clone();
    let snap = state.snapshot();
    let Some(pos) = climb_target(&snap, hold, to) else {
        return Ok(Climbed::Done);
    };
    if snap.gpu_block().is_some() {
        return held(state, &snap, hold).await;
    }

    let Some(plan) = Plan::of(state, &snap, &target, pos).await else {
        return Ok(Climbed::Done);
    };
    let guest = hold.origin() == Origin::Background;
    let vs = &snap.settings.vram;
    let budget =
        (vs.queue_timeout_seconds > 0).then(|| Duration::from_secs(vs.queue_timeout_seconds));
    // A guest has no outside-VRAM swap: a rung that is not free is denied
    // (§9), and the gate answers.
    let mut fallback = if guest {
        None
    } else {
        climb_fallback(state, &snap, hold)
    };

    if vs.enabled {
        let l = state.vram.ledger(state, &snap).await;
        if let Some(cap) = &l.capacity {
            plan.fits_the_card(&target, cap.total)?;
        }
    }
    // After the card-size check, so a rung no card could hold stays the
    // configuration error it is for the owner, guest or not (§4.7's rule).
    if let ClimbPermission::Denied(why) = may_climb(state, &snap, hold, &plan).await {
        tracing::info!(
            "not climbing chat model '{}' to rung {} for background traffic ('{}'): {why}",
            target.model_id,
            plan.rung_text(),
            hold.alias
        );
        return Ok(Climbed::Denied { why });
    }
    if vs.enabled {
        if let Some(fb) = fallback.as_mut() {
            match state
                .vram
                .verdict_for(state, &snap, &target, plan.needs, Fill::Verdict)
                .await
            {
                ExternalVerdict::External(short) => {
                    if fb.external.confirm().await {
                        log_external(&hold.alias, &short, false);
                        return Ok(fb.answer());
                    }
                }
                ExternalVerdict::Unavailable(why) => tracing::debug!(
                    "outside-VRAM fallback not evaluated for climbing '{}': {why}",
                    target.model_id
                ),
                ExternalVerdict::Fits | ExternalVerdict::TooLarge => {}
            }
        }
    }

    // 2. The mark.
    let started = Instant::now();
    // A guest marks, or joins, only a climb of a model that is still the
    // guests' — asked in the lock hold that marks (§12 entry 91).
    let marked = {
        let g = hold.guard();
        if guest {
            state.runtime().mark_climb_for_guest(&g, pos, reason)
        } else {
            state.runtime().mark_climb(&g, pos, reason)
        }
    };
    let mut ticket = match marked {
        Marked::Ticket(ticket) => ticket,
        Marked::Joined { wait, .. } => {
            // A climb to the very rung this request needs could not start it:
            // that failure is this request's too (review finding 7, §12
            // entry 52). Re-admitting the base only to climb to the same
            // broken rung again would cost a base load and a failed load per
            // joiner. Any other ending: judge again on what runs.
            return match wait.outcome().await {
                Settled::ClimbFailed {
                    rung: Some(failed),
                    cause,
                } if failed.index == pos.index => Err(climb_failed(&target, &plan, cause)),
                _ => Ok(Climbed::Done),
            };
        }
        Marked::Stale => return Ok(Climbed::Done),
        Marked::Refused(why) => {
            let why = format!("'{}' {why}", target.model_id);
            tracing::info!(
                "not climbing chat model '{}' to rung {} for background traffic ('{}'): {why}",
                target.model_id,
                plan.rung_text(),
                hold.alias
            );
            return Ok(Climbed::Denied { why });
        }
    };
    tracing::info!(
        "climbing chat model '{}' to rung {} ({reason})",
        target.model_id,
        plan.rung_text()
    );
    let queued = state.vram.enqueue(
        state,
        Waiter {
            id: state.vram.next_id(),
            alias: hold.alias.clone(),
            model: target.model_id.clone(),
            class: target.class,
            needs: plan.needs,
            since: started,
            stage: format!(
                "climbing to rung {}: draining the running rung",
                plan.rung_text()
            ),
            origin: hold.origin(),
        },
    );
    broadcast(state);

    // 3. The drain.
    let deadline = budget.map(|b| started + b);
    drain(state, &mut ticket, deadline, started, &target, &plan).await?;

    // The row, the hold, or another trigger's need may have changed while the
    // drain waited: plan again on what is true now (review findings 3–4, §12
    // entries 49–50). The holder's own rung is planned on the current row —
    // an edit made during the wait takes effect in this climb — and is what
    // this request is judged on: its size check, its admission wait, its
    // fallback. A joiner's raised rung is only taken when it fits right now;
    // otherwise the joiner judges again after this climb, on its own verdict.
    let snap = state.snapshot();
    if snap.gpu_block().is_some() {
        drop(ticket);
        return held(state, &snap, hold).await;
    }
    // The rung is picked on the row as it is now: by the request's need when
    // it has one, else by the index it asked for; either way it has to be a
    // bigger slot than the one that runs, or there is nothing to climb to
    // (§12 entry 73).
    let running = hold
        .gate_facts()
        .and_then(|f| f.per_request_ctx())
        .unwrap_or(0);
    let own_index = match need {
        Some(n) => {
            let floor = n.max(running.saturating_add(1));
            match smallest_rung_holding(state, &snap, &target, floor).await {
                Some(i) => i,
                None => return Ok(Climbed::Done),
            }
        }
        None => plan.pos.index,
    };
    let Some(own) = Plan::at(state, &snap, &target, own_index)
        .await
        .filter(|own| own.slot > running)
    else {
        return Ok(Climbed::Done);
    };
    let raised = ticket.to().index;
    let mut joiner = if raised > own.pos.index {
        Plan::at(state, &snap, &target, raised)
            .await
            .filter(|j| j.slot > own.slot)
    } else {
        None
    };
    let plan = own;
    if snap.settings.vram.enabled {
        if let Some(cap) = &state.vram.ledger(state, &snap).await.capacity {
            plan.fits_the_card(&target, cap.total)?;
            joiner = joiner.filter(|j| j.needs <= cap.total);
        }
    }
    state.vram.set_needs(queued.id, plan.needs);

    // 4. Admission as a replacement, and the claim. A guest's: tried, never
    // waited for, never evicting, its own rung only (§9).
    let (run, plan, for_joiner) = if guest {
        let running = state
            .vram
            .footprint_at(&snap, target.class, &target.model_id, ticket.running())
            .await
            .map_or(0, |f| f.total_bytes);
        let spec = StartSpec::of(&lifecycle_spec(state, &snap, &plan.runtime));
        let rung = plan.label(&target);
        match background::admit_climb_background(
            state, &snap, &target, ticket, spec, plan.needs, running, &rung,
        )
        .await
        {
            GuestClimb::Started(run) => (run, plan, false),
            GuestClimb::Denied(why) => {
                tracing::info!(
                    "not climbing chat model '{}' to rung {} for background traffic ('{}'): {why}",
                    target.model_id,
                    plan.rung_text(),
                    hold.alias
                );
                return Ok(Climbed::Denied { why });
            }
            GuestClimb::Gone => return Err(stopped_while_climbing(&target, &plan)),
        }
    } else if snap.settings.vram.enabled {
        let running = state
            .vram
            .footprint_at(&snap, target.class, &target.model_id, ticket.running())
            .await
            .map_or(0, |f| f.total_bytes);
        let admitted = state
            .vram
            .admit_climb(
                state,
                &snap,
                &target,
                ticket,
                (&plan, joiner.as_ref()),
                running,
                &hold.alias,
                queued.id,
                started,
                budget,
                fallback.as_mut().map(|fb| &mut fb.external),
            )
            .await?;
        match admitted {
            AdmittedClimb::Started {
                run,
                joiner: for_joiner,
            } => match (for_joiner, joiner) {
                (true, Some(j)) => (run, j, true),
                _ => (run, plan, false),
            },
            AdmittedClimb::External => {
                return fallback.as_ref().map(ClimbFallback::answer).ok_or_else(|| {
                    GatewayError::Internal(
                        "the climb's admission answered with a fallback nobody offered".into(),
                    )
                })
            }
            AdmittedClimb::Held => return held(state, &state.snapshot(), hold).await,
            AdmittedClimb::Gone => return Err(stopped_while_climbing(&target, &plan)),
        }
    } else {
        // Admission is off: the start is unarbitrated, exactly as a cold
        // start's `acquire` is then (per-model-containers §3.2) — and nothing
        // judges whether the joiner's rung fits, so one reload serves both.
        let for_joiner = joiner.is_some();
        let plan = joiner.unwrap_or(plan);
        let spec = StartSpec::of(&lifecycle_spec(state, &snap, &plan.runtime));
        let run = ticket
            .start(spec)
            .ok_or_else(|| stopped_while_climbing(&target, &plan))?;
        (run, plan, for_joiner)
    };
    drop(queued);
    broadcast(state);

    // 5. The start, in its own task: a trigger that goes away here does not
    // take the climb with it.
    let outcome = run.finish().await;
    state.vram.cache_pids(state);
    broadcast(state);
    match outcome {
        Ok(()) => Ok(Climbed::Done),
        Err(RuntimeError::Aborted { .. }) => Err(stopped_while_climbing(&target, &plan)),
        Err(e) => {
            tracing::warn!(
                "climbing chat model '{}' to rung {} failed: {e}",
                target.model_id,
                plan.rung_text()
            );
            if for_joiner {
                // The rung that failed was the joiner's, and the joiner is
                // told so ([`Settled::ClimbFailed`]). This request never needed
                // it: it judges again, and climbs to its own rung from the
                // base (§12 entries 49 and 52).
                return Ok(Climbed::Done);
            }
            Err(climb_failed(&target, &plan, e.to_string()))
        }
    }
}

/// The trigger's 502 when a climb's rung would not start (§12 entry 22).
fn climb_failed(target: &Target, plan: &Plan, why: String) -> GatewayError {
    GatewayError::Upstream {
        status: 502,
        provider_type: None,
        message: format!(
            "climbing '{}' to rung {} failed: {why}",
            target.model_id,
            plan.rung_text()
        ),
    }
}

/// The rung `to` names, when it is a climb at all: above the rung the hold's
/// container runs and within the row's ladder as it is now. `None` — the
/// caller's judgement is stale (it should have synced), or the ladder
/// changed under it — is answered [`Climbed::Done`]: judge again.
fn climb_target(snap: &Snapshot, hold: &LocalHold, to: usize) -> Option<RungPos> {
    if hold.class() != Class::Chat {
        return None;
    }
    let row = snap
        .local_models
        .iter()
        .find(|m| m.model_id == hold.model_id())?;
    let running = hold.rung().map_or(0, |r| r.index);
    (to > running && to <= row.top_rung()).then_some(RungPos {
        index: to,
        of: row.top_rung() + 1,
    })
}

/// The smallest rung of the row as `snap` has it whose slot — its per-slot
/// context, capped at its weights' trained context as llama-server caps it —
/// holds `floor` tokens; `None` when none does, or the row is gone.
async fn smallest_rung_holding(
    state: &SharedState,
    snap: &Snapshot,
    target: &Target,
    floor: u64,
) -> Option<usize> {
    let row = snap
        .local_models
        .iter()
        .find(|m| m.model_id == target.model_id)?;
    let trained =
        crate::gate::ladder::trained_contexts(state, &snap.settings.router.models_dir, row).await;
    (0..=row.top_rung()).find(|&i| {
        row.per_slot_ctx(i)
            .map(|c| crate::ladder::slot_ctx(c, trained.get(i).copied().flatten()))
            .and_then(|c| u64::try_from(c).ok())
            .is_some_and(|slot| slot >= floor)
    })
}

/// What one climb target costs and how it is started.
struct Plan {
    pos: RungPos,
    runtime: ModelRuntime,
    /// The rung's slot: per-slot context capped at its weights' trained
    /// context, as the gate will judge the container this start runs.
    slot: u64,
    /// The rung's footprint, sized like any row ([`VramScheduler::footprint_at`]).
    bytes: u64,
    /// `bytes` plus `vram.headroom_mb`: what admission waits for.
    needs: u64,
    headroom: u64,
    gguf: String,
}

impl Plan {
    /// [`Self::of`] rung `index` of the row as `snap` has it — `None` when the
    /// row is gone or no longer has that rung: the climb's judgement is
    /// stale, and the caller judges again.
    async fn at(
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        index: usize,
    ) -> Option<Self> {
        let row = snap
            .local_models
            .iter()
            .find(|m| m.model_id == target.model_id)
            .filter(|m| index <= m.top_rung())?;
        let pos = RungPos {
            index,
            of: row.top_rung() + 1,
        };
        Self::of(state, snap, target, pos).await
    }

    /// `None` when no row describes the model in `snap` (it was deleted
    /// meanwhile): there is nothing to climb, and the caller judges again.
    async fn of(
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        pos: RungPos,
    ) -> Option<Self> {
        // The rung was checked against this very snapshot, so the descriptor
        // renders that rung (never the base fallback).
        let runtime = model_runtime_at(snap, Class::Chat, &target.model_id, pos.index)?;
        let charge = runtime.rung_charge();
        let slot = match &runtime.llama {
            Some(crate::runtime::argv::LlamaArgs::Chat {
                gguf_path, params, ..
            }) => {
                let dir = &snap.settings.router.models_dir;
                let trained =
                    crate::capabilities::exposed::trained_context(state, dir, gguf_path).await;
                params
                    .per_request_ctx(None)
                    .map(|c| crate::ladder::slot_ctx(c, trained))
                    .and_then(|c| u64::try_from(c).ok())
                    .unwrap_or(0)
            }
            _ => 0,
        };
        let bytes = state
            .vram
            .footprint_at(snap, target.class, &target.model_id, charge.as_ref())
            .await
            .map_or(0, |f| f.total_bytes);
        let headroom = snap.settings.vram.headroom_mb.saturating_mul(MIB);
        Some(Self {
            pos,
            slot,
            gguf: charge
                .as_ref()
                .map(|c| c.gguf_file().to_string())
                .unwrap_or_default(),
            runtime,
            bytes,
            needs: bytes.saturating_add(headroom),
            headroom,
        })
    }

    /// `k/n (<gguf>)`, 1-based — how every message names the rung.
    fn rung_text(&self) -> String {
        format!("{}/{} ({})", self.pos.index + 1, self.pos.of, self.gguf)
    }

    /// "<id> rung k/n (<gguf>)" — the model, as the VRAM errors name it.
    fn label(&self, target: &Target) -> String {
        format!("{} rung {}", target.model_id, self.rung_text())
    }

    /// [`Self::label`] for the model `hold` is on.
    fn label_of(&self, hold: &LocalHold) -> String {
        format!("{} rung {}", hold.model_id(), self.rung_text())
    }

    /// A rung that cannot fit on an empty card is refused before anything is
    /// marked (`vram_too_large`): no eviction and no wait can make it fit.
    fn fits_the_card(&self, target: &Target, capacity: u64) -> Result<(), GatewayError> {
        if self.needs > capacity {
            return Err(GatewayError::VramTooLarge {
                model: self.label(target),
                need: fmt_bytes(self.bytes),
                headroom: fmt_bytes(self.headroom),
                capacity: fmt_bytes(capacity),
            });
        }
        Ok(())
    }
}

/// The request's fallback, for the outside-VRAM verdict on a climb (§12 entry
/// 8): present when the hold's request may fall back ([`LocalHold::policy`]),
/// the trigger is switched on and the row has a usable fallback. Whether it
/// may take this request is asked only once a verdict says it could.
struct ClimbFallback {
    alias: String,
    route: Route,
    external: ExternalFallback<'static>,
}

impl ClimbFallback {
    fn answer(&self) -> Climbed {
        Climbed::Fallback {
            alias: self.alias.clone(),
            route: self.route.clone(),
            reason: FallbackReason::ExternalVram,
        }
    }
}

fn climb_fallback(state: &SharedState, snap: &Snapshot, hold: &LocalHold) -> Option<ClimbFallback> {
    let policy = hold.policy()?.clone();
    if super::trigger_off(snap).is_some() {
        return None;
    }
    // The request's fallback: a candidate alias's own, never its candidate's
    // row fallback (candidate-aliases §4.1), even if the alias went away
    // mid-flight (§12 entry 86).
    let FallbackRoute::Usable { alias, route } =
        crate::gate::candidate::request_fallback(snap, hold)
    else {
        return None;
    };
    let facets = crate::gate::candidate::fallback_facets(snap, hold);
    let serves = {
        let (state, alias, route, requested) = (
            state.clone(),
            alias.clone(),
            route.clone(),
            hold.alias.clone(),
        );
        async move {
            fallback_serves(
                &state,
                &alias,
                &route,
                &requested,
                policy.check,
                policy.images,
                facets,
            )
            .await
        }
    };
    Some(ClimbFallback {
        alias,
        route,
        external: ExternalFallback::confirmed_by(serves),
    })
}

/// The GPU hold is on: no climb (ladder design §3.1 "GPU hold active") — the
/// request's fallback answers, as the hold's swap would have at resolve time
/// (so the route check runs, and the image rule does not, §12 entry 43 of the
/// unified-KV spec), or it is refused with `gpu_hold`. Pinned and direct
/// callers never fall back. A benchmark's lease is answered the same way
/// (reason `benchmark`, `gpu_benchmark`; benchmark design §3.2).
async fn held(
    state: &SharedState,
    snap: &Snapshot,
    hold: &LocalHold,
) -> Result<Climbed, GatewayError> {
    let block = snap
        .gpu_block()
        .unwrap_or(crate::bench::lease::GpuBlock::Hold);
    let refused = |detail: String| block.refusal(hold.model_id(), detail);
    let Some(policy) = hold.policy().cloned() else {
        return Err(refused(String::new()));
    };
    // A candidate alias's own fallback, never its candidate's row fallback
    // (candidate-aliases §4.1): the hold swap answers both alias kinds with it,
    // even if the alias went away mid-flight (§12 entry 86).
    let facets = crate::gate::candidate::fallback_facets(snap, hold);
    match crate::gate::candidate::request_fallback(snap, hold) {
        FallbackRoute::Usable { alias, route } => {
            if fallback_serves(
                state,
                &alias,
                &route,
                &hold.alias,
                policy.check,
                false,
                facets,
            )
            .await
            {
                Ok(Climbed::Fallback {
                    alias,
                    route,
                    reason: block.fallback_reason(),
                })
            } else {
                Err(refused(format!(
                    " (fallback '{alias}' cannot serve this endpoint)"
                )))
            }
        }
        FallbackRoute::Unusable { alias, why } => {
            Err(refused(format!(" (fallback '{alias}' {why})")))
        }
        FallbackRoute::None => Err(refused(String::new())),
    }
}

/// Step 3: lmgw's sends on the running rung, then the container's own view
/// of its slots — the busy probe eviction runs on a victim — until idle, all
/// within the climb's one budget. An unanswerable `/slots` counts as idle, as
/// it does for eviction and the hold sweep.
async fn drain(
    state: &SharedState,
    ticket: &mut ClimbTicket,
    deadline: Option<Instant>,
    started: Instant,
    target: &Target,
    plan: &Plan,
) -> Result<(), GatewayError> {
    let timed_out = |sends: u32, slots: usize| GatewayError::LadderDrainTimeout {
        model: target.model_id.clone(),
        rung: plan.rung_text(),
        waited_seconds: started.elapsed().as_secs(),
        sends,
        slots,
    };
    ticket.set_stage("draining the running rung's requests");
    match ticket.drain(deadline).await {
        Ok(()) => {}
        Err(DrainEnd::Timeout { sends }) => return Err(timed_out(sends, 0)),
        Err(DrainEnd::Gone) => return Err(stopped_while_climbing(target, plan)),
    }
    loop {
        match busy_slots(&state.http, ticket.port(), CONTROL_TIMEOUT).await {
            Some(0) | None => return Ok(()),
            Some(n) => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err(timed_out(0, n));
                }
                ticket.set_stage(format!(
                    "waiting for {n} slot(s) still generating on the running rung"
                ));
                let pause = deadline.map_or(POLL, |d| {
                    POLL.min(d.saturating_duration_since(Instant::now()))
                });
                tokio::time::sleep(pause).await;
            }
        }
    }
}

/// A stop won against the climb — an override, a delete or disable, the
/// shutdown (§12 races). The model is down, as that stop wanted, and nothing
/// was started again.
fn stopped_while_climbing(target: &Target, plan: &Plan) -> GatewayError {
    GatewayError::Upstream {
        status: 502,
        provider_type: None,
        message: format!(
            "'{}' was stopped while it was climbing to rung {} — nothing was restarted",
            target.model_id,
            plan.rung_text()
        ),
    }
}

/// Claim and spawn the start of `chosen` — the holder's own rung, or the
/// joiner's (`for_joiner`).
fn start_climb(
    state: &SharedState,
    snap: &Snapshot,
    ticket: ClimbTicket,
    chosen: &Plan,
    for_joiner: bool,
) -> AdmittedClimb {
    let spec = StartSpec::of(&lifecycle_spec(state, snap, &chosen.runtime));
    ticket
        .start(spec)
        .map_or(AdmittedClimb::Gone, |run| AdmittedClimb::Started {
            run,
            joiner: for_joiner,
        })
}

/// What [`VramScheduler::admit_climb`] settled on.
enum AdmittedClimb {
    /// Fits: the start is claimed and running — at the joiner's raised rung
    /// when `joiner`, otherwise at the holder's own.
    Started { run: ClimbRun, joiner: bool },
    /// While it waited, the shortfall became VRAM outside lmgw's control, and
    /// the request's fallback answers.
    External,
    /// The GPU hold was switched on while it waited: nothing is started.
    Held,
    /// A stop took the model before the start could be claimed.
    Gone,
}

impl VramScheduler {
    /// Step 4, the **replacement admission** (ladder design §3.4 step 3, §5):
    /// [`Self::decide`]'s loop for a model that is already resident and whose
    /// footprint changes.
    ///
    /// - the admission gate is taken the way `decide` takes it — in slices
    ///   while a fallback could still answer, keeping its FIFO place;
    /// - the running rung counts as freed: `free + fp(running) ≥ needs`.
    ///   The estimate is a lower bound of what its stop frees, so this is
    ///   conservative with a measured card, and exact under a declared budget;
    /// - short of that, one idle model of lmgw's is evicted, LRU-first, and
    ///   never this one ([`Self::evict_one`] excludes the target); with none
    ///   evictable it waits for a busy one, taking §4.7's verdict again each
    ///   pass while the request could fall back;
    /// - within the climb's one budget, counted from its mark — past it,
    ///   `vram_queue_timeout` naming the rung;
    /// - climbs that can only fit on each other's memory never wait on each
    ///   other: the one holding the gate gives way at once with
    ///   `vram_queue_timeout` naming the others ([`Self::climbs_deadlocked`],
    ///   candidate-aliases §12 entry 93), so its model can be evicted for
    ///   them once its request is over;
    /// - `plans` is the holder's own rung and, when another trigger raised the
    ///   target, the joiner's: the joiner's is claimed only when it fits at
    ///   the moment of a ledger read; everything else — the size it waits
    ///   and evicts for, the verdict that can send it to its fallback, the
    ///   timeout's name — is the holder's own (§12 entry 49);
    /// - when it fits, the start is claimed **while the gate is held**: the
    ///   entry flips to `starting` on the new rung, which is what the next
    ///   admission measures. Not a [`Reserved`](super::Reserved): a
    ///   reservation is skipped while any entry answers for its model, so it
    ///   would never be charged, and flipping only after the old rung's stop
    ///   would let another admission take the freed memory in between. Until
    ///   that stop completes the old rung is still in the driver's `used`
    ///   while the new one is charged too — a transient over-count, in the
    ///   safe direction.
    #[allow(clippy::too_many_arguments)]
    async fn admit_climb(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        ticket: ClimbTicket,
        (plan, joiner): (&Plan, Option<&Plan>),
        running: u64,
        alias: &str,
        waiter_id: u64,
        started: Instant,
        budget: Option<Duration>,
        mut fallback: Option<&mut ExternalFallback<'_>>,
    ) -> Result<AdmittedClimb, GatewayError> {
        let expired = |holding: String| GatewayError::VramQueueTimeout {
            model: plan.label(target),
            waited_seconds: started.elapsed().as_secs(),
            holding,
        };
        let stage = |text: &str| {
            self.set_stage(waiter_id, text);
            ticket.set_stage(text);
        };
        stage("waiting for the admission gate");
        ticket.set_at_gate(true);
        let mut gate = match self
            .take_gate(
                state,
                target,
                alias,
                Some(plan.needs),
                started,
                budget,
                &mut fallback,
            )
            .await
        {
            Gated::Held(gate) => gate,
            Gated::External(_) => return Ok(AdmittedClimb::External),
            Gated::Expired => {
                return Err(expired(
                    "another request was still being admitted to the GPU".into(),
                ))
            }
        };
        ticket.set_at_gate(false);

        stage("measuring GPU memory");
        let mut drain = None;
        loop {
            // Switched on since the mark: nothing new goes on the card. The
            // mark is cleared as the ticket drops; the running rung stays.
            if state.snapshot().gpu_block().is_some() {
                return Ok(AdmittedClimb::Held);
            }
            let l = self.ledger(state, snap).await;
            let Some(cap) = l.capacity.as_ref() else {
                // Telemetry vanished mid-wait: unarbitrated, like `decide`.
                return Ok(match joiner {
                    Some(j) => start_climb(state, snap, ticket, j, true),
                    None => start_climb(state, snap, ticket, plan, false),
                });
            };
            let free = cap.free.saturating_add(running);
            if let Some(j) = joiner.filter(|j| free >= j.needs) {
                self.set_stage(waiter_id, "starting the new rung");
                return Ok(start_climb(state, snap, ticket, j, true));
            }
            if free >= plan.needs {
                self.set_stage(waiter_id, "starting the new rung");
                return Ok(start_climb(state, snap, ticket, plan, false));
            }
            if budget.is_some_and(|d| started.elapsed() >= d) {
                return Err(expired(describe_holders(&l)));
            }
            // An owner's climb waiting for room drains every resident model
            // for them, like `decide`'s wait (candidate-aliases §4.5, §12
            // entry 47) — a guest's climb never gets here. Held until this
            // returns, however it returns.
            if drain.is_none() {
                drain = background::owner_drain(state, snap);
            }
            stage(&format!(
                "needs {}, {} free with the running rung's {} counted — evicting",
                fmt_bytes(plan.needs),
                fmt_bytes(free),
                fmt_bytes(running)
            ));
            if self
                .evict_one(state, target, &l, background::Victims::Any)
                .await
            {
                continue;
            }
            // Climbs that can only fit on each other's memory never end by
            // waiting (§12 entry 93): this one gives way now.
            if let Some(why) = self.climbs_deadlocked(state, target, &l, plan.needs, running) {
                tracing::warn!(
                    "climbing chat model '{}' to rung {} gives way: {why}",
                    target.model_id,
                    plan.rung_text()
                );
                return Err(expired(why));
            }
            stage("waiting for a busy model to finish");
            tokio::time::sleep(POLL).await;
            if self
                .climb_recheck(state, target, plan.needs, alias, &mut fallback)
                .await
                .is_some()
            {
                return Ok(AdmittedClimb::External);
            }
            // Another ladder's climb waiting behind this one (§12 entry 53).
            if self.climb_needs_the_gate(state, target) {
                drop(gate);
                ticket.set_at_gate(true);
                gate = match self
                    .take_gate(
                        state,
                        target,
                        alias,
                        Some(plan.needs),
                        started,
                        budget,
                        &mut fallback,
                    )
                    .await
                {
                    Gated::Held(gate) => gate,
                    Gated::External(_) => return Ok(AdmittedClimb::External),
                    // Out of time back in the queue: it was still waiting
                    // for the busy models it yielded over (§12 entry 74).
                    Gated::Expired => return Err(expired(describe_holders(&l))),
                };
                ticket.set_at_gate(false);
            }
        }
    }

    /// Whether this climb — holding the gate, `needs` bytes for its rung,
    /// its running rung `running`, nothing left it may evict — and every
    /// other climb queued for the gate are stuck on each other for good
    /// (final review X7, §12 entry 93). `Some` says who, for the refusal.
    ///
    /// A climbing model stays busy until its climb ends: its trigger's claim
    /// holds it, and eviction never takes a model that is being climbed. So
    /// among climbs that all wait for room, one can only ever get what the
    /// card holds less the other climbs' running rungs. When that is short
    /// of what *each* of them needs, none can finish first, and the yield of
    /// §12 entry 53 would hand the gate back and forth until the queue
    /// budget ends — never, at 0 — with `owner_drain` armed all along. The
    /// card's total is used, not the free figure: VRAM outside lmgw and
    /// lmgw's other models only take room away, and the running rungs'
    /// estimates are lower bounds, so this never names a set of climbs that
    /// could have finished one after another. A climb whose queue row is not
    /// found is not judged (`None`).
    fn climbs_deadlocked(
        &self,
        state: &SharedState,
        target: &Target,
        l: &Ledger,
        needs: u64,
        running: u64,
    ) -> Option<String> {
        let total = l.capacity.as_ref()?.total;
        let others: Vec<(Class, String)> = state
            .runtime()
            .list()
            .into_iter()
            .filter(|e| {
                e.state == RuntimeState::Ready
                    && e.climbing.as_ref().is_some_and(|c| c.at_gate)
                    && !(e.class == target.class && e.model_id == target.model_id)
            })
            .map(|e| (e.class, e.model_id))
            .collect();
        if others.is_empty() {
            return None;
        }
        // (model, its running rung's bytes, what its climb needs)
        let mut climbs = vec![(target.model_id.clone(), running, needs)];
        {
            let live = self.live.lock().unwrap();
            for (class, model) in others {
                let w = live
                    .queue
                    .iter()
                    .find(|w| w.class == class && w.model == model)?;
                let bytes = l
                    .residents
                    .iter()
                    .find(|r| r.container == class && r.model == model)
                    .map_or(0, |r| r.estimated_bytes);
                climbs.push((model, bytes, w.needs));
            }
        }
        let held: u64 = climbs.iter().map(|(_, bytes, _)| bytes).sum();
        let stuck = climbs
            .iter()
            .all(|(_, bytes, needs)| total.saturating_sub(held.saturating_sub(*bytes)) < *needs);
        if !stuck {
            return None;
        }
        let others: Vec<String> = climbs[1..]
            .iter()
            .map(|(model, bytes, needs)| {
                format!(
                    "'{model}' (running {}, needs {})",
                    fmt_bytes(*bytes),
                    fmt_bytes(*needs)
                )
            })
            .collect();
        Some(format!(
            "the climb of {} waits for room too, and on this card ({}) none of these climbs \
             fits next to the others' running rungs — a climbing model stays busy until its \
             climb ends, so no wait could end; this climb gives way, and its model can be \
             evicted for the other once this request is over",
            others.join(", "),
            fmt_bytes(total)
        ))
    }

    /// [`Self::recheck`] for a climb: §4.7's verdict on the new rung's `needed`
    /// ([`Self::verdict_for`]) — the model is up, so the plain verdict would
    /// always say it fits.
    pub(super) async fn climb_recheck(
        &self,
        state: &SharedState,
        target: &Target,
        needed: u64,
        alias: &str,
        fallback: &mut Option<&mut ExternalFallback<'_>>,
    ) -> Option<ExternalShortfall> {
        let fb = fallback.as_deref_mut().filter(|f| f.live())?;
        let snap = state.snapshot();
        let ExternalVerdict::External(short) = self
            .verdict_for(state, &snap, target, needed, Fill::Recheck)
            .await
        else {
            return None;
        };
        if !fb.confirm().await {
            return None;
        }
        log_external(alias, &short, true);
        Some(short)
    }
}
