//! Positions in lmgw's own clock: the line its sender reports lie on
//! (calls design §5.3, §5.4, §16).
//!
//! A bound client names what the phone plays by `playout_ntp_ms`:
//! libwebrtc's `estimatedPlayoutTimestamp`, the NTP time of the sample
//! playing now, in lmgw's clock, because libwebrtc maps the inbound RTP
//! through lmgw's RTCP sender reports. lmgw maps it back through the same
//! reports to an RTP time, and so to a frame of the record (`record`).
//!
//! **One report is enough.** str0m makes each sender report at the `now` it
//! sends it at: the NTP time is its process-wide fixed `Instant` →
//! wall-clock mapping of that `now`, the RTP time the last written frame's
//! carried forward by `now` minus that frame's wallclock. The frame clock
//! writes every frame with its slot's due time and RTP time (`clock`), so
//! every report lies on one line, and one report lmgw sent fixes it. str0m
//! has no accessor for it: the wiring reads one from
//! `Event::RawPacket(RawPacket::RtcpTx(Rtcp::SenderReport(..)))`, with
//! `RtcConfig::enable_raw_packets` on (copies of every packet both ways,
//! for one Opus stream about 100 of about 100 bytes a second; checked in
//! WP2 part A's `realtime_calls_net::sender_reports`). A later report
//! replaces the line, and lands on it.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

use super::record::Record;
use super::RTP_RATE;

/// From the NTP epoch (1900) to the Unix one (1970), in milliseconds.
const NTP_UNIX_MS: f64 = 2_208_988_800_000.0;

/// `t` in NTP milliseconds, as `playout_ntp_ms` carries it.
pub(crate) fn ntp_ms(t: SystemTime) -> f64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => NTP_UNIX_MS + d.as_secs_f64() * 1000.0,
        Err(e) => NTP_UNIX_MS - e.duration().as_secs_f64() * 1000.0,
    }
}

/// The line lmgw's sender reports lie on (module doc).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SentLine {
    ntp_ms: f64,
    rtp: u64,
}

impl SentLine {
    /// One sender report lmgw sent: its NTP time and its RTP time. The
    /// report's RTP time is 32 bits on the wire, so it is matched modulo
    /// 2^32 with `near`, the latest RTP time the frame clock wrote.
    pub fn new(ntp: SystemTime, rtp: u64, near: u64) -> Self {
        Self {
            ntp_ms: ntp_ms(ntp),
            rtp: extend(rtp, near),
        }
    }

    /// The RTP time that `ntp_ms`, a time in lmgw's NTP clock, names on the
    /// line; `None` for one before RTP time 0 or no number.
    pub fn rtp(&self, ntp_ms: f64) -> Option<u64> {
        let ticks = (ntp_ms - self.ntp_ms) * f64::from(RTP_RATE) / 1000.0;
        let rtp = self.rtp as f64 + ticks.round();
        (rtp.is_finite() && rtp >= 0.0).then_some(rtp as u64)
    }
}

/// `rtp`'s low 32 bits, extended to the value nearest `near`.
fn extend(rtp: u64, near: u64) -> u64 {
    const WRAP: u64 = 1 << 32;
    let c = (near & !(WRAP - 1)) | (rtp & (WRAP - 1));
    [c.checked_sub(WRAP), Some(c), c.checked_add(WRAP)]
        .into_iter()
        .flatten()
        .min_by_key(|v| v.abs_diff(near))
        .unwrap_or(c)
}

/// Where a position lands on the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Located {
    /// The RTP time it names.
    pub rtp: u64,
    /// When that sample left (`Record::sent_at`).
    pub sent: Instant,
}

/// Why a position lands nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unmapped {
    /// No sender report lmgw sent has been read yet: there is no line.
    NoLine,
    /// Before the stream's first frame.
    BeforeStream { rtp: Option<u64> },
    /// At or past the end of what was sent.
    PastEnd { rtp: u64, end: u64 },
    /// In a slot a late tick skipped: never sent (`Record::skipped`).
    NotSent { rtp: u64 },
}

impl fmt::Display for Unmapped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoLine => write!(
                f,
                "no sender report of lmgw's is known yet to map playout_ntp_ms through"
            ),
            Self::BeforeStream { rtp } => match rtp {
                Some(r) => write!(f, "it names RTP time {r}, before the output's first frame"),
                None => write!(f, "it names a time before the output's RTP clock began"),
            },
            Self::PastEnd { rtp, end } => write!(
                f,
                "it names RTP time {rtp}, past the last frame sent (which ends at {end})"
            ),
            Self::NotSent { rtp } => write!(
                f,
                "it names RTP time {rtp}, in a frame a late tick skipped, never sent"
            ),
        }
    }
}

/// Where `playout_ntp_ms` lands: the RTP time it names on `line` and when
/// that sample left.
pub(crate) fn locate(
    line: Option<&SentLine>,
    record: &Record,
    playout_ntp_ms: f64,
) -> Result<Located, Unmapped> {
    let line = line.ok_or(Unmapped::NoLine)?;
    let rtp = line
        .rtp(playout_ntp_ms)
        .ok_or(Unmapped::BeforeStream { rtp: None })?;
    if rtp < record.start_rtp() {
        return Err(Unmapped::BeforeStream { rtp: Some(rtp) });
    }
    if record.was_skipped(rtp) {
        return Err(Unmapped::NotSent { rtp });
    }
    match record.sent_at(rtp) {
        Some(sent) => Ok(Located { rtp, sent }),
        None => Err(Unmapped::PastEnd {
            rtp,
            end: record.end().unwrap_or(record.start_rtp()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_time_in_lmgws_clock_maps_to_the_rtp_time_on_the_line() {
        let report = UNIX_EPOCH + Duration::from_secs(1_760_000_000);
        // 2026: a little under 3.97e12 NTP milliseconds.
        let at = ntp_ms(report);
        assert!((3.96e12..3.98e12).contains(&at), "{at}");
        // The report's RTP time, 32 bits on the wire: matched to the
        // clock's own, five wraps on.
        let near = (5u64 << 32) + 100_000;
        let line = SentLine::new(report, 90_000, near);
        assert_eq!(line.rtp(at), Some((5 << 32) + 90_000));
        assert_eq!(line.rtp(at + 20.0), Some((5 << 32) + 90_960), "20 ms on");
        assert_eq!(line.rtp(at - 1.0), Some((5 << 32) + 89_952));
        assert_eq!(line.rtp(f64::NAN), None);
        // Across the wrap: a report just before it, the clock just past.
        assert_eq!(
            extend(u64::from(u32::MAX) - 10, (1 << 32) + 5),
            (1 << 32) - 11
        );
        assert_eq!(extend(3, (1 << 32) - 5), (1 << 32) + 3);
        assert_eq!(extend(7, 100), 7);
    }

    #[test]
    fn a_position_lands_on_a_frame_or_says_why_not() {
        use crate::realtime::calls::audio::clock::FrameClock;
        use crate::realtime::calls::audio::frames::OutFrame;
        let t0 = Instant::now();
        let mut clock = FrameClock::new(t0, 48_000);
        let mut record = Record::new(t0, 48_000);
        let silence = OutFrame {
            pcm: vec![0; 480],
            response_id: None,
            spans: Vec::new(),
            padded: 0,
            starved: None,
            started: None,
        };
        for k in 0..5 {
            let slot = clock.take(t0 + Duration::from_millis(20 * k)).unwrap();
            record.sent(slot, &silence, slot.due);
        }
        let ntp0 = 3_970_000_000_000.0;
        // lmgw's report said: NTP ntp0 is RTP 48 000, the stream's start.
        let line = SentLine {
            ntp_ms: ntp0,
            rtp: 48_000,
        };
        assert_eq!(locate(None, &record, ntp0), Err(Unmapped::NoLine));
        assert_eq!(
            locate(Some(&line), &record, ntp0 + 30.0),
            Ok(Located {
                rtp: 48_000 + 1440,
                sent: t0 + Duration::from_millis(30)
            })
        );
        assert_eq!(
            locate(Some(&line), &record, ntp0 - 5.0),
            Err(Unmapped::BeforeStream {
                rtp: Some(48_000 - 240)
            })
        );
        let past = locate(Some(&line), &record, ntp0 + 100.0).unwrap_err();
        assert_eq!(
            past,
            Unmapped::PastEnd {
                rtp: 48_000 + 4800,
                end: 48_000 + 4800
            }
        );
        assert!(
            past.to_string().contains("past the last frame sent"),
            "{past}"
        );
    }
}
