//! What the reconciliation pass remembers from one pass to the next: the
//! containers it has already told the log about, and the ones it keeps failing
//! on.
//!
//! **Backoff.** A container the pass removed and that is still listed (the
//! removal failed), or one it adopted that turned up unheld again (its entry
//! was dropped by a stop that failed), is a container the pass cannot get rid
//! of. Retried every tick, it would log the same failure every 15 s and keep a
//! `stopping` entry over the model's name half the time. So each one is
//! remembered by its name *and* its creation time — a container recreated
//! under the same name is a new one — and retried after a wait that doubles,
//! from two ticks up to [`BACKOFF_TICKS_MAX`] ticks. The first failure is
//! logged at WARN, the retries at DEBUG; a container that is gone, or whose
//! removal went through, is forgotten.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// The longest wait between two tries at a container the pass keeps failing
/// on, in reaper ticks: 16 ticks of 15 s, four minutes. A retry cadence, not a
/// bound on anything — a podman that recovers is noticed within it.
pub(super) const BACKOFF_TICKS_MAX: u32 = 16;

/// The pass's memory (module doc). Guarded by the pass lock, so every pass
/// reads what the one before it wrote.
#[derive(Debug, Default)]
pub struct PassMemory {
    /// Running containers the pass handled — adopted, or failed to remove —
    /// by name.
    seen: HashMap<String, Seen>,
    /// Containers under this instance's prefix that another lmgw owns, or that
    /// carry no owner: logged once each, never touched.
    foreign: HashSet<String>,
    /// Whether the last pass changed what the registry holds — what a caller
    /// that waited for it is told (`PassWait::Join`).
    pub(super) last_changed: bool,
}

#[derive(Debug)]
struct Seen {
    /// Podman's `Created`, in unix seconds: the same name with another time is
    /// another container.
    created: i64,
    /// Failures so far; 0 for a container adopted once and not seen since.
    tries: u32,
    /// Not before this.
    next: Instant,
}

impl PassMemory {
    /// Forget every container that is not in this listing any more.
    pub(super) fn prune(&mut self, listed: &HashSet<&str>) {
        self.seen.retain(|name, _| listed.contains(name.as_str()));
        self.foreign.retain(|name| listed.contains(name.as_str()));
    }

    /// Is this the first time the pass sees `name` as somebody else's?
    pub(super) fn first_foreign(&mut self, name: &str) -> bool {
        self.foreign.insert(name.to_string())
    }

    /// Is `(name, created)` one the pass handled before — adopted, or failed
    /// on — and so one it is meeting again?
    pub(super) fn again(&self, name: &str, created: i64) -> bool {
        self.seen.get(name).is_some_and(|s| s.created == created)
    }

    /// Is the pass waiting before it tries `(name, created)` again?
    pub(super) fn backing_off(&self, name: &str, created: i64, now: Instant) -> bool {
        self.seen
            .get(name)
            .is_some_and(|s| s.created == created && s.tries > 0 && now < s.next)
    }

    /// The pass adopted `(name, created)`.
    pub(super) fn adopted(&mut self, name: &str, created: i64, now: Instant) {
        if !self.again(name, created) {
            self.seen.insert(
                name.to_string(),
                Seen {
                    created,
                    tries: 0,
                    next: now,
                },
            );
        }
    }

    /// One more failure on `(name, created)`; `true` when it is the first, the
    /// one to log at WARN. `tick` is the backoff's unit.
    pub(super) fn failed(
        &mut self,
        name: &str,
        created: i64,
        now: Instant,
        tick: Duration,
    ) -> bool {
        let s = self.seen.entry(name.to_string()).or_insert(Seen {
            created,
            tries: 0,
            next: now,
        });
        if s.created != created {
            *s = Seen {
                created,
                tries: 0,
                next: now,
            };
        }
        s.tries += 1;
        s.next = now + backoff(s.tries, tick);
        s.tries == 1
    }

    /// `name` is gone: removed by the pass.
    pub(super) fn forget(&mut self, name: &str) {
        self.seen.remove(name);
    }
}

/// The wait after the `tries`-th failure: two ticks, doubling, at most
/// [`BACKOFF_TICKS_MAX`] ticks.
pub(super) fn backoff(tries: u32, tick: Duration) -> Duration {
    let ticks = 2u32.saturating_pow(tries.max(1)).min(BACKOFF_TICKS_MAX);
    tick * ticks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_doubles_from_two_ticks_to_the_most() {
        let tick = Duration::from_secs(15);
        let waits: Vec<u64> = (1..=6).map(|n| backoff(n, tick).as_secs()).collect();
        assert_eq!(waits, vec![30, 60, 120, 240, 240, 240]);
    }

    #[test]
    fn a_container_is_known_by_its_name_and_its_creation() {
        let now = Instant::now();
        let tick = Duration::from_secs(15);
        let mut m = PassMemory::default();
        assert!(m.failed("c", 100, now, tick), "the first failure is logged");
        assert!(!m.failed("c", 100, now, tick), "the second is not");
        assert!(m.backing_off("c", 100, now));
        assert!(!m.backing_off("c", 100, now + backoff(2, tick)));
        // Recreated under the same name: a new container, nothing held against
        // it.
        assert!(!m.again("c", 200));
        assert!(!m.backing_off("c", 200, now));
        assert!(
            m.failed("c", 200, now, tick),
            "its first failure is logged again"
        );
    }
}
