//! A WebRTC call's audio (calls design §4, §5; WP3): Opus in and out, and
//! the clocks that place both on the session's wall clock.
//!
//! **In** (§4). The call's reader hands each Opus frame str0m depacketized
//! to [`decode`], which decodes it at 24 kHz mono onto the session's one
//! timeline: a gap from in-band FEC and concealment within the wall-clock
//! time that passed, a larger jump as a discontinuity, a late packet
//! dropped. [`stamp`] places each frame on the wall clock by its RTP time
//! (§4.3), and what comes out is `Incoming::Audio { pcm, captured }`, the
//! input `Core::on_audio` takes. [`commit`] holds a push-to-talk commit and
//! a clear until the audio they cut has arrived (§4.4); [`capture`] says at
//! DEBUG how long after their capture the frames arrived, by the client's
//! sender reports, beside the commit window that has to cover it.
//!
//! **Out** (§5). [`clock`] ticks every 20 ms on RTP time for the call's
//! whole life; [`frames`] takes each tick's 20 ms from the writer's paced
//! queue (padding an underrun, saying when it starves); [`encode`] makes
//! the Opus payload (FEC tuned to the phone's loss, DTX on `usedtx=1`, the
//! bitrate). **RED is str0m's**: `RtcConfig::enable_opus(true, red)` folds a
//! RED payload type onto Opus, its send side wraps every written frame with
//! the previous one (`set_red_distances`, one level by default), so the
//! frame clock writes plain Opus either way. [`record`] keeps what each
//! frame carried and when it left; [`line`] maps lmgw's own sender reports;
//! [`playout`] turns the phone's playout reports, or the formula, into the
//! window's end, drained and the margin (§5.3); [`heard`] cuts what was
//! heard (§5.4); [`rtcp`] reads the phone's receiver reports into the
//! round trip, the jitter and the loss.
//!
//! **Wired** (WP3 slice B): the call's reader (`calls::link`) decodes,
//! stamps and orders the input; [`out`] runs the frame clock as a task of
//! the call's link, pulling the writer's paced queue (`writer::Pulled`, which
//! keeps the record, the playout and heard under the queue's lock).

use std::time::Duration;

pub(crate) mod capture;
pub(crate) mod clock;
pub(crate) mod commit;
pub(crate) mod decode;
pub(crate) mod encode;
#[cfg(test)]
pub(crate) mod fixtures;
pub(crate) mod frames;
pub(crate) mod heard;
pub(crate) mod line;
pub(crate) mod out;
pub(crate) mod playout;
pub(crate) mod record;
pub(crate) mod rtcp;
pub(crate) mod stamp;

/// The RTP clock of Opus, whatever the decode rate (RFC 7587 §4.1).
pub(crate) const RTP_RATE: u32 = 48_000;

/// The session's one PCM rate, in and out (realtime §2.2), which libopus
/// decodes to and encodes from without a resampler (§4.1).
pub(crate) const PCM_RATE: u32 = super::super::protocol::PCM_RATE;

/// The frame both directions use: 20 ms (§4.1, §5.1).
pub(crate) const FRAME: Duration = Duration::from_millis(20);

/// [`FRAME`] in RTP ticks.
pub(crate) const FRAME_TICKS: u64 = 960;

/// [`FRAME`] in PCM samples.
pub(crate) const FRAME_SAMPLES: usize = 480;

/// RTP ticks per PCM sample.
const TICKS_PER_SAMPLE: u64 = (RTP_RATE / PCM_RATE) as u64;

/// `ticks` of the RTP clock as a duration.
pub(crate) fn ticks_to_duration(ticks: u64) -> Duration {
    let nanos = u128::from(ticks) * 1_000_000_000 / u128::from(RTP_RATE);
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// `d` in whole RTP ticks, rounded down.
pub(crate) fn duration_to_ticks(d: Duration) -> u64 {
    let ticks = d.as_nanos() * u128::from(RTP_RATE) / 1_000_000_000;
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

/// RTP ticks as PCM samples.
pub(crate) fn ticks_to_samples(ticks: u64) -> u64 {
    ticks / TICKS_PER_SAMPLE
}

/// PCM samples as RTP ticks.
pub(crate) fn samples_to_ticks(samples: u64) -> u64 {
    samples * TICKS_PER_SAMPLE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clocks_agree() {
        assert_eq!(ticks_to_duration(FRAME_TICKS), FRAME);
        assert_eq!(duration_to_ticks(FRAME), FRAME_TICKS);
        assert_eq!(ticks_to_samples(FRAME_TICKS), FRAME_SAMPLES as u64);
        assert_eq!(samples_to_ticks(FRAME_SAMPLES as u64), FRAME_TICKS);
        assert_eq!(PCM_RATE, 24_000);
    }
}
