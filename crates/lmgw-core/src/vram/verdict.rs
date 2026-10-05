//! §4.7's outside-VRAM verdict, and the footprint/residency queries admission
//! decides against.

use crate::config::Snapshot;
use crate::runtime::descriptor::RungCharge;
use crate::runtime::registry::{RuntimeState, RuntimeView};
use crate::runtime::Class;
use crate::state::SharedState;

use super::admission::{external_table, log_external};
use super::attribution::Fill;
use super::ledger::trigger_off;
use super::plan::Footprint;
use super::scheduler::MIB;
use super::{ExternalFallback, ExternalShortfall, ExternalVerdict, Target, VramScheduler};

impl VramScheduler {
    /// §4.7's verdict for `target`: may a caller with a fallback answer with
    /// it right away? It never queues, never evicts, never starts or claims
    /// anything — it measures and answers ([`ExternalVerdict`]).
    ///
    /// **Pooled like the ledger.** `needed` is exactly what admission waits
    /// for (footprint + `vram.headroom_mb`), the capacity is the ledger's, and
    /// "free" and lmgw's share are summed over every device: there is no
    /// model-to-device mapping anywhere in lmgw, so a per-device verdict
    /// would be a guess about placement.
    ///
    /// Not taken under the admission gate: a request waiting in the queue
    /// holds it for as long as it waits, and the point of `External` is to
    /// answer without waiting. It needs no claim either — a start admitted
    /// meanwhile shows up as a reservation, which makes the next verdict
    /// `Unavailable`, never a wrong `External`.
    ///
    /// Called by [`admit_or_external`](crate::vram::admit_or_external), and by phase 4's non-background
    /// candidate aliases to decide between loading the primary and the
    /// alias's fallback. The caller logs what it does with `External`.
    pub async fn external_verdict(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
    ) -> ExternalVerdict {
        self.verdict(state, snap, target, Fill::Verdict).await
    }

    /// [`Self::external_verdict`], with how hard it may try to learn a PID
    /// ([`Fill`]).
    async fn verdict(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        fill: Fill,
    ) -> ExternalVerdict {
        if let Some(why) = trigger_off(snap).or_else(|| self.boot_unsettled()) {
            return ExternalVerdict::Unavailable(why);
        }
        if self.is_up(state, target) {
            return ExternalVerdict::Fits;
        }
        let Some(fp) = self.footprint(snap, target.class, &target.model_id).await else {
            return ExternalVerdict::Unavailable(format!(
                "no row describes {}/{}",
                target.class.as_str(),
                target.model_id
            ));
        };
        let needed = fp
            .total_bytes
            .saturating_add(snap.settings.vram.headroom_mb.saturating_mul(MIB));
        self.verdict_for(state, snap, target, needed, fill).await
    }

    /// §4.7's verdict for `needed` bytes on behalf of `target` — [`Self::verdict`]
    /// without its "the model is up, so it fits" exit.
    ///
    /// For a ladder climb (ladder design §12 entry 8), whose model *is* up and
    /// whose footprint changes: `needed` is the new rung's footprint plus the
    /// headroom, and lmgw's measured share already contains the running rung,
    /// so `free + lmgw_share ≥ needed` means exactly "lmgw can make room by
    /// evicting its other models and stopping the running rung".
    pub(crate) async fn verdict_for(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        needed: u64,
        fill: Fill,
    ) -> ExternalVerdict {
        if let Some(why) = trigger_off(snap).or_else(|| self.boot_unsettled()) {
            return ExternalVerdict::Unavailable(why);
        }
        let l = self.ledger(state, snap).await;
        let Some(cap) = l.capacity.as_ref() else {
            return ExternalVerdict::Unavailable(
                l.inactive_reason
                    .clone()
                    .unwrap_or_else(|| "nothing to measure against".into()),
            );
        };
        // Before any attribution: a model that cannot fit on an empty card
        // needs no `podman inspect` to be told so.
        if needed > cap.total {
            return ExternalVerdict::TooLarge;
        }
        match self.measure(state, snap, &l, fill).await {
            Ok(m) => external_table(target, needed, m.total, m.raw_free, m.shares),
            Err(why) => ExternalVerdict::Unavailable(why),
        }
    }

    /// While a request that has a fallback waits for room, look again (review
    /// finding 8): the shortfall can turn into VRAM outside lmgw's control a
    /// moment after the request queued — a start that made the first verdict
    /// unavailable has landed, a game was launched — and then waiting does
    /// not help. `Some` is the shortfall the fallback answers; the caller
    /// leaves the queue. Cheap in the steady state: every PID is cached, so a
    /// pass reads `/proc` and the driver and spawns nothing ([`Fill::Recheck`]
    /// never re-asks podman for a read that failed).
    pub(super) async fn recheck(
        &self,
        state: &SharedState,
        target: &Target,
        alias: &str,
        fallback: &mut Option<&mut ExternalFallback<'_>>,
    ) -> Option<ExternalShortfall> {
        let fb = fallback.as_deref_mut().filter(|f| f.live())?;
        let snap = state.snapshot();
        let ExternalVerdict::External(short) =
            self.verdict(state, &snap, target, Fill::Recheck).await
        else {
            return None;
        };
        if !fb.confirm().await {
            return None;
        }
        log_external(alias, &short, true);
        Some(short)
    }

    /// Is a container for this model up (or coming up) right now?
    pub(crate) fn is_up(&self, state: &SharedState, target: &Target) -> bool {
        state.runtime().list().iter().any(|e| {
            e.class == target.class
                && e.model_id == target.model_id
                && matches!(e.state, RuntimeState::Starting | RuntimeState::Ready)
        })
    }

    /// The context window a local model's KV cache was sized for, from the
    /// footprint the planner already computed and memoized: the row's own
    /// `ctx-size` when it sets one, otherwise the GGUF header's declared
    /// context length.
    ///
    /// This is what `GET /v1/models` publishes as `context_length` for aux
    /// models (§5) now that there is no always-on router to ask for a live
    /// `n_ctx`. `None` means the GGUF could not be read or declares nothing —
    /// reported as unknown, never invented.
    pub async fn context_tokens(
        &self,
        snap: &Snapshot,
        class: Class,
        model_id: &str,
    ) -> Option<u64> {
        self.footprint(snap, class, model_id).await?.ctx_tokens
    }

    /// What a registry entry is charged: the footprint of the rung its
    /// container was started at ([`RuntimeView::charge`], ladder design §5),
    /// or — on every row without a ladder — the row's own, exactly as before.
    /// An audio container is charged the residency learned for the
    /// configuration it was started with, which a class-level change may
    /// have moved the row away from while it kept running
    /// ([`super::residency::expected_for`]).
    pub(super) async fn footprint_of(&self, snap: &Snapshot, e: &RuntimeView) -> Option<Footprint> {
        if e.class == Class::Audio {
            let s = &snap.settings.audio;
            let row = snap
                .audio_models
                .iter()
                .find(|m| m.model_id == e.model_id)?;
            let files = self.plans.audio(&s.models_dir, row).await;
            // By the container's own placement: the row may have been
            // switched since it started.
            if e.placement == crate::runtime::Placement::Cpu {
                return Some(super::on_cpu::cpu_footprint(files));
            }
            return Some(super::residency::expected_for(
                files,
                row,
                s,
                e.resident_key.as_deref(),
            ));
        }
        self.footprint_at(snap, e.class, &e.model_id, e.charge.as_ref())
            .await
    }

    /// The footprint of `model_id` at a ladder rung: the row with that rung's
    /// weights and `--ctx-size` — the only two things a rung changes (§4.1) —
    /// planned the way every row is, so the plan cache keys it by file and
    /// context like any other. Everything else (cache types, projector,
    /// drafter) is the row's. `charge: None` is [`Self::footprint`], which
    /// is also what every cold start is sized by: a cold start is the base.
    pub(super) async fn footprint_at(
        &self,
        snap: &Snapshot,
        class: Class,
        model_id: &str,
        charge: Option<&RungCharge>,
    ) -> Option<Footprint> {
        let Some(c) = charge.filter(|_| class == Class::Chat) else {
            return self.footprint(snap, class, model_id).await;
        };
        let mut row = snap
            .local_models
            .iter()
            .find(|m| m.model_id == model_id)?
            .clone();
        row.gguf_path = c.gguf_path.clone();
        row.params.ctx_size = c.ctx_size;
        Some(
            self.plans
                .local(&snap.settings.router.models_dir, &row)
                .await,
        )
    }

    /// The footprint of a model this gateway configured, or `None` when no row
    /// describes it. Nothing unknown is ever charged for on a guess. Also
    /// what the realtime settings' VRAM budget sums (`ops::realtime_budget`),
    /// so the figure shown there is the one admission charges.
    pub(crate) async fn footprint(
        &self,
        snap: &Snapshot,
        class: Class,
        model_id: &str,
    ) -> Option<Footprint> {
        let s = &snap.settings;
        match class {
            Class::Chat => {
                let row = snap.local_models.iter().find(|m| m.model_id == model_id)?;
                Some(self.plans.local(&s.router.models_dir, row).await)
            }
            Class::Aux => {
                let row = snap.aux_models.iter().find(|m| m.model_id == model_id)?;
                Some(self.plans.aux(&s.aux_router.models_dir, row).await)
            }
            Class::Audio => {
                let row = snap.audio_models.iter().find(|m| m.model_id == model_id)?;
                // The plan is the files; what the row is charged is its
                // learned residency once it has one (realtime design §9.4).
                let files = self.plans.audio(&s.audio.models_dir, row).await;
                if crate::runtime::audio::placement(row, &s.audio).is_gpu() {
                    Some(super::residency::expected(files, row, &s.audio))
                } else {
                    Some(super::on_cpu::cpu_footprint(files))
                }
            }
            Class::Image => {
                let row = snap.image_models.iter().find(|m| m.model_id == model_id)?;
                Some(self.plans.image(&s.image.models_dir, row).await)
            }
        }
    }
}
