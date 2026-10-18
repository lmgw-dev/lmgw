//! The output's Opus encoder (calls design §5.2).
//!
//! - libopus for VoIP, 24 kHz mono input, 20 ms frames.
//! - **Bitrate** `realtime.webrtc_output_kbps`, capped by the phone's
//!   `maxaveragebitrate` when its offer names one within RFC 7587's range
//!   ([`bitrate`]); the cap, and a value outside the range ignored, are the
//!   call's status line's to show.
//! - **In-band FEC on**, the expected loss handed to libopus following the
//!   loss fraction the phone reports in its receiver reports (str0m's
//!   `MediaEgressStats.loss`, [`Encoder::loss`]), so FEC spends nothing on a
//!   clean link.
//! - **DTX only when the phone asks for it** (`usedtx=1` in its offer, RFC
//!   7587: the receiver's preference). Then a frame libopus encodes as DTX —
//!   two bytes or less — is not sent, as libwebrtc does it: its RTP time
//!   still passes, the first frame after the pause carries the marker bit,
//!   and libopus itself sends a comfort-noise update every 400 ms of
//!   silence (it ends a run of DTX frames after 20 of them). Without it,
//!   silence goes out as ordinary frames: libopus 1.6 makes 32 bytes of a
//!   20 ms frame of digital silence at 32 kbps, measured.
//! - **RED** is str0m's, on what is written (module doc of `audio`).

use std::fmt;

use opusic_c::{Application, Bitrate, Channels, InbandFec, SampleRate, Signal};

use super::FRAME_SAMPLES;

/// The largest Opus frame: 1275 bytes (RFC 6716 §3.2.1), what one 20 ms
/// frame can take at libopus's highest bitrate.
const MAX_PACKET: usize = 1275;

/// A DTX frame: libopus encodes a frame of silence under DTX in at most
/// this many bytes, which libwebrtc does not send.
const DTX_BYTES: usize = 2;

/// What `maxaveragebitrate` may be (RFC 7587 §6.1: 6000 to 510000 bits per
/// second, Opus's own range). A value outside it caps nothing: 0 would make
/// no encoder at all, and the call's output silent.
pub(crate) const MAX_AVERAGE_RANGE: std::ops::RangeInclusive<u32> = 6_000..=510_000;

/// The output bitrate a call encodes at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputBitrate {
    /// Bits per second.
    pub bps: u32,
    /// The phone's `maxaveragebitrate`, when it is what set `bps`.
    pub capped_by: Option<u32>,
    /// The phone's `maxaveragebitrate`, when it lay outside
    /// [`MAX_AVERAGE_RANGE`] and was ignored.
    pub ignored: Option<u32>,
}

impl fmt::Display for OutputBitrate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.capped_by {
            Some(cap) => write!(
                f,
                "{} kbps (the phone's maxaveragebitrate {cap}, below realtime.webrtc_output_kbps)",
                self.bps / 1000
            )?,
            None => write!(f, "{} kbps (realtime.webrtc_output_kbps)", self.bps / 1000)?,
        }
        if let Some(v) = self.ignored {
            write!(
                f,
                "; the phone's maxaveragebitrate {v} is ignored, outside RFC 7587's {} to {}",
                MAX_AVERAGE_RANGE.start(),
                MAX_AVERAGE_RANGE.end()
            )?;
        }
        Ok(())
    }
}

/// `realtime.webrtc_output_kbps`, capped by the phone's `maxaveragebitrate`
/// (bits per second) when its offer names one within
/// [`MAX_AVERAGE_RANGE`]; one outside it is ignored, and the call's status
/// line (INFO) says so.
pub(crate) fn bitrate(kbps: u32, max_average: Option<u32>) -> OutputBitrate {
    let bps = kbps.saturating_mul(1000);
    let ignored = max_average.filter(|v| !MAX_AVERAGE_RANGE.contains(v));
    match max_average.filter(|_| ignored.is_none()) {
        Some(cap) if cap < bps => OutputBitrate {
            bps: cap,
            capped_by: Some(cap),
            ignored,
        },
        _ => OutputBitrate {
            bps,
            capped_by: None,
            ignored,
        },
    }
}

/// One frame's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Encoded {
    /// Send it; `talkspurt` sets the marker bit (str0m's
    /// `start_of_talkspurt`): the first frame after a DTX pause.
    Send { payload: Vec<u8>, talkspurt: bool },
    /// A DTX frame: not sent, its RTP time passes.
    Skip,
}

/// A call's encoder (module doc).
pub(crate) struct Encoder {
    opus: opusic_c::Encoder,
    dtx: bool,
    /// The frame before was skipped as DTX.
    paused: bool,
}

impl Encoder {
    /// An encoder at `bitrate`, with DTX when the phone's offer asked for
    /// it.
    pub fn new(bitrate: OutputBitrate, dtx: bool) -> Result<Self, String> {
        let fail = |what: &str, e: opusic_c::ErrorCode| format!("libopus: {what}: {e:?}");
        let mut opus =
            opusic_c::Encoder::new(Channels::Mono, SampleRate::Hz24000, Application::Voip)
                .map_err(|e| fail("could not make an encoder", e))?;
        opus.set_bitrate(Bitrate::Value(bitrate.bps))
            .map_err(|e| fail("the bitrate", e))?;
        opus.set_signal(Signal::Voice)
            .map_err(|e| fail("the signal type", e))?;
        opus.set_inband_fec(InbandFec::Mode1)
            .map_err(|e| fail("in-band FEC", e))?;
        // Nothing is lost until the phone's first receiver report says so.
        opus.set_packet_loss(0)
            .map_err(|e| fail("the expected loss", e))?;
        opus.set_dtx(dtx).map_err(|e| fail("DTX", e))?;
        Ok(Self {
            opus,
            dtx,
            paused: false,
        })
    }

    /// The loss fraction the phone reported (0.0 to 1.0): the loss libopus
    /// expects, in whole percent, so FEC grows with it.
    pub fn loss(&mut self, fraction: f32) -> Result<(), String> {
        let pct = (fraction.clamp(0.0, 1.0) * 100.0).round() as u8;
        self.opus
            .set_packet_loss(pct)
            .map_err(|e| format!("libopus: the expected loss: {e:?}"))
    }

    /// Encode one 20 ms frame of PCM16 at 24 kHz.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Encoded, String> {
        if pcm.len() != FRAME_SAMPLES {
            return Err(format!(
                "a frame is {FRAME_SAMPLES} samples, not {}",
                pcm.len()
            ));
        }
        let input: Vec<u16> = pcm.iter().map(|&s| s as u16).collect();
        let mut out = vec![0u8; MAX_PACKET];
        let n = self
            .opus
            .encode_to_slice(&input, &mut out)
            .map_err(|e| format!("libopus could not encode a frame: {e:?}"))?;
        if self.dtx && n <= DTX_BYTES {
            self.paused = true;
            return Ok(Encoded::Skip);
        }
        out.truncate(n);
        let talkspurt = std::mem::take(&mut self.paused);
        Ok(Encoded::Send {
            payload: out,
            talkspurt,
        })
    }

    /// The settings libopus holds, as it reports them back.
    #[cfg(test)]
    fn settings(&mut self) -> (Bitrate, bool, InbandFec, u8) {
        (
            self.opus.get_bitrate().unwrap(),
            self.opus.get_dtx().unwrap(),
            self.opus.get_inband_fec().unwrap(),
            self.opus.get_packet_loss().unwrap(),
        )
    }
}

#[cfg(test)]
mod tests;
