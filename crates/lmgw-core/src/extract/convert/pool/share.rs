//! One caller's share of the tool pool (security round A, M1): a caller —
//! a key's conversions, all keyless requests together, an account's owner
//! keys, or lmgw's own Chat and knowledge-base reads — never takes the
//! pool's last step's worth, so another caller's next step can always start
//! beside it.
//!
//! - **Slots:** one caller holds at most the size less one (all of a pool
//!   of one: no reserve is possible there).
//! - **Memory** (where it is counted, `super::charge`): a caller's charges
//!   together leave at least the charge of the step it asks for free, so
//!   one more step like its own fits beside them.
//! - A caller that holds nothing may always start a step (the pool's own
//!   rules still apply), so no caller is ever kept out by its share alone.
//! - **A caller holding nothing goes first** (review S1): a slot that comes
//!   free goes to a waiting caller holding none, the one served longest ago
//!   first, before the round robin; so a caller whose step just ended does
//!   not take the slot back before a newcomer, and a newcomer waits at most
//!   for one step to end.
//!
//! **Only where a second caller can exist** (review S5,
//! `super::super::callers`): "Require API key" on and more than one key may
//! call the conversion routes. Otherwise the share is off ([`ToolPool::set_shared`]):
//! one caller may use the whole pool, and lmgw's own reads wait behind its
//! batch, as before. A step whose caller is at its share waits without
//! keeping its turn: the slot goes round robin to the next caller that waits
//! (`super::Inner::hand_out`).
//!
//! Who the share belongs to is [`super::Caller::share`]: the key (an owner
//! key's account), or [`KEYLESS`] for every request without one (auth off),
//! whose queues are still one per request (`super::request_caller`).

use std::collections::HashMap;
use std::sync::Arc;

/// The share every request without a key counts against.
pub const KEYLESS: &str = "keyless";

/// What one caller holds now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Held {
    pub slots: usize,
    pub bytes: u64,
}

/// Every caller's holdings, and when each was last given a slot.
#[derive(Debug, Default)]
pub struct Shares {
    by: HashMap<Arc<str>, Held>,
    last: HashMap<Arc<str>, u64>,
    seq: u64,
}

/// The most slots one caller holds in a pool of `size`: one less, at least
/// one.
pub fn slots_of(size: usize) -> usize {
    size.saturating_sub(1).max(1)
}

/// How many steps charged `bytes` one caller runs at once in a pool of
/// `size` slots over `memory` (`None`: not counted): its share's slots, and
/// as many as fit the memory with one more left free, at least one.
pub fn steps_of(size: usize, memory: Option<u64>, bytes: u64) -> usize {
    let slots = slots_of(size);
    match memory {
        None => slots,
        Some(m) => {
            let room = m.saturating_sub(bytes);
            let n = usize::try_from(room / bytes.max(1)).unwrap_or(usize::MAX);
            n.clamp(1, slots)
        }
    }
}

impl Shares {
    /// What `who` holds now.
    pub fn of(&self, who: &str) -> Held {
        self.by.get(who).copied().unwrap_or_default()
    }

    /// How many of the `want` slots `who` may take now in a pool of `size`:
    /// `None` when it may take none (it is at its share).
    pub fn cap(&self, who: &str, want: usize, size: usize) -> Option<usize> {
        let held = self.of(who).slots;
        let room = slots_of(size).saturating_sub(held);
        if held == 0 {
            return Some(want.clamp(1, slots_of(size)));
        }
        (room > 0).then(|| want.clamp(1, room))
    }

    /// Whether `who` may start `n` slots charged `bytes` with `memory`
    /// counted (`None`: not counted), in a pool of `size`.
    pub fn allows(
        &self,
        who: &str,
        n: usize,
        bytes: u64,
        size: usize,
        memory: Option<u64>,
    ) -> bool {
        let held = self.of(who);
        if held.slots == 0 {
            return true;
        }
        if held.slots + n > slots_of(size) {
            return false;
        }
        memory.is_none_or(|m| held.bytes.saturating_add(bytes) <= m.saturating_sub(bytes))
    }

    /// `Some(last)` while `who` holds no slot: lower goes first.
    pub fn idle_rank(&self, who: &str) -> Option<u64> {
        (self.of(who).slots == 0).then(|| self.last.get(who).copied().unwrap_or(0))
    }

    pub fn add(&mut self, who: &Arc<str>, slots: usize, bytes: u64) {
        if slots > 0 {
            self.seq += 1;
            self.last.insert(who.clone(), self.seq);
        }
        let h = self.by.entry(who.clone()).or_default();
        h.slots += slots;
        h.bytes = h.bytes.saturating_add(bytes);
    }

    pub fn sub(&mut self, who: &str, slots: usize, bytes: u64) {
        if let Some(h) = self.by.get_mut(who) {
            h.slots = h.slots.saturating_sub(slots);
            h.bytes = h.bytes.saturating_sub(bytes);
            if h.slots == 0 && h.bytes == 0 {
                self.by.remove(who);
            }
        }
    }
}

/// The waiting caller holding no slot that was served longest ago to the
/// front of the round robin (module doc).
pub(super) fn idle_first(g: &mut super::Inner) {
    let best = g
        .waiting
        .iter()
        .enumerate()
        .filter(|(_, w)| w.queue.iter().any(|x| !x.tx.is_closed()))
        .filter_map(|(i, w)| g.shares.idle_rank(&w.caller.share).map(|r| (r, i)))
        .min();
    if let Some((_, i)) = best.filter(|(_, i)| *i > 0) {
        if let Some(w) = g.waiting.remove(i) {
            g.waiting.push_front(w);
        }
    }
}

impl super::ToolPool {
    /// Whether one caller's share applies (`callers::configure`).
    pub fn set_shared(&self, on: bool) {
        let mut g = self.lock();
        if g.shared != on {
            g.shared = on;
            g.hand_out();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn one_caller_leaves_a_slot_and_one_step_of_memory() {
        assert_eq!((slots_of(1), slots_of(2), slots_of(32)), (1, 1, 31));
        // 16 GiB: two PDF steps of 6 GiB fit, one caller runs one.
        assert_eq!(steps_of(32, Some(16 * GIB), 6 * GIB), 1);
        // 125 GiB: twenty fit, one caller runs nineteen.
        assert_eq!(steps_of(32, Some(125 * GIB), 6 * GIB), 19);
        // Less memory than one step: still one.
        assert_eq!(steps_of(8, Some(GIB), 6 * GIB), 1);
        assert_eq!(steps_of(8, None, 6 * GIB), 7);
        let mut s = Shares::default();
        let a: Arc<str> = Arc::from("key:1");
        assert!(
            s.allows("key:1", 4, 100 * GIB, 8, Some(16 * GIB)),
            "holds none"
        );
        s.add(&a, 1, 6 * GIB);
        assert!(!s.allows("key:1", 1, 6 * GIB, 8, Some(16 * GIB)));
        assert!(s.allows("key:1", 1, 2 * GIB, 8, Some(16 * GIB)));
        assert!(s.allows("key:2", 1, 6 * GIB, 8, Some(16 * GIB)));
        assert_eq!(s.cap("key:1", 8, 8), Some(6));
        s.add(&a, 6, 0);
        assert_eq!(s.cap("key:1", 1, 8), None);
        s.sub("key:1", 7, 6 * GIB);
        assert_eq!(s.of("key:1"), Held::default());
        assert_eq!(s.cap("key:1", 8, 8), Some(7));
    }

    fn pool_of(size: u32) -> super::super::ToolPool {
        let p = super::super::ToolPool::new(super::super::pool_size::Machine {
            cores: 8,
            memory: None,
        });
        p.set_by_hand(size);
        p
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    /// Review S1: two keys at their share in a pool of two, a newcomer;
    /// the slot the first key's step gives back goes to the newcomer.
    #[tokio::test]
    async fn a_newcomer_goes_before_a_caller_whose_step_just_ended() {
        use super::super::as_caller;
        let p = pool_of(2);
        let a = as_caller("key:1", p.acquire()).await;
        let _b = as_caller("key:2", p.acquire()).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for who in ["key:1", "key:2", "key:3"] {
            let (p, tx) = (p.clone(), tx.clone());
            tokio::spawn(as_caller(who, async move {
                let s = p.acquire().await;
                let _ = tx.send(who);
                std::future::pending::<()>().await;
                drop(s);
            }));
            settle().await;
        }
        assert_eq!(p.facts().waiting, 3);
        drop(a);
        settle().await;
        assert_eq!(rx.try_recv(), Ok("key:3"));
        assert!(rx.try_recv().is_err());
    }

    /// Review S5: with the share off one caller runs the whole pool.
    #[tokio::test]
    async fn with_the_share_off_one_caller_takes_every_slot() {
        use super::super::as_caller;
        let p = pool_of(2);
        p.set_shared(false);
        let both = as_caller("key:1", async { (p.acquire().await, p.acquire().await) });
        let both = tokio::time::timeout(std::time::Duration::from_secs(5), both)
            .await
            .expect("both slots");
        assert_eq!(p.facts().running, 2);
        assert_eq!(p.facts().per_caller_slots, 2);
        drop(both);
        p.set_shared(true);
        assert_eq!(p.facts().per_caller_slots, 1);
    }
}
