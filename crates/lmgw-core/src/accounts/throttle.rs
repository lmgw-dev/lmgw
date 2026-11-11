//! The sign-in throttle (headless server design §5.4, D14; ruling 16).
//!
//! Keys, in memory, each a doubling delay after each failure from
//! [`LOGIN_BACKOFF_START`] up to [`LOGIN_BACKOFF_MAX`]: the pair (the name
//! with the client's address, or with the known-browser token it carries),
//! the name across all addresses, and the address across all names (an IPv6
//! client counts on its /64 and on its /56). A success clears the pair's
//! entry and the name's. A browser that signed in to the account before (a
//! known browser) is held by its own pair's delay only, and that pair is
//! keyed by its token, so a spray from the same address does not hold it.
//! No hard lockout: a lock after N failures would let anyone who knows a
//! sign-in name lock its owner out.
//!
//! **An attempt is counted before it runs** ([`Throttle::reserve`]): the
//! check and the count happen under one lock, so the second of two
//! concurrent attempts finds the first one's delay open and is refused, as
//! it would be had it come a moment later. A good sign-in clears what it
//! counted; an attempt that never reached a verification (no permit free,
//! the store failing) is undone.
//!
//! Names are keyed by the sha256 of the name as presented, folded as logins
//! are ([`name_key`]), whether or not it is one (D13): the map never holds a
//! presented name, and a 2 MiB one costs a hash.
//!
//! The map's size has a real bound: an entry outlives its attempt only when
//! the attempt reached a verification (an unknown name verifies too, D13),
//! and is dropped once [`LOGIN_BACKOFF_MAX`] has passed since its delay ran
//! out, so it holds at most four entries per verification of the last
//! 2 × [`LOGIN_BACKOFF_MAX`], which the verification permits bound (§5.10).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The first delay after a failure (§5.4).
pub const LOGIN_BACKOFF_START: Duration = Duration::from_secs(1);
/// The longest delay (§5.4).
pub const LOGIN_BACKOFF_MAX: Duration = Duration::from_secs(15 * 60);

/// The address keys of a client: an IPv4 address (or an IPv4-mapped one) as
/// itself; an IPv6 address as its /64 (one host usually holds a whole /64)
/// and its /56 (one site usually holds a /56, so a host that walks its /64s
/// still counts as one), the /64 first.
pub fn address_keys(ip: IpAddr) -> Vec<String> {
    match ip {
        IpAddr::V4(v4) => vec![v4.to_string()],
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => vec![v4.to_string()],
            None => {
                let s = v6.segments();
                vec![
                    format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3]),
                    format!("{:x}:{:x}:{:x}:{:x}::/56", s[0], s[1], s[2], s[3] & 0xff00),
                ]
            }
        },
    }
}

/// The throttle's key for a presented sign-in name: the sha256 (hex) of the
/// name trimmed and in ASCII lowercase, as [`super::normalize_login`] folds
/// it, whether or not it passes that check (D13).
pub fn name_key(presented: &str) -> String {
    crate::config::hash_api_key(&presented.trim().to_ascii_lowercase())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Pair(String, String),
    Name(String),
    Address(String),
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    failures: u32,
    last: Instant,
    /// A refusal under the delay since `last` was answered: the audit log
    /// writes one entry per hold (D91), at its first refusal.
    noted: bool,
}

impl Entry {
    fn delay(&self) -> Duration {
        let shift = self.failures.saturating_sub(1).min(20);
        (LOGIN_BACKOFF_START * (1u32 << shift)).min(LOGIN_BACKOFF_MAX)
    }

    fn wait(&self, now: Instant) -> Duration {
        (self.last + self.delay()).saturating_duration_since(now)
    }

    /// Whether the entry still says anything: its delay plus
    /// [`LOGIN_BACKOFF_MAX`] since its last failure. A capped entry kept
    /// that long holds the next failure at the cap rather than at 1 s.
    fn live(&self, now: Instant) -> bool {
        now < self.last + self.delay() + LOGIN_BACKOFF_MAX
    }
}

/// One attempt's keys.
#[derive(Debug, Clone)]
pub struct Attempt {
    /// [`name_key`] of the presented name.
    name: String,
    /// The pair's other half: the client's first address key, or the
    /// known-browser token's hash.
    pair: String,
    /// The client's address keys ([`address_keys`]); none for a check that
    /// is not a sign-in (the current password, [`Self::current_password`]).
    addresses: Vec<String>,
    /// A known browser of the account (§5.4): held by its pair only.
    known: bool,
}

impl Attempt {
    /// A password sign-in for the name `presented` from a client with
    /// `addresses`, carrying the hash of a known-browser token of the
    /// account it names, if it does.
    pub fn sign_in(
        presented: &str,
        addresses: Vec<String>,
        known_token_hash: Option<&str>,
    ) -> Self {
        let pair = match known_token_hash {
            Some(h) => format!("known:{h}"),
            None => addresses.first().cloned().unwrap_or_default(),
        };
        Self {
            name: name_key(presented),
            pair,
            addresses,
            known: known_token_hash.is_some(),
        }
    }

    /// The current-password check of `account_password_set` for oneself
    /// (§5.2): the account's name, paired with the session that asks.
    pub fn current_password(login: &str, session: i64) -> Self {
        Self {
            name: name_key(login),
            pair: format!("session:{session}"),
            addresses: Vec::new(),
            known: false,
        }
    }

    /// The name's key, for a log line that must not carry the name.
    pub fn name_key(&self) -> &str {
        &self.name
    }

    fn keys(&self) -> Vec<Key> {
        let mut keys = vec![
            Key::Pair(self.name.clone(), self.pair.clone()),
            Key::Name(self.name.clone()),
        ];
        keys.extend(self.addresses.iter().cloned().map(Key::Address));
        keys
    }
}

/// An attempt counted up front ([`Throttle::reserve`]). Dropped as it is,
/// it stays counted: a failure. [`Throttle::succeeded`] and
/// [`Throttle::undo`] take it back.
#[must_use = "a reservation is a counted failure until it is taken back"]
#[derive(Debug)]
pub struct Reservation {
    /// Each key with its entry's `last` before the reservation (`None`: the
    /// reservation made it).
    keys: Vec<(Key, Option<Instant>)>,
    at: Instant,
}

/// The throttle.
#[derive(Default)]
pub struct Throttle {
    entries: Mutex<HashMap<Key, Entry>>,
}

/// Why an attempt waits: which key holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    Name,
    Address,
}

impl Throttle {
    /// Check `attempt` against its keys' delays and, when none holds it,
    /// count it on every key at once, under one lock: the reservation, or
    /// how long it has to wait and what holds it.
    pub fn reserve(&self, attempt: &Attempt) -> Result<Reservation, (Duration, Held)> {
        self.reserve_at(attempt, Instant::now())
    }

    /// [`Self::reserve`], and with a refusal whether it is the first one
    /// the delay that holds it answers: the audit log's one entry per hold
    /// rather than one per `429` (D91).
    pub fn reserve_noting(&self, attempt: &Attempt) -> Result<Reservation, (Duration, Held, bool)> {
        self.reserve_inner(attempt, Instant::now())
    }

    fn reserve_at(&self, a: &Attempt, now: Instant) -> Result<Reservation, (Duration, Held)> {
        self.reserve_inner(a, now).map_err(|(w, h, _)| (w, h))
    }

    fn reserve_inner(
        &self,
        a: &Attempt,
        now: Instant,
    ) -> Result<Reservation, (Duration, Held, bool)> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, e| e.live(now));
        let keys = a.keys();
        let mut longest: Option<(Duration, Held, &Key)> = None;
        for k in &keys {
            let held = match k {
                Key::Pair(..) => Held::Name,
                // A known browser is held by its own pair only (§5.4).
                Key::Name(_) | Key::Address(_) if a.known => continue,
                Key::Name(_) => Held::Name,
                Key::Address(_) => Held::Address,
            };
            let wait = entries.get(k).map_or(Duration::ZERO, |e| e.wait(now));
            if !wait.is_zero() && longest.is_none_or(|(w, _, _)| wait > w) {
                longest = Some((wait, held, k));
            }
        }
        if let Some((wait, held, k)) = longest {
            let first = entries
                .get_mut(k)
                .is_some_and(|e| !std::mem::replace(&mut e.noted, true));
            return Err((wait, held, first));
        }
        let mut reserved = Vec::with_capacity(keys.len());
        for k in keys {
            let prior = entries.get(&k).map(|e| e.last);
            let e = entries.entry(k.clone()).or_insert(Entry {
                failures: 0,
                last: now,
                noted: false,
            });
            e.failures = e.failures.saturating_add(1);
            e.last = now;
            e.noted = false;
            reserved.push((k, prior));
        }
        Ok(Reservation {
            keys: reserved,
            at: now,
        })
    }

    /// Take a reservation back on the keys `pick` answers `true` for: one
    /// failure less, the entry's `last` as it was when nothing counted on
    /// it since, and gone at zero.
    fn take_back(entries: &mut HashMap<Key, Entry>, r: &Reservation, pick: impl Fn(&Key) -> bool) {
        for (k, prior) in &r.keys {
            if !pick(k) {
                continue;
            }
            let Some(e) = entries.get_mut(k) else {
                continue;
            };
            e.failures = e.failures.saturating_sub(1);
            if e.failures == 0 {
                entries.remove(k);
            } else if e.last == r.at {
                if let Some(prior) = prior {
                    e.last = *prior;
                }
            }
        }
    }

    /// An attempt that never reached a verification: not counted after all.
    pub fn undo(&self, r: Reservation) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        Self::take_back(&mut entries, &r, |_| true);
    }

    /// A good sign-in: the pair's entry and the name's go, and the address
    /// keys lose what this attempt counted on them.
    pub fn succeeded(&self, r: Reservation) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        for (k, _) in &r.keys {
            if matches!(k, Key::Pair(..) | Key::Name(_)) {
                entries.remove(k);
            }
        }
        Self::take_back(&mut entries, &r, |k| matches!(k, Key::Address(_)));
    }

    /// The entries now, for the Accounts card.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Everything goes (the desktop's switch off, §5.11).
    pub fn clear(&self) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

/// `429 login_throttled` with its sentence (D14); the route adds
/// `Retry-After`.
pub fn throttled(wait: Duration, held: Held) -> crate::principal::Refusal {
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    let what = match held {
        Held::Name => "this name",
        Held::Address => "this address",
    };
    crate::principal::Refusal {
        status: axum::http::StatusCode::TOO_MANY_REQUESTS,
        code: "login_throttled",
        message: format!(
            "too many failed sign-ins for {what}; try again in {secs} s (the wait doubles after \
             each failure, from {} s up to {} min)",
            LOGIN_BACKOFF_START.as_secs(),
            LOGIN_BACKOFF_MAX.as_secs() / 60
        ),
    }
}

/// The whole seconds `wait` rounds up to: `Retry-After`'s value.
pub fn retry_after_secs(wait: Duration) -> u64 {
    (wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1)
}

#[cfg(test)]
mod tests;
