//! Pairing tokens (device link design §3.3, D8): one-time, short-lived,
//! kept as their SHA-256 only, and their failed uses counted per address.
//!
//! - **The token**: `lmgw-enrol-` and 32 lowercase hex digits from the OS's
//!   random source ([`mint`]), shown once in the pairing link, stored as
//!   [`hash`]. One per device row; a new one replaces it.
//! - **Its life** is `devices.token_minutes` from its minting, or until the
//!   owner ends it. Used once by one key, it stays alive for that key's
//!   repeat until then (§3.3's note of 2026-10-15), or until the device
//!   makes its first request with the certificate it was answered, which
//!   proves it holds the answer ([`answered`], D8's note of 2026-10-15). The
//!   handshake's client certificate stays optional while any token is alive
//!   ([`PendingTokens`](super::PendingTokens), loaded by [`sync`]).
//! - **Failed uses** (an unknown, used, lapsed or ended token) are counted
//!   per address, an IPv6 address by its /64, in a sliding minute
//!   ([`EnrolFailures`]). Past `devices.enrol_refuse_per_min` an address is
//!   answered 429 until its window frees: far above any device's retries, so
//!   the phones behind one masquerading WireGuard peer are never refused,
//!   and 128 random bits need no lockout. The count is there so the owner
//!   sees someone trying. Its memory is the addresses of the last minute
//!   and a count per minute of the last hour, whatever the rate.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use lmgw_api_types::device_link::{DevicePairingStatus, TOKEN_PREFIX};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::state::AppState;

/// A new pairing token: [`TOKEN_PREFIX`] and 32 lowercase hex digits (128
/// random bits).
pub fn mint() -> String {
    let bytes: [u8; 16] = rand::random();
    format!("{TOKEN_PREFIX}{}", hex::encode(bytes))
}

/// What the store keeps of a token: its lowercase hex SHA-256. A token is
/// hashed as presented; lmgw writes its digits lowercase.
pub fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// The handshake's pending tokens from the store's (`store::alive_tokens`):
/// what a start and the CA's rotation, which ends every token at once,
/// leave [`PendingTokens`](super::PendingTokens) holding. Every other write
/// says its own token's change as it commits; a whole read racing them could
/// bring back a token just ended or drop one just minted.
pub async fn sync(state: &AppState) {
    match crate::store::alive_tokens(&state.db, Utc::now()).await {
        Ok(alive) => state.devices.pending.replace(alive.into_iter().map(|t| {
            // The serial the gate compares a certificate's with, read
            // as it reads them.
            let serial = t.answered.as_deref().and_then(|b64| {
                use base64::Engine;
                let der = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
                crate::server::DevicePeer::read(&der).ok().map(|p| p.serial)
            });
            (t.key_id, t.expires_at, serial)
        })),
        // The set keeps what it held: a token it misses makes a handshake
        // require a certificate (the enrolment's device meets
        // `certificate_required` and asks again), one it keeps lapses at its
        // expiry.
        Err(e) => tracing::warn!("the pending pairing tokens could not be read: {e}"),
    }
}

/// Device `key_id`'s request came with certificate `serial` and was let in:
/// when its pairing token answered that certificate, the token ends now (D8's
/// note of 2026-10-15). The device holds its answer, so no repeat is needed,
/// and the handshake without a certificate does not stay open for the rest
/// of the token's life. Confirm alone does not end it: a device whose answer
/// got lost has made no request with it. The store's write runs on its own
/// task, so the request does not wait for it.
pub fn answered(state: &crate::state::SharedState, key_id: i64, serial: &str) {
    if !state.devices.pending.take_answered(key_id, serial) {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        match crate::store::end_answered_token(&state.db, key_id, Utc::now()).await {
            Ok(true) => {
                tracing::info!(
                    "device {key_id} made its first request with the certificate its pairing \
                     token answered: the token ends"
                );
                state.telemetry.keys(key_id, "token");
            }
            Ok(false) => {}
            // The handshake set has let it go already, so no connection
            // comes without a certificate for it; the Keys list shows the
            // token alive until it lapses.
            Err(e) => {
                tracing::warn!("device {key_id}'s answered pairing token could not be ended: {e}")
            }
        }
    });
}

/// How long one failure counts toward the refusal: the setting is per
/// minute.
const WINDOW: Duration = Duration::from_secs(60);

/// The status's "last hour", in whole minutes: one counter each.
const MINUTES: usize = 60;

/// Failed enrolments, per address in a sliding minute, and per minute of
/// the last hour for the status. In memory: they start at zero with each
/// start, which the status says (`counted_since`).
///
/// The memory holds the same facts whatever the rate: the addresses whose
/// window holds a failure, swept once the map has doubled since its last
/// sweep, and [`MINUTES`] counters with the last failure's address and
/// time. No count is capped.
#[derive(Debug)]
pub struct EnrolFailures {
    inner: Mutex<Failures>,
    since: DateTime<Utc>,
    /// Where the minutes are counted from.
    epoch: Instant,
}

impl Default for EnrolFailures {
    fn default() -> Self {
        Self {
            inner: Mutex::default(),
            since: Utc::now(),
            epoch: Instant::now(),
        }
    }
}

#[derive(Debug)]
struct Failures {
    /// Per address, the failures of its last minute, oldest first. An
    /// address's entry goes once its window holds nothing: when it asks
    /// again, or at the next sweep.
    window: HashMap<IpAddr, VecDeque<Instant>>,
    /// How many addresses the last sweep kept: the next comes once the map
    /// has grown to twice that, so a sweep's cost is paid by the entries
    /// added since.
    swept: usize,
    /// The failures of each of the last [`MINUTES`] minutes, by the minute's
    /// number since the epoch modulo [`MINUTES`]: `(minute, count)`.
    minutes: [(u64, u64); MINUTES],
    /// The last failure: when, its address, and its wall-clock time.
    last: Option<(Instant, IpAddr, DateTime<Utc>)>,
}

impl Default for Failures {
    fn default() -> Self {
        Self {
            window: HashMap::new(),
            swept: 0,
            minutes: [(0, 0); MINUTES],
            last: None,
        }
    }
}

/// An address past the refusal threshold: how long until its window frees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimited {
    pub retry_after: Duration,
}

impl EnrolFailures {
    fn lock(&self) -> std::sync::MutexGuard<'_, Failures> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The minute `now` falls in, counted from the epoch.
    fn minute(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.epoch).as_secs() / 60
    }

    /// Whether `peer` may try now with `per_min` failures a minute allowed
    /// (`devices.enrol_refuse_per_min`), at `now`.
    pub fn check(&self, peer: IpAddr, per_min: u32, now: Instant) -> Result<(), RateLimited> {
        let key = crate::server::peer_key(peer);
        let mut f = self.lock();
        let Some(window) = f.window.get_mut(&key) else {
            return Ok(());
        };
        slide(window, now);
        if window.is_empty() {
            f.window.remove(&key);
            return Ok(());
        }
        if window.len() < per_min as usize {
            return Ok(());
        }
        // Free again once enough of the window has passed to fall below
        // the threshold: the oldest failure that must go.
        let must_go = window.len() + 1 - per_min as usize;
        let frees = window[must_go - 1] + WINDOW;
        Err(RateLimited {
            retry_after: frees
                .saturating_duration_since(now)
                .max(Duration::from_secs(1)),
        })
    }

    /// One failure from `peer` at `now` (`wall` on the wall clock), logged
    /// with the address, never the token.
    pub fn count(&self, peer: IpAddr, what: &str, now: Instant, wall: DateTime<Utc>) {
        let key = crate::server::peer_key(peer);
        let minute = self.minute(now);
        let mut f = self.lock();
        let window = f.window.entry(key).or_default();
        slide(window, now);
        window.push_back(now);
        if f.window.len() > f.swept.saturating_mul(2) {
            // Addresses that failed once and never came back: their windows
            // hold nothing any more.
            f.window
                .retain(|_, w| w.back().is_some_and(|at| now.duration_since(*at) < WINDOW));
            f.swept = f.window.len();
        }
        let slot = &mut f.minutes[(minute % MINUTES as u64) as usize];
        if slot.0 != minute {
            *slot = (minute, 0);
        }
        slot.1 += 1;
        f.last = Some((now, key, wall));
        tracing::info!("device enrolment from {peer}: {what}");
    }

    /// The status's pairing block, with the tokens alive (`tokens_alive`).
    pub fn status(&self, tokens_alive: u64, now: Instant) -> DevicePairingStatus {
        let minute = self.minute(now);
        let f = self.lock();
        let failures_last_hour = f
            .minutes
            .iter()
            .filter(|(m, _)| *m <= minute && minute - m < MINUTES as u64)
            .map(|(_, n)| n)
            .sum();
        let last = f
            .last
            .filter(|(at, _, _)| self.minute(*at) + (MINUTES as u64) > minute);
        DevicePairingStatus {
            tokens_alive,
            failures_last_hour,
            last_failure_from: last.map(|(_, ip, _)| ip.to_string()),
            last_failure_at: last.map(|(_, _, at)| crate::store::stamp(at)),
            counted_since: crate::store::stamp(self.since),
        }
    }

    /// How many addresses the sliding windows hold now.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.lock().window.len()
    }
}

/// `window` without the failures that left it by `now`.
fn slide(window: &mut VecDeque<Instant>, now: Instant) {
    while window
        .front()
        .is_some_and(|at| now.duration_since(*at) >= WINDOW)
    {
        window.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_says_what_it_is_and_is_kept_as_its_hash() {
        let t = mint();
        assert!(t.starts_with("lmgw-enrol-"), "{t}");
        assert_eq!(t.len(), TOKEN_PREFIX.len() + 32);
        assert!(t[TOKEN_PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_ne!(t, mint(), "128 random bits, every time");
        assert_eq!(hash(&t).len(), 64);
        assert_ne!(hash(&t), hash(&mint()));
    }

    /// The threshold counts one IPv6 /64 as one address, and frees as the
    /// window slides, never on a guessed lockout.
    #[test]
    fn failures_count_per_address_in_a_sliding_minute() {
        let f = EnrolFailures::default();
        let t0 = Instant::now();
        let wall = Utc::now();
        let a: IpAddr = "2001:db8:1:2::a".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::b".parse().unwrap();
        let other: IpAddr = "203.0.113.5".parse().unwrap();
        for i in 0..3 {
            let who = if i % 2 == 0 { a } else { b };
            assert!(f.check(who, 3, t0).is_ok());
            f.count(who, "unknown token", t0 + Duration::from_secs(i), wall);
        }
        let refused = f.check(b, 3, t0 + Duration::from_secs(10)).unwrap_err();
        assert_eq!(refused.retry_after, Duration::from_secs(50));
        assert!(f.check(other, 3, t0).is_ok(), "another address");
        // The first failure leaves the window at 60 s: below the threshold.
        assert!(f.check(a, 3, t0 + Duration::from_secs(60)).is_ok());
        let s = f.status(2, t0 + Duration::from_secs(61));
        assert_eq!((s.tokens_alive, s.failures_last_hour), (2, 3));
        assert_eq!(s.last_failure_from.as_deref(), Some("2001:db8:1:2::"));
        // An hour on, the status shows none.
        let s = f.status(0, t0 + Duration::from_secs(3_700));
        assert_eq!((s.failures_last_hour, s.last_failure_from), (0, None));
    }

    /// The last hour is counted per minute, every failure of it, and a
    /// minute's counter starts again when the hour comes round.
    #[test]
    fn the_last_hour_counts_every_failure_by_the_minute() {
        let f = EnrolFailures::default();
        let t0 = Instant::now();
        let wall = Utc::now();
        // 192.0.2.0 plus i: the first 256 sit in the documentation block.
        let ip = |i: u32| IpAddr::from(std::net::Ipv4Addr::from(0xc000_0200 + i));
        for i in 0..5_000u32 {
            f.count(
                ip(i),
                "unknown token",
                t0 + Duration::from_millis(i.into()),
                wall,
            );
        }
        f.count(
            ip(1),
            "unknown token",
            t0 + Duration::from_secs(1_800),
            wall,
        );
        let s = f.status(0, t0 + Duration::from_secs(1_801));
        assert_eq!(s.failures_last_hour, 5_001, "none capped");
        assert_eq!(s.last_failure_from.as_deref(), Some("192.0.2.1"));
        // An hour after the first minute, only the later one is left; the
        // first minute's counter starts again from one.
        let s = f.status(0, t0 + Duration::from_secs(3_600));
        assert_eq!(s.failures_last_hour, 1);
        f.count(
            ip(2),
            "unknown token",
            t0 + Duration::from_secs(3_600),
            wall,
        );
        let s = f.status(0, t0 + Duration::from_secs(3_601));
        assert_eq!(s.failures_last_hour, 2);
        assert_eq!(s.last_failure_from.as_deref(), Some("192.0.2.2"));
    }

    /// Addresses that failed and never came back are swept once the map
    /// has doubled since the last sweep, and the ones still in their window
    /// stay.
    #[test]
    fn addresses_whose_window_emptied_are_swept() {
        let f = EnrolFailures::default();
        let t0 = Instant::now();
        let wall = Utc::now();
        // 192.0.2.0 plus i: the first 256 sit in the documentation block.
        let ip = |i: u32| IpAddr::from(std::net::Ipv4Addr::from(0xc000_0200 + i));
        for i in 0..1_000 {
            f.count(ip(i), "unknown token", t0, wall);
        }
        assert_eq!(f.tracked(), 1_000);
        let recent = ip(5_000);
        f.count(recent, "unknown token", t0 + Duration::from_secs(30), wall);
        // A minute on, the first thousand's windows hold nothing; new
        // addresses arrive until the map has doubled and is swept.
        let later = t0 + Duration::from_secs(61);
        let mut added = 0;
        while f.tracked() > added + 1 {
            assert!(added <= 1_001, "no sweep after {added} new addresses");
            f.count(ip(10_000 + added as u32), "unknown token", later, wall);
            added += 1;
        }
        assert_eq!(f.tracked(), added + 1, "the new ones and the recent one");
        assert!(f.check(recent, 1, later).is_err(), "still in its window");
        assert!(f.check(ip(0), 1, later).is_ok());
    }
}
