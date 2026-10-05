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

use crate::config::Snapshot;
use crate::state::SharedState;

use super::ledger::Ledger;
use super::scheduler::MIB;
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
            headroom: snap.settings.vram.headroom_mb.saturating_mul(MIB),
            outside: outside_of(&l),
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
        let is = |r: &ResidentView, t: &Target| r.container == t.class && r.model == t.model_id;
        let is_kept = |r: &ResidentView| kept.iter().any(|k| is(r, k));
        let kept_bytes: u64 = l
            .residents
            .iter()
            .filter(|r| is_kept(r))
            .map(|r| r.estimated_bytes)
            .sum();
        if !l.residents.iter().any(is_kept) {
            return None;
        }
        // What evicting one gives back: its charge, the peak kept free for
        // it, and — where the driver is the measurement, which subtracted it
        // from the free figure — what it has still to load. A reservation (a
        // start another waiter has pending, `ledger`) is no victim: its
        // bytes are out of `free` already and it cannot be evicted (WP11
        // server review n2), as `outside_of` reads it too.
        let evictable: u64 = l
            .residents
            .iter()
            .filter(|r| !is_kept(r) && !is(r, target) && r.state != "reserved")
            .map(|r| {
                let pending = if cap.measured {
                    r.pending_bytes.unwrap_or(0)
                } else {
                    0
                };
                r.estimated_bytes
                    .saturating_add(r.peak_extra_bytes.unwrap_or(0))
                    .saturating_add(pending)
            })
            .fold(0u64, u64::saturating_add);
        let needs = fp
            .total_bytes
            .saturating_add(snap.settings.vram.headroom_mb.saturating_mul(MIB));
        let room = cap.free.saturating_add(evictable);
        (needs > room).then_some(Crowded {
            needs,
            room,
            kept: kept_bytes,
        })
    }
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
