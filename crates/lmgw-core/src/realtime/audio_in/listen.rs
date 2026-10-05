//! What the detector hears while the session speaks (realtime design §6.4,
//! §6.5): each Silero frame's capture instant, whether it fell inside the
//! client's playing window, and the arbiter that decides who judges it —
//! the detector, or the barge-in gate (`turn::arbiter`).
//!
//! The core says, per append, what it knows of its output ([`Listen`]): the
//! latest playing window and whether a response the client has heard is in
//! progress. The window is the writer's record of when the answer's audio
//! left (`writer::playback`) — wall-clock time — and the input is samples,
//! so every frame is placed on the wall clock first (`timeline`), by when
//! its append arrived. The window's end already carries the margin for the
//! transit both ways (`lifecycle::interrupt`).
//!
//! **Where the window began on the input timeline** — what the gate's guard
//! counts from — is estimated from every frame inside it, not fixed by the
//! first (B3 review 9): each frame says "the window opened this long before
//! me". A capture estimate is never early — the client cannot send audio
//! before it records it, and a burst is placed at the previous append's
//! arrival at the earliest (`timeline`) — only late, by transit and stalls.
//! A late estimate puts the start too early and shortens the guard, so the
//! latest start any frame gives is the best, and one stalled frame no
//! longer shortens the guard for the whole window. The gate keys its window
//! on the response generation, so a refined start is the same window.
//!
//! **A turn is never ended by the window of the response it cuts** (live
//! run 2, H1). The frames of one append are all judged against the window
//! the core saw when the append came in, and the core acts on a turn's
//! start only after the whole append: a turn that started just before an
//! answer's first audio, with the rest of the append inside that answer's
//! window, was cut short there by half duplex — before the core had even
//! cancelled the answer — and committed as a fragment ("I've"). So the
//! listener remembers which response the turn's start cancels
//! ([`Listen::cuts`]), and that response's window — before the cancel, and
//! up to its margin after it — is no window for that turn.

use std::time::Duration;

use tokio::time::Instant;

use super::super::turn::arbiter::{Arbiter, Checked, Context, Outcome, Verdict, WindowFrame};
use super::super::turn::barge_in::BargeInParams;
use super::super::turn::server_vad::{ServerVad, TurnEvent};
use super::timeline::Timeline;

/// The playing window as the core sees it for one append (§6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlayView {
    /// The response generation that plays.
    pub gen: u64,
    /// When its first audio left: where the window opens.
    pub first: Instant,
    /// Where it ends. `None` while the response is still producing audio:
    /// open-ended, so a gap while the next clause is synthesized stays
    /// inside the window — the client is mid-answer.
    pub end: Option<Instant>,
}

/// What the core knows of its output, for one append.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Listen {
    /// The latest playing window, if any audio of it has left.
    pub view: Option<PlayView>,
    /// A response is in progress and the client has heard some of it: a
    /// turn that starts now interrupts what the user was listening to
    /// (§6.5, B3 review 4).
    pub heard: bool,
    /// The generation a turn that starts now cancels: the active response,
    /// when a cancel still changes something and `interrupt_response` is
    /// on (module doc, `lifecycle::interrupt`).
    pub cuts: Option<u64>,
}

/// The barge-in half of the input (module doc).
#[derive(Debug)]
pub(super) struct Listener {
    timeline: Timeline,
    arbiter: Arbiter,
    /// Where the window of a generation began on the input timeline (ms):
    /// the latest estimate any of its frames gave (module doc).
    started: Option<(u64, u64)>,
    /// The previous frame's capture instant.
    last: Option<Instant>,
    /// Where the gate triggered the turn whose words are being checked: the
    /// cut is judged there (`turn::arbiter::check`).
    check_at: Option<Instant>,
    /// The generation the open turn's start cancels: its window is none of
    /// that turn's (module doc).
    cutting: Option<u64>,
    rate: u64,
}

impl Listener {
    pub fn new(rate: u32) -> Self {
        Self {
            timeline: Timeline::new(rate),
            arbiter: Arbiter::new(BargeInParams::default(), false, rate),
            started: None,
            last: None,
            check_at: None,
            cutting: None,
            rate: u64::from(rate.max(1)),
        }
    }

    /// The session's barge-in knobs (§6.4, §12).
    pub fn configure(&mut self, params: BargeInParams, half_duplex: bool, words: bool) {
        self.arbiter.set_params(params, half_duplex);
        self.arbiter.set_words(words);
    }

    /// Samples `from..end` arrived at `at`.
    pub fn stamp(&mut self, from: u64, end: u64, at: Instant) {
        self.timeline.stamp(from, end, at);
    }

    /// Judge the frame `from..to` (probability `p`): what it did, and when
    /// it was captured. `arrived` stands in for a capture the timeline does
    /// not know.
    pub fn frame(
        &mut self,
        vad: &mut ServerVad,
        p: f32,
        (from, to): (u64, u64),
        listen: &Listen,
        arrived: Instant,
    ) -> (Outcome, Instant) {
        let at = self.timeline.capture(to).unwrap_or(arrived);
        let gap = Duration::from_millis(u64::from(self.arbiter.gap_ms()));
        let wall_gap = self
            .last
            .is_some_and(|last| at.saturating_duration_since(last) > gap);
        self.last = Some(at);
        let open = vad.in_speech();
        if !open {
            self.cutting = None;
        }
        let window = listen.view.and_then(|v| {
            let inside = at >= v.first && v.end.is_none_or(|end| at < end);
            // The open turn cancels this response (module doc).
            let own = open && self.cutting == Some(v.gen);
            (inside && !own).then(|| WindowFrame {
                window: v.gen,
                started_ms: self.started_ms(&v, to, at),
            })
        });
        let cx = Context {
            window,
            wall_gap,
            heard: listen.heard,
        };
        let out = self.arbiter.frame(vad, p, from, to, cx);
        if out.check.as_ref().is_some_and(|c| c.trigger) {
            self.check_at = Some(at);
        }
        let started = out
            .judged
            .iter()
            .chain(&out.stopped)
            .any(|j| matches!(j.event, TurnEvent::SpeechStarted { .. }));
        if started {
            self.cutting = listen.cuts;
        }
        (out, at)
    }

    /// Whether word check `id` is the one its turn waits for.
    pub fn awaits(&self, id: u64) -> bool {
        self.arbiter.awaits(id)
    }

    /// The word check `id` decided `verdict`: what that did, and where the
    /// gate had triggered its turn.
    pub fn checked(
        &mut self,
        vad: &mut ServerVad,
        id: u64,
        verdict: Verdict,
    ) -> (Checked, Option<Instant>) {
        (self.arbiter.checked(vad, id, verdict), self.check_at)
    }

    /// The window's start on the input timeline: the frame ending at `to`
    /// was captured `at`, this long after the window opened — the latest
    /// such estimate of the window (module doc).
    fn started_ms(&mut self, v: &PlayView, to: u64, at: Instant) -> u64 {
        let into = at.saturating_duration_since(v.first).as_millis() as u64;
        let ms = (u128::from(to) * 1000 / u128::from(self.rate)) as u64;
        let ms = ms.saturating_sub(into);
        let best = match self.started {
            Some((g, kept)) if g == v.gen => kept.max(ms),
            _ => ms,
        };
        self.started = Some((v.gen, best));
        best
    }

    /// Everything up to sample `s` is scored.
    pub fn scored(&mut self, s: u64) {
        self.timeline.forget_before(s);
    }

    /// The gate's evidence is gone (a commit took the turn).
    pub fn reset_gate(&mut self) {
        self.arbiter.reset();
    }

    /// A clear, a mode switch, a detector that went down: nothing heard so
    /// far counts.
    pub fn reset(&mut self) {
        self.arbiter.reset();
        self.timeline.clear();
        self.started = None;
        self.last = None;
        self.cutting = None;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_window_s_start_is_the_latest_estimate_any_frame_gives() {
        // B3 review 9: the first frame inside the window was stamped 300 ms
        // late (a stall), which alone would start the window 300 ms early
        // on the input timeline and cut the guard short by that much.
        let mut l = Listener::new(24_000);
        let t0 = Instant::now();
        let view = PlayView {
            gen: 1,
            first: t0,
            end: None,
        };
        let ms = |ms: u64| ms * 24;
        // Frame ending at 2000 ms of input, captured "800 ms" into the
        // window (500 true + 300 stall): the window began at 1200.
        let late = l.started_ms(&view, ms(2000), t0 + Duration::from_millis(800));
        assert_eq!(late, 1200);
        // The next frame, on time: 2032 ms captured 532 ms in — 1500.
        let on_time = l.started_ms(&view, ms(2032), t0 + Duration::from_millis(532));
        assert_eq!(on_time, 1500);
        // Another stall does not move it back.
        let again = l.started_ms(&view, ms(2064), t0 + Duration::from_millis(900));
        assert_eq!(again, 1500);
        // A new window starts afresh.
        let next = PlayView { gen: 2, ..view };
        assert_eq!(
            l.started_ms(&next, ms(9000), t0 + Duration::from_millis(1000)),
            8000
        );
    }
}
