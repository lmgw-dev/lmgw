//! The output's frame clock (calls design §5.1).
//!
//! A call's output is one RTP stream for the call's whole life: a frame
//! every 20 ms, TTS audio while a response speaks and silence otherwise. Due
//! times are `start + k × 20 ms` and RTP times `rtp0 + k × 960`, so neither
//! drifts with timer jitter: a late tick sends late with the right
//! timestamp, which the phone's NetEQ absorbs, and a tick that runs very
//! late takes every slot it missed in turn (`out` skips those due longer
//! ago than the playout allowance, which the phone would discard).
//!
//! **What str0m is given.** Each frame is written with its slot's due time
//! as the wallclock and its RTP time as the media time. str0m's sender
//! reports carry the last written pair forward by the time since, so every
//! report lmgw sends lies on this clock's one line (`line`, calls design
//! §16, settled in WP2 part A).

use std::time::Duration;

use tokio::time::Instant;

use super::{FRAME, FRAME_TICKS};

/// One frame's slot on the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    /// Its index since the stream started.
    pub k: u64,
    /// When it is due: `start + k × 20 ms`, the wallclock str0m is given.
    pub due: Instant,
    /// Its RTP time: `rtp0 + k × 960`.
    pub rtp: u64,
}

/// A call's frame clock (module doc).
#[derive(Debug, Clone)]
pub(crate) struct FrameClock {
    start: Instant,
    rtp0: u64,
    next: u64,
}

impl FrameClock {
    /// A clock whose first frame is due at `start` with RTP time `rtp0`.
    pub fn new(start: Instant, rtp0: u64) -> Self {
        Self {
            start,
            rtp0,
            next: 0,
        }
    }

    /// Slot `k`.
    pub fn slot(&self, k: u64) -> Slot {
        let nanos = u128::from(k) * FRAME.as_nanos();
        Slot {
            k,
            due: self.start + Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX)),
            rtp: self.rtp0 + k * FRAME_TICKS,
        }
    }

    /// When the next frame is due: what the link's timer waits for.
    pub fn next_due(&self) -> Instant {
        self.slot(self.next).due
    }

    /// The next slot, if it is due at `now`, taken. Called until it answers
    /// `None`, a late tick takes every slot it missed.
    pub fn take(&mut self, now: Instant) -> Option<Slot> {
        let s = self.slot(self.next);
        if s.due > now {
            return None;
        }
        self.next += 1;
        Some(s)
    }

    /// The RTP time of the next frame to be sent: everything before it has
    /// left.
    pub fn next_rtp(&self) -> u64 {
        self.slot(self.next).rtp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn slots_are_due_every_20_ms_and_a_late_tick_takes_each_it_missed() {
        let t0 = Instant::now();
        let mut c = FrameClock::new(t0, 1_000);
        assert_eq!(c.next_due(), t0);
        assert_eq!(c.take(t0 - MS(1)), None, "not due yet");
        let s = c.take(t0).unwrap();
        assert_eq!((s.k, s.due, s.rtp), (0, t0, 1_000));
        assert_eq!(c.next_due(), t0 + MS(20));
        assert_eq!(c.take(t0 + MS(19)), None);
        // A tick 47 ms late, at 67 ms: slots 1 to 3 are due, each with its
        // own due time and RTP time; slot 4 is not.
        let now = t0 + MS(67);
        let late: Vec<Slot> = std::iter::from_fn(|| c.take(now)).collect();
        assert_eq!(late.len(), 3);
        assert_eq!((late[0].due, late[0].rtp), (t0 + MS(20), 1_960));
        assert_eq!((late[1].due, late[1].rtp), (t0 + MS(40), 2_920));
        assert_eq!((late[2].due, late[2].rtp), (t0 + MS(60), 3_880));
        assert_eq!(c.next_due(), t0 + MS(80));
        assert_eq!(c.next_rtp(), 1_000 + 4 * 960);
        // No drift: slot 50 000 is due exactly 1000 s in.
        assert_eq!(c.slot(50_000).due, t0 + Duration::from_secs(1000));
    }
}
