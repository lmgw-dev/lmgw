//! **The remembered quota state** (openai egress design §14.6): an upstream
//! row whose account answered that its quota is exhausted
//! (`insufficient_quota`) is remembered so, per row — the scope of a
//! provider key's credit — in memory, for the process's life.
//!
//! - **Set** by every quota answer ([`QuotaMemory::set`]), with the next
//!   check the upstream's `Retry-After` from now, or
//!   `settings.quota_recheck_seconds`.
//! - **Cleared** by any 2xx from that upstream, by an edit of the upstream
//!   row (the key may have changed), and by the owner's "Check now"
//!   (`upstream_quota_clear`) — [`QuotaMemory::clear`].
//! - **While set**, a hop on that upstream that has a fallback is skipped to
//!   it, reason `quota`, before anything is sent ([`QuotaMemory::skips`]).
//!   A model or alias without a fallback still sends, so its client gets the
//!   provider's own answer and the state learns when the credit is back.
//! - **The re-check.** Once the next check has passed, the first request
//!   that asks goes to the upstream again, and only that one: the next check
//!   moves on by the interval as it is taken, so the others keep going to
//!   the fallback until it answers. A 2xx clears the state; a quota sets it
//!   again; any other answer leaves it, and the next check after the
//!   interval asks again.
//!
//! One WARN when the state is set, one INFO when it clears, nothing in
//! between. A restart forgets it; the first request after one learns it
//! again, and a refused request costs nothing.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What lmgw remembers of one upstream's exhausted quota.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exhausted {
    /// Unix seconds of the first quota answer since the state was last
    /// clear.
    pub since: i64,
    /// The provider's code (`insufficient_quota`,
    /// `credit_balance_exhausted`, …), when it sent one.
    pub code: Option<String>,
    /// The provider's message, verbatim.
    pub message: String,
    /// Unix seconds of the next check.
    pub next_check: i64,
    next_check_at: Instant,
}

/// Per upstream row id (module doc).
#[derive(Debug, Default)]
pub struct QuotaMemory(Mutex<HashMap<i64, Exhausted>>);

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `interval` from `now`, as far as an [`Instant`] reaches: an interval past
/// what the clock can add (a `Retry-After` of 10^19 s, a setting of
/// `u64::MAX`) is the farthest instant it can represent, never a panic and
/// never a shorter cap of lmgw's own.
fn instant_after(now: Instant, interval: Duration) -> Instant {
    let mut d = interval;
    loop {
        if let Some(at) = now.checked_add(d) {
            return at;
        }
        d /= 2;
    }
}

/// `interval` from now in unix seconds, saturating.
fn unix_after(interval: Duration) -> i64 {
    now_unix().saturating_add(i64::try_from(interval.as_secs()).unwrap_or(i64::MAX))
}

impl QuotaMemory {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Exhausted>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A quota answer from the upstream `id` (`name`, for the log): it is
    /// remembered, the next check `interval` from now. The first one since
    /// the state was clear logs its WARN; a re-check that met the quota
    /// again only moves the next check.
    pub fn set(&self, id: i64, name: &str, code: Option<&str>, message: &str, interval: Duration) {
        let next = instant_after(Instant::now(), interval);
        let next_check = unix_after(interval);
        let mut map = self.map();
        match map.get_mut(&id) {
            Some(e) => {
                e.code = code.map(String::from);
                e.message = message.to_string();
                e.next_check = next_check;
                e.next_check_at = next;
            }
            None => {
                tracing::warn!(
                    upstream = %name,
                    "upstream '{name}' answered {}: models with a fallback answer with it until \
                     a check succeeds (next check in {} s)",
                    code.unwrap_or("insufficient_quota"),
                    interval.as_secs()
                );
                map.insert(
                    id,
                    Exhausted {
                        since: now_unix(),
                        code: code.map(String::from),
                        message: message.to_string(),
                        next_check,
                        next_check_at: next,
                    },
                );
            }
        }
    }

    /// The upstream `id` (`name`, for the log) answered, was edited, or the
    /// owner asked to check now: the state is forgotten. `why` ends the
    /// INFO line ("a request succeeded", "the row was edited", …). Whether
    /// there was one.
    pub fn clear(&self, id: i64, name: &str, why: &str) -> bool {
        let gone = self.map().remove(&id).is_some();
        if gone {
            tracing::info!(
                upstream = %name,
                "upstream '{name}' is no longer remembered out of quota: {why}"
            );
        }
        gone
    }

    /// Whether a hop on the upstream `id` that has a fallback is skipped to
    /// it now (module doc): while the state is set and before its next
    /// check. At or after the next check, this asker is the re-check — the
    /// next check moves on by `interval` — and is not skipped.
    pub fn skips(&self, id: i64, interval: Duration) -> bool {
        let mut map = self.map();
        let Some(e) = map.get_mut(&id) else {
            return false;
        };
        let now = Instant::now();
        if now < e.next_check_at {
            return true;
        }
        e.next_check_at = instant_after(now, interval);
        e.next_check = unix_after(interval);
        false
    }

    /// [`Self::skips`] without taking a re-check that is due: whether the
    /// state is set and its next check still ahead. What resolve asks, so
    /// that a due re-check is the send's to take.
    pub fn holds(&self, id: i64) -> bool {
        self.map()
            .get(&id)
            .is_some_and(|e| Instant::now() < e.next_check_at)
    }

    /// A send on `route` answered `status`: a `2xx` from a cloud upstream,
    /// on any request class, clears what was remembered of it (§14.6). The
    /// chat send and the native `/v1/responses` forward clear at their own
    /// status check; embeddings, rerank, System One, audio, images and
    /// legacy completions call this.
    pub fn answered(&self, route: &crate::config::Route, status: reqwest::StatusCode) {
        if status.is_success() && crate::vram::classify(route).is_none() {
            self.clear(
                route.upstream.id,
                &route.upstream.name,
                "a request to it succeeded",
            );
        }
    }

    /// What is remembered of the upstream `id`, if anything.
    pub fn get(&self, id: i64) -> Option<Exhausted> {
        self.map().get(&id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_skips_until_the_check_and_then_lets_one_request_through() {
        let m = QuotaMemory::default();
        assert!(!m.skips(1, Duration::from_secs(60)), "nothing remembered");
        m.set(
            1,
            "OpenAI",
            Some("insufficient_quota"),
            "no credit",
            Duration::from_secs(60),
        );
        assert!(m.skips(1, Duration::from_secs(60)));
        assert!(!m.skips(2, Duration::from_secs(60)), "per row");

        m.set(1, "OpenAI", None, "no credit", Duration::ZERO);
        assert!(
            !m.skips(1, Duration::from_secs(60)),
            "the re-check goes through"
        );
        assert!(m.skips(1, Duration::from_secs(60)), "and only that one");
        assert!(m.get(1).is_some(), "until it answers");

        assert!(!m.holds(1) || m.skips(1, Duration::from_secs(60)));
        assert!(m.clear(1, "OpenAI", "a request succeeded"));
        assert!(!m.clear(1, "OpenAI", "again"));
        assert!(!m.skips(1, Duration::from_secs(60)));
    }

    /// No interval panics, however long (review B4): a `Retry-After` of
    /// 10^19 s, `quota_recheck_seconds = u64::MAX`, `Duration::MAX`. The
    /// state holds, and its next check is as far as the clock reaches.
    #[test]
    fn an_interval_past_the_clock_saturates_rather_than_panics() {
        for interval in [
            Duration::from_secs(10_000_000_000_000_000_000),
            Duration::from_secs(u64::MAX),
            Duration::MAX,
        ] {
            let m = QuotaMemory::default();
            m.set(1, "OpenAI", None, "no credit", interval);
            assert!(m.holds(1) && m.skips(1, interval), "{interval:?}");
            assert_eq!(m.get(1).unwrap().next_check, i64::MAX, "{interval:?}");
            m.set(1, "OpenAI", None, "no credit", Duration::ZERO);
            assert!(!m.skips(1, interval), "the re-check is taken");
            assert!(m.skips(1, interval), "and the next one is far off");
        }
    }

    /// A `Retry-After` past what a `Duration` holds reads as the longest
    /// one, never a panic (review B4); a negative one as none.
    #[test]
    fn a_huge_retry_after_reads_as_the_longest_interval() {
        let hints = |name: &'static str, value: &str| {
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(name, value.parse().unwrap());
            crate::error::RateHints::from_headers(&h)
        };
        assert_eq!(
            hints("retry-after-ms", "1e25").retry_after(),
            Some(Duration::MAX)
        );
        assert_eq!(
            hints("retry-after", "1e20").retry_after(),
            Some(Duration::MAX)
        );
        assert_eq!(
            hints("retry-after", "1.5").retry_after(),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(hints("retry-after", "-3").retry_after(), None);
        // And the memory takes it.
        let m = QuotaMemory::default();
        let long = hints("retry-after-ms", "1e25").retry_after().unwrap();
        m.set(1, "OpenAI", None, "no credit", long);
        assert!(m.holds(1));
    }

    #[test]
    fn a_quota_again_keeps_since_and_takes_the_new_words() {
        let m = QuotaMemory::default();
        m.set(1, "OpenAI", Some("a"), "first", Duration::from_secs(60));
        let since = m.get(1).unwrap().since;
        m.set(1, "OpenAI", Some("b"), "second", Duration::from_secs(60));
        let e = m.get(1).unwrap();
        assert_eq!(
            (e.since, e.code.as_deref(), e.message.as_str()),
            (since, Some("b"), "second")
        );
    }
}
