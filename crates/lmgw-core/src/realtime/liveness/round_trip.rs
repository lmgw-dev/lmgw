//! The liveness ping's round trip (realtime design §6.4, B3 review 2): how
//! long the client's network takes there and back, measured on the pings the
//! session sends anyway.
//!
//! The barge-in window is judged on the server's clocks: when the writer
//! released the answer's audio, and when each append arrived. The client
//! plays the audio a transit later, and its microphone's samples arrive a
//! transit after they were recorded — the two delays add, they do not
//! cancel — so the end used for window membership is pushed back by this
//! round trip plus `echo_tail_ms` for what it cannot see (the client's
//! audio buffers, the room).
//!
//! The writer notes when a ping is **sent** ([`RoundTrip::pinged`]) — before
//! the send, so a pong that comes back while the send is still being
//! flushed is not lost, and the stale stamp then completed by the next pong
//! (E3). The socket's reader notes when its pong **arrived** — stamped as it
//! came off the socket, not when the session got round to it
//! ([`RoundTrip::ponged`]). **A pong answers the ping whose payload it
//! echoes** (RFC 6455 has the peer echo the ping's application data): each
//! ping carries a number of its own, and only the pong that carries the
//! outstanding one measures (B4 review). A ping sent while the one before
//! is still unanswered used to overwrite its stamp, and that one's late
//! pong then measured a round trip of almost nothing. A pong for an older
//! ping, and an unsolicited one (RFC 6455 allows them as heartbeats),
//! measure nothing.
//!
//! **What the margin uses** (E3): the least of the last [`KEPT`] round trips.
//! A longer one is queueing — a WiFi spike, a slow first pong, a client busy
//! elsewhere — and the window does not move by it; the network's own transit
//! is the least it takes. A sample longer than the ping interval is no round
//! trip at all (an unsolicited pong after a ping that got none) and is
//! dropped. A session with pings off (`realtime.ping_interval_s` 0) never
//! has one, and the margin is `echo_tail_ms` alone.
//!
//! **A client that never echoes** a ping's payload never gets a round trip
//! either, and its margin is `echo_tail_ms` alone too — which the session
//! says once, at INFO, when [`SILENT_PINGS`] pings went out and none
//! measured one ([`RoundTrip::unmeasured_once`], fix package B6).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// How many round trips the margin's minimum is taken over (module doc):
/// 100 s of pings at the default 20 s interval — enough to ride out a
/// spike, few enough to follow a network that really got slower.
pub(crate) const KEPT: usize = 5;

/// How many pings go out with no round trip measured before the session
/// says so (module doc): the first, on the client's first frame, and two
/// whole ping intervals.
pub(crate) const SILENT_PINGS: u64 = 3;

/// The session's ping round trip, shared by the writer and the reader.
#[derive(Debug, Default)]
pub(crate) struct RoundTrip {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// The unanswered ping: its number, and when it was sent.
    sent: Option<(u64, Instant)>,
    /// The last ping's number.
    pings: u64,
    /// The last round trips measured, oldest first.
    kept: VecDeque<Duration>,
    /// A round trip this long or longer is dropped: the ping interval.
    bound: Option<Duration>,
    /// That no round trip was measured has been said.
    said: bool,
}

impl RoundTrip {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Plain fields: a panic while it was held cannot leave them half
        // written.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Round trips of `bound` or longer are no round trips (module doc):
    /// the session's ping interval.
    pub fn bound(&self, bound: Duration) {
        self.lock().bound = Some(bound);
    }

    /// A ping is sent at `at`: the payload it carries (module doc).
    pub fn pinged(&self, at: Instant) -> [u8; 8] {
        let mut inner = self.lock();
        inner.pings += 1;
        let id = inner.pings;
        inner.sent = Some((id, at));
        id.to_be_bytes()
    }

    /// A pong carrying `payload` arrived at `at`: the answer to the
    /// outstanding ping when it echoes that one's payload — kept unless it
    /// is longer than the bound.
    pub fn ponged(&self, at: Instant, payload: &[u8]) {
        let mut inner = self.lock();
        let Some((id, sent)) = inner.sent else {
            return;
        };
        if payload != id.to_be_bytes() {
            return;
        }
        inner.sent = None;
        let rtt = at.saturating_duration_since(sent);
        if inner.bound.is_some_and(|b| rtt >= b) {
            return;
        }
        if inner.kept.len() == KEPT {
            inner.kept.pop_front();
        }
        inner.kept.push_back(rtt);
    }

    /// The least of the last round trips, if any ping was answered yet.
    pub fn get(&self) -> Option<Duration> {
        self.lock().kept.iter().min().copied()
    }

    /// The pings sent, once — the first time [`SILENT_PINGS`] or more went
    /// out and none measured a round trip (module doc); `None` otherwise.
    pub fn unmeasured_once(&self) -> Option<u64> {
        let mut inner = self.lock();
        if inner.said || inner.pings < SILENT_PINGS || !inner.kept.is_empty() {
            return None;
        }
        inner.said = true;
        Some(inner.pings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pong_answers_the_ping_it_echoes_and_an_unsolicited_one_measures_nothing() {
        let rt = RoundTrip::default();
        let t0 = Instant::now();
        rt.ponged(t0, &[]);
        assert_eq!(rt.get(), None, "no ping out");
        let p = rt.pinged(t0);
        rt.ponged(t0 + Duration::from_millis(1), &[]);
        assert_eq!(rt.get(), None, "a heartbeat pong answers no ping");
        rt.ponged(t0 + Duration::from_millis(42), &p);
        assert_eq!(rt.get(), Some(Duration::from_millis(42)));
        // A second pong for it: the measurement stands.
        rt.ponged(t0 + Duration::from_secs(5), &p);
        assert_eq!(rt.get(), Some(Duration::from_millis(42)));
        let p = rt.pinged(t0 + Duration::from_secs(20));
        rt.ponged(t0 + Duration::from_millis(20_007), &p);
        assert_eq!(rt.get(), Some(Duration::from_millis(7)));
    }

    #[test]
    fn a_client_that_never_echoes_is_said_once() {
        let rt = RoundTrip::default();
        let t0 = Instant::now();
        for k in 0..SILENT_PINGS {
            assert_eq!(rt.unmeasured_once(), None, "{k} pings");
            rt.pinged(t0 + Duration::from_secs(20 * k));
            rt.ponged(t0 + Duration::from_secs(20 * k + 1), &[]);
        }
        assert_eq!(rt.unmeasured_once(), Some(SILENT_PINGS));
        assert_eq!(rt.unmeasured_once(), None, "once");
        // One that echoes: nothing to say.
        let rt = RoundTrip::default();
        for k in 0..SILENT_PINGS {
            let p = rt.pinged(t0 + Duration::from_secs(20 * k));
            rt.ponged(t0 + Duration::from_millis(20_000 * k + 30), &p);
        }
        assert_eq!(rt.unmeasured_once(), None);
    }

    #[test]
    fn a_late_pong_of_a_ping_sent_over_measures_nothing() {
        // B4 review: a ping still unanswered when the next goes out. Its
        // late pong came right after the new stamp and measured ~0 ms.
        let rt = RoundTrip::default();
        let t0 = Instant::now();
        let old = rt.pinged(t0);
        let new = rt.pinged(t0 + Duration::from_secs(20));
        assert_ne!(old, new);
        rt.ponged(t0 + Duration::from_millis(20_001), &old);
        assert_eq!(rt.get(), None, "the old ping's pong");
        rt.ponged(t0 + Duration::from_millis(20_060), &new);
        assert_eq!(rt.get(), Some(Duration::from_millis(60)));
    }

    #[test]
    fn a_spike_does_not_move_the_margin_and_an_overlong_one_is_dropped() {
        // E3: the least of the last few is the network's transit.
        let rt = RoundTrip::default();
        rt.bound(Duration::from_secs(20));
        let t0 = Instant::now();
        let ping = |at_s: u64, rtt_ms: u64| {
            let at = t0 + Duration::from_secs(at_s);
            let p = rt.pinged(at);
            rt.ponged(at + Duration::from_millis(rtt_ms), &p);
        };
        ping(0, 40);
        ping(20, 900);
        assert_eq!(rt.get(), Some(Duration::from_millis(40)), "a spike");
        // A pong 25 s after a ping that got none: not a round trip.
        ping(40, 25_000);
        assert_eq!(rt.get(), Some(Duration::from_millis(40)));
        // The network really got slower: once the fast one is out of the
        // last KEPT, the margin follows.
        for k in 0..KEPT as u64 {
            ping(60 + 20 * k, 120);
        }
        assert_eq!(rt.get(), Some(Duration::from_millis(120)));
    }
}
