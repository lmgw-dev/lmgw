//! The playing window (realtime design §6.4, §8.2): when the client plays a
//! speaking response's audio, as the writer released it.
//!
//! **Why the writer keeps it.** The barge-in gate applies while the client
//! *plays*, not while the server generates: generation ends seconds before
//! playback does. Only the writer knows when each chunk really left, so it
//! records that here, under the paced queue's lock, at every place where a
//! generation's schedule changes: its first lead (`Pace`), each chunk that
//! left, and its purge. The core reads a copy ([`super::WriterHandle::playback`]).
//!
//! **Why it outlives the schedule.** The pacer of a generation goes when its
//! drained marker is taken — and a marker taken off the lane with nothing
//! queued used to take it right away (package B review 11). The window does
//! not end there: the client still plays what it holds. This record stays
//! until the next speaking response paces, so frames captured inside the
//! window are judged by it even when they reach the core after the
//! response's `response.done` (§6.4's residual race).
//!
//! **Where it ends** is the modelled client's playback end: each chunk plays
//! from when it left, or from the end of the chunk before it if that is
//! later — the model the pacer and the heard table already use (§7.3,
//! §8.2). At the steady state of pacing that is the paced send's end plus
//! the lead; an answer shorter than the lead, or the tail after an underrun,
//! ends earlier, and the record says so rather than claiming a full lead the
//! client never held. A purge ends it at once: the client was told to stop.
//!
//! **Audio still waiting** ([`Playback::waiting`], B3 review 8): the end
//! counts only what has left. While the paced queue still holds the
//! generation's audio, that end can pass with seconds of the answer to come
//! — a lead of 0, timer jitter, a writer that stalls behind its socket. The
//! barge-in window then stays open-ended (`lifecycle::interrupt`): the
//! answer is not over. The synthesis bound reads the same two facts the
//! other way round — an end that passed while audio waits is a stall
//! (`responder::speech::room`).

use std::time::Duration;

use tokio::time::Instant;

/// One speaking response's playback, as released so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Playback {
    /// The response generation.
    pub gen: u64,
    /// When its first audio chunk left: where the window opens.
    pub first: Option<Instant>,
    /// When the modelled client finishes what has left (module doc).
    pub play_end: Option<Instant>,
    /// Its purge — a cancel: the client was told to stop here.
    pub ended: Option<Instant>,
    /// When this copy was read: audio of it still waited in the paced
    /// queue (module doc). Never set on the record itself.
    pub waiting: bool,
}

impl Playback {
    pub fn new(gen: u64) -> Self {
        Self {
            gen,
            first: None,
            play_end: None,
            ended: None,
            waiting: false,
        }
    }

    /// A chunk of `samples` (at `rate`) left at `at`. Nothing moves a window
    /// that was cut.
    pub fn released(&mut self, at: Instant, samples: u64, rate: u32) {
        if self.ended.is_some() {
            return;
        }
        self.first.get_or_insert(at);
        let nanos = u128::from(samples) * 1_000_000_000 / u128::from(rate.max(1));
        let dur = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
        let starts = self.play_end.map_or(at, |end| end.max(at));
        self.play_end = Some(starts + dur);
    }

    /// The generation was purged at `at`.
    pub fn cut(&mut self, at: Instant) {
        self.ended.get_or_insert(at);
    }

    /// Where the window ends: the purge, else the modelled playback end;
    /// `None` before any audio left (module doc).
    pub fn end(&self) -> Option<Instant> {
        self.ended.or(self.play_end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;
    /// 100 ms at 24 kHz.
    const CHUNK: u64 = 2400;

    #[test]
    fn the_window_is_the_modelled_playback_and_a_purge_cuts_it() {
        let t0 = Instant::now();
        let mut p = Playback::new(3);
        assert_eq!(p.end(), None, "nothing left yet");
        // A 500 ms lead at once, then one chunk per 100 ms: the client holds
        // the lead when the last one lands, and plays to 1 s.
        for _ in 0..5 {
            p.released(t0, CHUNK, 24_000);
        }
        for k in 1..=5 {
            p.released(t0 + MS(100 * k), CHUNK, 24_000);
        }
        assert_eq!(p.first, Some(t0));
        assert_eq!(p.end(), Some(t0 + MS(1000)));
        // An underrun: the next chunk leaves after the client ran dry, and
        // plays from when it left.
        p.released(t0 + MS(1500), CHUNK, 24_000);
        assert_eq!(p.end(), Some(t0 + MS(1600)));
        // A purge ends it there, and nothing moves it afterwards.
        p.cut(t0 + MS(1550));
        p.released(t0 + MS(1560), CHUNK, 24_000);
        p.cut(t0 + MS(1570));
        assert_eq!(p.end(), Some(t0 + MS(1550)));
    }

    #[test]
    fn an_answer_shorter_than_the_lead_ends_with_its_audio() {
        // All of it at once: the window is its length, not the lead.
        let t0 = Instant::now();
        let mut p = Playback::new(1);
        for _ in 0..3 {
            p.released(t0, CHUNK, 24_000);
        }
        assert_eq!(p.end(), Some(t0 + MS(300)));
        // A cut before any audio is still an end.
        let mut q = Playback::new(2);
        q.cut(t0);
        assert_eq!((q.first, q.end()), (None, Some(t0)));
    }
}
