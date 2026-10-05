//! PCM16 and WAV conversion at the edges of the realtime session.
//!
//! Why this exists: audio crosses three borders in a voice turn, each with
//! its own encoding.
//! - **Client → server:** `input_audio_buffer.append` carries base64
//!   PCM16-LE at 24 kHz (realtime §2.2, §4.1). The bytes are client
//!   controlled, so a bad alphabet or an odd byte count is an error the
//!   session echoes, never a panic.
//! - **Server → ASR:** the committed segment goes up as a 16 kHz mono PCM16
//!   WAV (realtime §4.2), written by [`write_wav_pcm16_mono`].
//! - **TTS → server:** audio.cpp answers with a WAV even when `pcm` is asked
//!   for, so the header is always parsed to learn the model's native rate
//!   (realtime §8.2). [`parse_wav`] accepts PCM16 and IEEE float32 with any
//!   channel count (downmixed to mono), skips unknown chunks, and tolerates
//!   streaming headers whose `data` size is 0 or `0xFFFFFFFF`.
//!
//! Allocation is always bounded by the bytes actually present, never by a
//! size a header claims.

use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use base64::Engine as _;
use std::fmt;

/// Standard alphabet; padding is optional on decode (some clients strip it)
/// and always written on encode, which is what OpenAI's SDKs expect.
const B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Errors for client-supplied PCM (realtime §4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PcmError {
    /// The payload is not valid base64.
    Base64(String),
    /// PCM16 needs an even number of bytes; this many were decoded.
    OddByteCount(usize),
}

impl fmt::Display for PcmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Base64(e) => write!(f, "audio is not valid base64: {e}"),
            Self::OddByteCount(n) => {
                write!(f, "PCM16 audio needs an even number of bytes, got {n}")
            }
        }
    }
}

impl std::error::Error for PcmError {}

/// Decodes `input_audio_buffer.append`'s `audio` field: base64 of PCM16-LE.
pub fn decode_pcm16(b64: &str) -> Result<Vec<i16>, PcmError> {
    let bytes = B64
        .decode(b64)
        .map_err(|e| PcmError::Base64(e.to_string()))?;
    pcm16_from_le_bytes(&bytes)
}

/// PCM16-LE bytes to samples. An odd trailing byte is an error rather than
/// silently dropped: it means the client's framing is wrong.
pub fn pcm16_from_le_bytes(bytes: &[u8]) -> Result<Vec<i16>, PcmError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(PcmError::OddByteCount(bytes.len()));
    }
    let (pairs, _) = bytes.as_chunks::<2>();
    Ok(pairs.iter().map(|&b| i16::from_le_bytes(b)).collect())
}

/// Encodes samples as base64 PCM16-LE (`response.output_audio.delta`).
pub fn encode_pcm16(samples: &[i16]) -> String {
    encode_pcm16_le(&pcm16_to_le_bytes(samples))
}

/// Samples as PCM16-LE bytes — how paced output audio waits in the writer
/// until it is due (realtime §8.2).
pub fn pcm16_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    bytes
}

/// PCM16-LE bytes as base64: [`encode_pcm16`] for audio already in bytes.
pub fn encode_pcm16_le(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

/// A raised-cosine fade over the first and the last `n` samples (at most
/// half the clip each): the first and last samples become 0, the length
/// stays. Why: a TTS clause that starts at full amplitude at sample 0 clicks
/// where it joins the clause before (realtime §8.2, §23 L11 — the German
/// Pocket voice did, and the ASR lost the first word until it was padded).
pub fn fade_edges(samples: &mut [i16], n: usize) {
    let n = n.min(samples.len() / 2);
    let last = samples.len().saturating_sub(1);
    for i in 0..n {
        let gain = 0.5 - 0.5 * (std::f32::consts::PI * i as f32 / n as f32).cos();
        for at in [i, last - i] {
            samples[at] = (f32::from(samples[at]) * gain).round() as i16;
        }
    }
}

/// PCM16 to float in `[-1, 1)`, scaled by 32768 so the round trip through
/// [`f32_to_pcm16`] is exact.
pub fn pcm16_to_f32(samples: &[i16]) -> Vec<f32> {
    samples.iter().map(|&s| f32::from(s) / 32768.0).collect()
}

/// Float to PCM16 with rounding and saturation. NaN becomes 0.
pub fn f32_to_pcm16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        // `as` saturates and maps NaN to 0, so no input can panic here.
        .map(|&x| (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
        .collect()
}

/// A decoded WAV, downmixed to mono.
#[derive(Debug, Clone, PartialEq)]
pub struct Wav {
    /// Mono samples in `[-1, 1]` (float WAVs may exceed it; non-finite
    /// values are replaced by 0 so they cannot poison a resampler or VAD).
    pub samples: Vec<f32>,
    /// The header's sample rate (never 0).
    pub rate: u32,
    /// Channel count of the source before the downmix.
    pub channels: u16,
}

/// Errors for WAV parsing and writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WavError {
    /// No `RIFF`/`RF64` magic or no `WAVE` form type.
    NotWave,
    /// A non-`data` chunk claims more bytes than remain.
    Truncated { chunk: String },
    /// The `fmt ` chunk is shorter than its 16 mandatory bytes.
    FmtTooShort(usize),
    /// No `fmt ` chunk before the audio ends.
    MissingFmt,
    /// No `data` chunk.
    MissingData,
    /// Only PCM 16-bit and IEEE float 32-bit are decoded.
    Unsupported { format_tag: u16, bits: u16 },
    /// The header declares zero channels.
    ZeroChannels,
    /// The header declares a zero sample rate.
    ZeroRate,
    /// A WAV's sizes are 32-bit; this much audio does not fit one.
    TooLong { samples: usize },
}

impl fmt::Display for WavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotWave => write!(f, "not a RIFF/WAVE file"),
            Self::Truncated { chunk } => write!(f, "WAV chunk '{chunk}' is truncated"),
            Self::FmtTooShort(n) => write!(f, "WAV fmt chunk has {n} bytes, needs 16"),
            Self::MissingFmt => write!(f, "WAV has no fmt chunk"),
            Self::MissingData => write!(f, "WAV has no data chunk"),
            Self::Unsupported { format_tag, bits } => write!(
                f,
                "WAV format {format_tag:#06x} with {bits} bits is not supported \
                 (PCM 16-bit or IEEE float 32-bit)"
            ),
            Self::ZeroChannels => write!(f, "WAV declares zero channels"),
            Self::ZeroRate => write!(f, "WAV declares a zero sample rate"),
            Self::TooLong { samples } => {
                write!(f, "{samples} samples do not fit a WAV's 32-bit sizes")
            }
        }
    }
}

impl std::error::Error for WavError {}

#[derive(Clone, Copy)]
enum SampleKind {
    I16,
    F32,
}

struct Fmt {
    kind: SampleKind,
    channels: u16,
    rate: u32,
}

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn parse_fmt(body: &[u8]) -> Result<Fmt, WavError> {
    if body.len() < 16 {
        return Err(WavError::FmtTooShort(body.len()));
    }
    let mut tag = u16_at(body, 0);
    let channels = u16_at(body, 2);
    let rate = u32_at(body, 4);
    let bits = u16_at(body, 14);
    // WAVE_FORMAT_EXTENSIBLE: the real tag is the first two bytes of the
    // sub-format GUID at offset 24 (Python's soundfile writes these).
    if tag == WAVE_FORMAT_EXTENSIBLE && body.len() >= 26 {
        tag = u16_at(body, 24);
    }
    let kind = match (tag, bits) {
        (WAVE_FORMAT_PCM, 16) => SampleKind::I16,
        (WAVE_FORMAT_IEEE_FLOAT, 32) => SampleKind::F32,
        (format_tag, bits) => return Err(WavError::Unsupported { format_tag, bits }),
    };
    if channels == 0 {
        return Err(WavError::ZeroChannels);
    }
    if rate == 0 {
        return Err(WavError::ZeroRate);
    }
    Ok(Fmt {
        kind,
        channels,
        rate,
    })
}

/// Parses a RIFF/WAVE (or RF64) file into mono samples and its rate.
///
/// Leniency, all deliberate for TTS output (realtime §8.2):
/// - a `data` size of 0 or `0xFFFFFFFF` (streaming writers) or larger than
///   the file means "the rest of the file";
/// - a trailing partial frame is dropped;
/// - unknown chunks are skipped, `RIFF` sizes are ignored.
pub fn parse_wav(bytes: &[u8]) -> Result<Wav, WavError> {
    let (fmt, data) = locate(bytes)?;
    Ok(Wav {
        samples: decode_frames(data, &fmt),
        rate: fmt.rate,
        channels: fmt.channels,
    })
}

/// The length of a WAV [`parse_wav`] would read, in milliseconds — from its
/// header and data size alone, nothing decoded (a dictation's `audio_ms`).
pub fn wav_duration_ms(bytes: &[u8]) -> Result<u64, WavError> {
    let (fmt, data) = locate(bytes)?;
    let width: u64 = match fmt.kind {
        SampleKind::I16 => 2,
        SampleKind::F32 => 4,
    };
    let frames = data.len() as u64 / (width * u64::from(fmt.channels));
    Ok(frames * 1000 / u64::from(fmt.rate))
}

/// The `fmt ` chunk and the `data` bytes, by [`parse_wav`]'s leniency.
fn locate(bytes: &[u8]) -> Result<(Fmt, &[u8]), WavError> {
    if bytes.len() < 12
        || !(&bytes[0..4] == b"RIFF" || &bytes[0..4] == b"RF64")
        || &bytes[8..12] != b"WAVE"
    {
        return Err(WavError::NotWave);
    }
    let mut pos = 12usize;
    let mut fmt: Option<Fmt> = None;
    let mut data: Option<&[u8]> = None;
    while bytes.len().saturating_sub(pos) >= 8 {
        let id = &bytes[pos..pos + 4];
        let size = u32_at(bytes, pos + 4);
        let body = pos + 8;
        let remaining = bytes.len() - body;
        if id == b"data" {
            let len = match size {
                0 | u32::MAX => remaining,
                n => (n as usize).min(remaining),
            };
            data = Some(&bytes[body..body + len]);
            if fmt.is_some() {
                break;
            }
            pos = body.saturating_add(len).saturating_add(len & 1);
            continue;
        }
        let len = size as usize;
        if len > remaining {
            return Err(WavError::Truncated {
                chunk: String::from_utf8_lossy(id).into_owned(),
            });
        }
        if id == b"fmt " {
            fmt = Some(parse_fmt(&bytes[body..body + len])?);
        }
        // Chunks are word aligned: an odd size is followed by a pad byte.
        pos = body.saturating_add(len).saturating_add(len & 1);
    }
    let fmt = fmt.ok_or(WavError::MissingFmt)?;
    let data = data.ok_or(WavError::MissingData)?;
    Ok((fmt, data))
}

fn decode_frames(data: &[u8], fmt: &Fmt) -> Vec<f32> {
    let width = match fmt.kind {
        SampleKind::I16 => 2,
        SampleKind::F32 => 4,
    };
    let channels = usize::from(fmt.channels);
    let frame_bytes = width * channels;
    let inv = 1.0 / channels as f32;
    data.chunks_exact(frame_bytes)
        .map(|frame| {
            let sum: f32 = frame
                .chunks_exact(width)
                .map(|s| match fmt.kind {
                    SampleKind::I16 => f32::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0,
                    SampleKind::F32 => {
                        let x = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                        if x.is_finite() {
                            x
                        } else {
                            0.0
                        }
                    }
                })
                .sum();
            sum * inv
        })
        .collect()
}

/// Writes a canonical 44-byte-header PCM16 mono WAV (the ASR upload,
/// realtime §4.2).
pub fn write_wav_pcm16_mono(samples: &[i16], rate: u32) -> Result<Vec<u8>, WavError> {
    if rate == 0 {
        return Err(WavError::ZeroRate);
    }
    let too_long = WavError::TooLong {
        samples: samples.len(),
    };
    let data_len = samples
        .len()
        .checked_mul(2)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| n.checked_add(36).is_some())
        .ok_or(too_long.clone())?;
    let byte_rate = rate.checked_mul(2).ok_or(too_long)?;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
