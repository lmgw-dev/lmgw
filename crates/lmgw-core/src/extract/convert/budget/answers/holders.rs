//! One caller's share of the answers in flight (security round A review,
//! B2), the rule `pool::share` has for tool slots: a caller holding shares
//! takes another only while the bound keeps one like it free beside them,
//! and a caller holding none is served first, the one served longest ago
//! first (so a caller whose share just went back does not take the next
//! one before a newcomer, review S1).

use std::collections::HashMap;
use std::sync::Arc;

/// What one caller holds, and when it was last given a share.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Held {
    pub count: u32,
    pub bytes: u64,
    /// The hand-out it got its last share at (0: never).
    pub last: u64,
}

/// Every caller's holdings; a caller holding none keeps its `last`.
#[derive(Debug, Default)]
pub struct Holders {
    by: HashMap<Arc<str>, Held>,
    seq: u64,
}

impl Holders {
    pub fn of(&self, who: &str) -> Held {
        self.by.get(who).copied().unwrap_or_default()
    }

    /// Whether `who` may take `bytes` more under `limit` (`None`: not
    /// bounded): it holds none, or its shares and this one leave one like
    /// this one free.
    pub fn allows(&self, who: &str, bytes: u64, limit: Option<u64>) -> bool {
        let h = self.of(who);
        h.count == 0
            || limit.is_none_or(|l| h.bytes.saturating_add(bytes) <= l.saturating_sub(bytes))
    }

    /// `Some(last)` while `who` holds none: lower goes first.
    pub fn idle_rank(&self, who: &str) -> Option<u64> {
        let h = self.of(who);
        (h.count == 0).then_some(h.last)
    }

    pub fn add(&mut self, who: &Arc<str>, bytes: u64) {
        self.seq += 1;
        let seq = self.seq;
        let h = self.by.entry(who.clone()).or_default();
        h.count += 1;
        h.bytes = h.bytes.saturating_add(bytes);
        h.last = seq;
    }

    pub fn sub(&mut self, who: &str, bytes: u64) {
        if let Some(h) = self.by.get_mut(who) {
            h.count = h.count.saturating_sub(1);
            h.bytes = h.bytes.saturating_sub(bytes);
        }
    }

    /// One of `who`'s shares went from `from` to `to` bytes.
    pub fn resize(&mut self, who: &str, from: u64, to: u64) {
        if let Some(h) = self.by.get_mut(who) {
            h.bytes = h.bytes.saturating_sub(from).saturating_add(to);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caller_leaves_one_share_like_its_own_and_idle_callers_rank_by_last_turn() {
        let mut h = Holders::default();
        let (a, b): (Arc<str>, Arc<str>) = (Arc::from("key:1"), Arc::from("key:2"));
        assert!(h.allows("key:1", 4, Some(8)), "holds none");
        h.add(&a, 2);
        h.add(&a, 2);
        assert!(h.allows("key:1", 2, Some(8)), "4 + 2 leaves 2");
        h.add(&a, 2);
        assert!(!h.allows("key:1", 2, Some(8)), "6 + 2 leaves none");
        assert!(h.allows("key:2", 8, Some(8)));
        assert!(h.allows("key:1", 2, None));
        h.add(&b, 1);
        h.sub("key:2", 1);
        assert_eq!(h.idle_rank("key:1"), None);
        assert_eq!(h.idle_rank("key:3"), Some(0), "never served goes first");
        assert_eq!(h.idle_rank("key:2"), Some(4));
        h.resize("key:1", 2, 1);
        assert_eq!(h.of("key:1").bytes, 5);
    }
}
