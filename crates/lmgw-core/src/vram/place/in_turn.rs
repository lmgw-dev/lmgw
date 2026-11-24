//! Several models placed in turn on the cards (device-ledger design §3.1,
//! WP5): a warm group (chat-voice design §4.2) and the realtime budget's
//! cascade (realtime design §12) ask whether their models fit side by side.
//! On a machine of one card that is the pooled sum of before; on several it
//! is [`super::place`] asked for each model in turn, each model's charge on
//! its card counted before the next is asked. A group that fits only
//! spread over several cards is fine: each model is a container of its own.
//!
//! **What counts as room**, as the pooled check of before: each card's total
//! less what is not lmgw's there now (where `outside` is asked), lmgw's
//! other models counted as room — every model of the group is admitted as
//! a request is, which evicts them. A model of the group that runs is
//! placed where it runs. What a model keeps free beside it is admission's
//! per-card rule (§5.6): its headroom on a card that holds another model
//! of the group, what it keeps last on one that holds none (the floor; the
//! floor and its margin for a row that adapts, WP13). What not lmgw's
//! there is: the card's used memory less what lmgw's models are charged on
//! that card alone (a model charged as a group counts as not lmgw's, the
//! larger figure, as the finder's card does).

use crate::config::Snapshot;
use crate::state::SharedState;

use super::super::ledger::Ledger;
use super::super::nvml::DeviceId;
use super::super::{Footprint, GpuMemory, Target, VramScheduler};
use super::{place, Ask, Card, Holder, Mode, Placed};

/// Where one model of a group goes ([`VramScheduler::place_in_turn`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InTurn {
    /// The card; `None` where no card has room for it beside the models
    /// placed before it.
    pub card: Option<DeviceId>,
    /// That card by name and index (§2.1).
    pub label: Option<String>,
    /// What the engine itself costs there (§4.2).
    pub engine: u64,
    /// What it keeps free there.
    pub keep: u64,
}

/// The cards a group is placed on: each card's total and what is not
/// lmgw's there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupCards {
    pub devices: Vec<GpuMemory>,
    /// Per card, in the order of `devices`: what is not lmgw's there now.
    pub outside: Vec<u64>,
}

impl GroupCards {
    /// What lmgw may use of every card together: each total, less what is
    /// not lmgw's there where `outside` is counted.
    pub(crate) fn usable(&self, outside: bool) -> u64 {
        self.devices
            .iter()
            .zip(&self.outside)
            .map(|(d, o)| match outside {
                true => d.total_bytes.saturating_sub(*o),
                false => d.total_bytes,
            })
            .fold(0, u64::saturating_add)
    }

    /// What is not lmgw's on every card together.
    pub(crate) fn outside_sum(&self) -> u64 {
        self.outside.iter().copied().fold(0, u64::saturating_add)
    }
}

/// A group placed in turn ([`VramScheduler::place_group`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupPlaced {
    pub cards: GroupCards,
    /// Each model, in the order asked, beside what is not lmgw's on the
    /// cards now.
    pub now: Vec<InTurn>,
    /// Each model on the cards' totals alone: could they ever hold it.
    pub ever: Vec<InTurn>,
}

impl VramScheduler {
    /// `models` placed in turn on the cards of one ledger read (module
    /// doc), each with its footprint as admission charges it; `None` where
    /// the cards are not what decides, and the pooled figures do as before:
    /// no device read, one card, a declared budget (a bound on the whole,
    /// §9.2), or a model lmgw cannot hand a card (§5.7).
    pub(crate) async fn place_group(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        models: &[(Target, Footprint)],
    ) -> Option<GroupPlaced> {
        let targets: Vec<Target> = models.iter().map(|(t, _)| t.clone()).collect();
        let (cards, l) = self.group_cards(state, snap, &targets).await?;
        let now = self.place_in_turn(snap, &cards, &l, models, true);
        let ever = self.place_in_turn(snap, &cards, &l, models, false);
        Some(GroupPlaced { cards, now, ever })
    }

    /// What the engine itself costs a model of `target` on the cards the
    /// pooled figures charge it on (§4.2): the one card of a machine of
    /// one, each card a model charged as a group sees, as the last device
    /// read listed them; nothing for a model that is no llama.cpp model.
    pub(crate) fn engine_pooled(&self, snap: &Snapshot, target: &Target) -> u64 {
        let devices = self.last_devices.lock().unwrap().clone();
        self.candidates(snap, target, &devices)
            .cards
            .iter()
            .map(|c| self.engine_on(target.class, c))
            .fold(0, u64::saturating_add)
    }

    /// The cards a group of `targets` is placed on, and the ledger read
    /// they come from ([`Self::place_group`]).
    async fn group_cards(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        targets: &[Target],
    ) -> Option<(GroupCards, Ledger)> {
        let l = self.ledger(state, snap).await;
        if l.budget > 0 || l.devices.len() < 2 || l.telemetry_error.is_some() {
            return None;
        }
        if targets
            .iter()
            .any(|t| !self.candidates(snap, t, &l.devices).hand_out)
        {
            return None;
        }
        let outside = l
            .devices
            .iter()
            .map(|d| {
                let held: u64 = l
                    .residents
                    .iter()
                    .filter(|r| r.state != "reserved")
                    .filter_map(|r| r.charge.alone_on(&d.id))
                    .fold(0, u64::saturating_add);
                d.used_bytes.saturating_sub(held)
            })
            .collect();
        Some((
            GroupCards {
                devices: l.devices.clone(),
                outside,
            },
            l,
        ))
    }

    /// `models` placed in turn on `cards` (module doc), each with its
    /// footprint as admission charges it; `outside`: count what is not
    /// lmgw's on the cards now, else the cards' totals alone (could they
    /// ever hold it).
    fn place_in_turn(
        &self,
        snap: &Snapshot,
        cards: &GroupCards,
        l: &Ledger,
        models: &[(Target, Footprint)],
        outside: bool,
    ) -> Vec<InTurn> {
        let heads = self.cards_headroom(snap);
        // What the models placed so far take on each card, and who they are.
        let mut taken: Vec<u64> = vec![0; cards.devices.len()];
        let mut holders: Vec<Vec<Holder>> = vec![Vec::new(); cards.devices.len()];
        let mut out = Vec::with_capacity(models.len());
        for (target, fp) in models {
            let cand = self.candidates(snap, target, &l.devices);
            // Where it runs, it stays: the card it is charged on alone.
            let running: Option<DeviceId> = l
                .residents
                .iter()
                .filter(|r| r.container == target.class && r.model == target.model_id)
                .find_map(|r| match r.charge.cards().as_slice() {
                    [one] => Some(one.clone()),
                    _ => None,
                });
            let ids: Vec<DeviceId> = match running {
                Some(c) => vec![c],
                None => cand.cards.clone(),
            };
            let ask = Ask {
                bytes: fp.total_bytes,
                adapts: false,
                split: None,
            };
            let adapts = fp.llama.is_some() || fp.partial.is_some();
            let as_cards: Vec<Card> = cards
                .devices
                .iter()
                .enumerate()
                .filter(|(_, d)| ids.contains(&d.id))
                .map(|(i, d)| {
                    let r = self.rules_on(snap, target, fp, &heads, &d.id);
                    let total = d.total_bytes;
                    let room = match outside {
                        true => total.saturating_sub(cards.outside[i]),
                        false => total,
                    };
                    Card {
                        id: d.id.clone(),
                        index: d.index,
                        total,
                        free: room.saturating_sub(taken[i]),
                        headroom: r.headroom,
                        margin: r.margin,
                        // As admission's (`cards_for_place`): a row that
                        // adapts keeps its margin, fitted smaller first.
                        last: match adapts {
                            true => r.margin,
                            false => r.floor,
                        },
                        structural: r.structural,
                        engine: self.engine_on(target.class, &d.id),
                        holders: holders[i].clone(),
                        pool: None,
                    }
                })
                .collect();
            let placed = match place(&ask, &as_cards, Mode::Owner) {
                Placed::Now(card) => as_cards.iter().find(|c| c.id == card).cloned(),
                _ => None,
            };
            let Some(c) = placed else {
                out.push(InTurn {
                    card: None,
                    label: None,
                    engine: 0,
                    keep: 0,
                });
                continue;
            };
            let i = cards
                .devices
                .iter()
                .position(|d| d.id == c.id)
                .unwrap_or_default();
            let keep = match holders[i].is_empty() {
                true => c.last,
                false => c.headroom,
            };
            taken[i] = taken[i]
                .saturating_add(fp.total_bytes)
                .saturating_add(c.engine);
            holders[i].push(Holder {
                class: target.class,
                model: target.model_id.clone(),
                generation: 0,
                bytes: fp.total_bytes.saturating_add(c.engine),
                idle: false,
                guest: false,
                idle_seconds: 0,
                elsewhere: Vec::new(),
            });
            out.push(InTurn {
                card: Some(c.id.clone()),
                label: Some(super::notice::card_label(&cards.devices, &c.id)),
                engine: c.engine,
                keep,
            });
        }
        out
    }
}
