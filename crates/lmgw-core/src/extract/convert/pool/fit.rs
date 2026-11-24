//! A slot fitted to what its step holds now ([`Slot::fit`]): less is given
//! back ([`Slot::shrink`]), more is taken only when the pool has it free at
//! once, nobody waits for it, and the caller's share allows it. A step
//! never waits for more memory while it holds a slot (two such steps
//! holding the memory would wait on each other for good); one that cannot
//! grow gives its slot back and takes a new one charged what it needs, as a
//! step of its own, in the queue like any other (`file/pdf/runs.rs`).

use std::sync::Arc;

use super::Slot;

impl Slot {
    /// These slots as one slot charged `bytes` from now: shrunk to it, or
    /// grown to it (review 2 of the poppler limit, S3 and its nits) when
    /// - no step waits (a grow never passes the queue, as a newcomer
    ///   never does),
    /// - and either this step is the only one running (a step heavier than
    ///   all the memory runs alone, as it is admitted), or the memory fits
    ///   now and, where callers' shares apply, the caller's charges with it
    ///   still leave a step like this one's first charge free beside them
    ///   (`share`'s rule for a step it starts).
    ///
    /// `Err(self)`, as it was, when it cannot grow now or a lease of it
    /// lives (a stopped tool not reaped yet, whose charge stays as it was).
    /// A grown pair (a PDF's two geometry runs side by side) keeps one
    /// slot: what follows is the one-core read of their outputs.
    pub fn fit(mut self, bytes: u64) -> Result<Slot, Slot> {
        if bytes <= self.held.bytes {
            return Ok(self.shrink(1, bytes));
        }
        let Some(held) = Arc::get_mut(&mut self.held) else {
            return Err(self);
        };
        let more = bytes - held.bytes;
        let mut g = held.inner.lock().unwrap_or_else(|e| e.into_inner());
        let alone = g.running == held.count;
        let fits = alone
            || (g
                .memory()
                .is_none_or(|m| g.charged.saturating_add(more) <= m)
                && (!g.shared
                    || g.memory().is_none_or(|m| {
                        g.shares.of(&held.share).bytes.saturating_add(more)
                            <= m.saturating_sub(held.bytes)
                    })));
        if g.waiters() > 0 || !fits {
            drop(g);
            return Err(self);
        }
        let freed = held.count.saturating_sub(1);
        g.running = g.running.saturating_sub(freed);
        g.charged = g.charged.saturating_add(more);
        g.shares.sub(&held.share, freed, 0);
        g.shares.add(&held.share, 0, more);
        (held.count, held.bytes) = (held.count.min(1), bytes);
        g.hand_out();
        drop(g);
        Ok(self)
    }

    /// The memory these slots are charged.
    pub fn bytes(&self) -> u64 {
        self.held.bytes
    }
}

#[cfg(test)]
mod tests;
