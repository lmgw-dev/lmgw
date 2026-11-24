//! Choosing the card (device-ledger design §5, WP5): [`place`].
//!
//! One function answers for a model's whole (its footprint as the row has
//! it), a mode (§5.6) and the candidate cards as one ledger read finds them
//! ([`Card`]): which card it goes on, and what has to happen there first.
//! It is pure; the scheduler builds the cards from its ledger
//! (`VramScheduler::cards_for_place`) and every decision asks it before it
//! measures, evicts or waits — and then does that on the card it was
//! answered, as it did on the pooled card before.
//!
//! **The order** (§5.2, D6), a step only where every step before it has no
//! answer:
//! 1. **One card, now**: the candidates with room for the whole and what it
//!    keeps free there. Of those, the one it leaves least free on (D7);
//!    equal, the lower index (the driver's order, which is the PCI order).
//! 2. **One card, after evicting** (ruling 5): the candidates where stopping
//!    idle models of that card (within the mode, LRU first, §6.1) makes room.
//!    The fewest victims; then the cards whose victims could all be started
//!    again on another card now (§6.2, [`Holder::elsewhere`]); then the
//!    victims used longest ago; then D7.
//! 3. **Several cards, now** (a chat model whose header says what each
//!    layer holds, WP6): the fewest cards a split holds now
//!    ([`split::fewest`], §5.4), each keeping free beside its part what step
//!    1 keeps free on a card.
//! 4. **Several cards, after evicting**: as 3, with the idle models of
//!    those cards stopped (within the mode), the ones each card needs gone,
//!    least recently used first.
//!
//! **What a card keeps free, and the learned room and the margin given up
//! last** (§5.6, §9.1, WP13; the margin by the owner's decision of
//! 2026-11-17): steps 1 and 2 are asked first with every card keeping its
//! whole headroom — its floor, its learned room and the margin
//! ([`Keep::Whole`]); only where neither has an answer again with today's
//! rule of §5.6 ([`Keep::Left`]): the whole headroom while something of
//! lmgw's on that card could be waited for or evicted, the floor and the
//! margin once nothing is (the learned room given up); and only where
//! neither has an answer then, once more with the floor alone on such a
//! card ([`Keep::Last`], [`Card::last`]: the margin given up too). So a
//! single model evicts an idle model on another card before it takes the
//! learned room on an empty one, and takes an empty card's learned room —
//! or evicts for it — before it gives its margin up. Steps 3 and 4 likewise:
//! a split puts its layers on another card before it takes a card's learned
//! room, and that before its margin. A model that one card holds by giving
//! them up is not split instead (D6: a split only where no card holds it).
//! A row that adapts keeps its margin here (its `last` is the floor and the
//! margin): it is fitted smaller before it gives the margin up
//! (`partial::decide`).
//! 5. **Waiting for a busy model** (an owner's start of a model that goes on
//!    one card): the card where its busy models leaving would make room; the
//!    fewest to wait for, then D7.
//! 6. **Smaller**: the candidate with the most room, where the row is fitted
//!    (a llama-server row on `--fit`, an image pipeline that adapts) or, for
//!    any other model, where it waits.
//!
//! **A row that adapts** (§5.3, ruling 15) is placed by steps 1 and 2 at its
//! whole, and otherwise on the roomiest candidate, without waiting first:
//! never split, since a row that sets no context has no split to ask for
//! ([`Ask::split`]). One that sets its context and whose layers `--fit`
//! could move is split (steps 3 and 4) before it is fitted smaller (D6). A model `place` cannot split (any other class, a header
//! that does not say what each layer holds, an engine without
//! `--tensor-split`) goes from step 2 to step 5: lmgw never leaves a split
//! to the engine (D9).
//!
//! **A row's own split** (§7.2, ruling 11, WP11) kept on its pin is not
//! asked of `place`: it goes on its pin's cards as written
//! ([`own_split::fixed`]).

use crate::runtime::Class;

use super::nvml::DeviceId;

mod audio_device;
mod choose;
pub(crate) mod engine;
pub(crate) mod in_turn;
pub(crate) mod marks;
mod notice;
mod own_split;
mod repack;
pub(in crate::vram) mod split;
mod split_start;
pub(in crate::vram) use choose::{others_hold_on, waits_on, Candidates, Placement, TooLarge};
pub(crate) use notice::{card_label, in_words};
pub(crate) use own_split::{fate as own_fate, OwnFate};
pub(in crate::vram) use split::Split;
pub(in crate::vram) use split_start::split_grant;

/// One candidate card, as one ledger read finds it (§5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::vram) struct Card {
    pub id: DeviceId,
    /// The driver's index: the order among equal cards.
    pub index: u32,
    /// What lmgw may use of it: the driver's total, under a declared budget
    /// the smaller of the two (§9.2).
    pub total: u64,
    /// What is free here for this start: the driver's free less what the
    /// ledger keeps free here and what other starts' claims take here
    /// (§5.5); under a declared budget, never more than the budget leaves
    /// on all cards together (§9.2).
    pub free: u64,
    /// What this start keeps free here while something of lmgw's on this
    /// card could be waited for or evicted: the card's headroom and its
    /// margin (§5.6).
    pub headroom: u64,
    /// What it keeps free here once nothing of lmgw's is left on it and
    /// the learned room is given up: the floor and its margin.
    pub margin: u64,
    /// What it keeps free here once nothing of lmgw's is left on it and
    /// nothing else fits, the learned room and the margin given up (WP13):
    /// the floor alone; the floor and its margin for a row that adapts,
    /// which is fitted smaller before it gives its margin up.
    pub last: u64,
    /// What it needs beside it on this card with nothing else of lmgw's
    /// there (§5.8): the fixed headroom or what other programs hold here at
    /// rest, never the learned room nor the margin.
    pub structural: u64,
    /// What the engine itself costs on this card (§4.2, ruling 17).
    pub engine: u64,
    /// lmgw's models with bytes on this card (§6.1).
    pub holders: Vec<Holder>,
    /// Under a declared budget over several cards, what the budget leaves
    /// for the start beside everything in use on every card (the same on
    /// each card): a split's parts on its cards together, the engine's cost
    /// included, fit within it (owner, 2026-11-18), as each card's part
    /// fits its card. `None` without a budget and on one card.
    pub pool: Option<u64>,
}

/// One of lmgw's models on a [`Card`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::vram) struct Holder {
    pub class: Class,
    pub model: String,
    pub generation: u64,
    /// What stopping it frees on this card.
    pub bytes: u64,
    /// No request of lmgw's is on it.
    pub idle: bool,
    /// A guest started it (background traffic).
    pub guest: bool,
    /// Seconds since it was last used: the eviction order.
    pub idle_seconds: u64,
    /// The other cards it could be started again on now, were it stopped
    /// for room here (§6.2, `VramScheduler::elsewhere`); empty where it
    /// could not be — busy, moved and not used since, an engine lmgw
    /// cannot hand a card, no other card that holds it now.
    pub elsewhere: Vec<Spare>,
}

/// A card a [`Holder`] stopped for room could be started again on now
/// (§6.2): what that card would have left beside it, and what it would take
/// there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::vram) struct Spare {
    pub id: DeviceId,
    pub index: u32,
    /// The card's free memory less what the model needs there — its
    /// footprint, the engine's cost and what it keeps free beside it.
    pub spare: u64,
    /// What it would take there: its footprint and the engine's cost.
    pub takes: u64,
}

/// Who asks (§5.6, D16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::vram) enum Mode {
    /// An owner's start: idle models of any origin are victims, and a busy
    /// one is waited for.
    Owner,
    /// A guest's: only idle guests are victims, and nothing is waited for.
    Guest,
    /// A start that never evicts or waits (a warm, boot or operator start,
    /// `check_background_start`): nothing of lmgw's is a victim or waited
    /// for, so only a card with room now takes it — on a card nothing of
    /// lmgw's is on, with the learned room and then the margin given up —
    /// else the roomiest candidate by its free memory.
    Still,
}

/// What is placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::vram) struct Ask {
    /// The whole, without the engine's cost (§5.2): the footprint of the
    /// row as written.
    pub bytes: u64,
    /// A row that adapts (§5.3), or a pipeline that can run with less on
    /// the card: it is never waited for whole, and goes on the roomiest
    /// candidate where no card holds its whole.
    pub adapts: bool,
    /// A chat model that may be split over several of its candidates where
    /// no single card holds it (steps 3 and 4): what it puts where by
    /// layer. `None` for every model that goes on one card.
    pub split: Option<std::sync::Arc<super::plan::chat::by_layer::ByLayer>>,
}

/// The answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::vram) enum Placed {
    /// Step 1: on this card, now.
    Now(DeviceId),
    /// Step 2: on this card once these idle models are stopped, in this
    /// order.
    Evict {
        card: DeviceId,
        victims: Vec<Holder>,
    },
    /// Step 5: on this card once these models have left.
    Wait { card: DeviceId, on: Vec<Holder> },
    /// Step 6, or a row that adapts (§5.3): on the roomiest candidate.
    Roomiest(DeviceId),
    /// Steps 3 and 4: split over these cards, once these idle models are
    /// stopped (none: now).
    Split { split: Split, victims: Vec<Holder> },
    /// Over these cards as a group (§5.7): an engine lmgw cannot hand a
    /// card.
    Group(Vec<DeviceId>),
    /// No candidate at all.
    Nowhere,
}

impl Placed {
    /// The cards it names.
    pub(in crate::vram) fn cards(&self) -> Vec<DeviceId> {
        match self {
            Placed::Now(c) | Placed::Roomiest(c) => vec![c.clone()],
            Placed::Evict { card, .. } | Placed::Wait { card, .. } => vec![card.clone()],
            Placed::Split { split, .. } => split.cards.clone(),
            Placed::Group(cards) => cards.clone(),
            Placed::Nowhere => Vec::new(),
        }
    }

    /// The split, where it is one.
    pub(in crate::vram) fn split(&self) -> Option<&Split> {
        match self {
            Placed::Split { split, .. } => Some(split),
            _ => None,
        }
    }
}

/// How much of its headroom a card keeps in a pass of [`place`] (module
/// doc, WP13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// The whole headroom on every card: the learned room is kept.
    Whole,
    /// The whole headroom while something of lmgw's is left on the card to
    /// make room with, the floor and the margin once nothing is (§5.6): the
    /// learned room given up.
    Left,
    /// As [`Keep::Left`], with [`Card::last`] once nothing is: the margin
    /// given up too.
    Last,
}

impl Card {
    /// What `ask` needs here beside it, keeping `keep` free.
    fn needs(&self, ask: &Ask, keep: u64) -> u64 {
        ask.bytes.saturating_add(self.engine).saturating_add(keep)
    }

    /// What it keeps free here in pass `pass` while `left` of lmgw's
    /// models stay (§5.6).
    fn keep(&self, left: bool, pass: Keep) -> u64 {
        match (pass, left) {
            (Keep::Whole, _) | (_, true) => self.headroom,
            (Keep::Left, false) => self.margin,
            (Keep::Last, false) => self.last,
        }
    }

    /// Could it hold `ask` with nothing else of lmgw's on it (§5.8)?
    fn could_hold(&self, ask: &Ask) -> bool {
        self.needs(ask, self.structural) <= self.total
    }

    /// The holders `mode` may count as room here, in the order they would
    /// leave: the idle ones least recently used first, then (waiting) the
    /// busy ones, least recently used first.
    fn freeable(&self, mode: Mode, busy: bool) -> Vec<&Holder> {
        let mut out: Vec<&Holder> = self
            .holders
            .iter()
            .filter(|h| match mode {
                Mode::Owner => h.idle || busy,
                Mode::Guest => h.idle && h.guest,
                Mode::Still => false,
            })
            .collect();
        out.sort_by_key(|h| (!h.idle, std::cmp::Reverse(h.idle_seconds), h.generation));
        out
    }

    /// The fewest of `holders` (in their order) whose leaving makes room for
    /// `ask` in pass `pass`, and what is free then beyond what it needs;
    /// `None` where all of them are not enough.
    fn after<'a>(
        &self,
        ask: &Ask,
        holders: &[&'a Holder],
        pass: Keep,
    ) -> Option<(Vec<&'a Holder>, u64)> {
        let mut free = self.free;
        let mut gone: Vec<&Holder> = Vec::new();
        let total_here = self.holders.len();
        loop {
            let left = gone.len() < total_here;
            let needs = self.needs(ask, self.keep(left, pass));
            if free >= needs && !gone.is_empty() {
                return Some((gone, free - needs));
            }
            let next = holders.get(gone.len())?;
            free = free.saturating_add(next.bytes);
            gone.push(next);
        }
    }

    /// The room it has for a model fitted to it: its free memory and what
    /// `mode` may count as room here, less the engine's cost.
    fn room(&self, mode: Mode) -> u64 {
        self.freeable(mode, true)
            .iter()
            .fold(self.free, |a, h| a.saturating_add(h.bytes))
            .saturating_sub(self.engine)
    }
}

/// Where `ask` goes on `cards`, for `mode` (module doc).
pub(in crate::vram) fn place(ask: &Ask, cards: &[Card], mode: Mode) -> Placed {
    if cards.is_empty() {
        return Placed::Nowhere;
    }
    // Step 1: now, with the headroom; with the learned room given up (a
    // card nothing of lmgw's is on, with the floor and margin, §5.6) only
    // once step 2 has no answer with it kept, and with the margin given up
    // too only once neither has an answer then (WP13).
    let now = |pass: Keep| {
        cards
            .iter()
            .filter_map(|c| {
                if pass != Keep::Whole && !c.holders.is_empty() {
                    return None;
                }
                let needs = c.needs(ask, c.keep(!c.holders.is_empty(), pass));
                (c.free >= needs).then(|| (c.free - needs, c.index, c))
            })
            .min_by_key(|(left, index, _)| (*left, *index))
            .map(|(_, _, c)| Placed::Now(c.id.clone()))
    };
    // Step 2: after evicting idle models of one card.
    let evict = |pass: Keep| {
        cards
            .iter()
            .filter_map(|c| {
                let victims = c.freeable(mode, false);
                let (gone, left) = c.after(ask, &victims, pass)?;
                let youngest = gone.iter().map(|h| h.idle_seconds).min().unwrap_or(0);
                let stay = !all_move(&gone, &c.id);
                Some((
                    (gone.len(), stay, std::cmp::Reverse(youngest), left, c.index),
                    c,
                    gone,
                ))
            })
            .min_by_key(|(key, _, _)| *key)
            .map(|(_, c, gone)| Placed::Evict {
                card: c.id.clone(),
                victims: gone.into_iter().cloned().collect(),
            })
    };
    if let Some(p) = now(Keep::Whole)
        .or_else(|| evict(Keep::Whole))
        .or_else(|| now(Keep::Left))
        .or_else(|| evict(Keep::Left))
        .or_else(|| now(Keep::Last))
        .or_else(|| evict(Keep::Last))
    {
        return p;
    }
    // Steps 3 and 4: a chat model with its whole set, over the fewest
    // cards that hold it now, then once idle models of theirs are stopped;
    // each with the learned room kept first, then the margin (WP13).
    if let Some(cost) = ask.split.as_deref() {
        if let Some(p) = split_now(cost, cards, Keep::Whole)
            .or_else(|| split_after(cost, cards, mode, Keep::Whole))
            .or_else(|| split_now(cost, cards, Keep::Left))
            .or_else(|| split_after(cost, cards, mode, Keep::Left))
            .or_else(|| split_now(cost, cards, Keep::Last))
            .or_else(|| split_after(cost, cards, mode, Keep::Last))
        {
            return p;
        }
    }
    let roomiest = || {
        cards
            .iter()
            .max_by_key(|c| (c.room(mode), std::cmp::Reverse(c.index)))
            .map(|c| Placed::Roomiest(c.id.clone()))
            .unwrap_or(Placed::Nowhere)
    };
    // A row that adapts is fitted to the roomiest candidate rather than
    // waited for whole (§5.3); so is a guest's, and a start that never
    // waits.
    if ask.adapts || mode != Mode::Owner {
        return roomiest();
    }
    // Step 5: once busy models of one card have left; the margin given up
    // only where no card would hold it with it (WP13).
    let wait_for = |pass: Keep| {
        cards
            .iter()
            .filter(|c| c.could_hold(ask))
            .filter_map(|c| {
                let holders = c.freeable(mode, true);
                let (gone, left) = c.after(ask, &holders, pass)?;
                Some(((gone.len(), left, c.index), c, gone))
            })
            .min_by_key(|(key, _, _)| *key)
    };
    let wait = wait_for(Keep::Left).or_else(|| wait_for(Keep::Last));
    if let Some((_, c, gone)) = wait {
        return Placed::Wait {
            card: c.id.clone(),
            on: gone.into_iter().cloned().collect(),
        };
    }
    // Step 6: the roomiest candidate.
    roomiest()
}

/// Whether every one of `gone`, stopped for room on `card`, could be started
/// again on another card now (§5.2 step 2, §6.2): in their order, each on
/// the card it leaves least spare on, what the ones before it take there
/// counted.
fn all_move(gone: &[&Holder], card: &DeviceId) -> bool {
    let mut taken: Vec<(DeviceId, u64)> = Vec::new();
    for h in gone {
        let used = |id: &DeviceId| {
            taken
                .iter()
                .filter(|(t, _)| t == id)
                .fold(0u64, |a, (_, b)| a.saturating_add(*b))
        };
        let best = h
            .elsewhere
            .iter()
            .filter(|s| &s.id != card)
            .filter_map(|s| {
                s.spare
                    .checked_sub(used(&s.id))
                    .map(|left| (left, s.index, s))
            })
            .min_by_key(|(left, index, _)| (*left, *index));
        match best {
            Some((_, _, s)) => taken.push((s.id.clone(), s.takes)),
            None => return false,
        }
    }
    true
}

/// Step 3: `cost` over the fewest of `cards` that hold it now, each card
/// keeping free what pass `pass` keeps there ([`Keep`]).
fn split_now(
    cost: &super::plan::chat::by_layer::ByLayer,
    cards: &[Card],
    pass: Keep,
) -> Option<Placed> {
    let rooms: Vec<(&Card, u64)> = cards
        .iter()
        .map(|c| {
            let keep = c.keep(!c.holders.is_empty(), pass);
            (c, c.free.saturating_sub(keep).saturating_sub(c.engine))
        })
        .collect();
    let split = split::fewest(cost, &rooms)?;
    Some(Placed::Split {
        split,
        victims: Vec::new(),
    })
}

/// Step 4: as [`split_now`] in pass `pass`, with the idle models `mode` may
/// stop counted as room on each card; then, card by card in the split's order, the
/// fewest of them (least recently used first) whose leaving makes room for
/// its part. A model on several of its cards that leaves frees each.
fn split_after(
    cost: &super::plan::chat::by_layer::ByLayer,
    cards: &[Card],
    mode: Mode,
    pass: Keep,
) -> Option<Placed> {
    let rooms: Vec<(&Card, u64)> = cards
        .iter()
        .map(|c| {
            let victims = c.freeable(mode, false);
            let freed = victims.iter().fold(0u64, |a, h| a.saturating_add(h.bytes));
            let keep = c.keep(c.holders.len() > victims.len(), pass);
            let room = c
                .free
                .saturating_add(freed)
                .saturating_sub(keep)
                .saturating_sub(c.engine);
            (c, room)
        })
        .collect();
    let split = split::fewest(cost, &rooms)?;
    let mut chosen: Vec<Holder> = Vec::new();
    for (j, id) in split.cards.iter().enumerate() {
        let c = cards.iter().find(|c| &c.id == id)?;
        let order = c.freeable(mode, false);
        loop {
            let gone: Vec<&Holder> = c
                .holders
                .iter()
                .filter(|h| chosen.iter().any(|x| x.generation == h.generation))
                .collect();
            let freed = gone.iter().fold(0u64, |a, h| a.saturating_add(h.bytes));
            let keep = c.keep(c.holders.len() > gone.len(), pass);
            let needs = split.bytes[j]
                .saturating_add(split.engine[j])
                .saturating_add(keep);
            if c.free.saturating_add(freed) >= needs {
                break;
            }
            let next = order
                .iter()
                .find(|h| !chosen.iter().any(|x| x.generation == h.generation))?;
            chosen.push((*next).clone());
        }
    }
    Some(Placed::Split {
        split,
        victims: chosen,
    })
}

/// The cards a split of `ask` would go on once every model of lmgw's on
/// them has left, the busy ones too (an owner's wait, step 5's rule): what
/// an owner's start that only a split holds waits for, where `place`
/// answered [`Placed::Roomiest`] (owner, 2026-11-18). `None` where `ask`
/// has no split or not even that holds it.
pub(in crate::vram) fn split_once_left(ask: &Ask, cards: &[Card]) -> Option<Vec<DeviceId>> {
    let cost = ask.split.as_deref()?;
    let rooms = |pass: Keep| -> Vec<(&Card, u64)> {
        cards
            .iter()
            .map(|c| {
                let freed = c
                    .freeable(Mode::Owner, true)
                    .iter()
                    .fold(0u64, |a, h| a.saturating_add(h.bytes));
                let room = c
                    .free
                    .saturating_add(freed)
                    .saturating_sub(c.keep(false, pass))
                    .saturating_sub(c.engine);
                (c, room)
            })
            .collect()
    };
    split::fewest(cost, &rooms(Keep::Left))
        .or_else(|| split::fewest(cost, &rooms(Keep::Last)))
        .map(|s| s.cards)
}

#[cfg(test)]
mod tests;
