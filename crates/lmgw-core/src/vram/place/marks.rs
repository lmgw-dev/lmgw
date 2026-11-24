//! What a fit mark over several cards asks of placing (device-ledger design
//! §13 WP12): a model that is not a row yet — a file in the finder or a
//! Hugging Face file list — judged on the cards as one ledger read finds
//! them, by the rules a start decides by.
//!
//! - **Min. cards** ([`CardsNow::min_cards`]): the fewest cards whose total
//!   memory holds it, each keeping free beside its part what a start keeps
//!   free there with nothing else of lmgw's on it (§5.8, the "too large"
//!   rule) and the engine's cost there; two or more by the split lmgw
//!   would make (§4.3, [`super::split::fewest`]).
//! - **Free VRAM** ([`CardsNow::free`]): where [`super::place`] would put
//!   it now for an owner's start: on one card or a split now, after
//!   unloading idle models of lmgw's, after busy ones finish, or not.
//!
//! The cards are those of a new model of the chat (or aux) class: every
//! card the computer lists, each with lmgw's models on it, its free memory
//! less what other starts keep there, its headroom, margin and engine
//! cost. Whether lmgw could hand such a model its cards and tell its engine
//! a split is read beside them ([`CardsNow::hand_out`], [`CardsNow::split`]).

use std::sync::Arc;

use lmgw_api_types::model_find::CardPart;

use crate::config::Snapshot;
use crate::runtime::Class;
use crate::state::SharedState;

use super::super::fit_vocab::SplitVocab;
use super::super::plan::chat::by_layer::ByLayer;
use super::super::{Footprint, Target, VramScheduler};
use super::{place, split, Ask, Card, Holder, Mode, Placed};

/// The cards now, for a model that is not a row yet (module doc).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CardsNow {
    cards: Vec<Card>,
    /// Each card's name, by its place in `cards`.
    names: Vec<String>,
    /// `Err(why)` where lmgw could not hand a new chat model its cards:
    /// not NVIDIA cards, the native launcher, the class's run arguments.
    pub hand_out: Result<(), String>,
    /// What the class's llama-server can be told of a split; `Err(why)`
    /// where it has no `--tensor-split` or was not read.
    pub split: Result<SplitVocab, String>,
    /// A row's own split kept on its pin, whose cards these are (§7.2):
    /// it goes on them as written ([`Self::own_fits`]).
    own: Option<super::OwnFate>,
    /// Under a declared budget over several cards, the budget: what a
    /// split's parts together may take with nothing else of lmgw's there
    /// (Min. cards; the cards' `pool` is what it leaves now, Free VRAM's).
    budget: Option<u64>,
}

/// Min. cards (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Min {
    /// One card holds it: the largest that does, and what it takes there.
    One(CardPart),
    /// A split over these cards holds it, and no one card does.
    Split(Vec<CardPart>),
    /// No card holds it, and its size says it would take about this many
    /// cards together; what each part is is not known (no layers to
    /// split by: an aux model, a header not read).
    About(u32),
    /// Not even every card together (up to 16) holds it.
    TooLarge {
        /// What it needs on the largest card, beside what is kept free.
        needs: u64,
        /// The largest card's total, and every card's together.
        largest: u64,
        all: u64,
        /// What every card keeps beside it together: the engine's cost and
        /// what it needs beside a model at rest (§5.8).
        kept: u64,
    },
}

/// Free VRAM (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Free {
    /// The cards have room now.
    Now(Vec<CardPart>),
    /// Once these idle models of lmgw's unload.
    Unload(Vec<CardPart>, Vec<String>),
    /// Once these busy models finish and unload.
    Wait(Vec<CardPart>, Vec<String>),
    /// Not even with lmgw's models there unloaded: what it needs (beside
    /// what it keeps free), what the cards have with lmgw's models gone,
    /// how much of that is lmgw's, and the cards (the roomiest one, or
    /// for a split the roomiest as many as Min. cards counts).
    Short {
        needs: u64,
        has: u64,
        ours: u64,
        cards: Vec<CardPart>,
    },
}

impl CardsNow {
    /// How many cards the computer has.
    pub(crate) fn count(&self) -> usize {
        self.cards.len()
    }

    /// Leaves lmgw's `class` models named `models` off the cards, their
    /// bytes free: the rows that serve the planned file, whose place a
    /// start of it takes (`modelinfo::fit::CardNow::beside_itself`).
    pub(crate) fn leave_out(&mut self, class: &str, models: &[&str]) {
        for c in &mut self.cards {
            let (gone, kept): (Vec<Holder>, Vec<Holder>) = std::mem::take(&mut c.holders)
                .into_iter()
                .partition(|h| h.class.as_str() == class && models.contains(&h.model.as_str()));
            c.holders = kept;
            c.free = gone
                .iter()
                .fold(c.free, |a, h| a.saturating_add(h.bytes))
                .min(c.total);
        }
    }

    fn part(&self, c: &Card, bytes: u64, layers: Option<u64>, keep: u64) -> CardPart {
        let name = self
            .cards
            .iter()
            .position(|x| x.id == c.id)
            .and_then(|i| self.names.get(i))
            .cloned()
            .unwrap_or_default();
        CardPart {
            index: c.index,
            name,
            bytes,
            layers,
            total_bytes: c.total,
            free_bytes: c.free,
            keep_bytes: keep,
        }
    }

    /// Whether a row's own split kept on these cards (its pin) holds a
    /// footprint that `cost` takes apart, as admission decides it: each
    /// card its part, the engine's cost there and what it keeps free
    /// there — beside what runs now, its free memory with nothing of
    /// lmgw's left to make room (the floor alone) or the whole headroom
    /// beside lmgw's models; on the whole card, its free memory with
    /// lmgw's models there unloaded, beside the floor. `None` where no own split is kept here, or the cost
    /// cannot be taken apart by its entries.
    /// A row's own split is kept on these cards.
    pub(crate) fn has_own(&self) -> bool {
        self.own.is_some()
    }

    pub(crate) fn own_fits(&self, cost: Option<&ByLayer>, whole: bool) -> Option<bool> {
        let own = self.own.as_ref()?;
        let sp = own.over(cost, &self.cards)?;
        Some(sp.cards.iter().enumerate().all(|(j, id)| {
            let Some(c) = self.card(id) else {
                return false;
            };
            let part = sp.bytes[j].saturating_add(sp.engine[j]);
            match whole {
                // lmgw's models there unloaded, as the one-card fit's
                // whole card (`modelinfo::fit`).
                true => {
                    let freed = c
                        .holders
                        .iter()
                        .fold(c.free, |a, h| a.saturating_add(h.bytes));
                    part.saturating_add(c.last) <= freed
                }
                false => {
                    let keep = c.keep(!c.holders.is_empty(), super::Keep::Last);
                    part.saturating_add(keep) <= c.free
                }
            }
        }))
    }

    fn card(&self, id: &super::super::nvml::DeviceId) -> Option<&Card> {
        self.cards.iter().find(|c| &c.id == id)
    }

    fn split_parts(&self, sp: &split::Split, keep_of: impl Fn(&Card) -> u64) -> Vec<CardPart> {
        sp.cards
            .iter()
            .enumerate()
            .filter_map(|(j, id)| {
                let c = self.card(id)?;
                let bytes = sp.bytes[j].saturating_add(sp.engine[j]);
                Some(self.part(c, bytes, Some(sp.counts[j]), keep_of(c)))
            })
            .collect()
    }

    /// Min. cards for a model of `bytes` (its footprint, the engine's cost
    /// left out) that `cost` says how to split, where it may be split.
    pub(crate) fn min_cards(&self, bytes: u64, cost: Option<&ByLayer>) -> Min {
        let ask = Ask {
            bytes,
            adapts: false,
            split: None,
        };
        // The largest card that holds it whole.
        if let Some(c) = self
            .cards
            .iter()
            .filter(|c| c.could_hold(&ask))
            .max_by_key(|c| (c.total, std::cmp::Reverse(c.index)))
        {
            return Min::One(self.part(c, bytes.saturating_add(c.engine), None, c.structural));
        }
        // With nothing else of lmgw's there: under a budget, a split's parts
        // together within the budget itself.
        let whole: Vec<Card> = self
            .cards
            .iter()
            .map(|c| Card {
                pool: self.budget,
                ..c.clone()
            })
            .collect();
        let rooms: Vec<(&Card, u64)> = whole
            .iter()
            .map(|c| {
                (
                    c,
                    c.total
                        .saturating_sub(c.structural)
                        .saturating_sub(c.engine),
                )
            })
            .collect();
        let largest = self.cards.iter().map(|c| c.total).max().unwrap_or(0);
        let all = self
            .cards
            .iter()
            .fold(0u64, |a, c| a.saturating_add(c.total));
        let needs = self
            .cards
            .iter()
            .max_by_key(|c| c.total)
            .map_or(bytes, |c| c.needs(&ask, c.structural));
        // Over every card a split, each card's engine holding more on it
        // (`split::SPLIT_ENGINE_EXTRA`).
        let extra = match self.cards.len() {
            0 | 1 => 0,
            _ => split::SPLIT_ENGINE_EXTRA,
        };
        let kept = self.cards.iter().fold(0u64, |a, c| {
            a.saturating_add(c.structural)
                .saturating_add(c.engine)
                .saturating_add(extra)
        });
        match cost {
            Some(cost) => match split::fewest(cost, &rooms) {
                Some(sp) => Min::Split(self.split_parts(&sp, |c| c.structural)),
                None => Min::TooLarge {
                    needs,
                    largest,
                    all,
                    kept,
                },
            },
            // By size: the fewest roomiest cards whose rooms add up to it.
            None => {
                let mut by_room: Vec<u64> = rooms.iter().map(|(_, r)| *r).collect();
                by_room.sort_unstable_by(|a, b| b.cmp(a));
                let mut sum = 0u64;
                for (k, r) in by_room
                    .iter()
                    .enumerate()
                    .take(crate::runtime::argv::MAX_SPLIT)
                {
                    sum = sum.saturating_add(*r);
                    if k >= 1 && sum >= bytes {
                        return Min::About(u32::try_from(k + 1).unwrap_or(u32::MAX));
                    }
                }
                Min::TooLarge {
                    needs,
                    largest,
                    all,
                    kept,
                }
            }
        }
    }

    /// Free VRAM for a model of `bytes` that `cost` says how to split, where
    /// lmgw may split it: where an owner's start would go now (module doc).
    /// `min`: how many cards Min. cards counts, for the cards a short split
    /// is measured on.
    pub(crate) fn free(&self, bytes: u64, cost: Option<Arc<ByLayer>>, min: usize) -> Free {
        let ask = Ask {
            bytes,
            adapts: false,
            split: cost.clone(),
        };
        let names = |hs: &[Holder]| -> Vec<String> {
            let mut out: Vec<String> = Vec::new();
            for h in hs {
                let n = format!("{}/{}", h.class.as_str(), h.model);
                if !out.contains(&n) {
                    out.push(n);
                }
            }
            out
        };
        let one = |id: &super::super::nvml::DeviceId, left: bool| {
            self.card(id)
                .map(|c| {
                    vec![self.part(
                        c,
                        bytes.saturating_add(c.engine),
                        None,
                        c.keep(left && !c.holders.is_empty(), super::Keep::Left),
                    )]
                })
                .unwrap_or_default()
        };
        match place(&ask, &self.cards, Mode::Owner) {
            Placed::Now(id) => Free::Now(one(&id, true)),
            Placed::Evict { card, victims } => {
                let left = self
                    .card(&card)
                    .is_some_and(|c| c.holders.len() > victims.len());
                Free::Unload(one(&card, left), names(&victims))
            }
            Placed::Wait { card, on } => {
                let left = self.card(&card).is_some_and(|c| c.holders.len() > on.len());
                Free::Wait(one(&card, left), names(&on))
            }
            Placed::Split { split, victims } => {
                let parts = self.split_parts(&split, |c| {
                    let gone = c
                        .holders
                        .iter()
                        .filter(|h| victims.iter().any(|v| v.generation == h.generation))
                        .count();
                    c.keep(c.holders.len() > gone, super::Keep::Left)
                });
                match victims.is_empty() {
                    true => Free::Now(parts),
                    false => Free::Unload(parts, names(&victims)),
                }
            }
            Placed::Roomiest(_) | Placed::Group(_) | Placed::Nowhere => {
                self.short(bytes, cost.as_deref(), min.max(1))
            }
        }
    }

    /// [`Free::Short`]: on the roomiest card, or the roomiest `k` for a
    /// split, with every model of lmgw's there gone.
    fn short(&self, bytes: u64, cost: Option<&ByLayer>, k: usize) -> Free {
        let mut by_room: Vec<(&Card, u64, u64)> = self
            .cards
            .iter()
            .map(|c| {
                let ours = c
                    .holders
                    .iter()
                    .fold(0u64, |a, h| a.saturating_add(h.bytes));
                (c, c.free.saturating_add(ours), ours)
            })
            .collect();
        by_room.sort_by_key(|(c, has, _)| (std::cmp::Reverse(*has), c.index));
        let k = match cost {
            Some(_) => k.min(by_room.len()),
            None => 1,
        };
        let chosen = &by_room[..k.min(by_room.len())];
        let (has, ours) = chosen.iter().fold((0u64, 0u64), |(a, o), (_, h, ours)| {
            (a.saturating_add(*h), o.saturating_add(*ours))
        });
        // A split card's engine holds more (`split::SPLIT_ENGINE_EXTRA`).
        let extra = match k {
            1 => 0,
            _ => split::SPLIT_ENGINE_EXTRA,
        };
        let fixed = chosen.iter().fold(0u64, |a, (c, _, _)| {
            a.saturating_add(c.engine)
                .saturating_add(extra)
                .saturating_add(c.last)
        });
        // A split's parts: the model's bytes on the chosen cards together,
        // with each card's compute buffer and the first and last cards'
        // own parts, as `ByLayer` divides it.
        let model = match (cost, k) {
            (Some(c), k) if k > 1 => c
                .layers
                .iter()
                .fold(0u64, |a, b| a.saturating_add(*b))
                .saturating_add(c.first)
                .saturating_add(c.last)
                .saturating_add(c.computes(k)),
            _ => bytes,
        };
        // On one card, what it takes there (a bug found 2026-11-18: the
        // one-card choice read "0 B on card 2"); a split's division over the
        // cards is not worked out where it does not fit.
        let mut cards: Vec<CardPart> = chosen
            .iter()
            .map(|(c, _, _)| {
                let takes = match k {
                    1 => bytes.saturating_add(c.engine),
                    _ => 0,
                };
                self.part(c, takes, None, c.last)
            })
            .collect();
        cards.sort_by_key(|p| p.index);
        Free::Short {
            needs: model.saturating_add(fixed),
            has,
            ours,
            cards,
        }
    }
}

impl VramScheduler {
    /// The cards now for a model that is not a row yet (module doc), from
    /// one ledger read; `None` without a device reading. `row`: a row's
    /// fit (Fit again): only its candidates — its pin's cards — as
    /// admission reads them for it, with its own split where it is kept
    /// there (§8, §7.2; owner, 2026-11-18). Under a declared
    /// budget each card is read as admission reads it
    /// (`Ledger::capacity_on`, through `cards_for_place`): its total no
    /// more than the budget, its free memory no more than the budget leaves
    /// beside everything in use, its own rules and engine cost (the owner's
    /// ruling, 2026-11-18: the marks read every card as admission does, since any other
    /// reading would confuse). `read`: read the class's llama-server `--help` where what it
    /// can be told of a
    /// split is not known yet and the computer has several cards.
    pub(crate) async fn cards_now(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        read: bool,
        row: Option<&Target>,
    ) -> Option<CardsNow> {
        let l = self.ledger(state, snap).await;
        l.capacity.as_ref()?;
        if l.devices.is_empty() || l.telemetry_error.is_some() {
            return None;
        }
        // No row has this id: the cards as a new model of the class finds
        // them, with every model of lmgw's on them as a holder.
        let new = Target {
            class: Class::Chat,
            model_id: "\u{0}new".into(),
        };
        let fp = Footprint::new(1, 0, None, None);
        let cand = row.map(|t| self.candidates(snap, t, &l.devices));
        let (target, ids, own) = match (row, &cand) {
            (Some(t), Some(c)) if c.hand_out && !c.cards.is_empty() => {
                (t.clone(), c.cards.clone(), c.own_kept().cloned())
            }
            _ => (new, l.devices.iter().map(|d| d.id.clone()).collect(), None),
        };
        let cards = self.cards_for_place(snap, &target, &fp, &l, &ids, None, false);
        let names = cards
            .iter()
            .map(|c| {
                l.devices
                    .iter()
                    .find(|d| d.id == c.id)
                    .map(|d| d.name.clone())
                    .unwrap_or_default()
            })
            .collect();
        let router = &snap.settings.router;
        let hand_out = if self.engine_build(Class::Chat).is_some() {
            Err(
                "lmgw runs llama-server on this computer itself, not in a container, and \
                 cannot hand it cards one by one"
                    .to_string(),
            )
        } else if !ids.iter().all(super::engine::is_cuda) {
            Err(
                "not every card is an NVIDIA card, and lmgw hands an engine its cards one \
                 by one only on NVIDIA's"
                    .to_string(),
            )
        } else if matches!(
            crate::runtime::launcher::native::gpu::selection(
                &router.extra_run_args,
                crate::runtime::Placement::Gpu
            ),
            crate::runtime::launcher::native::gpu::Selection::None
        ) || !crate::runtime::argv::passes_cdi(&router.extra_run_args)
        {
            Err(
                "the chat class's run arguments do not pass the cards through CDI, so lmgw \
                 cannot hand its engine cards one by one"
                    .to_string(),
            )
        } else {
            Ok(())
        };
        if read && cards.len() > 1 && hand_out.is_ok() {
            self.read_class_vocab(state, snap).await;
        }
        Some(CardsNow {
            cards,
            names,
            split: self.class_split_vocab(&router.image),
            hand_out,
            own,
            budget: l.budget_total(),
        })
    }
}

#[cfg(test)]
impl CardsNow {
    /// Cards for a test, every one handed out and split by its image.
    pub(in crate::vram) fn for_tests(cards: Vec<Card>) -> Self {
        let names = cards.iter().map(|c| format!("Card {}", c.index)).collect();
        Self {
            cards,
            names,
            own: None,
            budget: None,
            hand_out: Ok(()),
            split: Ok(SplitVocab {
                fit: true,
                devd: true,
            }),
        }
    }
}

/// A stated card for a test outside `vram`: its index, total and free
/// bytes, what a model keeps free beside it there, and lmgw's chat models
/// on it (name, bytes, idle).
#[cfg(test)]
pub(crate) struct StatedCard<'a> {
    pub index: u32,
    pub total: u64,
    pub free: u64,
    pub keep: u64,
    pub holders: &'a [(&'a str, u64, bool)],
}

#[cfg(test)]
impl CardsNow {
    /// NVIDIA cards as stated (no margin, no engine cost).
    pub(crate) fn stated(cards: &[StatedCard<'_>]) -> Self {
        Self::for_tests(
            cards
                .iter()
                .map(|c| Card {
                    id: super::super::nvml::DeviceId::new(format!("GPU-{}", c.index)),
                    index: c.index,
                    total: c.total,
                    free: c.free,
                    headroom: c.keep,
                    margin: 0,
                    last: 0,
                    structural: c.keep,
                    engine: 0,
                    holders: c
                        .holders
                        .iter()
                        .enumerate()
                        .map(|(i, (m, b, idle))| Holder {
                            class: Class::Chat,
                            model: (*m).to_string(),
                            generation: u64::from(c.index) * 100 + i as u64,
                            bytes: *b,
                            idle: *idle,
                            guest: false,
                            idle_seconds: 10,
                            elsewhere: Vec::new(),
                        })
                        .collect(),
                    pool: None,
                })
                .collect(),
        )
    }

    /// The same cards where the class's engine cannot be told a split.
    pub(crate) fn without_split(mut self, why: &str) -> Self {
        self.split = Err(why.to_string());
        self
    }
}

#[cfg(test)]
mod tests;
