//! How long after its capture each of the client's frames arrives, by the
//! client's own RTCP sender reports (calls design §4.4, §13.2): the offset
//! `realtime.webrtc_commit_window_ms` has to cover.
//!
//! A sender report pairs the client's NTP time with the RTP time of the
//! sample captured then (libwebrtc maps its RTP clock to the capture time),
//! so a frame's last sample was captured at the report's NTP time plus its
//! RTP distance from the report. The frame's arrival on lmgw's wall clock
//! less that capture is its capture to arrival: the client's capture
//! pipeline and framing, its send, and the network's transit.
//!
//! It is meaningful only where the client's clock is lmgw's — one host, as
//! in the interop rig (`scripts/webrtc-interop.sh`), or a phone whose
//! NTP-synced clock agrees within its sync error — so it is a DEBUG line,
//! never an input of anything: the least, the median and the most over the
//! frames of each [`EVERY`].

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

use super::RTP_RATE;
use crate::realtime::calls::handle::SenderReport;

/// How often the line is said: the frames between two lines are its
/// sample. An estimator's horizon, as realtime §6.4's evidence gap is: it
/// bounds, drops and delays nothing.
pub(crate) const EVERY: Duration = Duration::from_secs(5);

/// One window's figures, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Summary {
    pub frames: usize,
    pub least: f64,
    pub median: f64,
    pub most: f64,
}

/// The estimator (module doc).
#[derive(Debug, Default)]
pub(crate) struct CaptureOffsets {
    /// The client's latest sender report: its NTP time in µs since the Unix
    /// epoch, and its RTP time's low 32 bits.
    report: Option<(i128, u32)>,
    /// The offsets of the window's frames, in µs.
    window: Vec<i128>,
    since: Option<Instant>,
}

impl CaptureOffsets {
    /// The client's sender report `sr`.
    pub fn report(&mut self, sr: &SenderReport) {
        self.report = Some((micros(sr.ntp_time), sr.rtp_time as u32));
    }

    /// A frame whose first sample has RTP time `rtp` and which is `ticks`
    /// of the RTP clock long arrived at `arrived`: once a window is over,
    /// its figures.
    pub fn frame(&mut self, rtp: u64, ticks: u64, arrived: Instant) -> Option<Summary> {
        let (ntp, sr_rtp) = self.report?;
        // The frame's last sample, from the report, modulo 2^32 (str0m
        // extends the two on anchors of their own).
        let ahead = i128::from((rtp.wrapping_add(ticks) as u32).wrapping_sub(sr_rtp) as i32);
        let captured = ntp + ahead * 1_000_000 / i128::from(RTP_RATE);
        let now = Instant::now();
        let at =
            micros(SystemTime::now()) - now.saturating_duration_since(arrived).as_micros() as i128;
        self.window.push(at - captured);
        let since = *self.since.get_or_insert(arrived);
        if arrived.saturating_duration_since(since) < EVERY {
            return None;
        }
        self.since = Some(arrived);
        summary(std::mem::take(&mut self.window))
    }
}

/// `t` in µs since the Unix epoch, negative before it.
fn micros(t: SystemTime) -> i128 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_micros() as i128,
        Err(e) => -(e.duration().as_micros() as i128),
    }
}

fn summary(mut v: Vec<i128>) -> Option<Summary> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let ms = |us: i128| us as f64 / 1000.0;
    Some(Summary {
        frames: v.len(),
        least: ms(v[0]),
        median: ms(v[v.len() / 2]),
        most: ms(v[v.len() - 1]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame captured at the report's NTP time arrives 40 ms after its
    /// last sample: the line says 40 ms whatever the wrap of the RTP clock.
    #[tokio::test]
    async fn a_frame_arrives_its_offset_after_its_last_samples_capture() {
        let mut c = CaptureOffsets::default();
        assert_eq!(c.frame(0, 960, Instant::now()), None, "no report yet");
        // The first frame arrived one window ago; the report names its RTP
        // time 60 ms before that on the wall clock, so its last sample,
        // 20 ms later, was captured 40 ms before it arrived. The RTP time
        // wraps within the frame.
        let then = Instant::now().checked_sub(EVERY).unwrap();
        let wall_then = SystemTime::now() - EVERY;
        let sr_rtp = u64::from(u32::MAX) - 479;
        c.report(&SenderReport {
            ntp_time: wall_then - Duration::from_millis(60),
            rtp_time: sr_rtp,
            received: then,
        });
        assert_eq!(c.frame(sr_rtp, 960, then), None, "the window runs");
        // The second is one window of RTP later and arrives now.
        let s = c
            .frame(sr_rtp + 960 * 250, 960, Instant::now())
            .expect("a window is over");
        assert_eq!(s.frames, 2);
        for v in [s.least, s.median, s.most] {
            assert!((v - 40.0).abs() < 25.0, "{s:?}");
        }
    }
}
