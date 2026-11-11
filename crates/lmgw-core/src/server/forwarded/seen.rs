//! What proxy mode has seen since the server started, or since the trusted
//! proxies were last saved (headless server design §4.5, D53): per main
//! listener the peer of its first request, the first from outside loopback
//! after a loopback one (the health check and the CLI come from loopback),
//! and the first from a trusted peer (so a scanner that reaches the port
//! first cannot hide the proxy's line), so the administrator can tell what
//! address a proxy reaches lmgw from — behind the outer container's pasta,
//! a connection from the host's loopback shows as the namespace's own
//! address, which a trusted CIDR cannot tell apart from anything else on
//! the host — and the last request whose forwarded headers were refused.

use std::net::IpAddr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use crate::server::Via;

/// What the record covers: from the start of the server, or from the save
/// of the trusted proxies that changed them (the desktop applies a save at
/// once, §4.7).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Since {
    #[default]
    Start,
    Save,
}

impl std::fmt::Display for Since {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Start => "since the start",
            Self::Save => "since the trusted proxies were last saved",
        })
    }
}

/// Which of a listener's lines a [`FirstPeer`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    /// The listener's first request.
    First,
    /// The first from outside loopback, after a loopback first.
    Outside,
    /// The first from a trusted peer, after lines that were not.
    Trusted,
}

/// One listener's first peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstPeer {
    pub via: Via,
    pub line: Line,
    pub since: Since,
    /// The TCP peer, canonical.
    pub peer: IpAddr,
    /// Whether the peer is the connection's own local address while not a
    /// loopback one: this machine reaching itself on one of its addresses,
    /// which is how pasta presents the host's loopback to the container.
    pub own_address: bool,
    /// Whether it was inside `trusted_proxies` then.
    pub trusted: bool,
    pub at: DateTime<Utc>,
}

impl FirstPeer {
    /// The log's and the card's line.
    pub fn line(&self) -> String {
        format!(
            "the first request{} on the {} listener {} came from {}{} ({})",
            match self.line {
                Line::First => "",
                Line::Outside => " from outside loopback",
                Line::Trusted => " from a trusted proxy",
            },
            self.via,
            self.since,
            self.peer,
            if self.own_address {
                ", this machine's own address on that connection"
            } else {
                ""
            },
            if self.trusted {
                "a trusted proxy: its forwarded headers are read"
            } else {
                "not a trusted proxy: forwarded headers from it are ignored"
            }
        )
    }

    /// The warning it makes, when it makes one: a trusted peer that is this
    /// machine's own address (§4.5's pasta caveat).
    pub fn warning(&self) -> Option<String> {
        (self.trusted && self.own_address).then(|| {
            format!(
                "the {} listener trusts {} as a proxy, and that is this machine's own address on \
                 the connection: in the server image that is how the outer container's pasta \
                 shows every connection from the host's loopback, so every process on the host \
                 that reaches lmgw's published port (and other machines, unless the port is \
                 published on the host's loopback only) can write its own client address and \
                 scheme. Run the proxy in the same pod and trust 127.0.0.1 instead, with lmgw's \
                 port unpublished; publishing it on the host's loopback only \
                 (PublishPort=127.0.0.1:…) keeps other machines out but still trusts every \
                 process on the host",
                self.via, self.peer
            )
        })
    }
}

/// The last refused request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub via: Via,
    pub peer: IpAddr,
    pub why: String,
    pub at: DateTime<Utc>,
    /// Refusals since the start.
    pub count: u64,
}

/// The record, reset at each start of the server and at each save that
/// changes the trusted proxies.
#[derive(Debug, Default)]
pub struct ProxySeen {
    inner: Mutex<Inner>,
    /// Per main listener (the plain one, the TLS one): which of its lines
    /// are taken ([`TAKEN_FIRST`], [`TAKEN_OUTSIDE`], [`TAKEN_TRUSTED`]),
    /// so a request that could make none skips the lock. Written under it.
    taken: [AtomicU8; 2],
}

/// The listener's first line is taken.
const TAKEN_FIRST: u8 = 1;
/// A line from outside loopback is taken: no "first from outside loopback"
/// is wanted any more.
const TAKEN_OUTSIDE: u8 = 2;
/// A line from a trusted peer is taken.
const TAKEN_TRUSTED: u8 = 4;

#[derive(Debug, Default)]
struct Inner {
    first: Vec<FirstPeer>,
    refused: Option<Refused>,
    since: Since,
}

impl ProxySeen {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A start of the server, or a save that changed the trusted proxies:
    /// what was seen before is forgotten.
    pub fn reset(&self, since: Since) {
        let mut g = self.lock();
        *g = Inner {
            since,
            ..Inner::default()
        };
        for t in &self.taken {
            t.store(0, Ordering::Release);
        }
    }

    /// What the record covers.
    pub fn since(&self) -> Since {
        self.lock().since
    }

    /// `via`'s bits; the device listener's requests never reach the edge
    /// and share the TLS one's.
    fn taken(&self, via: Via) -> &AtomicU8 {
        &self.taken[usize::from(via != Via::Plain)]
    }

    /// Whether a request from a peer that is (not) loopback and (not)
    /// trusted makes a line, with `taken` the listener's bits: the
    /// listener's first; the first from outside loopback; the first from a
    /// trusted peer. With every line it could make taken, none.
    fn wanted(taken: u8, loopback: bool, trusted: bool) -> Option<Line> {
        if taken & TAKEN_FIRST == 0 {
            Some(Line::First)
        } else if !loopback && taken & TAKEN_OUTSIDE == 0 {
            Some(Line::Outside)
        } else if trusted && taken & TAKEN_TRUSTED == 0 {
            Some(Line::Trusted)
        } else {
            None
        }
    }

    /// A request on `via` from `peer`, arrived on `local`: the record it
    /// makes, when it makes one ([`Line`]). At most three lines per
    /// listener per start or save; once a listener's lines that such a
    /// request could make are taken, it reads one atomic, not the lock.
    pub fn saw(&self, via: Via, peer: IpAddr, local: IpAddr, trusted: bool) -> Option<FirstPeer> {
        let loopback = peer.is_loopback();
        let bits = self.taken(via);
        Self::wanted(bits.load(Ordering::Acquire), loopback, trusted)?;
        let mut g = self.lock();
        // Again under the lock, which every write of the bits holds.
        let line = Self::wanted(bits.load(Ordering::Acquire), loopback, trusted)?;
        let seen = FirstPeer {
            via,
            line,
            since: g.since,
            peer,
            own_address: peer == local && !loopback,
            trusted,
            at: Utc::now(),
        };
        g.first.push(seen.clone());
        let mut add = TAKEN_FIRST;
        if !loopback {
            add |= TAKEN_OUTSIDE;
        }
        if trusted {
            add |= TAKEN_TRUSTED;
        }
        bits.fetch_or(add, Ordering::Release);
        Some(seen)
    }

    /// A request whose forwarded headers were refused: whether it is the
    /// first since the start (said in the log; later ones are counted on
    /// the card).
    pub fn refused(&self, via: Via, peer: IpAddr, why: &str) -> bool {
        let mut g = self.lock();
        let count = g.refused.as_ref().map_or(0, |r| r.count) + 1;
        g.refused = Some(Refused {
            via,
            peer,
            why: why.to_string(),
            at: Utc::now(),
            count,
        });
        count == 1
    }

    /// The first peers, in the order they came.
    pub fn first_peers(&self) -> Vec<FirstPeer> {
        self.lock().first.clone()
    }

    /// The last refusal.
    pub fn last_refused(&self) -> Option<Refused> {
        self.lock().refused.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(t: &str) -> IpAddr {
        t.parse().unwrap()
    }

    #[test]
    fn the_first_peer_and_the_first_from_outside_loopback_are_kept() {
        let seen = ProxySeen::default();
        let lo = ip("127.0.0.1");
        let ns = ip("192.0.2.2");
        let first = seen.saw(Via::Plain, lo, lo, false).unwrap();
        assert_eq!(first.line, Line::First);
        assert!(!first.own_address, "loopback is not the pasta case");
        assert!(seen.saw(Via::Plain, lo, lo, false).is_none());
        let proxy = seen.saw(Via::Plain, ns, ns, true).unwrap();
        assert_eq!(proxy.line, Line::Outside);
        assert!(proxy.own_address && proxy.warning().is_some());
        assert!(
            proxy.line().contains("this machine's own address"),
            "{}",
            proxy.line()
        );
        assert!(seen.saw(Via::Plain, ip("192.0.2.9"), ns, true).is_none());
        // The TLS listener has its own.
        let tls = seen.saw(Via::Tls, ip("192.0.2.9"), ns, true).unwrap();
        assert!(!tls.own_address && tls.warning().is_none());
        assert_eq!(seen.first_peers().len(), 3);
        assert!(seen.saw(Via::Tls, ip("192.0.2.10"), ns, true).is_none());
        seen.reset(Since::Start);
        assert!(seen.first_peers().is_empty());
        assert!(seen.saw(Via::Tls, ip("192.0.2.10"), ns, true).is_some());
        assert!(seen.refused(Via::Plain, ns, "both"));
        assert!(!seen.refused(Via::Plain, ns, "both again"));
        assert_eq!(seen.last_refused().unwrap().count, 2);
        // A save starts a record of its own, and says so.
        seen.reset(Since::Save);
        assert!(seen.last_refused().is_none());
        let line = seen.saw(Via::Plain, lo, lo, false).unwrap().line();
        assert!(
            line.contains("listener since the trusted proxies were last saved came from 127.0.0.1"),
            "{line}"
        );
    }

    #[test]
    fn a_scanner_first_does_not_hide_the_proxy_s_line() {
        let seen = ProxySeen::default();
        let ns = ip("192.0.2.2");
        let scanner = seen.saw(Via::Plain, ip("198.51.100.7"), ns, false).unwrap();
        assert_eq!((scanner.line, scanner.trusted), (Line::First, false));
        assert!(seen
            .saw(Via::Plain, ip("198.51.100.8"), ns, false)
            .is_none());
        let proxy = seen.saw(Via::Plain, ip("10.89.0.2"), ns, true).unwrap();
        assert_eq!(proxy.line, Line::Trusted);
        assert!(
            proxy.line().starts_with(
                "the first request from a trusted proxy on the plain listener since the start \
                 came from 10.89.0.2"
            ),
            "{}",
            proxy.line()
        );
        assert!(seen.saw(Via::Plain, ip("10.89.0.3"), ns, true).is_none());
        // A trusted loopback peer behind an untrusted loopback first (the
        // proxy in the pod after the health check, with 127.0.0.1 trusted
        // only after a save) still makes its line.
        let seen = ProxySeen::default();
        let lo = ip("127.0.0.1");
        assert!(seen.saw(Via::Tls, lo, lo, false).is_some());
        assert_eq!(
            seen.saw(Via::Tls, lo, lo, true).unwrap().line,
            Line::Trusted
        );
    }

    #[test]
    fn a_request_that_makes_no_line_reads_only_the_atomic() {
        let seen = ProxySeen::default();
        let lo = ip("127.0.0.1");
        assert!(seen.saw(Via::Plain, lo, lo, true).is_some());
        // Loopback and trusted: the first and the trusted line are taken,
        // and a loopback peer makes no "outside" line, so the bits alone
        // turn every later loopback request away before the lock.
        let bits = seen.taken(Via::Plain).load(Ordering::Acquire);
        assert_eq!(bits, TAKEN_FIRST | TAKEN_TRUSTED);
        assert_eq!(ProxySeen::wanted(bits, true, true), None);
        assert_eq!(ProxySeen::wanted(bits, true, false), None);
        assert_eq!(ProxySeen::wanted(bits, false, false), Some(Line::Outside));
        assert!(seen.saw(Via::Plain, lo, lo, true).is_none());
    }
}
