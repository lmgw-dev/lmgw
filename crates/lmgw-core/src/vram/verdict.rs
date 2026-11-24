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
use super::ledger::{Ledger, Measured, Shares};
use super::plan::Footprint;
use super::{ExternalFallback, ExternalShortfall, ExternalVerdict, Target, VramScheduler};

impl VramScheduler {
    /// §4.7's verdict for `target`: may a caller with a fallback answer with
    /// it right away? It never queues, never evicts, never starts or claims
    /// anything — it measures and answers ([`ExternalVerdict`]).
    ///
    /// **Per card** (device-ledger design §5.6). What it needs is what
    /// admission goes ahead with once nothing of lmgw's is left (its least
    /// footprint, the engine's cost and the floor — never the learned
    /// growth nor its margin, both given up last, `headroom`), on each card
    /// it may be placed on; it fits where
    /// one of them has that room in its free memory and lmgw's share there,
    /// busy models included — or, for a model placed on its cards as a
    /// group (§5.7), where they have it together.
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
        // Once per request that would start the model, not on every
        // recheck of its wait (`fit_vocab`).
        if !self.is_up(state, target) {
            self.read_fit_vocab(state, snap, target).await;
        }
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
        // What it needs beside it whatever other programs do next: its
        // margin, never the learned room (`headroom`), which only makes
        // a start wait for or evict lmgw's own models — so a growth the card
        // cannot leave beside it is never a shortfall the fallback answers,
        // and only other programs' use now is.
        //
        // A pipeline that can run with less on the card goes ahead without
        // waiting with its context and the working budget it has rendered
        // with free, else its on-card part (`partial`); a llama-server row
        // fitted to the card, with its least placement (`partial::least`):
        // only less than that is a shortfall the fallback answers.
        //
        // Per card (device-ledger design §5.6): everything of lmgw's on a
        // card counts as room there, busy models included, and the verdict
        // is "fits" where one card it may go on has the room — or, for a
        // model placed on its cards as a group (§5.7), where they have it
        // together.
        let l = self.ledger(state, snap).await;
        if l.capacity.is_none() {
            return ExternalVerdict::Unavailable(
                l.inactive_reason
                    .clone()
                    .unwrap_or_else(|| "nothing to measure against".into()),
            );
        }
        // A pinned card that is gone: it does not start (device-ledger
        // design §8), whatever the other cards have.
        if let Some(why) = self.pin_gone(snap, target, &l.devices) {
            return ExternalVerdict::Unavailable(format!(
                "{}/{} {why}",
                target.class.as_str(),
                target.model_id
            ));
        }
        // Before any attribution: a model that cannot fit on an empty card
        // needs no `podman inspect` to be told so.
        if self.too_large(snap, target, &fp, &l).is_some() {
            return ExternalVerdict::TooLarge;
        }
        let cand = self.candidates(snap, target, &l.devices);
        // A row's own split kept on its pin (device-ledger design §7.2):
        // every card of its pin holds what the split puts there, or it is
        // short on the first that does not.
        if cand.hand_out && cand.own_kept().is_some() && !l.devices.is_empty() {
            return self
                .own_verdict(state, snap, target, &fp, &l, &cand, fill)
                .await;
        }
        let group = super::place::Placed::Group(cand.cards.clone());
        let pooled = self.placement_on(snap, target, &fp, &l, &cand, group);
        // Each card it may go on alone, with what it needs there; the
        // candidates together for a model that may go over them.
        let mut asks: Vec<(Vec<super::nvml::DeviceId>, u64)> = Vec::new();
        if cand.hand_out && !l.devices.is_empty() {
            for c in &cand.cards {
                let one = super::place::Placed::Now(c.clone());
                let p = self.placement_on(snap, target, &fp, &l, &cand, one);
                let least = super::partial::least(&fp, Some(1));
                let needed = least.saturating_add(p.floor).saturating_add(p.engine);
                // One whose least is more than the card holds would go
                // ahead with what is free there: the card is what it needs.
                let total = l.capacity_on(&p.cards, None).map_or(needed, |c| c.total);
                asks.push((p.cards, needed.min(total)));
            }
        }
        // A chat model lmgw may split (§5.2 step 3): over the fewest cards
        // that hold it with lmgw's models there counted as room.
        let split = (cand.hand_out && !l.devices.is_empty())
            .then(|| self.splits(snap, target, &fp).ok())
            .flatten()
            .map(|cost| {
                let rooms: Vec<(super::place::Card, u64)> = cand
                    .cards
                    .iter()
                    .map(|c| {
                        let p = self.placement_on(
                            snap,
                            target,
                            &fp,
                            &l,
                            &cand,
                            super::place::Placed::Now(c.clone()),
                        );
                        let index = l.devices.iter().find(|d| &d.id == c).map_or(0, |d| d.index);
                        let card = super::place::Card {
                            id: c.clone(),
                            index,
                            total: 0,
                            free: 0,
                            headroom: p.floor,
                            margin: p.floor,
                            last: p.floor,
                            structural: p.structural,
                            engine: p.engine,
                            holders: Vec::new(),
                            pool: None,
                        };
                        (card, p.floor.saturating_add(p.engine))
                    })
                    .collect();
                (cost, rooms)
            });
        if !cand.hand_out || l.devices.is_empty() {
            let least = super::partial::least(&fp, pooled.count());
            let mut needed = least
                .saturating_add(pooled.floor)
                .saturating_add(pooled.engine);
            if let Some(total) = l.capacity_on(&pooled.cards, None).map(|c| c.total) {
                needed = needed.min(total);
            }
            asks.push((pooled.cards.clone(), needed));
        }
        match self
            .measure(state, snap, &l, fill)
            .await
            .and_then(|m| Ok((m.shares()?, m)))
        {
            Ok((_, m)) => match per_card(target, &l, &m, &asks) {
                ExternalVerdict::Fits => ExternalVerdict::Fits,
                other => match split.as_ref() {
                    Some((cost, cards)) if split_fits(cost, cards, &m, l.budget > 0) => {
                        ExternalVerdict::Fits
                    }
                    _ => other,
                },
            },
            Err(why) => ExternalVerdict::Unavailable(why),
        }
    }

    /// [`Self::verdict`] for a row's own split kept on its pin (§7.2,
    /// ruling 11): on its pin's cards as ledger `l` reads them, each card's
    /// part, the engine's cost and what it keeps there last (the floor, its
    /// margin given up, WP13) against what is free there and lmgw's share
    /// there. Where lmgw cannot say what each card holds
    /// of the split, the pin's cards together.
    #[allow(clippy::too_many_arguments)]
    async fn own_verdict(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        fp: &super::Footprint,
        l: &super::ledger::Ledger,
        cand: &super::place::Candidates,
        fill: Fill,
    ) -> ExternalVerdict {
        let p = self.placement(snap, target, fp, l, super::place::Mode::Owner, None);
        let asks: Vec<(Vec<super::nvml::DeviceId>, u64)> = match &p.split {
            Some((sp, _)) => sp
                .cards
                .iter()
                .enumerate()
                .map(|(j, c)| {
                    (
                        vec![c.clone()],
                        sp.bytes[j]
                            .saturating_add(sp.engine[j])
                            .saturating_add(sp.last[j]),
                    )
                })
                .collect(),
            None => vec![(
                cand.cards.clone(),
                fp.total_bytes
                    .saturating_add(p.floor)
                    .saturating_add(p.engine),
            )],
        };
        match self
            .measure(state, snap, l, fill)
            .await
            .and_then(|m| Ok((m.shares()?, m)))
        {
            // Every card its part: the first that is short says why.
            Ok((_, m)) => asks
                .iter()
                .map(|ask| per_card(target, l, &m, std::slice::from_ref(ask)))
                .find(|v| !matches!(v, ExternalVerdict::Fits))
                .unwrap_or(ExternalVerdict::Fits),
            Err(why) => ExternalVerdict::Unavailable(why),
        }
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
        // The cards its running rung is on (device-ledger design §5.6),
        // else every card.
        let on: Vec<super::nvml::DeviceId> = l
            .residents
            .iter()
            .find(|r| r.container == target.class && r.model == target.model_id)
            .map(|r| r.charge.cards())
            .unwrap_or_default();
        let total = l.capacity_on(&on, None).map_or(cap.total, |c| c.total);
        // Before any attribution: a model that cannot fit on an empty card
        // needs no `podman inspect` to be told so.
        if needed > total {
            return ExternalVerdict::TooLarge;
        }
        match self
            .measure(state, snap, &l, fill)
            .await
            .and_then(|m| Ok((m.shares()?, m)))
        {
            Ok((_, m)) => per_card(target, &l, &m, &[(on, needed)]),
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
        if e.class == Class::Image {
            // By the engine the container runs: the row's figures count
            // only where they were learned under it.
            let row = snap
                .image_models
                .iter()
                .find(|m| m.model_id == e.model_id)?;
            let key = self.entry_engine_key(snap, e, row);
            let learned = super::plan::image::learned_for(row, &key);
            return Some(
                self.plans
                    .image(&snap.settings.image.models_dir, row, learned)
                    .await,
            );
        }
        if e.class == Class::Aux {
            // By the container's own placement, as audio above: a row
            // switched to (or from) `backend: cpu` while its container runs
            // keeps the placement it was started with (decision-models
            // design §3.3), and the plan reads `backend`.
            let mut row = snap
                .aux_models
                .iter()
                .find(|m| m.model_id == e.model_id)?
                .clone();
            row.backend = match e.placement {
                crate::runtime::Placement::Cpu => Some(crate::config::AuxBackend::Cpu),
                crate::runtime::Placement::Gpu => None,
            };
            return Some(self.plans.aux(&snap.settings.aux_router, &row).await);
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
        // A rung is charged as written: a climb renders no fitted placement
        // (`partial::llama`, not done yet for rungs).
        let mut fp = self
            .plans
            .local(&snap.settings.router.models_dir, &row)
            .await;
        fp.llama = None;
        Some(fp)
    }

    /// `fp` fitted to a card of `total` bytes that can give this start
    /// `avail` now, where llama.cpp's `--fit` would shrink it
    /// (`partial::llama`): beside the margin a start of `class` is
    /// admitted with, never the learned room.
    pub(super) fn fit_on(
        &self,
        snap: &Snapshot,
        target: &super::Target,
        fp: Footprint,
        (total, avail): (u64, u64),
    ) -> Footprint {
        if fp.llama.is_none() {
            return fp;
        }
        let card = super::partial::llama::Card {
            total,
            avail,
            margin: self.start_margin(snap, target.class, &target.model_id, &fp),
            structural: self.card_headroom(snap, &fp),
        };
        let cards = self.cards_for(snap, target);
        super::partial::llama::fit_to_card(fp, card, cards)
    }

    /// [`Self::fit_on`] the card's last known total, less what other
    /// programs held at the newest reading; unchanged while no
    /// total, or no count of the cards the engine sees, is known — a
    /// decision reads the card itself and fits it then, and a fitting
    /// against an unknown count would charge it as written for good.
    fn fit_to_card(
        &self,
        snap: &Snapshot,
        class: Class,
        model_id: &str,
        fp: Footprint,
    ) -> Footprint {
        if fp.llama.is_none() {
            return fp;
        }
        let Some(total) = self.known_total(snap) else {
            return fp;
        };
        let target = super::Target {
            class,
            model_id: model_id.to_string(),
        };
        if self.cards_for(snap, &target).is_none() {
            return fp;
        }
        // On several cards: the candidate with the most room for it, one
        // card (device-ledger design §5.3, D14).
        if let Some((_, card)) = self.roomiest_now(snap, &target, &fp) {
            return super::partial::llama::fit_to_card(fp, card, Some(1));
        }
        // A preview: other programs' use as last read. Every decision fits
        // it again on its own reading of the card (`Self::fit_in_on`).
        let avail = total.saturating_sub(self.outside_recent());
        self.fit_on(snap, &target, fp, (total, avail))
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
                let fp = self.plans.local(&s.router.models_dir, row).await;
                let fp = self.gate_fit(snap, model_id, fp);
                Some(self.fit_to_card(snap, class, model_id, fp))
            }
            Class::Aux => {
                let row = snap.aux_models.iter().find(|m| m.model_id == model_id)?;
                Some(self.plans.aux(&s.aux_router, row).await)
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
                let key = self.image_engine_key(snap, row);
                let learned = super::plan::image::learned_for(row, &key);
                Some(self.plans.image(&s.image.models_dir, row, learned).await)
            }
        }
    }
}

/// Whether a split of `cost` holds on `cards` — each with what it keeps
/// free and its engine's cost there — once everything of lmgw's on them
/// has left, as `m` measured them (§5.6's verdict over a split). A card
/// whose share is unknown is no room. Under a declared budget the verdict
/// does not split (the budget is a bound on the whole, §9.2, which the
/// cards' answers do not divide): the cards' own answers stand.
fn split_fits(
    cost: &super::plan::chat::by_layer::ByLayer,
    cards: &[(super::place::Card, u64)],
    m: &Measured,
    budget: bool,
) -> bool {
    if budget {
        return false;
    }
    let rooms: Vec<(&super::place::Card, u64)> = cards
        .iter()
        .filter_map(|(c, keep)| {
            let s = m.cards.iter().find(|s| s.id == c.id)?;
            let lmgw = s.lmgw.as_ref().ok()?;
            Some((c, s.free.saturating_add(*lmgw).saturating_sub(*keep)))
        })
        .collect();
    super::place::split::fewest(cost, &rooms).is_some()
}

/// §4.7's table per card (device-ledger design §5.6): each of `asks` is a
/// set of cards (none: every card) and what the start needs there; it
/// fits where one set's free memory and lmgw's share there hold it. Else
/// the shortfall of the set with the most room — what lmgw could free and
/// what is free there, and the outside use there.
fn per_card(
    target: &Target,
    l: &Ledger,
    m: &Measured,
    asks: &[(Vec<super::nvml::DeviceId>, u64)],
) -> ExternalVerdict {
    let mut best: Option<(u64, u64, Shares, u64)> = None;
    for (cards, needed) in asks {
        let on: Vec<&super::ledger::CardShares> = m
            .cards
            .iter()
            .filter(|c| cards.is_empty() || cards.contains(&c.id))
            .collect();
        let mut lmgw = 0u64;
        let mut used = 0u64;
        let mut free = 0u64;
        let mut known = true;
        for c in &on {
            match &c.lmgw {
                Ok(b) => lmgw = lmgw.saturating_add(*b),
                Err(_) => known = false,
            }
            used = used.saturating_add(c.used);
            free = free.saturating_add(c.free);
        }
        if !known {
            continue;
        }
        // Under a declared budget, never more than it leaves (§9.2).
        if l.budget > 0 {
            free = free.min(m.raw_free);
        }
        let total = l.capacity_on(cards, None).map_or(m.total, |c| c.total);
        let shares = Shares {
            lmgw,
            outside: used.saturating_sub(lmgw),
        };
        match external_table(target, *needed, total, free, shares) {
            ExternalVerdict::Fits => return ExternalVerdict::Fits,
            _ => {
                let room = free.saturating_add(lmgw);
                if best.is_none_or(|b| room > b.0.saturating_add(b.2.lmgw)) {
                    best = Some((free, *needed, shares, total));
                }
            }
        }
    }
    match best {
        Some((free, needed, shares, total)) => {
            external_table(target, needed, total.max(needed), free, shares)
        }
        None => match m.shares() {
            Ok(shares) => external_table(
                target,
                asks.first().map_or(0, |a| a.1),
                m.total,
                m.raw_free,
                shares,
            ),
            Err(why) => ExternalVerdict::Unavailable(why),
        },
    }
}
