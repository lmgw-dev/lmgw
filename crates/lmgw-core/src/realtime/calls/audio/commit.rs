//! Events and audio in order on a call: the commit window (calls design
//! §4.4).
//!
//! On a WebSocket, appends and the other events are one ordered stream. On
//! a call they are two (RTP and SCTP), and a push-to-talk
//! `input_audio_buffer.commit` can overtake the turn's last frames, still in
//! the phone's capture pipeline or behind a reorder window. So, between the
//! call's reader and the core:
//!
//! - **a commit** takes every frame stamped up to its own stamp plus the
//!   commit window (`realtime.webrtc_commit_window_ms`), and waits that full
//!   window: it is applied once a frame stamped after its cut has been
//!   released, or once the window plus the reorder window has passed since
//!   it arrived, whichever is first;
//! - **a clear** cuts at its own stamp and waits only for the frames stamped
//!   before it, or the reorder window: a frame of silence before a press
//!   that lands in the new turn is harmless, while a window there would cut
//!   the start of what the user says next;
//! - **every other event** goes to the core at once, unless an event waits
//!   before it: events keep their order among themselves;
//! - **a frame later than the cut** — stamped before an applied cut, but
//!   released after it — lands in the next turn, and [`LateFrame`] says how
//!   late it was, so the window can be tuned from the log;
//! - **the End** (§8.1) applies whatever waits at once, with the frames
//!   already in ([`Order::flush`]).
//!
//! An event's stamp is when its message came off the socket; a frame's is
//! its capture plus the least transit (`stamp`). Both are lmgw's clock.
//!
//! **The seam.** [`Order`] is generic over the event, so the session loop
//! classifies each one ([`Cut`]) and hands it over with its stamp; what
//! comes back goes to the core in order, the events before the frame that
//! released them. The loop's timer waits for [`Order::deadline`].

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use tokio::time::Instant;

/// What an event is to the order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cut {
    /// `input_audio_buffer.commit`: cuts at its stamp plus the window.
    Commit,
    /// `input_audio_buffer.clear`: cuts at its stamp.
    Clear,
    /// Anything else: waits for nothing but the events before it.
    Other,
}

/// An event that waits.
#[derive(Debug)]
struct Waiting<E> {
    event: E,
    cut: Cut,
    at: Instant,
}

/// The last cut applied, for [`LateFrame`].
#[derive(Debug, Clone, Copy)]
struct Applied {
    cut: Cut,
    /// Where it cut, on the frames' clock.
    stamp: Instant,
    /// When it went to the core.
    at: Instant,
}

/// A frame that belonged before an applied cut but came after it: it lands
/// in the next turn (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LateFrame {
    pub cut: Cut,
    /// How far before the cut it was stamped.
    pub before_cut: Duration,
    /// How long after the cut was applied it was released.
    pub after_applied: Duration,
}

impl fmt::Display for LateFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.cut {
            Cut::Commit => "commit (realtime.webrtc_commit_window_ms)",
            Cut::Clear => "clear",
            Cut::Other => "event",
        };
        write!(
            f,
            "a frame stamped {} ms before the {what}'s cut arrived {} ms after the cut was \
             applied: it lands in the next turn",
            self.before_cut.as_millis(),
            self.after_applied.as_millis()
        )
    }
}

/// The order of one call's events against its audio (module doc).
#[derive(Debug)]
pub(crate) struct Order<E> {
    /// `realtime.webrtc_commit_window_ms`.
    window: Duration,
    /// `realtime.webrtc_reorder_ms`.
    reorder: Duration,
    waiting: VecDeque<Waiting<E>>,
    /// The latest stamp of a frame released to the core.
    released: Option<Instant>,
    applied: Option<Applied>,
}

impl<E> Order<E> {
    pub fn new(window: Duration, reorder: Duration) -> Self {
        Self {
            window,
            reorder,
            waiting: VecDeque::new(),
            released: None,
            applied: None,
        }
    }

    /// Where a cut of `cut` that arrived at `at` cuts, on the frames' clock.
    fn cuts_at(&self, cut: Cut, at: Instant) -> Instant {
        match cut {
            Cut::Commit => at + self.window,
            Cut::Clear | Cut::Other => at,
        }
    }

    /// When a cut of `cut` that arrived at `at` stops waiting for frames.
    fn gives_up(&self, cut: Cut, at: Instant) -> Instant {
        match cut {
            Cut::Commit => at + self.window + self.reorder,
            Cut::Clear => at + self.reorder,
            Cut::Other => at,
        }
    }

    /// An event of kind `cut` arrived at `at`: the events that go to the
    /// core now, in order — it, or nothing while it or an event before it
    /// waits.
    pub fn event(&mut self, event: E, cut: Cut, at: Instant) -> Vec<E> {
        self.waiting.push_back(Waiting { event, cut, at });
        self.release(None, at)
    }

    /// A frame stamped `captured` is about to go to the core at `now`: the
    /// events to apply before it, in order, and whether it came after the
    /// cut it belonged before.
    pub fn frame(&mut self, captured: Instant, now: Instant) -> (Vec<E>, Option<LateFrame>) {
        let out = self.release(Some(captured), now);
        let late = self
            .applied
            .filter(|a| captured <= a.stamp && now > a.at)
            .map(|a| LateFrame {
                cut: a.cut,
                before_cut: a.stamp - captured,
                after_applied: now - a.at,
            });
        self.released = Some(self.released.map_or(captured, |r| r.max(captured)));
        (out, late)
    }

    /// The time is `now`: the events whose wait is over, in order.
    pub fn due(&mut self, now: Instant) -> Vec<E> {
        self.release(None, now)
    }

    /// When the first waiting event stops waiting, for the loop's timer;
    /// `None` when nothing waits.
    pub fn deadline(&self) -> Option<Instant> {
        let w = self.waiting.front()?;
        Some(self.gives_up(w.cut, w.at))
    }

    /// The End (§8.1): everything waiting, in order, at once.
    pub fn flush(&mut self) -> Vec<E> {
        self.waiting.drain(..).map(|w| w.event).collect()
    }

    /// Release the waiting events from the front whose wait is over at
    /// `now`, or whose cut a frame stamped `next` has passed.
    fn release(&mut self, next: Option<Instant>, now: Instant) -> Vec<E> {
        let mut out = Vec::new();
        while let Some(w) = self.waiting.front() {
            let stamp = self.cuts_at(w.cut, w.at);
            let passed = self.released.max(next).is_some_and(|s| s > stamp);
            if w.cut != Cut::Other && !passed && now < self.gives_up(w.cut, w.at) {
                break;
            }
            let w = self.waiting.pop_front().expect("the front is there");
            if w.cut != Cut::Other {
                self.applied = Some(Applied {
                    cut: w.cut,
                    stamp,
                    at: now,
                });
            }
            out.push(w.event);
        }
        out
    }
}

#[cfg(test)]
mod tests;
