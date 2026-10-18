//! A call's liveness (calls design §8.1): no pings, two facts.
//!
//! On a WebSocket one pong proves two things: the client is there, and it
//! reads — the pong travels behind the session's own output, so a client
//! that stopped reading never answers. A call has no such answer. The
//! phone's ICE agent sends consent checks, its microphone RTP and its stack
//! RTCP whatever its app does, so its traffic proves it is there and
//! nothing of its reading `oai-events`; taken as a pong it would lift the
//! stalled-output bound, and a phone with its mic and ICE up that stopped
//! taking events could hold a close's flush, its binding and its slot for
//! good. So a call's verdict reads the two facts apart, both kept by the
//! call's driver in the registry (`calls::Evidence`):
//!
//! - **alive**: the client's last authenticated traffic
//!   (`CallInfo::last_heard`: a consent check whose integrity the driver
//!   checked, or a datagram str0m made media, channel data, a channel's
//!   event, a sender report or the connection of). One whole
//!   `realtime.ping_interval_s` with none is the end, as an unanswered ping
//!   is a WebSocket's;
//! - **reading**: while the session's output waits on `oai-events` — str0m
//!   counts written data as buffered until the client acknowledges it —
//!   SCTP acknowledging some of it (`CallInfo::unacked_since`, moved by
//!   each acknowledgement that takes some off). One whole interval of output
//!   in flight with no acknowledgement is the stalled verdict, the
//!   WebSocket's "took nothing for a whole interval while events waited",
//!   in the session loop and in `flush_alive` alike. With nothing in flight
//!   there is nothing to read, and the fact holds.
//!
//! Either failing is a [`super::Beat::Dead`] whose reason names which, and
//! the setting. Each fact counts from the session's start at the earliest:
//! the open path has its own bounds (never opened, pending), and a session
//! gives its client one whole interval, as a WebSocket session's first ping
//! does. The verdict comes exactly one interval after the last evidence:
//! the facts are read when the earlier of the two could first be due, and
//! again from there (`Watch::due`), which costs one registry read per
//! interval while the client keeps both.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::realtime::calls::{Calls, Evidence};

/// A session's watch over its call's evidence (module doc).
pub(crate) struct Watch {
    calls: Calls,
    id: Arc<str>,
    /// When the session started: the facts count from here at the earliest.
    since: Instant,
    /// When the facts are next worth reading: the earliest a verdict could
    /// come. Kept across the `select!`s that drop [`Self::verdict`]'s
    /// future, so each reads the registry once per due time.
    due: Option<Instant>,
}

/// What the facts say at a moment.
#[derive(Debug, PartialEq, Eq)]
enum Read {
    /// A fact failed: the reason.
    Dead(String),
    /// Both hold until at least then.
    Until(Instant),
}

impl Watch {
    /// The watch over call `id`'s evidence, from now.
    pub fn new(calls: Calls, id: Arc<str>) -> Self {
        Self {
            calls,
            id,
            since: Instant::now(),
            due: None,
        }
    }

    /// The next verdict (module doc), with `every` the interval. Never while
    /// the client keeps both facts, nor once its call is gone or let go:
    /// the call's reader ends the session then.
    pub async fn verdict(&mut self, every: Duration) -> String {
        loop {
            if let Some(due) = self.due {
                tokio::time::sleep_until(due).await;
            }
            let Some(evidence) = self.calls.evidence(&self.id) else {
                return std::future::pending().await;
            };
            match judge(evidence, self.since, every, Instant::now()) {
                Read::Dead(reason) => return reason,
                Read::Until(t) => self.due = Some(t),
            }
        }
    }
}

/// The facts `e` at `now`, counted from `since` at the earliest (module
/// doc).
fn judge(e: Evidence, since: Instant, every: Duration, now: Instant) -> Read {
    let heard = e.last_heard.map_or(since, |t| t.max(since));
    let silent = heard + every;
    if now >= silent {
        return Read::Dead(format!(
            "no traffic from the client within {} s, setting realtime.ping_interval_s",
            every.as_secs_f64()
        ));
    }
    let Some(waiting) = e.unacked_since.map(|t| t.max(since)) else {
        return Read::Until(silent);
    };
    let stalled = waiting + every;
    if now >= stalled {
        return Read::Dead(format!(
            "the client acknowledged none of the session's output within {} s, setting \
             realtime.ping_interval_s",
            every.as_secs_f64()
        ));
    }
    Read::Until(silent.min(stalled))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY: Duration = Duration::from_secs(20);

    fn at(base: Instant, s: u64) -> Instant {
        base + Duration::from_secs(s)
    }

    #[test]
    fn traffic_within_the_interval_keeps_the_client_and_its_absence_ends_it() {
        let t0 = Instant::now();
        let heard = |s| Evidence {
            last_heard: Some(at(t0, s)),
            unacked_since: None,
        };
        // Heard at 5: due at 25, not before.
        assert_eq!(
            judge(heard(5), t0, EVERY, at(t0, 24)),
            Read::Until(at(t0, 25))
        );
        let Read::Dead(reason) = judge(heard(5), t0, EVERY, at(t0, 25)) else {
            panic!("one whole interval without traffic");
        };
        assert_eq!(
            reason,
            "no traffic from the client within 20 s, setting realtime.ping_interval_s"
        );
        // Nothing heard yet, or heard before the session: counted from its
        // start.
        let none = Evidence {
            last_heard: None,
            unacked_since: None,
        };
        assert_eq!(judge(none, t0, EVERY, t0), Read::Until(at(t0, 20)));
        let early = Evidence {
            last_heard: Some(t0 - Duration::from_secs(60)),
            unacked_since: None,
        };
        assert_eq!(judge(early, t0, EVERY, at(t0, 1)), Read::Until(at(t0, 20)));
    }

    #[test]
    fn output_unacknowledged_for_an_interval_is_stalled_whatever_the_traffic() {
        let t0 = Instant::now();
        // The client sends all the time (RTP, consent checks), and has
        // acknowledged nothing since 3.
        let busy = Evidence {
            last_heard: Some(at(t0, 22)),
            unacked_since: Some(at(t0, 3)),
        };
        assert_eq!(judge(busy, t0, EVERY, at(t0, 22)), Read::Until(at(t0, 23)));
        let Read::Dead(reason) = judge(busy, t0, EVERY, at(t0, 23)) else {
            panic!("nothing acknowledged for a whole interval");
        };
        assert!(
            reason.starts_with("the client acknowledged none")
                && reason.contains("realtime.ping_interval_s"),
            "{reason}"
        );
        // An acknowledgement at 22 moves it on.
        let acked = Evidence {
            unacked_since: Some(at(t0, 22)),
            ..busy
        };
        assert_eq!(judge(acked, t0, EVERY, at(t0, 23)), Read::Until(at(t0, 42)));
    }
}
