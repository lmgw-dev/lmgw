//! A candidate's WAV: its length, and its leading and trailing silence cut.
//!
//! Cloning models copy what their reference starts with, a pause included,
//! so a saved sample starts where its speech starts. Only whole frames are
//! cut, at the start and at the end; every sample kept is the candidate's
//! own, at its own loudness — nothing is normalized, faded or resampled.
//!
//! Where the speech starts is read from 10 ms windows' RMS: the first and
//! the last window within [`FLOOR_DB`] of the loudest one (and above
//! [`ABSOLUTE_FLOOR`], so a near-silent clip keeps its noise). A margin is
//! left before the first ([`LEAD_PAD_MS`]: a soft onset under the floor) and
//! after the last ([`TAIL_PAD_MS`]: a word's natural decay). The answer says
//! how much was cut, so nothing about it is hidden.
//!
//! Reads integer PCM (8, 16, 24, 32 bit) and IEEE float (32, 64 bit), plain
//! or `WAVE_FORMAT_EXTENSIBLE`. A data chunk whose size is unknown runs to
//! the end of the file: one declaring `0xFFFFFFFF` (what a streaming writer
//! puts there, ffmpeg's to a pipe), and one declaring 0 in a file whose
//! RIFF size is unknown too (0 or `0xFFFFFFFF`: a header written before
//! the sizes were known and never patched). In a file whose RIFF size is
//! set, a data size of 0 means what it says: no samples. A declared size
//! past the end of the file is cut to it (a truncated file).
//!
//! A trimmed float (or other non-PCM) WAV keeps its `fact` chunk, its
//! sample count set to the frames kept: the format requires one there.

use lmgw_api_types::voice_lab::VoiceTrim;

/// A window this far below the loudest one is silence, in dB.
pub const FLOOR_DB: f64 = -40.0;
/// Nothing quieter than this (linear RMS, full scale 1.0, about -60 dBFS) is
/// speech, however quiet the loudest window is.
pub const ABSOLUTE_FLOOR: f64 = 0.001;
/// Kept before the first window over the floor.
pub const LEAD_PAD_MS: u64 = 40;
/// Kept after the last.
pub const TAIL_PAD_MS: u64 = 120;
/// The window the RMS is read over.
const WINDOW_MS: u64 = 10;

/// A parsed PCM WAV, borrowing the bytes it was read from.
#[derive(Debug, Clone, Copy)]
pub struct Wav<'a> {
    /// 1: integer PCM, 3: IEEE float (an extensible header's sub-format).
    pub format: u16,
    pub channels: u16,
    pub rate: u32,
    pub bits: u16,
    /// The whole `fmt ` chunk, its header included: copied as it is.
    pub fmt_chunk: &'a [u8],
    /// The file had a `fact` chunk (a non-PCM format's sample count): a
    /// trimmed copy has one too, counting what it kept.
    pub fact: bool,
    /// The sample bytes, whole frames only.
    pub data: &'a [u8],
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

impl<'a> Wav<'a> {
    /// Read `bytes` as a PCM or float WAV; `None` for anything else.
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.get(0..4)? != b"RIFF" || bytes.get(8..12)? != b"WAVE" {
            return None;
        }
        // A RIFF size of 0 or all ones: written before the sizes were known.
        let streamed = matches!(u32_at(bytes, 4)?, 0 | u32::MAX);
        let mut at = 12;
        let mut fmt: Option<&[u8]> = None;
        let mut data: Option<&[u8]> = None;
        let mut fact = false;
        while at + 8 <= bytes.len() {
            let id = &bytes[at..at + 4];
            let declared = u32_at(bytes, at + 4)?;
            let body = at + 8;
            let left = bytes.len() - body;
            let unknown = declared == u32::MAX || (declared == 0 && streamed);
            let size = if id == b"data" && unknown {
                left
            } else {
                (declared as usize).min(left)
            };
            match id {
                b"fmt " => fmt = Some(&bytes[at..body + size]),
                b"data" => data = Some(&bytes[body..body + size]),
                b"fact" => fact = true,
                _ => {}
            }
            if data.is_some() && fmt.is_some() {
                break;
            }
            at = body + size + (size & 1);
        }
        let fmt = fmt?;
        let body = &fmt[8..];
        let mut format = u16_at(body, 0)?;
        let channels = u16_at(body, 2)?;
        let rate = u32_at(body, 4)?;
        let bits = u16_at(body, 14)?;
        if format == 0xFFFE {
            // WAVE_FORMAT_EXTENSIBLE: the sub-format GUID's first two bytes.
            format = u16_at(body, 24)?;
        }
        let ok_bits = match format {
            1 => matches!(bits, 8 | 16 | 24 | 32),
            3 => matches!(bits, 32 | 64),
            _ => false,
        };
        if !ok_bits || channels == 0 || rate == 0 {
            return None;
        }
        let frame = usize::from(channels) * usize::from(bits / 8);
        let data = data?;
        let whole = data.len() - data.len() % frame;
        Some(Self {
            format,
            channels,
            rate,
            bits,
            fmt_chunk: fmt,
            fact,
            data: &data[..whole],
        })
    }

    /// Bytes per frame (one sample of every channel).
    pub fn frame_bytes(&self) -> usize {
        usize::from(self.channels) * usize::from(self.bits / 8)
    }

    pub fn frames(&self) -> usize {
        self.data.len() / self.frame_bytes()
    }

    /// The length in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        frames_ms(self.frames(), self.rate)
    }

    /// Frame `i`'s loudest channel, full scale 1.0.
    fn level(&self, i: usize) -> f64 {
        let width = usize::from(self.bits / 8);
        let start = i * self.frame_bytes();
        (0..usize::from(self.channels))
            .map(|c| {
                sample(
                    self.format,
                    self.bits,
                    &self.data[start + c * width..][..width],
                )
                .abs()
            })
            .fold(0.0, f64::max)
    }
}

/// One sample, full scale 1.0.
fn sample(format: u16, bits: u16, b: &[u8]) -> f64 {
    match (format, bits) {
        (1, 8) => (f64::from(b[0]) - 128.0) / 128.0,
        (1, 16) => f64::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0,
        (1, 24) => {
            let v = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
            f64::from(v) / 8_388_608.0
        }
        (1, 32) => f64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]])) / 2_147_483_648.0,
        (3, 32) => f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        (3, 64) => f64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8])),
        _ => 0.0,
    }
}

fn frames_ms(frames: usize, rate: u32) -> u64 {
    (frames as u64).saturating_mul(1000) / u64::from(rate.max(1))
}

/// A WAV's length in milliseconds; `None` for bytes that are no PCM WAV.
pub fn duration_ms(bytes: &[u8]) -> Option<u64> {
    Wav::parse(bytes).map(|w| w.duration_ms())
}

/// `bytes` with its leading and trailing silence cut (module doc), and what
/// was cut. Bytes that are no PCM WAV, and a clip with nothing over the
/// floor, come back as they are, with the reason in the trim's `note`.
pub fn trim_silence(bytes: &[u8]) -> (Vec<u8>, VoiceTrim) {
    let Some(w) = Wav::parse(bytes) else {
        return (
            bytes.to_vec(),
            VoiceTrim {
                note: Some("not a PCM WAV lmgw can read: kept as it came, untrimmed".into()),
                ..VoiceTrim::default()
            },
        );
    };
    let frames = w.frames();
    let kept_all = VoiceTrim {
        kept_ms: w.duration_ms(),
        ..VoiceTrim::default()
    };
    let window = ((u64::from(w.rate) * WINDOW_MS / 1000) as usize).max(1);
    let rms: Vec<f64> = (0..frames)
        .step_by(window)
        .map(|start| {
            let end = (start + window).min(frames);
            let sum: f64 = (start..end).map(|i| w.level(i).powi(2)).sum();
            (sum / (end - start) as f64).sqrt()
        })
        .collect();
    let loudest = rms.iter().copied().fold(0.0, f64::max);
    let floor = (loudest * 10f64.powf(FLOOR_DB / 20.0)).max(ABSOLUTE_FLOOR);
    let (Some(first), Some(last)) = (
        rms.iter().position(|r| *r >= floor),
        rms.iter().rposition(|r| *r >= floor),
    ) else {
        return (
            bytes.to_vec(),
            VoiceTrim {
                note: Some("nothing in it is above the silence floor: kept as it came".into()),
                ..kept_all
            },
        );
    };
    let pad = |ms: u64| (u64::from(w.rate) * ms / 1000) as usize;
    let start = (first * window).saturating_sub(pad(LEAD_PAD_MS));
    let end = ((last + 1) * window + pad(TAIL_PAD_MS)).min(frames);
    let fb = w.frame_bytes();
    let samples = &w.data[start * fb..end * fb];
    let fact = w.fact.then_some((end - start) as u32);
    (
        assemble(w.fmt_chunk, fact, samples),
        VoiceTrim {
            lead_ms: frames_ms(start, w.rate),
            tail_ms: frames_ms(frames - end, w.rate),
            kept_ms: frames_ms(end - start, w.rate),
            note: None,
        },
    )
}

/// A WAV of the `fmt ` chunk `fmt_chunk` (as read), a `fact` chunk with
/// `fact` frames when given, and `samples`.
pub fn assemble(fmt_chunk: &[u8], fact: Option<u32>, samples: &[u8]) -> Vec<u8> {
    let pad = fmt_chunk.len() & 1;
    let fact_len = if fact.is_some() { 12 } else { 0 };
    let riff = 4 + fmt_chunk.len() + pad + fact_len + 8 + samples.len();
    let mut out = Vec::with_capacity(8 + riff);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(fmt_chunk);
    if pad == 1 {
        out.push(0);
    }
    if let Some(frames) = fact {
        out.extend_from_slice(b"fact");
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&frames.to_le_bytes());
    }
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    out.extend_from_slice(samples);
    out
}

/// A mono 16-bit PCM WAV of `samples` at `rate` — the shape audio.cpp
/// answers in, for tests.
pub fn pcm16_mono(rate: u32, samples: &[i16]) -> Vec<u8> {
    let mut fmt = Vec::with_capacity(24);
    fmt.extend_from_slice(b"fmt ");
    fmt.extend_from_slice(&16u32.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate * 2).to_le_bytes());
    fmt.extend_from_slice(&2u16.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());
    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    assemble(&fmt, None, &data)
}
