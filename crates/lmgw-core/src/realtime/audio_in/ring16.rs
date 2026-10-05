//! The 16 kHz audio Smart Turn scores (realtime design §6.3): the
//! streaming resampler's output, which already feeds Silero, kept as it
//! comes — before the VAD's level normalizer, which Smart Turn must not
//! see (it normalizes its own window). Scoring from here spares a 24 → 16
//! kHz resample of the 8 s excerpt per score (13 ms, owner's decision).
//!
//! The stream is aligned to the input timeline: its sample `j` (counted
//! from the detector's restart at timeline sample `base`) stands for
//! timeline sample `base + 1.5 j`. A request's span on the timeline maps
//! to two thirds of its offset.
//!
//! **What is kept** is what the open pause's request can still ask for:
//! Smart Turn's 8 s before the pause began, and the pause itself — so a
//! request that comes late, after the word check let a turn go on (§6.4),
//! finds its audio. Between turns it is the last 8 s. Memory follows the
//! pause, never a parameter.

use std::collections::VecDeque;

use super::super::turn::mel::WINDOW_SAMPLES;

#[derive(Debug, Default)]
pub(super) struct Ring16 {
    ring: VecDeque<f32>,
    /// Stream index of `ring[0]`.
    first: u64,
    /// The timeline sample stream index 0 stands for.
    base: u64,
}

impl Ring16 {
    /// The stream restarts at timeline sample `base` (`Dsp::restart`).
    pub fn restart(&mut self, base: u64) {
        self.ring = VecDeque::new();
        self.first = 0;
        self.base = base;
    }

    /// The resampler's next output.
    pub fn push(&mut self, samples: &[f32]) {
        self.ring.extend(samples);
    }

    /// The stream moved on by `n` samples nobody will score (plain
    /// `server_vad`): nothing is kept, and the stream stays in step with the
    /// timeline for a switch to `semantic_vad`.
    pub fn skip(&mut self, n: usize) {
        self.first += (self.ring.len() + n) as u64;
        self.ring.clear();
    }

    /// Stream index of timeline sample `s` (before the restart: 0).
    fn index(&self, s: u64) -> u64 {
        (u128::from(s.saturating_sub(self.base)) * 2 / 3) as u64
    }

    /// The kept audio of the timeline span `start..end`.
    pub fn span(&self, start: u64, end: u64) -> Vec<f32> {
        let last = self.first + self.ring.len() as u64;
        let at = |s: u64| (self.index(s).clamp(self.first, last) - self.first) as usize;
        let (from, to) = (at(start), at(end));
        self.ring.range(from..to.max(from)).copied().collect()
    }

    /// Keep what a request for a pause that began at timeline sample
    /// `pause` may ask for — `None` between pauses: the last 8 s.
    pub fn trim(&mut self, pause: Option<u64>) {
        let end = self.first + self.ring.len() as u64;
        let from = pause.map_or(end, |p| self.index(p));
        let keep = from.saturating_sub(WINDOW_SAMPLES as u64);
        if keep > self.first {
            let n = ((keep - self.first) as usize).min(self.ring.len());
            self.ring.drain(..n);
            self.first += n as u64;
        }
    }

    /// Gives back room beyond twice what is kept (`AudioIn::release`).
    pub fn release(&mut self) {
        let want = self.ring.len().max(WINDOW_SAMPLES);
        if self.ring.capacity() > 2 * want {
            self.ring.shrink_to(want);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_map_the_timeline_to_the_stream() {
        let mut r = Ring16::default();
        r.restart(2400);
        r.push(&(0..32_000).map(|i| i as f32).collect::<Vec<_>>());
        // Timeline 2400 + 1.5 j is stream j.
        assert_eq!(
            r.span(2400 + 3000, 2400 + 3006),
            vec![2000.0, 2001.0, 2002.0, 2003.0]
        );
        // Before the restart and past the end clamp to what is there.
        assert_eq!(r.span(0, 2403), vec![0.0, 1.0]);
        assert_eq!(r.span(2400 + 47_997, 99_999_999), vec![31_998.0, 31_999.0]);
    }

    #[test]
    fn it_keeps_8_s_before_the_pause() {
        let mut r = Ring16::default();
        r.restart(0);
        r.push(&vec![0.5; 20 * 16_000]);
        // A pause that began at 15 s of the timeline: keep from 7 s on.
        r.trim(Some(15 * 24_000));
        assert_eq!(r.first, 7 * 16_000);
        assert_eq!(r.span(7 * 24_000, 15 * 24_000).len(), 8 * 16_000);
        // Between pauses: the last 8 s.
        r.trim(None);
        assert_eq!(r.first, 12 * 16_000);
        assert_eq!(r.ring.len(), WINDOW_SAMPLES);
        // Skipped audio keeps the stream in step with the timeline.
        r.skip(16_000);
        r.push(&[1.0, 2.0]);
        assert_eq!(r.span(21 * 24_000, 21 * 24_000 + 3), vec![1.0, 2.0]);
    }
}
