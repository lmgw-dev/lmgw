//! Decoding a call's Opus onto the session's timeline (calls design §4.1,
//! §4.2).
//!
//! **The decoder** is libopus at 24 kHz mono, the session's one rate:
//! libopus resamples internally (no rubato stage, no added delay) and
//! downmixes a stereo stream. Samples come back as `u16`; one cast makes
//! them PCM16. The RTP clock is 48 kHz whatever the decode rate.
//!
//! **Gaps.** str0m holds frames behind a missing packet until it arrives or
//! the reorder window (`realtime.webrtc_reorder_ms`) passes; the window is
//! str0m's, and only its length is read here. A frame whose RTP time is
//! past the one expected meets a gap:
//! - **within the wall-clock bound** — the arrival gap between the frame
//!   before and this one, plus the reorder window — the gap becomes
//!   samples, so the timeline stays aligned with RTP time: the last missing
//!   frame from this packet's in-band FEC, every earlier one by libopus's
//!   concealment (which fades to silence by itself). A gap with no packet
//!   missing (`contiguous`: the phone skipped frames, DTX) is concealed
//!   whole, since no packet's FEC describes it;
//! - **beyond it**, a sender discontinuity (a clock reset, a replaced
//!   track): the timeline gets the wall-clock gap's worth of concealment —
//!   the arrival gap less the frame before's own length, the time in which
//!   nothing arrived — and no more, the stamps re-anchor on this frame
//!   (`stamp`), and the reader says so at INFO ([`Gap`]'s `Display`).
//!
//! **Behind the timeline.** A frame whose RTP time is behind the one
//! expected, within the wall-clock bound, is a late packet whose frame was
//! already concealed — dropped, and counted at DEBUG — unless its sequence
//! number is newer than the frame before's: then the sender's RTP clock
//! jumped backwards, a discontinuity like a forward one. Further behind
//! than the bound it is a discontinuity whatever its sequence number: no
//! packet is that late here (str0m already drops a packet whose sequence
//! number is behind the last frame it released, whenever it holds frames
//! back at all, and the call's hold-back is never 0, `calls::offer`), and a
//! stream that restarts with both its RTP time and its sequence numbers
//! behind would otherwise be dropped for good.
//!
//! **A payload libopus cannot read** is treated as lost: nothing comes out
//! and the timeline does not move, so the next frame's gap covers it.

use std::fmt;
use std::time::Duration;

use opusic_c::{Channels, Decoder, SampleRate};
use tokio::time::Instant;

use super::stamp::Stamps;
use super::{
    duration_to_ticks, samples_to_ticks, ticks_to_duration, ticks_to_samples, FRAME_SAMPLES,
    PCM_RATE,
};

/// The shortest run libopus conceals or recovers: 2.5 ms (its `frame_size`
/// for a lost packet must be a multiple of Fs/400). A shorter remainder of
/// a gap is filled with zeros, so the timeline stays sample-exact.
const PLC_STEP: usize = (PCM_RATE / 400) as usize;

/// One frame as the call's reader receives it (str0m's `MediaData`, passed
/// on as WP2's `calls::Frame`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct InFrame<'a> {
    /// Its RTP time, extended past the 32-bit wrap, in 48 kHz ticks.
    pub rtp: u64,
    /// When its first packet came off the socket.
    pub arrived: Instant,
    /// No packet is missing between it and the frame before it.
    pub contiguous: bool,
    /// Its first extended RTP sequence number.
    pub seq: u64,
    /// The Opus payload.
    pub payload: &'a [u8],
}

/// What became of a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// PCM for the session: `Incoming::Audio`.
    Audio(Decoded),
    /// Behind the timeline: dropped (module doc).
    Late(Late),
    /// libopus could not read it: treated as lost (module doc).
    Invalid(String),
}

/// A frame decoded onto the timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decoded {
    /// PCM16 at 24 kHz: what filled the gap before the frame, if any, then
    /// the frame.
    pub pcm: Vec<i16>,
    /// When the last sample was captured (§4.3): what `AudioIn::append`
    /// takes as its `arrived`.
    pub captured: Instant,
    /// What filled the timeline before the frame.
    pub gap: Option<Gap>,
}

/// What filled the timeline between the frame before and this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gap {
    /// Missing audio within the wall-clock bound: `concealed` samples by
    /// concealment, then `fec` samples from the frame's in-band FEC (0 when
    /// no packet was missing or the packet carried none).
    Filled {
        missing: Duration,
        concealed: u64,
        fec: u64,
    },
    /// A jump of the sender's RTP clock past `bound`, the wall-clock time
    /// that allowed, while `wall` passed with nothing arriving: `concealed`
    /// samples, the stamps re-anchored.
    Discontinuity {
        jump: Jump,
        bound: Duration,
        wall: Duration,
        concealed: u64,
    },
}

/// Which way the sender's RTP clock jumped, and how far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Jump {
    Forward(Duration),
    Backward(Duration),
}

impl fmt::Display for Gap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = |samples: u64| samples * 1000 / u64::from(PCM_RATE);
        match self {
            Self::Filled {
                missing,
                concealed,
                fec,
            } => write!(
                f,
                "{} ms of input missing: {} ms concealed, {} ms from the next packet's FEC",
                missing.as_millis(),
                ms(*concealed),
                ms(*fec)
            ),
            Self::Discontinuity {
                jump,
                bound,
                wall,
                concealed,
            } => {
                let (way, by) = match jump {
                    Jump::Forward(d) => ("forward", d),
                    Jump::Backward(d) => ("backward", d),
                };
                write!(
                    f,
                    "the phone's RTP clock jumped {way} by {} ms, past the {} ms the wall clock \
                     allows (the arrival gap plus realtime.webrtc_reorder_ms); {} ms passed with \
                     nothing arriving, so {} ms were concealed and the input re-anchored on the \
                     frame after the jump",
                    by.as_millis(),
                    bound.as_millis(),
                    wall.as_millis(),
                    ms(*concealed)
                )
            }
        }
    }
}

/// A late frame, dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Late {
    /// How far behind the timeline its RTP time was.
    pub behind: Duration,
    /// Late frames dropped on this call so far, this one included.
    pub dropped: u64,
}

impl fmt::Display for Late {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a frame {} ms behind the input arrived after its time was concealed: dropped ({} \
             so far)",
            self.behind.as_millis(),
            self.dropped
        )
    }
}

/// The frame before: where the timeline expects the next, and what the
/// next is judged by.
#[derive(Debug, Clone, Copy)]
struct Last {
    /// Its end's RTP time: where the next frame should start.
    next: u64,
    arrived: Instant,
    seq: u64,
    /// Its length in RTP ticks.
    ticks: u64,
}

/// One call's decoder (module doc).
pub(crate) struct Decode {
    opus: Decoder,
    /// `realtime.webrtc_reorder_ms`: part of the wall-clock bound.
    reorder: Duration,
    last: Option<Last>,
    stamps: Stamps,
    late: u64,
    /// libopus's concealment steps so far.
    #[cfg(test)]
    plc_steps: u64,
}

impl Decode {
    /// A decoder for a call whose reorder window is `reorder`.
    pub fn new(reorder: Duration) -> Result<Self, String> {
        let opus = Decoder::new(Channels::Mono, SampleRate::Hz24000)
            .map_err(|e| format!("libopus could not make a decoder: {e:?}"))?;
        Ok(Self {
            opus,
            reorder,
            last: None,
            stamps: Stamps::default(),
            late: 0,
            #[cfg(test)]
            plc_steps: 0,
        })
    }

    /// Late frames dropped so far.
    #[cfg(test)]
    pub fn dropped(&self) -> u64 {
        self.late
    }

    /// Decode `f` onto the timeline (module doc).
    pub fn frame(&mut self, f: InFrame<'_>) -> Outcome {
        let samples = match self.opus.get_nb_samples(f.payload) {
            Ok(n) if n > 0 && n <= MAX_FRAME => n,
            Ok(n) => return Outcome::Invalid(format!("an Opus packet of {n} samples")),
            Err(e) => return Outcome::Invalid(format!("not an Opus packet libopus reads: {e:?}")),
        };
        let mut pcm = Vec::new();
        let gap = match self.last {
            None => {
                self.stamps.anchor(f.rtp, f.arrived);
                None
            }
            Some(last) if f.rtp < last.next => {
                let behind = ticks_to_duration(last.next - f.rtp);
                let bound = self.bound(&f, last);
                if behind <= bound && f.seq <= last.seq {
                    self.late += 1;
                    return Outcome::Late(Late {
                        behind,
                        dropped: self.late,
                    });
                }
                Some(self.discontinuity(&mut pcm, &f, last, Jump::Backward(behind), bound))
            }
            Some(last) if f.rtp == last.next => None,
            Some(last) => {
                let missing = f.rtp - last.next;
                let bound = self.bound(&f, last);
                if ticks_to_duration(missing) <= bound {
                    Some(self.fill(&mut pcm, &f, missing, samples))
                } else {
                    let jump = Jump::Forward(ticks_to_duration(missing));
                    Some(self.discontinuity(&mut pcm, &f, last, jump, bound))
                }
            }
        };
        let gap = match gap.transpose() {
            Ok(g) => g,
            Err(e) => return Outcome::Invalid(e),
        };
        let mut buf = [0u16; MAX_FRAME];
        let n = match self
            .opus
            .decode_to_slice(f.payload, &mut buf[..samples], false)
        {
            Ok(n) => n,
            Err(e) => return Outcome::Invalid(format!("libopus could not decode it: {e:?}")),
        };
        pcm.extend(buf[..n].iter().map(|&s| s as i16));
        let ticks = samples_to_ticks(n as u64);
        let end = f.rtp + ticks;
        let captured = self.stamps.received(end, f.arrived);
        self.last = Some(Last {
            next: end,
            arrived: f.arrived,
            seq: f.seq,
            ticks,
        });
        Outcome::Audio(Decoded { pcm, captured, gap })
    }

    /// The wall-clock time a gap before `f` may fill (§4.2).
    fn bound(&self, f: &InFrame<'_>, last: Last) -> Duration {
        f.arrived.saturating_duration_since(last.arrived) + self.reorder
    }

    /// A gap of `missing` RTP ticks within the bound, before `f` of
    /// `samples` samples: concealment, then FEC for the last missing frame
    /// when a packet was lost.
    fn fill(
        &mut self,
        pcm: &mut Vec<i16>,
        f: &InFrame<'_>,
        missing: u64,
        samples: usize,
    ) -> Result<Gap, String> {
        let gap = ticks_to_samples(missing);
        let fec_len = if !f.contiguous && gap >= samples as u64 {
            samples
        } else {
            0
        };
        let concealed = gap - fec_len as u64;
        self.conceal(pcm, concealed)?;
        let mut fec = 0;
        if fec_len > 0 {
            let mut buf = [0u16; MAX_FRAME];
            // libopus decodes the frame before from this packet's FEC, or
            // conceals it when the packet carries none.
            let n = self
                .opus
                .decode_to_slice(f.payload, &mut buf[..fec_len], true)
                .map_err(|e| format!("libopus could not recover from FEC: {e:?}"))?;
            pcm.extend(buf[..n].iter().map(|&s| s as i16));
            pcm.resize(pcm.len() + (fec_len - n), 0);
            if has_fec(f.payload) {
                fec = fec_len as u64;
            }
        }
        Ok(Gap::Filled {
            missing: ticks_to_duration(missing),
            concealed: concealed + (fec_len as u64 - fec),
            fec,
        })
    }

    /// A jump past the bound: the wall-clock gap concealed, the stamps
    /// re-anchored on `f`.
    fn discontinuity(
        &mut self,
        pcm: &mut Vec<i16>,
        f: &InFrame<'_>,
        last: Last,
        jump: Jump,
        bound: Duration,
    ) -> Result<Gap, String> {
        let wall = f
            .arrived
            .saturating_duration_since(last.arrived)
            .saturating_sub(ticks_to_duration(last.ticks));
        let concealed = ticks_to_samples(duration_to_ticks(wall));
        self.conceal(pcm, concealed)?;
        self.stamps.anchor(f.rtp, f.arrived);
        Ok(Gap::Discontinuity {
            jump,
            bound,
            wall,
            concealed,
        })
    }

    /// `n` samples of libopus's concealment, in steps it takes; a remainder
    /// shorter than [`PLC_STEP`] as zeros, and so is the rest of a long gap
    /// once libopus's concealment has faded to silence (a step of nothing
    /// but zeros): what it would make from there is zeros too, and a gap of
    /// minutes would otherwise cost 50 calls a second of it.
    fn conceal(&mut self, pcm: &mut Vec<i16>, n: u64) -> Result<(), String> {
        let mut left = usize::try_from(n).map_err(|_| format!("a gap of {n} samples"))?;
        pcm.reserve(left);
        let mut buf = [0u16; FRAME_SAMPLES];
        while left >= PLC_STEP {
            let step = left.min(FRAME_SAMPLES) / PLC_STEP * PLC_STEP;
            let got = self
                .opus
                .decode_to_slice(&[], &mut buf[..step], false)
                .map_err(|e| format!("libopus could not conceal a lost frame: {e:?}"))?;
            if got == 0 {
                break;
            }
            #[cfg(test)]
            {
                self.plc_steps += 1;
            }
            pcm.extend(buf[..got].iter().map(|&s| s as i16));
            left -= got.min(left);
            if buf[..got].iter().all(|&s| s == 0) {
                break;
            }
        }
        pcm.resize(pcm.len() + left, 0);
        Ok(())
    }
}

/// The longest frame an Opus packet holds: 120 ms (RFC 6716 §3.2.1), in
/// samples at the decode rate.
const MAX_FRAME: usize = (PCM_RATE as usize) * 120 / 1000;

/// Whether an Opus packet carries in-band FEC (LBRR) for the frame before
/// it.
fn has_fec(payload: &[u8]) -> bool {
    let Ok(len) = i32::try_from(payload.len()) else {
        return false;
    };
    // SAFETY: libopus reads `len` bytes from `payload`, a live slice of
    // exactly that length, and writes nothing.
    unsafe { opusic_c::sys::opus_packet_has_lbrr(payload.as_ptr(), len) > 0 }
}

#[cfg(test)]
mod tests;
