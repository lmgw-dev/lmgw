//! The input timeline against the wall clock (realtime design §6.4): when
//! was a sample captured?
//!
//! The playing window is wall-clock time — when the writer released the
//! answer's audio — and the input is samples. To judge a frame against the
//! window the session needs the frame's capture instant, which the client
//! never sends. It is estimated from when each append **arrived**, assuming
//! the client sends audio as it records it: the last sample of an append
//! was captured about when the append arrived, and the samples before it
//! that much earlier at the input rate. The arrival is when the frame came
//! off the socket (`inbox`). The estimate is late by the client's input
//! transit and buffer, and the client hears the window late by its output
//! transit and buffer: the two add rather than cancel, and the window's end
//! carries a margin for both (`lifecycle::interrupt`, B3 review 2).
//!
//! What other senders get:
//! - **Faster than real time** (a burst after other appends): the
//!   back-extrapolation stops at the previous append's arrival, so a burst
//!   is judged at its arrival rather than spread over a past it was not
//!   sent in. Audio uploaded during playback must then pass the gate —
//!   the stricter side. An **isolated** append — the session's first, or
//!   one after a pause longer than itself — has no such floor and is placed
//!   back over its own duration, as if recorded right before it was sent.
//! - **A stall, then a burst:** judged late by the stall — it may fall
//!   after the window and be a normal turn (the safe side).
//! - **A muted client** (no appends): no frames, no evidence; the arbiter
//!   resets its evidence on a wall-clock gap between two frames
//!   (`turn::arbiter`).
//!
//! `audio_start_ms` and every back-dating stay pure sample math: the
//! estimate only decides window membership and the guard. Memory follows
//! the audio not yet scored: anchors behind the scored frames are dropped.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

/// One append: samples `from..end` arrived at `at`.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    from: u64,
    end: u64,
    at: Instant,
    /// The previous append's arrival: no sample of this one was captured
    /// before it was (module doc). `None` for the session's first.
    floor: Option<Instant>,
}

/// The appends whose samples are not scored yet.
#[derive(Debug)]
pub(crate) struct Timeline {
    anchors: VecDeque<Anchor>,
    last: Option<Instant>,
    rate: u64,
}

impl Timeline {
    pub fn new(rate: u32) -> Self {
        Self {
            anchors: VecDeque::new(),
            last: None,
            rate: u64::from(rate.max(1)),
        }
    }

    fn duration(&self, samples: u64) -> Duration {
        let nanos = u128::from(samples) * 1_000_000_000 / u128::from(self.rate);
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    /// Samples `from..end` of the timeline arrived at `at`.
    pub fn stamp(&mut self, from: u64, end: u64, at: Instant) {
        if end <= from {
            return;
        }
        let at = self.last.map_or(at, |last| at.max(last));
        self.anchors.push_back(Anchor {
            from,
            end,
            at,
            floor: self.last,
        });
        self.last = Some(at);
    }

    /// When the sample ending at `s` was captured (module doc); `None` for a
    /// sample no append covered, or one already forgotten.
    pub fn capture(&self, s: u64) -> Option<Instant> {
        let a = self
            .anchors
            .iter()
            .find(|a| a.from < s && s <= a.end)
            .or_else(|| self.anchors.back().filter(|a| s > a.end))?;
        let back = self.duration(a.end.saturating_sub(s));
        let t = a.at.checked_sub(back).unwrap_or(a.at);
        Some(a.floor.map_or(t, |f| t.max(f)))
    }

    /// The frames up to sample `s` are scored: their appends are not needed
    /// any more.
    pub fn forget_before(&mut self, s: u64) {
        while self.anchors.front().is_some_and(|a| a.end <= s) {
            self.anchors.pop_front();
        }
    }

    /// Forget every anchor (a clear, a mode switch); arrivals still order
    /// what comes next.
    pub fn clear(&mut self) {
        self.anchors.clear();
    }

    /// Anchors kept.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.anchors.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;
    /// Samples per millisecond at 24 kHz.
    const S: u64 = 24;

    /// A client sending `chunk_ms` appends in real time from `t0`, for
    /// `n` appends.
    fn real_time(t: &mut Timeline, t0: Instant, chunk_ms: u64, n: u64) {
        for k in 0..n {
            t.stamp(
                k * chunk_ms * S,
                (k + 1) * chunk_ms * S,
                t0 + MS((k + 1) * chunk_ms),
            );
        }
    }

    #[test]
    fn a_real_time_sender_s_samples_are_captured_where_they_were() {
        for chunk in [20, 100, 200] {
            let t0 = Instant::now();
            let mut t = Timeline::new(24_000);
            real_time(&mut t, t0, chunk, 10);
            // Sample at 1234 ms (inside some append) was captured at 1234 ms.
            for ms in [1, 37, 640, 1000, 1234, 10 * chunk] {
                if ms <= 10 * chunk {
                    assert_eq!(t.capture(ms * S), Some(t0 + MS(ms)), "{chunk} ms: {ms}");
                }
            }
        }
    }

    #[test]
    fn a_burst_is_judged_at_its_arrival_not_spread_over_the_past() {
        let t0 = Instant::now();
        let mut t = Timeline::new(24_000);
        real_time(&mut t, t0, 100, 5);
        // Ten seconds uploaded at once, at 2 s: no sample of it was captured
        // before the append before it arrived (500 ms) — and its last sample
        // at its arrival.
        t.stamp(500 * S, 10_500 * S, t0 + MS(2000));
        assert_eq!(t.capture(10_500 * S), Some(t0 + MS(2000)));
        assert_eq!(t.capture(9_000 * S), Some(t0 + MS(500)));
        assert_eq!(t.capture(600 * S), Some(t0 + MS(500)));
        // Monotone over the burst.
        let caps: Vec<_> = (6..=105).map(|k| t.capture(k * 100 * S).unwrap()).collect();
        assert!(caps.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn a_stall_then_a_burst_is_judged_late_and_a_mute_leaves_a_gap() {
        let t0 = Instant::now();
        let mut t = Timeline::new(24_000);
        real_time(&mut t, t0, 100, 3);
        // The client stalls for a second, then sends 300 ms at once.
        t.stamp(300 * S, 600 * S, t0 + MS(1300));
        assert_eq!(t.capture(600 * S), Some(t0 + MS(1300)));
        assert_eq!(t.capture(400 * S), Some(t0 + MS(1100)));
        // Muted for two seconds, then real time again: the frames before and
        // after the mute are two seconds apart on the wall clock.
        t.stamp(600 * S, 700 * S, t0 + MS(3400));
        let gap = t.capture(620 * S).unwrap() - t.capture(600 * S).unwrap();
        assert!(gap >= MS(1900), "{gap:?}");
    }

    #[test]
    fn frames_past_the_last_append_are_its_arrival_and_scored_anchors_go() {
        let t0 = Instant::now();
        let mut t = Timeline::new(24_000);
        real_time(&mut t, t0, 20, 50);
        assert_eq!(t.capture(1_200 * S), Some(t0 + MS(1000)));
        assert_eq!(t.len(), 50);
        t.forget_before(500 * S);
        assert_eq!(t.len(), 25);
        assert_eq!(t.capture(100 * S), None, "forgotten");
        assert_eq!(t.capture(510 * S), Some(t0 + MS(510)));
        t.forget_before(10_000 * S);
        assert_eq!(t.len(), 0);
        // Arrivals stay monotone across a clear, and an empty append is no
        // anchor.
        t.clear();
        t.stamp(1000 * S, 1000 * S, t0);
        assert_eq!(t.len(), 0);
        t.stamp(1000 * S, 1020 * S, t0);
        assert_eq!(t.capture(1020 * S), Some(t0 + MS(1000)));
    }
}
