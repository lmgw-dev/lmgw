//! The most one request's converted output may add up to (document-conversion
//! design §7.6): `MAX_UNCOMPRESSED_BYTES`, the named bound the readers
//! already hold their own output to, here over the sum of every file's
//! content in one request. Files convert side by side, so the sum is one
//! shared counter: the file whose output crosses it fails with
//! [`ErrorCode::OutputTooLarge`], files already converted stay converted, and
//! files that finish after it fail the same way (which file crosses it
//! depends on the order they finish in).
//!
//! **Spent stays spent.** A [`OutputBudget::take`] that does not fit fails
//! its file and marks the request's budget spent: every later `take` and
//! `try_take` fails, whatever is given back afterwards, so files that
//! finish after it fail the same way. A failed take is never undone.
//! [`OutputBudget::try_take`] is the one way to ask without that: a part
//! of a file that can go without its bytes (a picture, which keeps its
//! placeholder) asks so, and when it does not fit nothing changes.
//! [`OutputBudget::give_back`] returns what a reservation did not need (a
//! picture's bytes held for the answer, [`Spend`]), which later files may
//! use while the budget is not spent.
//!
//! **Reservations.** A picture's bytes are reserved when it gets them, at
//! the size the answer writes them in (base64 and its entry), and a file's
//! output takes go through a [`Spend`] holding those reservations: each
//! take uses the held bytes first, so the bytes are counted once, and what
//! no take used is given back when the file's output is done.

pub mod answers;
pub mod upload;

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use lmgw_api_types::documents::convert::ErrorCode;

/// One request's output so far, against its limit.
#[derive(Debug)]
pub struct OutputBudget {
    limit: u64,
    used: AtomicU64,
    /// A take did not fit: the request's output is spent (module doc).
    spent: AtomicBool,
}

impl OutputBudget {
    /// A request's budget: `MAX_UNCOMPRESSED_BYTES`, or half the
    /// answers' bound where that is less (`answers::Answers::output_budget`,
    /// security round A review S4).
    pub fn request() -> Self {
        Self::with_limit(answers::global().output_budget())
    }

    /// The budget as refusals name it: its size and where it comes from.
    pub fn named(&self) -> String {
        named(self.limit)
    }

    /// A budget of `limit` bytes (a test's small one).
    pub fn with_limit(limit: u64) -> Self {
        Self {
            limit,
            used: AtomicU64::new(0),
            spent: AtomicBool::new(false),
        }
    }

    /// Count `bytes` of one file's output; the error is that file's.
    pub fn take(&self, bytes: usize) -> Result<(), (ErrorCode, String)> {
        self.take_u64(u64::try_from(bytes).unwrap_or(u64::MAX))
    }

    /// [`OutputBudget::take`] of a count that may not fit a `usize` (a
    /// table's grid, charged before it is written).
    pub fn take_u64(&self, bytes: u64) -> Result<(), (ErrorCode, String)> {
        if !self.spent.load(Ordering::Relaxed) {
            let fits = self
                .used
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |u| {
                    u.checked_add(bytes).filter(|n| *n <= self.limit)
                })
                .is_ok();
            if fits {
                return Ok(());
            }
            // Never undone (module doc).
            self.spent.store(true, Ordering::Relaxed);
        }
        Err((ErrorCode::OutputTooLarge, self.over_message(bytes)))
    }

    /// Take `bytes` only when they fit, leaving the counter as it was when
    /// they do not: a part of a file that can go without its bytes (a
    /// picture, which keeps its placeholder) asks this way, so the files
    /// after it still convert.
    pub fn try_take(&self, bytes: u64) -> bool {
        !self.spent.load(Ordering::Relaxed)
            && self
                .used
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |u| {
                    u.checked_add(bytes).filter(|n| *n <= self.limit)
                })
                .is_ok()
    }

    /// What is counted now (tests).
    #[cfg(test)]
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// What still fits now (none once spent): a bound a child is given for
    /// what it makes (a page's picture crops), which the takes then check.
    pub fn left(&self) -> u64 {
        if self.is_spent() {
            return 0;
        }
        self.limit.saturating_sub(self.used.load(Ordering::Relaxed))
    }

    /// Whether a take failed: nothing more fits, whatever is given back.
    pub fn is_spent(&self) -> bool {
        self.spent.load(Ordering::Relaxed)
    }

    /// Give back `bytes` taken before (a reservation a [`Spend`] did not
    /// use). It never reopens a spent budget.
    pub fn give_back(&self, bytes: u64) {
        let _ = self
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |u| {
                Some(u.saturating_sub(bytes))
            });
    }

    fn over_message(&self, bytes: u64) -> String {
        format!(
            "this file's converted output ({}) passed what one request's files may produce \
             together, {}; send the files in separate requests",
            crate::hf::fmt_bytes(bytes),
            self.named()
        )
    }
}

/// A budget of `limit` as refusals name it.
pub fn named(limit: u64) -> String {
    format!(
        "{} (MAX_UNCOMPRESSED_BYTES, or half the memory for answers where that is less)",
        crate::hf::fmt_bytes(limit)
    )
}

/// One file's output takes, with the bytes it reserved before (its
/// pictures'), which the takes use first. Dropping it gives back what no
/// take used.
pub struct Spend<'a> {
    budget: &'a OutputBudget,
    held: Cell<u64>,
}

impl<'a> Spend<'a> {
    pub fn new(budget: &'a OutputBudget, held: u64) -> Self {
        Self {
            budget,
            held: Cell::new(held),
        }
    }

    /// [`OutputBudget::take`], the held bytes used first.
    pub fn take(&self, bytes: usize) -> Result<(), (ErrorCode, String)> {
        self.take_u64(u64::try_from(bytes).unwrap_or(u64::MAX))
    }

    /// [`OutputBudget::take_u64`], the held bytes used first.
    pub fn take_u64(&self, bytes: u64) -> Result<(), (ErrorCode, String)> {
        let held = self.held.get();
        let used = held.min(bytes);
        self.held.set(held - used);
        self.budget.take_u64(bytes - used)
    }

    /// Give back `bytes` taken through this (a text taken twice).
    pub fn give_back(&self, bytes: u64) {
        self.budget.give_back(bytes);
    }
}

impl Drop for Spend<'_> {
    fn drop(&mut self) {
        self.budget.give_back(self.held.get());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_that_crosses_the_budget_fails_and_so_do_later_ones() {
        let b = OutputBudget::with_limit(100);
        assert!(b.take(60).is_ok());
        let (code, why) = b.take(60).unwrap_err();
        assert_eq!(code, ErrorCode::OutputTooLarge);
        assert!(why.contains("100 B (MAX_UNCOMPRESSED_BYTES"), "{why}");
        // Past it, even a small file no longer fits.
        assert!(b.take(1).is_err());
        // A count near the top never wraps the counter.
        let b = OutputBudget::with_limit(100);
        assert!(b.take_u64(u64::MAX).is_err());
        assert!(b.take_u64(u64::MAX).is_err());
        assert!(b.take(1).is_err());
    }

    #[test]
    fn a_try_that_does_not_fit_leaves_the_budget_and_a_give_back_frees() {
        let b = OutputBudget::with_limit(100);
        assert!(b.try_take(60));
        assert!(!b.try_take(60));
        assert!(b.try_take(40));
        b.give_back(50);
        assert!(b.take(50).is_ok());
        b.give_back(u64::MAX);
        assert!(b.take(100).is_ok());
    }

    #[test]
    fn what_is_left_shrinks_with_takes_and_is_none_once_spent() {
        let b = OutputBudget::with_limit(100);
        assert_eq!(b.left(), 100);
        assert!(b.try_take(30));
        assert_eq!(b.left(), 70);
        assert!(b.take(80).is_err());
        assert_eq!(b.left(), 0);
    }

    #[test]
    fn a_failed_take_is_never_undone_by_what_is_given_back() {
        let b = OutputBudget::with_limit(100);
        assert!(b.try_take(80));
        assert!(b.take(30).is_err());
        assert!(b.is_spent());
        // Another file's reservation comes back: the request stays spent.
        b.give_back(80);
        assert!(b.take(1).is_err());
        assert!(!b.try_take(1));
        // A try that does not fit spends nothing.
        let b = OutputBudget::with_limit(100);
        assert!(!b.try_take(101));
        assert!(!b.is_spent());
        assert!(b.take(100).is_ok());
    }

    #[test]
    fn a_spend_uses_its_held_bytes_first_and_gives_back_the_rest() {
        let b = OutputBudget::with_limit(100);
        assert!(b.try_take(70));
        {
            let s = Spend::new(&b, 70);
            // 50 of the held 70, then 30 held and 10 new.
            assert!(s.take(50).is_ok());
            assert!(s.take(40).is_ok());
        }
        // 90 counted: what the output really is, once.
        assert!(b.take(10).is_ok());
        assert!(b.take(1).is_err());
        let b = OutputBudget::with_limit(100);
        assert!(b.try_take(60));
        drop(Spend::new(&b, 60));
        assert!(b.take(100).is_ok());
    }

    #[test]
    fn a_request_is_bounded_by_the_named_constant() {
        let b = OutputBudget::request();
        let (_, why) = b.take(usize::MAX).unwrap_err();
        assert!(why.contains("1.0 GiB (MAX_UNCOMPRESSED_BYTES"), "{why}");
    }
}
