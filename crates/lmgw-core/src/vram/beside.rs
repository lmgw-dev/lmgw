//! Room for a warm group (chat-voice design §4.2): the capacity its check
//! measures against, and whether a stage still waiting for admission could
//! get room without the claims its siblings keep.
//!
//! A press warms its stages side by side and keeps every claim until all of
//! them are up, so a sibling's admission cannot evict a stage that has just
//! loaded. The price is that a stage can wait for room only its own group's
//! claims could free — and a waiting admission holds the global gate
//! (`VramScheduler::decide`), so every cold start in lmgw would wait with it.
//! Two reads keep that from happening:
//! - [`VramScheduler::group_capacity`], before anything starts: what lmgw may
//!   use and what is held outside it, so a group that cannot fit beside the
//!   other programs on the card is not admitted as one;
//! - [`VramScheduler::crowded_beside`], while a stage waits: whether the free
//!   memory plus every model it could evict — every one on the card but the
//!   group's kept ones — covers it. When it does not, nothing but the group
//!   itself could make room, and the warm gives that stage up.
//!
//! **Several cards** (device-ledger design §3.1, WP5): the group check is
//! the models placed in turn on the cards (`place::in_turn`), each where
//! `place` puts it beside the ones before it, and a stage waiting is
//! crowded out only where no card it may go on would hold it once every
//! model of lmgw's there but the kept ones had left. A machine of one
//! card, a declared budget and a model lmgw cannot hand a card keep the
//! pooled figures below.

use crate::config::Snapshot;
use crate::state::SharedState;

use super::ledger::Ledger;
use super::{ResidentView, Target, VramScheduler};

/// What a warm group is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GroupCapacity {
    /// What lmgw may use: `vram.budget_mb`, or the devices' total.
    pub capacity: u64,
    /// `vram.headroom_mb`.
    pub headroom: u64,
    /// Memory on the card that is not lmgw's to free ([`outside_of`]);
    /// `None` when it cannot be told.
    pub outside: Option<u64>,
}

/// The card now, for a fit beside what runs (find-a-model design D120):
/// [`GroupCapacity`]'s figures from the same ledger read, and lmgw's own
/// models on the card, each its charge plus the peak admission keeps free
/// for it, as [`crowded`] adds them up. A reservation counts: its start is
/// under way and its bytes are about to be on the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OnCardNow {
    pub capacity: u64,
    /// What admission keeps beside a llama.cpp start on the card with
    /// nothing else of lmgw's there, to call it not too large (§5.8): the
    /// engine's cost there and what the card needs beside a model at rest
    /// (`Rules::structural`) — the whole card's measure.
    pub headroom: u64,
    /// Beside what runs now, with nothing of lmgw's on the card: the
    /// engine's cost and the floor alone, what admission starts it with
    /// once the learned room and the margin are given up (WP13).
    pub alone: u64,
    /// Beside lmgw's models on the card, which stay: the engine's cost and
    /// the whole headroom with the margin (`Rules::headroom`).
    pub with: u64,
    pub outside: Option<u64>,
    pub residents: Vec<(crate::runtime::Class, String, u64)>,
    /// Which card, on a computer with several (device-ledger design §10);
    /// `None` on one card and without a device read.
    pub card: Option<lmgw_api_types::model_find::FitCard>,
}

/// A stage that cannot get room beside the claims its group keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Crowded {
    /// Its footprint plus the headroom.
    pub needs: u64,
    /// The free memory plus everything it could evict: every model on the
    /// card but the kept ones and its own.
    pub room: u64,
    /// What the kept models are charged.
    pub kept: u64,
}

impl VramScheduler {
    /// What a warm group is checked against (module doc), from one ledger
    /// read. `None` while admission has nothing to measure against.
    pub(crate) async fn group_capacity(
        &self,
        state: &SharedState,
        snap: &Snapshot,
    ) -> Option<GroupCapacity> {
        let l = self.ledger(state, snap).await;
        let capacity = l.capacity.as_ref().map(|c| c.total).filter(|t| *t > 0)?;
        Some(GroupCapacity {
            capacity,
            headroom: self.headroom(snap).bytes,
            outside: outside_of(&l),
        })
    }

    /// The card now ([`OnCardNow`]), from one ledger read. `None` while
    /// admission has nothing to measure against. It is the card with the
    /// most room for lmgw — its total less what is not lmgw's there —
    /// which is where a model that adapts is fitted (device-ledger design
    /// §5.3): its total, its headroom, what is not lmgw's there and lmgw's
    /// models on it alone. `row`: a row of lmgw's whose fit this is (Fit
    /// again): only the cards admission may put it on are read, its pin's
    /// (§8; owner, 2026-11-18: the fit ignored a row's pin).
    pub(crate) async fn on_card_now(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        row: Option<&Target>,
    ) -> Option<OnCardNow> {
        let l = self.ledger(state, snap).await;
        let capacity = l.capacity.as_ref().map(|c| c.total).filter(|t| *t > 0)?;
        // Every card, under a declared budget too, read as admission reads
        // it: a model goes on one of them by its own rules and the engine's
        // cost there (owner, 2026-11-18).
        let among: Vec<&super::GpuMemory> = match row.map(|t| self.candidates(snap, t, &l.devices))
        {
            Some(c) if c.hand_out && !c.cards.is_empty() => l
                .devices
                .iter()
                .filter(|d| c.cards.contains(&d.id))
                .collect(),
            _ => l.devices.iter().collect(),
        };
        if !among.is_empty() {
            return self.roomiest_card(snap, &l, &among);
        }
        // A budget without a device read: the pooled sums.
        let (r, engine) = self.llama_rules(snap, None);
        Some(OnCardNow {
            capacity,
            headroom: engine.saturating_add(r.structural),
            alone: engine.saturating_add(r.floor),
            with: engine.saturating_add(r.headroom),
            outside: outside_of(&l),
            residents: residents_now(&l.residents),
            card: None,
        })
    }

    /// [`Self::on_card_now`]'s card among `among`, each read as admission
    /// reads a card (`Ledger::capacity_on`): its total (under a declared
    /// budget no more than the budget), and what is free there for a start
    /// — the driver's free memory less what the ledger keeps free and other
    /// starts claim, under a budget no more than the budget leaves. What
    /// is neither free nor lmgw's models' there (other programs, the
    /// driver's own reserve, what is kept free) is not lmgw's to free, so
    /// "beside what runs now" is exactly what admission would give a start
    /// (a bug found 2026-11-18: the fit read the total less the used memory,
    /// the driver's reserve counted as room). Named on a machine of
    /// several cards.
    fn roomiest_card(
        &self,
        snap: &Snapshot,
        l: &Ledger,
        among: &[&super::GpuMemory],
    ) -> Option<OnCardNow> {
        among
            .iter()
            .filter_map(|d| {
                let cap = l.capacity_on(std::slice::from_ref(&d.id), None)?;
                // Only what is charged on this card alone is lmgw's here: a
                // group's part of it is not known, and counting none of it
                // as lmgw's never reads room that is not there. On a machine
                // of one card every model of lmgw's is on it.
                let own: Vec<(crate::runtime::Class, String, u64)> = match l.devices.len() {
                    1 => residents_now(&l.residents),
                    _ => l
                        .residents
                        .iter()
                        .filter_map(|r| {
                            let b = r.charge.alone_on(&d.id).unwrap_or(0);
                            (b > 0)
                                .then(|| (r.container, r.model.clone(), Ledger::held_on(r, &d.id)))
                        })
                        .collect(),
                };
                let own_bytes = own.iter().fold(0u64, |a, (_, _, b)| a.saturating_add(*b));
                let outside = cap.total.saturating_sub(own_bytes).saturating_sub(cap.free);
                let rules = self.llama_rules(snap, Some(&d.id));
                Some((d, cap.total, outside, own, rules))
            })
            .max_by_key(|(d, capacity, outside, _, _)| {
                (
                    capacity.saturating_sub(*outside),
                    std::cmp::Reverse(d.index),
                )
            })
            .map(|(d, capacity, outside, own, (r, engine))| OnCardNow {
                capacity,
                headroom: engine.saturating_add(r.structural),
                alone: engine.saturating_add(r.floor),
                with: engine.saturating_add(r.headroom),
                outside: Some(outside),
                residents: own,
                card: (l.devices.len() > 1).then(|| lmgw_api_types::model_find::FitCard {
                    index: d.index,
                    name: d.name.clone(),
                    of: u32::try_from(l.devices.len()).unwrap_or(u32::MAX),
                }),
            })
    }

    /// `Some` when `target`, still waiting for admission, cannot get room
    /// while the models `kept` stay claimed (module doc): its footprint plus
    /// the headroom is more than the free memory plus every other model on
    /// the card, busy or not — those finish, the kept ones do not while the
    /// group waits on this stage.
    ///
    /// `None` — keep waiting, as the request it announces would — when
    /// admission does not arbitrate this start (switched off, nothing to
    /// measure against, a CPU row, no figure for it), when it is up or
    /// coming up already (the admission joins it), and when none of the
    /// kept models is on the card: then the group holds nothing it could
    /// need.
    pub(crate) async fn crowded_beside(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        target: &Target,
        kept: &[Target],
    ) -> Option<Crowded> {
        if !snap.settings.vram.enabled
            || !snap.placement(target.class, &target.model_id).is_gpu()
            || self.is_up(state, target)
        {
            return None;
        }
        let fp = self.footprint(snap, target.class, &target.model_id).await?;
        let l = self.ledger(state, snap).await;
        let cap = l.capacity.as_ref()?;
        // On a machine of several cards, per card (device-ledger design
        // §3.1): the stage can wait where one card it may go on would hold it
        // once everything of lmgw's there but the kept models had left.
        let cand = self.candidates(snap, target, &l.devices);
        if l.devices.len() > 1 && l.budget == 0 && cand.hand_out {
            return self.crowded_on_cards(snap, &l, target, kept, &fp, &cand.cards);
        }
        let needs = fp.total_bytes.saturating_add(self.start_headroom(
            snap,
            target.class,
            &target.model_id,
            &fp,
        ));
        crowded(cap.free, &l.residents, target, kept, needs)
    }
}

impl VramScheduler {
    /// [`Self::crowded_beside`] on `cards`, the candidates of `target` on a
    /// machine of several: `None` where one of them would hold it — its
    /// footprint, the engine's cost and its headroom there — once every
    /// model of lmgw's there but the kept ones had left, or where no kept
    /// model is on any of them; else the figures of the card with the most
    /// room.
    fn crowded_on_cards(
        &self,
        snap: &Snapshot,
        l: &Ledger,
        target: &Target,
        kept: &[Target],
        fp: &super::Footprint,
        cards: &[super::nvml::DeviceId],
    ) -> Option<Crowded> {
        let is = |r: &ResidentView, t: &Target| r.container == t.class && r.model == t.model_id;
        let is_kept = |r: &ResidentView| kept.iter().any(|k| is(r, k));
        if !l
            .residents
            .iter()
            .any(|r| is_kept(r) && Ledger::is_on(r, cards))
        {
            return None;
        }
        let heads = self.cards_headroom(snap);
        let mut best: Option<Crowded> = None;
        for c in cards {
            let cap = l.capacity_on(std::slice::from_ref(c), None)?;
            let headroom = self.rules_on(snap, target, fp, &heads, c).headroom;
            let needs = fp
                .total_bytes
                .saturating_add(self.engine_on(target.class, c))
                .saturating_add(headroom);
            let (mut evictable, mut kept_bytes) = (0u64, 0u64);
            for r in l.residents.iter().filter(|r| r.state != "reserved") {
                let on = Ledger::held_on(r, c);
                if is_kept(r) {
                    kept_bytes = kept_bytes.saturating_add(on);
                } else if !is(r, target) && on > 0 {
                    evictable = evictable
                        .saturating_add(on)
                        .saturating_add(r.peak_extra_bytes.unwrap_or(0));
                }
            }
            let room = cap.free.saturating_add(evictable);
            if needs <= room {
                return None;
            }
            if best.is_none_or(|b| room > b.room) {
                best = Some(Crowded {
                    needs,
                    room,
                    kept: kept_bytes,
                });
            }
        }
        best
    }
}

/// [`VramScheduler::crowded_beside`]'s arithmetic on one ledger's figures:
/// `free` is the capacity's free memory, `needs` the target's footprint plus
/// the headroom.
fn crowded(
    free: u64,
    residents: &[ResidentView],
    target: &Target,
    kept: &[Target],
    needs: u64,
) -> Option<Crowded> {
    let is = |r: &ResidentView, t: &Target| r.container == t.class && r.model == t.model_id;
    let is_kept = |r: &ResidentView| kept.iter().any(|k| is(r, k));
    let kept_bytes: u64 = residents
        .iter()
        .filter(|r| is_kept(r))
        .map(|r| r.estimated_bytes)
        .sum();
    if !residents.iter().any(is_kept) {
        return None;
    }
    // What evicting one gives back: its charge and the peak kept free for
    // it. The charge is all of it in both capacity modes. Where the driver
    // is the measurement, eviction frees what the container holds on the
    // card (in `used`) and drops what is kept free for it (the starting or
    // pending part, out of `free` already) — and for an audio container
    // that has not loaded, those two are its expected residency, the charge:
    // the pending figure is that residency less what it holds
    // ([`super::residency::pending`]), so adding it on top credited the
    // pending load twice. Without a device reading the charge is in `free`
    // whole. A reservation (a start another waiter has pending, `ledger`) is
    // no victim: its bytes are out of `free` already and it cannot be
    // evicted (WP11 server review n2), as `outside_of` reads it too.
    let evictable: u64 = residents
        .iter()
        .filter(|r| !is_kept(r) && !is(r, target) && r.state != "reserved")
        .map(|r| {
            r.estimated_bytes
                .saturating_add(r.peak_extra_bytes.unwrap_or(0))
        })
        .fold(0u64, u64::saturating_add);
    let room = free.saturating_add(evictable);
    (needs > room).then_some(Crowded {
        needs,
        room,
        kept: kept_bytes,
    })
}

/// lmgw's own models on the card, as a fit beside them charges them
/// ([`OnCardNow`]): each its charge plus the peak kept free for it.
fn residents_now(residents: &[ResidentView]) -> Vec<(crate::runtime::Class, String, u64)> {
    residents
        .iter()
        .map(|r| {
            (
                r.container,
                r.model.clone(),
                r.estimated_bytes
                    .saturating_add(r.peak_extra_bytes.unwrap_or(0))
                    .saturating_add(r.kept_bytes),
            )
        })
        .collect()
}

/// Memory on the card that is not lmgw's: the devices' used bytes less what
/// lmgw's containers on the card are charged — the other programs (a game,
/// the desktop), and whatever lmgw's own use exceeds its estimates by.
/// `None` without a device reading, or when one of those containers has no
/// figure (nothing is guessed). A start still loading is subtracted whole,
/// so the figure errs low while one is under way.
fn outside_of(l: &Ledger) -> Option<u64> {
    if l.telemetry_error.is_some() || l.devices.is_empty() {
        return None;
    }
    let used: u64 = l.devices.iter().map(|d| d.used_bytes).sum();
    let mut own = 0u64;
    for r in l.residents.iter().filter(|r| r.state != "reserved") {
        if r.estimated_bytes == 0 {
            return None;
        }
        own = own.saturating_add(r.estimated_bytes);
    }
    Some(used.saturating_sub(own))
}

#[cfg(test)]
mod tests;
