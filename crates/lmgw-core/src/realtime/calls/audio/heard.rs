//! What the listener heard of a cut answer on a call (calls design §5.4,
//! C11).
//!
//! On a cancel, a barge-in, a stop or a `clear` the server stops sending at
//! once; what NetEQ holds still plays on a client that does not mute
//! (libwebrtc offers no flush of a remote track). Heard, per item, as the
//! heard table cuts it (`HeardTable::keep`, samples at 24 kHz) — the shape
//! `WriterHandle::purge`'s `Sent::audio` has:
//!
//! - **at a position** ([`at_position`]): a bound client's stop with
//!   `playout_ntp_ms` says what the phone played when it muted. It is
//!   mapped through lmgw's sender reports to an RTP time, found in the
//!   record, and every item cut at the sample playing there; padding counts
//!   as nothing, and an item whose first frame lies after the position was
//!   not heard at all. A position past the last frame sent is clamped to it
//!   with a DEBUG line: heard never exceeds sent;
//! - **on a barge-in of a bound call** ([`on_barge_in`]): the phone mutes
//!   when `output_audio_buffer.cleared` reaches it, half a round trip after
//!   the cut, and plays a sample half a round trip plus the allowance after
//!   it left, so it played up to the sample that left the allowance before
//!   the cut (`Record::rtp_at`), clamped to what was sent: a cut while the
//!   answer still leaves takes the allowance off what left by then, one
//!   confirmed after its last frame left takes less, or nothing;
//! - **otherwise** ([`sent`]): a stop before the phone had a position, a
//!   stock client's `response.cancel` or `clear`, and a barge-in of an
//!   unbound call hear what was sent — exact here, counted by the frame
//!   (`frames::Frames::purge` answers the same).

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use tokio::time::Instant;

use super::line::{locate, SentLine, Unmapped};
use super::record::Record;
use super::{duration_to_ticks, ticks_to_samples, FRAME_SAMPLES, FRAME_TICKS};

/// Samples heard per item of the latest response, every item it sent
/// named (0: none of it).
pub(crate) type Heard = HashMap<String, u64>;

/// A position past what was sent, clamped (DEBUG).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Clamped {
    pub rtp: u64,
    pub end: u64,
}

impl fmt::Display for Clamped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a stop's playout_ntp_ms names RTP time {}, past the last frame sent (ending at {}): \
             heard is clamped to what was sent",
            self.rtp, self.end
        )
    }
}

/// What the phone heard of the latest response when it played the sample
/// at RTP time `rtp`: each item up to that sample.
pub(crate) fn at_rtp(record: &Record, rtp: u64) -> Heard {
    let mut heard = Heard::new();
    for f in record.frames() {
        for s in &f.spans {
            heard.entry(s.item_id.clone()).or_insert(0);
        }
    }
    for f in record.frames().iter().take_while(|f| f.rtp < rtp) {
        let played = ticks_to_samples(rtp - f.rtp).min(FRAME_SAMPLES as u64);
        for s in &f.spans {
            let n = played.saturating_sub(s.at as u64).min(s.to - s.from);
            if n > 0 {
                heard.insert(s.item_id.clone(), s.from + n);
            }
        }
    }
    heard
}

/// Heard at a bound client's stop that carried `playout_ntp_ms` (module
/// doc), with the DEBUG note of a clamp. `Err` when the position lands
/// nowhere it can be used — no sender report to map through, or before the
/// stream — and heard is then what was sent.
pub(crate) fn at_position(
    record: &Record,
    line: Option<&SentLine>,
    playout_ntp_ms: f64,
) -> Result<(Heard, Option<Clamped>), Unmapped> {
    match locate(line, record, playout_ntp_ms) {
        Ok(at) => Ok((at_rtp(record, at.rtp), None)),
        Err(Unmapped::PastEnd { rtp, end }) => {
            Ok((at_rtp(record, end), Some(Clamped { rtp, end })))
        }
        // A skipped slot carried no audio: heard is what came before it.
        Err(Unmapped::NotSent { rtp }) => Ok((at_rtp(record, rtp), None)),
        Err(e) => Err(e),
    }
}

/// Heard on a barge-in of a bound call cut at `cut`: up to the sample that
/// left the playout allowance before it (module doc), in whole samples,
/// clamped to what was sent.
pub(crate) fn on_barge_in(record: &Record, allowance: Duration, cut: Instant) -> Heard {
    let rtp = cut
        .checked_sub(allowance)
        .and_then(|t| record.rtp_at(t))
        .unwrap_or(0)
        .min(last_end(record));
    at_rtp(record, rtp)
}

/// The playout allowance in whole samples at 24 kHz, rounded down: what a
/// barge-in's log line names it as.
pub(crate) fn allowance_samples(allowance: Duration) -> u64 {
    ticks_to_samples(duration_to_ticks(allowance))
}

/// Everything of the latest response that was sent: what
/// `frames::Frames::purge` answers, the check `on_barge_in` is held to.
#[cfg(test)]
pub(crate) fn sent(record: &Record) -> Heard {
    at_rtp(record, last_end(record))
}

/// The RTP time after the latest response's last frame.
fn last_end(record: &Record) -> u64 {
    record.frames().last().map_or(0, |f| f.rtp + FRAME_TICKS)
}

#[cfg(test)]
mod tests;
