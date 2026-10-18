//! The cells in flight (labs-store design §2.6): a cell is claimed before its
//! dispatch and released once its run is recorded, so a second post of a cell
//! that is being answered is the lab's 409 without a second dispatch (on a
//! cloud alias, a second bill). In process, because the gateway is its data
//! dir's only writer; the run's `UNIQUE (group_id, cell)` stays the last word.
//!
//! On the gateway's state rather than in a `static`: two gateways in one test
//! process must not see each other's group ids.
//!
//! A group with a cell claimed is **in flight** (`LabGroupView::in_flight`):
//! the history shows it so rather than as stopped, and deleting it is the
//! 409 `in_flight` until its run is in or its request is dropped.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// The `(group, cell)` pairs being answered now.
#[derive(Debug, Default, Clone)]
pub struct CellClaims {
    live: Arc<Mutex<HashSet<(i64, i64)>>>,
}

/// One claimed cell; released when dropped, so a handler whose client went
/// away mid-dispatch releases it too.
#[derive(Debug)]
pub struct CellClaim {
    live: Arc<Mutex<HashSet<(i64, i64)>>>,
    key: (i64, i64),
}

impl CellClaims {
    /// Claim cell `cell` of group `group`; `None` when it is claimed already.
    pub fn claim(&self, group: i64, cell: i64) -> Option<CellClaim> {
        let key = (group, cell);
        let fresh = self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key);
        fresh.then(|| CellClaim {
            live: self.live.clone(),
            key,
        })
    }

    /// Is a cell of group `group` claimed now?
    pub fn live(&self, group: i64) -> bool {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(g, _)| *g == group)
    }
}

impl Drop for CellClaim {
    fn drop(&mut self) {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cell_is_claimed_once_until_its_claim_is_dropped() {
        let claims = CellClaims::default();
        let first = claims.claim(1, 0).expect("free");
        assert!(claims.claim(1, 0).is_none(), "in flight");
        assert!(claims.claim(1, 1).is_some(), "another cell");
        assert!(claims.claim(2, 0).is_some(), "another group");
        assert!(claims.live(1), "group 1 is in flight");
        assert!(!claims.live(2), "group 2's claim was dropped at once");
        drop(first);
        assert!(!claims.live(1), "its run is in");
        assert!(claims.claim(1, 0).is_some(), "released");
    }
}
