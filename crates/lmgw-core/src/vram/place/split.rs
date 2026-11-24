//! Several cards (device-ledger design §4.3, §5.2 steps 3 and 4, §5.4,
//! WP6): a chat model with its whole set that no single card holds, now or
//! after evicting, goes over the fewest cards that hold it, each card given
//! a run of whole layers.
//!
//! **The fewest cards** (§5.4): the candidates by room, most first; cards
//! are added until a split over them holds the model. Among sets of equal
//! size the one chosen this way has the most room; D7 does not apply.
//!
//! **The split** (§4.3): the cards are filled in that order, each but the
//! last to its room and the rest on the last, so what is left over stays on
//! one card in one block. Which layers a card gets follows from the cards'
//! order on the bus — llama.cpp gives the first card the first layers and
//! the last card the output layer — so a card is filled by the widest run
//! of that many layers it could be given, and the split is then checked
//! card by card with the layers each one does get.
//!
//! **Room** is what a card can take of the model: its free memory less what
//! the start keeps free there (§5.6: its headroom while something of
//! lmgw's on the card could be waited for or evicted, what it keeps last
//! once nothing is, [`Card::last`]) and the engine's own cost there (§4.2).

use super::super::nvml::DeviceId;
use super::super::plan::chat::by_layer::ByLayer;
use super::Card;

/// What a card of a split holds for the engine beyond that card's engine
/// cost, which one-card starts teach (owner, 2026-11-18). Measured 2026-11-18
/// on splits on a four-card test server as what the processes hold on a card
/// less llama.cpp's own buffers there: an RTX PRO 6000 Blackwell (606 MiB
/// learned) 623, 625 and 667 MiB (the 667 the middle card of a three-card split
/// without a pipeline), an RTX 6000 Ada (477 learned) 497, another Ada never
/// learned (640 charged) 479–491; so up to 61 MiB more than one card's
/// figure. Charged on every card of a split, the first and the others, on
/// a pipeline or not; a split feeds no engine reading.
pub(in crate::vram) const SPLIT_ENGINE_EXTRA: u64 = 64 * 1024 * 1024;

/// A split over several cards (module doc), in the cards' order on the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::vram) struct Split {
    pub cards: Vec<DeviceId>,
    /// The repeating layers on each card.
    pub counts: Vec<u64>,
    /// What the model puts on each card, the engine's cost left out
    /// ([`ByLayer::on_cards`]).
    pub bytes: Vec<u64>,
    /// The engine's own cost on each (§4.2) and [`SPLIT_ENGINE_EXTRA`].
    pub engine: Vec<u64>,
    /// What the start keeps free beside it on each while lmgw's own models
    /// there could be waited for or evicted, and once none are (§5.6,
    /// [`Card::last`]: the learned room and the margin given up).
    pub headroom: Vec<u64>,
    pub last: Vec<u64>,
    /// The row runs a draft model, confined to the first card.
    pub drafter: bool,
    /// The card the output layer is on, by its place among `cards`: the
    /// last, but for a row's own split whose last entries are 0 (§7.2).
    pub output: usize,
}

impl Split {
    /// `--tensor-split`'s entries: the layers on each card, the last card's
    /// with the output layer (WP0: they sum to `n_layer + 1`).
    pub(in crate::vram) fn entries(&self) -> Vec<u64> {
        let last = self.counts.len().saturating_sub(1);
        self.counts
            .iter()
            .enumerate()
            .map(|(j, n)| n + u64::from(j == last))
            .collect()
    }

    /// What the model and the engine put on each card together.
    pub(in crate::vram) fn charged(&self) -> Vec<(DeviceId, u64)> {
        self.cards
            .iter()
            .zip(self.bytes.iter().zip(&self.engine))
            .map(|(c, (b, e))| (c.clone(), b.saturating_add(*e)))
            .collect()
    }

    /// The model's bytes on every card together, the engine's left out.
    pub(in crate::vram) fn total(&self) -> u64 {
        self.bytes.iter().fold(0, |a, b| a.saturating_add(*b))
    }
}

/// The fewest of `rooms` — each a card and what it can take of the model
/// (module doc) — that hold `cost` split over them; `None` where no two or
/// more of them do. At most [`crate::runtime::argv::MAX_SPLIT`] cards.
pub(in crate::vram) fn fewest(cost: &ByLayer, rooms: &[(&Card, u64)]) -> Option<Split> {
    let mut by_room: Vec<&(&Card, u64)> = rooms.iter().collect();
    by_room.sort_by_key(|(c, room)| (std::cmp::Reverse(*room), c.index));
    let most = by_room.len().min(crate::runtime::argv::MAX_SPLIT);
    (2..=most).find_map(|k| over(cost, &by_room[..k]))
}

/// `cost` split over exactly `chosen` (in the order they are filled), or
/// `None` where they do not hold it.
fn over(cost: &ByLayer, chosen: &[&(&Card, u64)]) -> Option<Split> {
    let k = chosen.len();
    // The cards' order on the bus, which is the engine's
    // (`CUDA_DEVICE_ORDER=PCI_BUS_ID`, §2.2).
    let mut bus: Vec<usize> = (0..k).collect();
    bus.sort_by_key(|&i| chosen[i].0.index);
    // What each card holds beside its layers, by its place on the bus: a
    // split's compute buffer ([`ByLayer::compute_on`]), its own parts, and
    // what the engine holds more on a split card ([`SPLIT_ENGINE_EXTRA`]:
    // the rooms are net of the card's one-card engine cost only).
    let fixed = |pos: usize| {
        let mut b = cost.compute_on(pos, k).saturating_add(SPLIT_ENGINE_EXTRA);
        if pos == 0 {
            b = b.saturating_add(cost.first);
        }
        if pos + 1 == k {
            b = b.saturating_add(cost.last);
        }
        b
    };
    let pos_of = |i: usize| bus.iter().position(|&b| b == i).unwrap_or(0);
    let layers = cost.layers.len();
    let mut counts = vec![0usize; k];
    let mut left = layers;
    // Filled in the order of room, each but the last to its room.
    for i in 0..k - 1 {
        let room = chosen[i].1.checked_sub(fixed(pos_of(i)))?;
        // The most layers whose widest run fits (`widest` grows with n).
        let (mut lo, mut hi) = (0usize, left);
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            if cost.widest(mid) <= room {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let n = lo;
        counts[i] = n;
        left -= n;
    }
    counts[k - 1] = left;
    // Every card a layer at least: one that would get none takes one from
    // the card that got the most.
    for i in 0..k {
        if counts[i] == 0 {
            let from = (0..k).max_by_key(|&j| counts[j])?;
            if counts[from] < 2 {
                return None;
            }
            counts[from] -= 1;
            counts[i] += 1;
        }
    }
    let on_bus: Vec<u64> = bus.iter().map(|&i| counts[i] as u64).collect();
    let bytes = cost.on_cards(&on_bus)?;
    for (pos, &i) in bus.iter().enumerate() {
        if bytes[pos].saturating_add(SPLIT_ENGINE_EXTRA) > chosen[i].1 {
            return None;
        }
    }
    let cards: Vec<&Card> = bus.iter().map(|&i| chosen[i].0).collect();
    // Under a declared budget, the parts together, the engine's cost
    // included, within what it leaves ([`Card::pool`]).
    if let Some(pool) = cards.iter().filter_map(|c| c.pool).min() {
        let together = bytes.iter().zip(&cards).fold(0u64, |a, (b, c)| {
            a.saturating_add(*b)
                .saturating_add(c.engine)
                .saturating_add(SPLIT_ENGINE_EXTRA)
        });
        if together > pool {
            return None;
        }
    }
    Some(Split {
        cards: cards.iter().map(|c| c.id.clone()).collect(),
        counts: on_bus,
        bytes,
        engine: cards
            .iter()
            .map(|c| c.engine.saturating_add(SPLIT_ENGINE_EXTRA))
            .collect(),
        headroom: cards.iter().map(|c| c.headroom).collect(),
        last: cards.iter().map(|c| c.last).collect(),
        drafter: cost.drafter,
        output: k - 1,
    })
}
