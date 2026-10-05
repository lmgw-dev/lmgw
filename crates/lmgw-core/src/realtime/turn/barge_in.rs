//! The barge-in evidence gate (realtime §6.4).
//!
//! Why it exists: clients stop playback on `speech_started`, so while the
//! client is playing that event must not fire on a cough or a short noise.
//! Inside the **playing window** (from the first audio delta until the
//! client's modelled playback end, §6.4, §8.2) speech has to earn the
//! interruption.
//! This is an earlier prototype's evidence accumulator, generalised from ASR
//! tokens to VAD-only evidence:
//! - voiced time accumulates;
//! - unvoiced time that reaches the gap (800 ms) throws the evidence away;
//! - nothing counts in the first `guard_ms` of playback (500 ms);
//! - at `min_ms` of evidence (200 ms, E7) it triggers, back-dated to the
//!   first voiced frame of the run.
//!
//! Outside the window it never triggers and holds no state: the normal
//! detector ([`super::server_vad`]) owns speech there. After a trigger it
//! stays quiet for the rest of that window — the turn it started is the
//! detector's, and the arbiter ([`super::arbiter`]) resets it for the next
//! one. `min_ms = 0` and `guard_ms = 0` make every voiced frame in the
//! window trigger, i.e. plain OpenAI behaviour (§6.2 parity).
//!
//! **Echo is not solved here.** Silero scores echoed speech as speech; the
//! gate only filters short noises and relies on the client's echo
//! cancellation (§6.4).

/// Gate parameters (milliseconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BargeInParams {
    /// Voiced evidence needed to interrupt (`barge_in_min_ms`, 200).
    pub min_ms: u32,
    /// Start of playback in which nothing counts (`barge_in_guard_ms`, 500).
    pub guard_ms: u32,
    /// Unvoiced time that resets the evidence (prototype: 800).
    pub gap_reset_ms: u32,
}

impl Default for BargeInParams {
    fn default() -> Self {
        Self {
            min_ms: 200,
            guard_ms: 500,
            gap_reset_ms: 800,
        }
    }
}

/// One VAD frame as the gate sees it. Times are milliseconds on the
/// session's input timeline; the session maps the playback start onto it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BargeInFrame {
    /// Silero said speech (probability ≥ the session's threshold).
    pub voiced: bool,
    /// The frame's length (32 for Silero).
    pub frame_ms: u32,
    /// The frame's end.
    pub now_ms: u64,
    /// The client is inside the playing window.
    pub playing: bool,
    /// Which window: a new value is a new one (the session's response
    /// generation).
    pub window: u64,
    /// When this playback started: the guard counts from here. An
    /// estimate, which may be refined within the same window.
    pub playback_started_ms: u64,
}

/// The gate fired: cancel the response; the user's turn starts at `onset_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trigger {
    pub onset_ms: u64,
}

/// The earlier prototype's evidence accumulator over the playing window.
#[derive(Debug, Clone, Default)]
pub struct BargeIn {
    params: BargeInParams,
    window: Option<u64>,
    fired: bool,
    evidence_ms: u64,
    quiet_ms: u64,
    onset_ms: Option<u64>,
}

impl BargeIn {
    pub fn new(params: BargeInParams) -> Self {
        Self {
            params,
            ..Self::default()
        }
    }

    /// Feeds one frame; returns a trigger at most once per playback window.
    pub fn push(&mut self, f: BargeInFrame) -> Option<Trigger> {
        if !f.playing {
            self.reset();
            return None;
        }
        if self.window != Some(f.window) {
            self.reset();
            self.window = Some(f.window);
        }
        if self.fired {
            return None;
        }
        let frame_start = f.now_ms.saturating_sub(u64::from(f.frame_ms));
        let guard_end = f
            .playback_started_ms
            .saturating_add(u64::from(self.params.guard_ms));
        if frame_start < guard_end {
            // The guard: echo of the first syllables and the user's own
            // reaction to the voice starting do not count.
            return None;
        }
        if f.voiced {
            let onset = *self.onset_ms.get_or_insert(frame_start);
            self.evidence_ms += u64::from(f.frame_ms);
            self.quiet_ms = 0;
            if self.evidence_ms >= u64::from(self.params.min_ms) {
                self.clear_evidence();
                self.fired = true;
                return Some(Trigger { onset_ms: onset });
            }
        } else if self.onset_ms.is_some() {
            self.quiet_ms += u64::from(f.frame_ms);
            if self.quiet_ms >= u64::from(self.params.gap_reset_ms) {
                self.clear_evidence();
            }
        }
        None
    }

    /// The onset a trigger would be back-dated to, if evidence is pending
    /// (the session holds that audio, see `ServerVad::hold_from`).
    pub fn pending_onset_ms(&self) -> Option<u64> {
        self.onset_ms
    }

    /// Evidence so far in the current window.
    pub fn evidence_ms(&self) -> u64 {
        self.evidence_ms
    }

    /// Forgets the window and any evidence.
    pub fn reset(&mut self) {
        self.clear_evidence();
        self.window = None;
        self.fired = false;
    }

    fn clear_evidence(&mut self) {
        self.evidence_ms = 0;
        self.quiet_ms = 0;
        self.onset_ms = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds 32 ms frames from `start_ms` with playback started at `play`;
    /// returns (frame index, trigger).
    fn feed(
        g: &mut BargeIn,
        start_ms: u64,
        play: Option<u64>,
        voiced: &[bool],
    ) -> Vec<(usize, Trigger)> {
        voiced
            .iter()
            .enumerate()
            .filter_map(|(i, &v)| {
                let now_ms = start_ms + 32 * (i as u64 + 1);
                g.push(BargeInFrame {
                    voiced: v,
                    frame_ms: 32,
                    now_ms,
                    playing: play.is_some(),
                    window: play.unwrap_or(0),
                    playback_started_ms: play.unwrap_or(0),
                })
                .map(|t| (i, t))
            })
            .collect()
    }

    /// The knobs these tests' frame counts were written for: 300 ms of
    /// evidence, the default before E7 lowered it to 200.
    fn at_300() -> BargeInParams {
        BargeInParams {
            min_ms: 300,
            ..BargeInParams::default()
        }
    }

    fn run(pattern: &[(bool, usize)]) -> Vec<bool> {
        pattern
            .iter()
            .flat_map(|&(v, n)| std::iter::repeat_n(v, n))
            .collect()
    }

    #[test]
    fn speech_after_the_guard_triggers_back_dated() {
        let mut g = BargeIn::new(at_300());
        // Playback at 1000; quiet until 1512, then speech: 10 frames are
        // 320 ms >= 300, so the trigger comes on the 10th voiced frame.
        let t = feed(&mut g, 1000, Some(1000), &run(&[(false, 16), (true, 20)]));
        assert_eq!(t, [(25, Trigger { onset_ms: 1512 })]);
    }

    #[test]
    fn nothing_counts_during_the_guard() {
        let mut g = BargeIn::new(at_300());
        // Echo-like voice for the whole 500 ms guard: no evidence.
        assert!(feed(&mut g, 0, Some(0), &[true; 15]).is_empty());
        assert_eq!(g.evidence_ms(), 0);
        // A run that continues past the guard counts from the guard's end
        // only: onset at 512 (frame 16 starts there), trigger 10 frames on.
        let mut g = BargeIn::new(at_300());
        let t = feed(&mut g, 0, Some(0), &[true; 40]);
        assert_eq!(t, [(25, Trigger { onset_ms: 512 })]);
    }

    #[test]
    fn coughs_below_the_minimum_do_not_trigger() {
        let mut g = BargeIn::new(at_300());
        // Two 128 ms coughs a second apart: the 800 ms gap resets between.
        let t = feed(
            &mut g,
            0,
            Some(0),
            &run(&[(false, 20), (true, 4), (false, 30), (true, 4), (false, 30)]),
        );
        assert!(t.is_empty());
        assert_eq!(g.pending_onset_ms(), None, "reset by the gap");
    }

    #[test]
    fn short_gaps_accumulate_long_gaps_reset() {
        let mut g = BargeIn::new(at_300());
        // 5 + 5 voiced frames with a 640 ms pause: 320 ms of evidence,
        // back-dated to the first burst (frame 20 starts at 640).
        let t = feed(
            &mut g,
            0,
            Some(0),
            &run(&[(false, 20), (true, 5), (false, 20), (true, 5)]),
        );
        assert_eq!(t, [(49, Trigger { onset_ms: 640 })]);
        // With an 800 ms pause the first burst is forgotten.
        let mut g = BargeIn::new(at_300());
        let t = feed(
            &mut g,
            0,
            Some(0),
            &run(&[(false, 20), (true, 5), (false, 25), (true, 5)]),
        );
        assert!(t.is_empty());
        assert_eq!(g.evidence_ms(), 160);
        assert_eq!(g.pending_onset_ms(), Some(50 * 32));
    }

    #[test]
    fn outside_the_window_it_never_triggers_and_forgets() {
        let mut g = BargeIn::new(at_300());
        assert!(feed(&mut g, 0, None, &[true; 50]).is_empty());
        // Evidence from one window does not leak into the next.
        feed(&mut g, 0, Some(0), &run(&[(false, 16), (true, 5)]));
        assert_eq!(g.evidence_ms(), 160);
        feed(&mut g, 672, None, &[false]);
        assert_eq!(g.evidence_ms(), 0);
        // A new playback start is a new window with its own guard.
        feed(&mut g, 704, Some(704), &[true; 5]);
        assert_eq!(g.evidence_ms(), 0, "inside the new guard");
    }

    #[test]
    fn fires_once_per_window() {
        let mut g = BargeIn::new(at_300());
        assert_eq!(
            feed(&mut g, 0, Some(0), &run(&[(false, 16), (true, 40)])).len(),
            1
        );
        assert!(feed(&mut g, 1792, Some(0), &[true; 40]).is_empty());
        // The next response's playback can be interrupted again.
        assert_eq!(
            feed(&mut g, 4000, Some(4000), &run(&[(false, 16), (true, 10)])).len(),
            1
        );
    }

    #[test]
    fn the_default_catches_the_owner_s_quick_stopp() {
        // E7: the owner's "Stopp" has 256 ms of voice and "Stop" 288 — under the old
        // 300 ms the gate never opened for them. At 200 the seventh voiced
        // frame (224 ms) triggers.
        let mut g = BargeIn::new(BargeInParams::default());
        let t = feed(&mut g, 1000, Some(1000), &run(&[(false, 16), (true, 8)]));
        assert_eq!(t, [(22, Trigger { onset_ms: 1512 })]);
        let mut g = BargeIn::new(at_300());
        assert!(feed(&mut g, 1000, Some(1000), &run(&[(false, 16), (true, 8)])).is_empty());
    }

    #[test]
    fn zero_parameters_are_plain_behaviour() {
        let mut g = BargeIn::new(BargeInParams {
            min_ms: 0,
            guard_ms: 0,
            gap_reset_ms: 800,
        });
        assert_eq!(
            feed(&mut g, 0, Some(0), &[false, true]),
            [(1, Trigger { onset_ms: 32 })]
        );
    }
}
