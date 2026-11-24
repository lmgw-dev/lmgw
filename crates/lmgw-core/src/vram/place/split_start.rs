//! A start `place` split over several cards (device-ledger design §4.3,
//! §5.5, §7.2, WP6), as each decision works with it: whether every card
//! holds its part now, which card is short, the grant that tells
//! llama-server the split, and what the start needs on each card.
//!
//! **Card by card.** A split fits where each of its cards has its part
//! free — the layers the split gives it, the engine's own cost there and
//! what the start keeps free there: its headroom while something else of
//! lmgw's is on that card, what it keeps last once nothing is (§5.6). The cards'
//! sum is no test: one card short is not made up by another with room.
//! Where one is short, an eviction or a wait is on that card.

use super::super::ledger::Ledger;
use super::super::nvml::DeviceId;
use super::super::partial::Grant;
use super::super::{Target, VramScheduler};
use super::choose::{others_hold_on, Placement};

impl VramScheduler {
    /// What split start `p` needs on each of its cards on ledger `l` now
    /// (module doc), in its cards' order; empty for a start on one card.
    pub(in crate::vram) fn split_needs(
        &self,
        l: &Ledger,
        target: &Target,
        p: &Placement,
    ) -> Vec<(DeviceId, u64)> {
        let Some((sp, _)) = &p.split else {
            return Vec::new();
        };
        sp.cards
            .iter()
            .enumerate()
            .map(|(j, c)| {
                let keep = match others_hold_on(l, target, std::slice::from_ref(c)) {
                    true => sp.headroom[j],
                    false => sp.last[j],
                };
                let needs = sp.bytes[j]
                    .saturating_add(sp.engine[j])
                    .saturating_add(keep);
                (c.clone(), needs)
            })
            .collect()
    }

    /// The first card of split start `p` short of its part on ledger `l`
    /// (module doc), with what it needs there and what is free there for
    /// this start (`own`: its claim); `None` where every card holds its
    /// part now, and for a start on one card. Under a declared budget over
    /// several cards the parts together, the engine's cost included, also
    /// fit within what the budget leaves (owner, 2026-11-18, as
    /// `split::over` checks an lmgw split; a row's own split kept on its pin
    /// is checked here): where they do not, the split's first card, with
    /// the parts' sum and what the budget leaves.
    pub(in crate::vram) fn split_short(
        &self,
        l: &Ledger,
        target: &Target,
        p: &Placement,
        own: Option<u64>,
    ) -> Option<(DeviceId, u64, u64)> {
        let card_short = self
            .split_needs(l, target, p)
            .into_iter()
            .find_map(|(c, needs)| {
                let free = l
                    .capacity_on(std::slice::from_ref(&c), own)
                    .map_or(0, |cap| cap.free);
                (free < needs).then_some((c, needs, free))
            });
        card_short.or_else(|| {
            let (sp, _) = p.split.as_ref()?;
            let left = l.budget_left(own)?;
            let together = sp
                .bytes
                .iter()
                .zip(&sp.engine)
                .fold(0u64, |a, (b, e)| a.saturating_add(*b).saturating_add(*e));
            let first = sp.cards.first()?.clone();
            (together > left).then_some((first, together, left))
        })
    }
}

impl VramScheduler {
    /// Why `target`, a chat row that sets its context, is not split over
    /// several cards, as the end of a refusal or a wait's log line (§7.2:
    /// "the start says why"); empty for every other model and on a machine
    /// of one card.
    pub(in crate::vram) fn no_split_hint(
        &self,
        snap: &crate::config::Snapshot,
        target: &Target,
        fp: &super::super::Footprint,
    ) -> String {
        if self.last_devices.lock().unwrap().len() < 2 {
            return String::new();
        }
        match self.splits(snap, target, fp) {
            Err(Some(why)) => format!(". lmgw does not split it over several cards: {why}"),
            _ => String::new(),
        }
    }
}

/// The grant of split start `p` (§7.2): the split's bytes on every card
/// together, the engine's left out, and the split rendered over the row
/// with `--fit off` where the image has `--fit`; `None` for a start on one
/// card. What it is told is on the start's warnings already (the
/// placement's note), so the grant says nothing of its own. The row's own
/// split kept on its pin (ruling 11) is the row as written: its whole
/// grant, nothing rendered over it.
pub(in crate::vram) fn split_grant(p: &Placement) -> Option<Grant> {
    let (sp, tensor) = p.split.as_ref()?;
    let Some(tensor) = tensor else {
        return Some(Grant::Whole(sp.total()));
    };
    Some(Grant::Partial(crate::runtime::registry::PartialGrant {
        bytes: sp.total(),
        max_vram_bytes: 0,
        warning: String::new(),
        llama: Some(crate::runtime::registry::LlamaPlacement {
            split: Some(*tensor),
            ..Default::default()
        }),
    }))
}
