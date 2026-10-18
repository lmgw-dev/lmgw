//! A WebRTC call's audio (calls design §4.2, §4.4, §5.2, §9): three keys of
//! the `realtime` section (`realtime.webrtc_reorder_ms`,
//! `realtime.webrtc_commit_window_ms`, `realtime.webrtc_output_kbps`), kept
//! here and flattened into [`super::RealtimeSettings`]. A call reads them
//! when it starts, so a change applies to the next call.
//!
//! Each range is a real one, refused by name on a save and clamped on load
//! with a warning ([`WebRtcAudioSettings::clamp_loaded`]):
//! - the reorder window, from 0 (no wait) to [`WEBRTC_REORDER_MAX_MS`]:
//!   str0m holds at most 4096 packets of a stream behind a gap
//!   (`MAX_BUFFERED_PACKETS` in its depacketizer), and the call asks it to
//!   hold the window over the answer's 10 ms `minptime` plus one (§3.5), so
//!   past that the count would bind before the time and str0m would drop
//!   the oldest packet it holds;
//! - the output bitrate, Opus's own range, 6 to 510 kbps (RFC 6716 §2.1.1;
//!   RFC 7587's `maxaveragebitrate` takes the same);
//! - the commit window has none: how long a push-to-talk turn's end waits
//!   is the owner's call.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The longest reorder window str0m can hold a gap for (module doc).
pub const WEBRTC_REORDER_MAX_MS: u32 = (4096 - 1) * 10;

/// Opus's lowest bitrate, in kbps.
pub const WEBRTC_OUTPUT_KBPS_MIN: u32 = 6;

/// Opus's highest bitrate, in kbps.
pub const WEBRTC_OUTPUT_KBPS_MAX: u32 = 510;

const REORDER_WHY: &str = "str0m holds at most 4096 packets of a stream behind a gap, which a \
                           wider window over 10 ms packets would pass";
const KBPS_WHY: &str = "Opus encodes from 6 to 510 kbps";

/// `realtime.webrtc_reorder_ms`, `realtime.webrtc_commit_window_ms` and
/// `realtime.webrtc_output_kbps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebRtcAudioSettings {
    /// How long str0m holds a call's frame behind a missing earlier one, in
    /// milliseconds (§4.2); frames in order are released at once, so the
    /// window costs latency only behind a loss. Concealment of a gap may
    /// cover the wall-clock time that passed plus this. **0 = no wait**: a
    /// frame behind a gap goes on at once, and the gap is concealed.
    pub webrtc_reorder_ms: u32,
    /// How far past its own stamp a push-to-talk `input_audio_buffer.commit`
    /// on a call takes frames, and how long it waits for them, in
    /// milliseconds (§4.4): the phone's capture pipeline still holds the
    /// turn's last audio when the commit leaves it. A push-to-talk turn's
    /// end costs this window. Not tied to `webrtc_reorder_ms`.
    pub webrtc_commit_window_ms: u32,
    /// A call's Opus output bitrate, in kbps (§5.2), capped by the phone's
    /// `maxaveragebitrate` when its offer names one.
    pub webrtc_output_kbps: u32,
}

impl Default for WebRtcAudioSettings {
    fn default() -> Self {
        Self {
            // Three 20 ms packets: the reordering a cellular link shows,
            // short against the silence that ends a turn (§4.2).
            webrtc_reorder_ms: 60,
            // Twice a measured Android capture-to-send offset of about
            // 50–60 ms (§4.4); the test rig and a test phone refine it (WP6).
            webrtc_commit_window_ms: 120,
            // libwebrtc's own for mono speech (§5.2).
            webrtc_output_kbps: 32,
        }
    }
}

impl WebRtcAudioSettings {
    /// The reorder window.
    pub fn reorder(&self) -> Duration {
        Duration::from_millis(self.webrtc_reorder_ms.into())
    }

    /// The commit window.
    pub fn commit_window(&self) -> Duration {
        Duration::from_millis(self.webrtc_commit_window_ms.into())
    }

    /// Set `realtime.webrtc_reorder_ms` from a save, refusing one past its
    /// range by name.
    pub fn set_reorder_ms(&mut self, v: u32) -> Result<(), String> {
        if v > WEBRTC_REORDER_MAX_MS {
            return Err(format!(
                "realtime.webrtc_reorder_ms is from 0 (no wait) to {WEBRTC_REORDER_MAX_MS}: \
                 {REORDER_WHY}; got {v}"
            ));
        }
        self.webrtc_reorder_ms = v;
        Ok(())
    }

    /// Set `realtime.webrtc_output_kbps` from a save, refusing one outside
    /// Opus's range by name.
    pub fn set_output_kbps(&mut self, v: u32) -> Result<(), String> {
        if !(WEBRTC_OUTPUT_KBPS_MIN..=WEBRTC_OUTPUT_KBPS_MAX).contains(&v) {
            return Err(format!(
                "realtime.webrtc_output_kbps is from {WEBRTC_OUTPUT_KBPS_MIN} to \
                 {WEBRTC_OUTPUT_KBPS_MAX}: {KBPS_WHY}; got {v}"
            ));
        }
        self.webrtc_output_kbps = v;
        Ok(())
    }

    /// Stored values a save would refuse now (written by hand) are clamped
    /// into range on load, with a warning naming the setting.
    pub fn clamp_loaded(&mut self) {
        let clamp = |key: &str, v: &mut u32, min: u32, max: u32, why: &str| {
            let fixed = (*v).clamp(min, max);
            if fixed != *v {
                tracing::warn!(
                    "stored setting realtime.{key} = {} is out of range ({min} to {max}: \
                     {why}); using {fixed} until it is saved again",
                    *v
                );
                *v = fixed;
            }
        };
        clamp(
            "webrtc_reorder_ms",
            &mut self.webrtc_reorder_ms,
            0,
            WEBRTC_REORDER_MAX_MS,
            REORDER_WHY,
        );
        clamp(
            "webrtc_output_kbps",
            &mut self.webrtc_output_kbps,
            WEBRTC_OUTPUT_KBPS_MIN,
            WEBRTC_OUTPUT_KBPS_MAX,
            KBPS_WHY,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_designs_and_a_save_refuses_by_name() {
        let mut w = WebRtcAudioSettings::default();
        assert_eq!(
            (w.reorder(), w.commit_window(), w.webrtc_output_kbps),
            (Duration::from_millis(60), Duration::from_millis(120), 32)
        );
        w.set_reorder_ms(0).unwrap();
        w.set_reorder_ms(WEBRTC_REORDER_MAX_MS).unwrap();
        let e = w.set_reorder_ms(WEBRTC_REORDER_MAX_MS + 1).unwrap_err();
        assert!(
            e.contains("realtime.webrtc_reorder_ms") && e.contains("40951"),
            "{e}"
        );
        assert_eq!(w.webrtc_reorder_ms, WEBRTC_REORDER_MAX_MS, "kept");
        for ok in [6, 32, 510] {
            w.set_output_kbps(ok).unwrap();
        }
        for bad in [0, 5, 511] {
            let e = w.set_output_kbps(bad).unwrap_err();
            assert!(e.contains("realtime.webrtc_output_kbps"), "{e}");
        }
    }

    #[test]
    fn a_stored_value_out_of_range_is_clamped_on_load() {
        let mut w: WebRtcAudioSettings = serde_json::from_str(
            r#"{"webrtc_reorder_ms": 90000, "webrtc_output_kbps": 2, "webrtc_commit_window_ms": 0}"#,
        )
        .unwrap();
        w.clamp_loaded();
        assert_eq!(
            w,
            WebRtcAudioSettings {
                webrtc_reorder_ms: WEBRTC_REORDER_MAX_MS,
                webrtc_commit_window_ms: 0,
                webrtc_output_kbps: 6
            }
        );
        let mut w: WebRtcAudioSettings =
            serde_json::from_str(r#"{"webrtc_output_kbps": 900}"#).unwrap();
        w.clamp_loaded();
        assert_eq!(w.webrtc_output_kbps, 510);
        assert_eq!(w.webrtc_reorder_ms, 60, "absent: the default");
    }
}
