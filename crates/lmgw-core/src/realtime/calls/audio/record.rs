//! What each output frame carried and when it left (calls design §5.3).
//!
//! The phone plays a frame a transit and NetEQ's delay after it leaves, and
//! names what it played by RTP time (a playout report's or a stop's
//! `playout_ntp_ms`, mapped through `line`). So each frame of a response is
//! recorded as it leaves — beside the writer's `Playback::released`, as a
//! WebSocket's chunks are — with its RTP time, its send time, and which
//! item's audio it carried up to which offset, padding marked as such:
//! `playout` measures the phone's delay from it, `heard` cuts by it.
//!
//! **What is kept** is the latest response's frames, from its first (audio
//! or padding) on: a response's first frame drops the last one's. Heard and
//! the playing window only ever concern the latest response, and its
//! frames are bounded by its own audio, as the paced queue's are. A frame
//! outside it — silence between answers — is not kept; its send time is
//! its slot's due time, which the frame clock keeps exactly
//! ([`Record::sent_at`]).
//!
//! **Slots a late tick skipped** (the phone would have discarded them,
//! `out`, §5.5) were never sent: the record marks those that fall in the
//! latest response's run of frames ([`Record::skipped`]), so a position
//! there maps to no send time, and heard counts nothing for them (they
//! carried no audio: what was due in them leaves in the frames after).

use std::time::Duration;

use tokio::time::Instant;

use super::clock::Slot;
use super::frames::{OutFrame, Span};
use super::{
    duration_to_ticks, samples_to_ticks, ticks_to_duration, ticks_to_samples, FRAME, FRAME_TICKS,
};

/// One frame of the latest response, as it left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SentFrame {
    /// Its RTP time.
    pub rtp: u64,
    /// When it left: the tick's own instant, late or not.
    pub sent: Instant,
    /// The items' audio it carried.
    pub spans: Vec<Span>,
    /// Samples of padding at its end.
    pub padded: usize,
}

impl SentFrame {
    /// When its last sample left the server, played at real time: its send
    /// end (§5.3).
    pub fn send_end(&self) -> Instant {
        self.sent + FRAME
    }
}

/// A call's record of its output (module doc).
#[derive(Debug)]
pub(crate) struct Record {
    /// The frame clock's origin: slot 0's due time and RTP time.
    start: Instant,
    rtp0: u64,
    /// The latest response and its frames, in order.
    response: Option<String>,
    frames: Vec<SentFrame>,
    /// The RTP time after the last frame that left: nothing at or past it
    /// was sent.
    end: Option<u64>,
    /// The slots skipped in the latest response's run, as RTP time ranges
    /// (`start..end`), joined where they touch (module doc).
    skipped: Vec<(u64, u64)>,
}

impl Record {
    /// A record for a frame clock that starts at `start` with RTP time
    /// `rtp0` (`clock::FrameClock::new`).
    pub fn new(start: Instant, rtp0: u64) -> Self {
        Self {
            start,
            rtp0,
            response: None,
            frames: Vec::new(),
            end: None,
            skipped: Vec::new(),
        }
    }

    /// `frame` left in `slot` at `at` (module doc).
    pub fn sent(&mut self, slot: Slot, frame: &OutFrame, at: Instant) {
        self.end = Some(slot.rtp + FRAME_TICKS);
        let Some(r) = &frame.response_id else {
            return;
        };
        if self.response.as_ref() != Some(r) {
            self.response = Some(r.clone());
            self.frames.clear();
            self.skipped.clear();
        }
        self.frames.push(SentFrame {
            rtp: slot.rtp,
            sent: at,
            spans: frame.spans.clone(),
            padded: frame.padded,
        });
    }

    /// `slot` was skipped, never sent (module doc): marked when it carries
    /// on the latest response's run — right after its last frame or the
    /// last slot skipped after it.
    pub fn skipped(&mut self, slot: Slot) {
        let run_end = [
            self.frames.last().map(|f| f.rtp + FRAME_TICKS),
            self.skipped.last().map(|r| r.1),
        ]
        .into_iter()
        .flatten()
        .max();
        if run_end != Some(slot.rtp) {
            return;
        }
        let end = slot.rtp + FRAME_TICKS;
        match self.skipped.last_mut() {
            Some(r) if r.1 == slot.rtp => r.1 = end,
            _ => self.skipped.push((slot.rtp, end)),
        }
    }

    /// Whether the sample at RTP time `rtp` lies in a slot that was skipped.
    pub fn was_skipped(&self, rtp: u64) -> bool {
        self.skipped
            .iter()
            .any(|&(from, to)| (from..to).contains(&rtp))
    }

    /// The latest response that sent a frame.
    pub fn response(&self) -> Option<&str> {
        self.response.as_deref()
    }

    /// The latest response's frames, in order.
    pub fn frames(&self) -> &[SentFrame] {
        &self.frames
    }

    /// The RTP time after the last frame that left, if any did.
    pub fn end(&self) -> Option<u64> {
        self.end
    }

    /// The RTP time of the stream's first frame.
    pub fn start_rtp(&self) -> u64 {
        self.rtp0
    }

    /// When the sample at RTP time `rtp` left: its frame's send time plus
    /// its place in the frame; a frame outside the latest response by its
    /// slot's due time. `None` before the stream, at or past its end, or in
    /// a slot that was skipped.
    pub fn sent_at(&self, rtp: u64) -> Option<Instant> {
        if rtp < self.rtp0 || self.end.is_none_or(|end| rtp >= end) || self.was_skipped(rtp) {
            return None;
        }
        let i = self.frames.partition_point(|f| f.rtp <= rtp);
        if let Some(f) = i.checked_sub(1).map(|i| &self.frames[i]) {
            if rtp < f.rtp + FRAME_TICKS {
                return Some(f.sent + ticks_to_duration(rtp - f.rtp));
            }
        }
        Some(self.start + ticks_to_duration(rtp - self.rtp0))
    }
}

impl Record {
    /// The RTP time of the latest response's sample that left at `t`, its
    /// frames played at real time — each from when it left, or from the end
    /// of the one before it if that is later, so a late tick's burst plays
    /// one frame after another, as a WebSocket's chunks do
    /// (`writer::Playback`) — in whole samples. Before the first frame, its
    /// start; between two, the end of the first; past the last one's end,
    /// that end. `None` without a frame of it. (`heard::on_barge_in`.)
    pub fn rtp_at(&self, t: Instant) -> Option<u64> {
        let mut end: Option<Instant> = None;
        for f in &self.frames {
            let starts = end.map_or(f.sent, |e| e.max(f.sent));
            let Some(into) = t.checked_duration_since(starts) else {
                return Some(f.rtp);
            };
            if into < FRAME {
                return Some(f.rtp + samples_to_ticks(ticks_to_samples(duration_to_ticks(into))));
            }
            end = Some(starts + FRAME);
        }
        self.frames.last().map(|f| f.rtp + FRAME_TICKS)
    }
}

/// Half of `rtt`: the one-way transit "half the round trip" assumes (§5.3).
pub(crate) fn half(rtt: Option<Duration>) -> Duration {
    rtt.map_or(Duration::ZERO, |r| r / 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realtime::calls::audio::clock::FrameClock;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    fn frame(response: Option<&str>, padded: usize) -> OutFrame {
        OutFrame {
            pcm: vec![0; 480],
            response_id: response.map(String::from),
            spans: Vec::new(),
            padded,
            starved: None,
            started: None,
        }
    }

    #[test]
    fn the_latest_responses_frames_are_kept_with_their_send_times() {
        let t0 = Instant::now();
        let mut clock = FrameClock::new(t0, 10_000);
        let mut r = Record::new(t0, 10_000);
        assert_eq!(r.sent_at(10_000), None, "nothing sent");
        // Two frames of silence, then r1 for three (one sent 7 ms late),
        // then silence again.
        let plan = [None, None, Some("r1"), Some("r1"), Some("r1"), None];
        for (k, resp) in plan.iter().enumerate() {
            let slot = clock.take(t0 + MS(20 * k as u64)).unwrap();
            let late = if k == 3 { MS(7) } else { MS(0) };
            r.sent(slot, &frame(*resp, 0), slot.due + late);
        }
        assert_eq!(r.response(), Some("r1"));
        assert_eq!(r.frames().len(), 3);
        assert_eq!(r.end(), Some(10_000 + 6 * 960));
        // A sample in r1's late frame left when that frame did.
        assert_eq!(
            r.sent_at(10_000 + 3 * 960 + 480),
            Some(t0 + MS(60 + 7 + 10))
        );
        // Silence is placed by its due time.
        assert_eq!(r.sent_at(10_000 + 960), Some(t0 + MS(20)));
        assert_eq!(r.sent_at(10_000 + 5 * 960 + 48), Some(t0 + MS(101)));
        assert_eq!(r.sent_at(10_000 + 6 * 960), None, "not sent yet");
        assert_eq!(r.sent_at(9_999), None, "before the stream");
        assert_eq!(r.frames()[2].send_end(), t0 + MS(100));
        // The next response's first frame, padding included, replaces r1's.
        let slot = clock.take(t0 + MS(120)).unwrap();
        r.sent(slot, &frame(Some("r2"), 480), slot.due);
        assert_eq!((r.response(), r.frames().len()), (Some("r2"), 1));
        assert_eq!(r.frames()[0].padded, 480);
        assert_eq!(half(Some(MS(80))), MS(40));
        assert_eq!(half(None), Duration::ZERO);
    }

    #[test]
    fn slots_a_late_tick_skipped_in_a_responses_run_map_to_no_send_time() {
        let t0 = Instant::now();
        let mut clock = FrameClock::new(t0, 0);
        let mut r = Record::new(t0, 0);
        let mut slot = |k: u64| clock.take(t0 + MS(20 * k)).unwrap();
        // A slot skipped before any response is not kept.
        let s0 = slot(0);
        r.skipped(s0);
        assert!(!r.was_skipped(0));
        let s1 = slot(1);
        r.sent(s1, &frame(Some("r1"), 0), s1.due);
        // Slots 2 and 3 skipped by a late tick, slot 4 sent with them.
        let (s2, s3, s4) = (slot(2), slot(3), slot(4));
        r.skipped(s2);
        r.skipped(s3);
        r.sent(s4, &frame(Some("r1"), 0), t0 + MS(90));
        assert!(r.was_skipped(2 * 960) && r.was_skipped(4 * 960 - 1));
        assert!(!r.was_skipped(4 * 960));
        assert_eq!(r.sent_at(2 * 960 + 10), None, "never sent");
        assert_eq!(r.sent_at(4 * 960), Some(t0 + MS(90)));
        // Silence after the response, then a skip that does not carry on its
        // run: not kept.
        let s5 = slot(5);
        r.sent(s5, &frame(None, 0), s5.due);
        let (_s6, s7) = (slot(6), slot(7));
        r.skipped(s7);
        assert!(!r.was_skipped(7 * 960));
        // The next response's first frame drops the marks with r1's frames.
        let s8 = slot(8);
        r.sent(s8, &frame(Some("r2"), 0), s8.due);
        assert!(!r.was_skipped(2 * 960));
    }
}
