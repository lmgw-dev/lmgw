//! The device listener's connections that have not authenticated yet
//! (device link design §4.3): handshakes in progress, connections that
//! finished one without a certificate (enrolment's, while a token is
//! pending), and connections whose certificate opens no working device (no
//! row holds its key, or the row is disabled, unconfirmed or expired). Each
//! holds a descriptor while it waits, so they are bounded, from every peer
//! together (`devices.unauthenticated_max`) and from one peer
//! (`devices.unauthenticated_per_peer`, an IPv4 address or an IPv6 /64).
//!
//! **At a bound the oldest goes, not the newcomer:** a new connection from a
//! peer at its bound evicts that peer's oldest unauthenticated connection,
//! and one past the total evicts the oldest of all. A stranger idling at
//! the handshake then holds places only while it out-paces real handshakes
//! (milliseconds on a LAN), so a paired device that keeps retrying gets
//! through even from the stranger's address. A connection gives its place
//! back the moment it authenticates.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

/// How often the eviction's warning is said at most: a host that keeps
/// opening connections past a bound would otherwise write a line per
/// connection. The count in the status (`evicted`) keeps every one.
const EVICTION_WARN_EVERY: Duration = Duration::from_secs(60);

/// The places, oldest first, and the evictions since the listener started.
#[derive(Debug, Default)]
pub(super) struct PreAuth {
    places: Mutex<Places>,
    evicted: AtomicU64,
    warned: Mutex<Option<Instant>>,
}

#[derive(Debug, Default)]
struct Places {
    next: u64,
    /// By admission order: the first is the oldest.
    held: BTreeMap<u64, Held>,
    per_peer: HashMap<IpAddr, usize>,
}

#[derive(Debug)]
struct Held {
    key: IpAddr,
    /// Told when the place is taken from it.
    evict: Arc<Notify>,
}

/// Which bound an eviction made room under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Bound {
    Total,
    PerPeer,
}

impl Bound {
    fn setting(self) -> &'static str {
        match self {
            Self::Total => "devices.unauthenticated_max",
            Self::PerPeer => "devices.unauthenticated_per_peer",
        }
    }
}

/// A connection's place, held until it authenticates ([`Slot::release`]),
/// ends, or is evicted ([`Slot::evicted`]).
#[derive(Debug)]
pub(super) struct Slot {
    pre: Arc<PreAuth>,
    id: u64,
    evict: Arc<Notify>,
}

impl Slot {
    /// Give the place back: the connection authenticated (or ended).
    pub(super) fn release(&mut self) {
        let mut p = self.pre.places();
        if let Some(held) = p.held.remove(&self.id) {
            p.forget(held.key);
        }
    }

    /// Resolves when the place is taken from this connection to make room
    /// for a newer one: it is to close then.
    pub(super) fn evicted(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let evict = self.evict.clone();
        async move { evict.notified().await }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.release();
    }
}

impl Places {
    fn forget(&mut self, key: IpAddr) {
        if let Some(n) = self.per_peer.get_mut(&key) {
            *n -= 1;
            if *n == 0 {
                self.per_peer.remove(&key);
            }
        }
    }

    /// Take the place of the oldest connection `from` the peer `key` (any
    /// peer for `None`), and tell it.
    fn evict_oldest(&mut self, key: Option<IpAddr>) -> bool {
        let Some(id) = self
            .held
            .iter()
            .find(|(_, h)| key.is_none_or(|k| h.key == k))
            .map(|(id, _)| *id)
        else {
            return false;
        };
        let held = self.held.remove(&id).expect("found above");
        self.forget(held.key);
        // A permit when the connection's task has not started waiting yet.
        held.evict.notify_one();
        true
    }
}

/// The key a peer is counted under: its IPv4 address, or its IPv6 address's
/// /64 (one host's privacy addresses, one LAN segment's SLAAC). Failed
/// enrolments count under the same key (`devices::EnrolFailures`).
pub fn peer_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(a) => {
            let s = a.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
        v4 => v4,
    }
}

impl PreAuth {
    fn places(&self) -> std::sync::MutexGuard<'_, Places> {
        self.places.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A place for a connection from `peer`: at the peer's bound its oldest
    /// connection is evicted, past the total the oldest of all. Each
    /// eviction is counted, and said at most every
    /// [`EVICTION_WARN_EVERY`].
    pub(super) fn admit(self: &Arc<Self>, peer: IpAddr, max: u32, per_peer: u32) -> Slot {
        let key = peer_key(peer);
        let mut made_room = Vec::new();
        let slot = {
            let mut p = self.places();
            while p.per_peer.get(&key).copied().unwrap_or(0) >= per_peer as usize
                && p.evict_oldest(Some(key))
            {
                made_room.push(Bound::PerPeer);
            }
            while p.held.len() >= max as usize && p.evict_oldest(None) {
                made_room.push(Bound::Total);
            }
            p.next += 1;
            let id = p.next;
            let evict = Arc::new(Notify::new());
            p.held.insert(
                id,
                Held {
                    key,
                    evict: evict.clone(),
                },
            );
            *p.per_peer.entry(key).or_default() += 1;
            Slot {
                pre: self.clone(),
                id,
                evict,
            }
        };
        for bound in made_room {
            self.said(bound, peer, key, max, per_peer);
        }
        slot
    }

    fn said(&self, bound: Bound, peer: IpAddr, key: IpAddr, max: u32, per_peer: u32) {
        let n = self.evicted.fetch_add(1, Ordering::Relaxed) + 1;
        let mut warned = self.warned.lock().unwrap_or_else(|e| e.into_inner());
        if warned.is_some_and(|at| at.elapsed() < EVICTION_WARN_EVERY) {
            return;
        }
        *warned = Some(Instant::now());
        let (limit, from) = match bound {
            Bound::Total => (max, "every address together".to_string()),
            Bound::PerPeer => (per_peer, key.to_string()),
        };
        tracing::warn!(
            "device listener: {limit} connections without a working device certificate were open \
             from {from} ({} = {limit}); the oldest was closed to admit one from {peer}; {n} \
             closed so since the listener started (said at most every {} s)",
            bound.setting(),
            EVICTION_WARN_EVERY.as_secs()
        );
    }

    /// The connections that have not authenticated, now.
    pub(super) fn open(&self) -> usize {
        self.places().held.len()
    }

    /// The connections evicted since the listener started.
    pub(super) fn evicted(&self) -> u64 {
        self.evicted.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `slot` was told to close, without waiting.
    fn told(slot: &Slot) -> bool {
        futures::FutureExt::now_or_never(slot.evicted()).is_some()
    }

    #[test]
    fn at_a_bound_the_oldest_goes_and_the_newcomer_stays() {
        let pre = Arc::new(PreAuth::default());
        let a: IpAddr = "203.0.113.5".parse().unwrap();
        let b: IpAddr = "203.0.113.6".parse().unwrap();
        let first = pre.admit(a, 3, 2);
        let second = pre.admit(a, 3, 2);
        // The peer's bound: its oldest goes.
        let third = pre.admit(a, 3, 2);
        assert!(told(&first) && !told(&second) && !told(&third));
        assert_eq!((pre.open(), pre.evicted()), (2, 1));
        // The total: the oldest of all goes, whoever's it is.
        let fourth = pre.admit(b, 3, 2);
        assert_eq!(pre.open(), 3);
        let fifth = pre.admit(b, 3, 2);
        assert!(told(&second), "the oldest of all");
        assert!(!told(&third) && !told(&fourth) && !told(&fifth));
        assert_eq!((pre.open(), pre.evicted()), (3, 2));
        // An evicted slot dropped later frees nothing a second time.
        drop(first);
        drop(second);
        assert_eq!(pre.open(), 3);
        // Authenticated: its place is free, and nobody is evicted for it.
        let mut third = third;
        third.release();
        let _sixth = pre.admit(a, 3, 2);
        assert!(!told(&fourth) && !told(&fifth));
        assert_eq!((pre.open(), pre.evicted()), (3, 2));
    }

    #[test]
    fn an_ipv6_peer_counts_by_its_64() {
        let pre = Arc::new(PreAuth::default());
        let one: IpAddr = "2001:db8:1:2::a".parse().unwrap();
        let other_host: IpAddr = "2001:db8:1:2:ffff::b".parse().unwrap();
        let other_net: IpAddr = "2001:db8:1:3::a".parse().unwrap();
        let a = pre.admit(one, 10, 1);
        let b = pre.admit(other_host, 10, 1);
        assert!(told(&a), "the same /64: its oldest makes room");
        let c = pre.admit(other_net, 10, 1);
        assert!(!told(&b) && !told(&c));
        let mapped: IpAddr = "::ffff:203.0.113.5".parse().unwrap();
        assert_eq!(peer_key(mapped), "203.0.113.5".parse::<IpAddr>().unwrap());
    }
}
