//! Output pacing (realtime §8.2): when may each audio chunk be sent?
//!
//! Why pace at all: the writer could push a whole answer's audio in one
//! burst, but then `output_audio.done` and `response.done` would arrive
//! long before the client finished playing. The stock SDK's playback state
//! dies early, the barge-in window (§6.4) has no true end, and a cancel
//! leaves the client holding audio the server thinks was never heard
//! (§7.3). So audio leaves **paced to real time, plus a lead**:
//! - the first `lead` of audio goes out at once (no first-audio cost);
//! - after that a chunk leaves when the client's unplayed audio has fallen
//!   to `lead - chunk`, so it holds at most `lead` once the chunk lands,
//!   and never later than the moment the client would run dry (that
//!   matters only for chunks longer than the lead, e.g. `lead = 0`);
//! - a chunk that is ready late (slow TTS) leaves when ready; the client
//!   underruns, restarts on arrival, and a fresh lead builds up;
//! - a chunk that **leaves** late — the socket stalled, the writer could not
//!   send — re-bases the schedule on when it really left
//!   ([`Pacer::release`]): the client ran dry meanwhile and plays it from
//!   its arrival, so the chunks behind it are due from there, rather than
//!   all at once as a burst that would leave the client holding the stall
//!   plus the lead, and every count after it (what was sent, the barge-in
//!   window) off by the stall.
//!
//! The logic is pure: times are `Duration`s on whatever clock the writer
//! injects (`ready` is "now" when the chunk was queued), and the writer does
//! the sleeping. The client is modelled as playing each chunk from arrival
//! with no extra buffering, which is what the heard table assumes too.

use std::time::Duration;

/// Pacing errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingError {
    /// The output rate must not be zero.
    ZeroRate,
}

impl std::fmt::Display for PacingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "output sample rate must not be zero")
    }
}

impl std::error::Error for PacingError {}

/// The release schedule of one response's audio.
///
/// Constant in size however long the answer (package B review, small
/// items): a schedule needs only its first and latest release, not every
/// chunk's — a looping model's hour of audio is 36 000 chunks.
#[derive(Debug, Clone)]
pub struct Pacer {
    rate: u64,
    lead: Duration,
    /// The earliest and the latest release, and how many there were.
    first: Option<Duration>,
    last: Option<Duration>,
    chunks: usize,
    /// When the modelled client finishes everything released so far.
    play_end: Option<Duration>,
    total_samples: u64,
}

impl Pacer {
    /// `rate` is the output rate (24000); `lead` is `output_lead_ms`.
    pub fn new(rate: u32, lead: Duration) -> Result<Self, PacingError> {
        if rate == 0 {
            return Err(PacingError::ZeroRate);
        }
        Ok(Self {
            rate: u64::from(rate),
            lead,
            first: None,
            last: None,
            chunks: 0,
            play_end: None,
            total_samples: 0,
        })
    }

    fn duration(&self, samples: u64) -> Duration {
        let nanos = u128::from(samples) * 1_000_000_000 / u128::from(self.rate);
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    /// Schedules the next chunk of `samples` (output rate) that became
    /// ready at `ready`, assuming it leaves on time; returns the clock time
    /// at which it may be sent. [`Self::due`] then [`Self::release`].
    pub fn push(&mut self, samples: u64, ready: Duration) -> Duration {
        let release = self.due(samples, ready);
        self.release(samples, release);
        release
    }

    /// When the next chunk of `samples`, ready at `ready`, may be sent,
    /// given what has left so far — without scheduling it.
    pub fn due(&self, samples: u64, ready: Duration) -> Duration {
        let dur = self.duration(samples);
        match (self.play_end, self.last) {
            (Some(end), Some(last)) => {
                let due = end.saturating_sub(self.lead.saturating_sub(dur));
                ready.max(due).max(last)
            }
            _ => ready,
        }
    }

    /// The next chunk of `samples` left at `at` — on time, or late (module
    /// doc): the modelled client plays it from then, and what follows is
    /// due from there.
    pub fn release(&mut self, samples: u64, at: Duration) {
        let dur = self.duration(samples);
        let starts = self.play_end.map_or(at, |end| end.max(at));
        self.play_end = Some(starts.saturating_add(dur));
        // Total over any order of `at`: the span is from the earliest to
        // the latest release.
        self.first = Some(self.first.map_or(at, |f| f.min(at)));
        self.last = Some(self.last.map_or(at, |l| l.max(at)));
        self.chunks += 1;
        self.total_samples = self.total_samples.saturating_add(samples);
    }

    /// Chunks scheduled so far.
    pub fn chunks(&self) -> usize {
        self.chunks
    }

    /// How long the paced send has taken so far, in milliseconds: from the
    /// first release to the latest.
    pub fn send_span_ms(&self) -> Option<u64> {
        let span = self.last?.saturating_sub(self.first?);
        Some(u64::try_from(span.as_millis()).unwrap_or(u64::MAX))
    }

    /// The clock time of the last release: the paced send's end, when
    /// `output_audio.done` and the closing events go out.
    pub fn send_end(&self) -> Option<Duration> {
        self.last
    }

    /// When the modelled client finishes playing everything scheduled. At
    /// the steady state of pacing that is the paced send's end plus the
    /// lead; it is earlier when the client never held a full lead at the end
    /// — an answer shorter than the lead, the tail after an underrun — and
    /// later when the last chunk is longer than the lead (`lead = 0`). The
    /// playing window the barge-in gate judges by is the writer's record of
    /// the same model (`writer::playback`, §6.4).
    pub fn playback_end(&self) -> Option<Duration> {
        self.play_end
    }

    /// Audio scheduled so far.
    pub fn audio_duration(&self) -> Duration {
        self.duration(self.total_samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;
    /// 100 ms at 24 kHz, the §8.2 delta size.
    const CHUNK: u64 = 2400;

    /// Push `n` chunks ready at `ready`: when each may be sent, in ms after
    /// the first release of the schedule.
    fn push_n(p: &mut Pacer, n: usize, ready: Duration, out: &mut Vec<u64>) {
        for _ in 0..n {
            let at = p.push(CHUNK, ready);
            let first = p.first.unwrap();
            out.push((at - first).as_millis() as u64);
        }
    }

    #[test]
    fn lead_goes_out_at_once_then_real_time() {
        let mut p = Pacer::new(24_000, MS(500)).unwrap();
        let mut at = Vec::new();
        push_n(&mut p, 10, MS(0), &mut at);
        assert_eq!(at, [0, 0, 0, 0, 0, 100, 200, 300, 400, 500]);
        assert_eq!((p.chunks(), p.send_span_ms()), (10, Some(500)));
        assert_eq!(p.send_end(), Some(MS(500)));
        assert_eq!(p.playback_end(), Some(MS(1000)));
        assert_eq!(p.audio_duration(), MS(1000));
    }

    #[test]
    fn times_are_relative_to_the_first_release() {
        // The clock is the writer's: here the response starts at 7 s.
        let mut p = Pacer::new(24_000, MS(200)).unwrap();
        assert_eq!(p.push(CHUNK, MS(7000)), MS(7000));
        p.push(CHUNK, MS(7000));
        assert_eq!(p.push(CHUNK, MS(7000)), MS(7100));
        assert_eq!(p.send_span_ms(), Some(100));
        assert_eq!(
            (p.first, p.playback_end()),
            (Some(MS(7000)), Some(MS(7300)))
        );
    }

    #[test]
    fn a_late_chunk_underruns_and_rebuilds_the_lead() {
        let mut p = Pacer::new(24_000, MS(500)).unwrap();
        let mut at = Vec::new();
        push_n(&mut p, 3, MS(0), &mut at);
        // TTS for the next clause is ready only at 800 ms: the client ran
        // dry at 300 and restarts at 800; a fresh lead goes out at once.
        push_n(&mut p, 6, MS(800), &mut at);
        assert_eq!(at, [0, 0, 0, 800, 800, 800, 800, 800, 900]);
        assert_eq!(p.playback_end(), Some(MS(1400)));
    }

    #[test]
    fn a_short_answer_s_window_is_its_audio_not_the_lead() {
        let mut p = Pacer::new(24_000, MS(500)).unwrap();
        for _ in 0..2 {
            p.push(CHUNK, MS(0));
        }
        assert_eq!(p.send_end(), Some(MS(0)));
        assert_eq!(p.playback_end(), Some(MS(200)));
    }

    #[test]
    fn zero_lead_is_just_in_time() {
        let mut p = Pacer::new(24_000, MS(0)).unwrap();
        let mut at = Vec::new();
        push_n(&mut p, 4, MS(0), &mut at);
        assert_eq!(at, [0, 100, 200, 300]);
        // Send end + lead would be 300; the client plays until 400.
        assert_eq!(p.playback_end(), Some(MS(400)));
    }

    #[test]
    fn chunks_longer_than_the_lead_never_starve_the_client() {
        let mut p = Pacer::new(24_000, MS(50)).unwrap();
        let mut at = Vec::new();
        push_n(&mut p, 3, MS(0), &mut at);
        assert_eq!(at, [0, 100, 200]);
    }

    #[test]
    fn uneven_chunks_and_odd_rates() {
        // A clause's last chunk is short; 22050 Hz does not divide into ms.
        let mut p = Pacer::new(22_050, MS(100)).unwrap();
        let at = [
            p.push(2205, MS(0)), // 100 ms
            p.push(441, MS(0)),  // 20 ms: leaves once the client holds 80 ms
            p.push(2205, MS(0)),
        ];
        assert_eq!(at, [MS(0), MS(20), MS(120)]);
        assert_eq!(p.playback_end(), Some(MS(220)));
        assert_eq!(Pacer::new(0, MS(1)).unwrap_err(), PacingError::ZeroRate);
        let empty = Pacer::new(24_000, MS(1)).unwrap();
        assert_eq!((empty.send_end(), empty.send_span_ms()), (None, None));
        assert_eq!(empty.playback_end(), None);
    }

    #[test]
    fn a_late_release_re_bases_the_schedule_instead_of_bursting() {
        let mut p = Pacer::new(24_000, MS(300)).unwrap();
        // The lead at once.
        for _ in 0..3 {
            let due = p.due(CHUNK, MS(0));
            p.release(CHUNK, due);
        }
        assert_eq!(p.due(CHUNK, MS(0)), MS(100));
        // The socket stalls: the chunk due at 100 ms leaves at 2 s. The
        // client ran dry at 300 ms and plays it from 2 s on.
        p.release(CHUNK, MS(2000));
        assert_eq!(p.playback_end(), Some(MS(2100)));
        // A fresh lead from there — not the 1.9 s of chunks that fell due
        // during the stall in one burst — then real time again.
        let mut next = Vec::new();
        for _ in 0..4 {
            let due = p.due(CHUNK, MS(0));
            p.release(CHUNK, due);
            next.push(due);
        }
        assert_eq!(next, [MS(2000), MS(2000), MS(2100), MS(2200)]);
    }

    #[test]
    fn huge_inputs_saturate_instead_of_panicking() {
        let mut p = Pacer::new(1, MS(0)).unwrap();
        p.push(u64::MAX / 2, MS(0));
        assert!(p.playback_end().is_some());
    }

    #[test]
    fn releases_out_of_order_are_no_panic_and_no_growth() {
        // A clock that steps back (it cannot, on the writer's monotonic
        // one — but the schedule is total anyway).
        let mut p = Pacer::new(24_000, MS(0)).unwrap();
        p.release(CHUNK, MS(500));
        p.release(CHUNK, MS(100));
        assert_eq!(p.send_span_ms(), Some(400));
        assert_eq!(p.send_end(), Some(MS(500)));
        assert_eq!(p.first, Some(MS(100)));
        // An hour of audio: counted, with nothing kept per chunk.
        for k in 0..36_000u64 {
            p.release(CHUNK, MS(1000 + 100 * k));
        }
        assert_eq!(p.chunks(), 36_002);
        assert_eq!(p.send_span_ms(), Some(3_600_900 - 100));
    }
}
