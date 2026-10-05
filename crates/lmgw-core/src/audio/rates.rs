//! The sample rate each audio row speaks at, as its last WAV through lmgw
//! said (audio-class gap 8, orchestrator decision 7).
//!
//! A streamed speech answer is bare PCM16 — base64 in `speech.audio.delta`
//! events, or raw bytes for `stream_format: audio` — with no header to say
//! its rate, and the rate differs per model (Supertonic 44.1 kHz, Pocket
//! 24 kHz). lmgw learns it from the row's own buffered answers, which are
//! WAVs, and says it on a streamed answer as `x-lmgw-sample-rate` and in
//! `capabilities.speech.sample_rate`. Kept in memory: until a row has
//! answered once with a WAV since lmgw started, its rate is unknown and the
//! header absent.

use std::collections::HashMap;
use std::sync::Mutex;

/// The response header naming a streamed answer's sample rate, in Hz.
pub const SAMPLE_RATE_HEADER: &str = "x-lmgw-sample-rate";

/// How much of a WAV's start is searched for its `fmt ` chunk. audio.cpp
/// writes the canonical 44-byte header; the room above it is for an
/// encoder that puts a chunk or two first. A WAV whose `fmt ` comes later
/// teaches nothing — the rate stays what it was.
pub const HEADER_WINDOW: usize = 512;

/// Learned rates by audio row (model id).
#[derive(Debug, Default)]
pub struct SampleRates {
    rates: Mutex<HashMap<String, u32>>,
}

impl SampleRates {
    pub fn get(&self, model_id: &str) -> Option<u32> {
        self.rates.lock().unwrap().get(model_id).copied()
    }

    /// Learn `model_id`'s rate from the start of a WAV it answered with.
    pub fn learn(&self, model_id: &str, wav_start: &[u8]) {
        if let Some(rate) = wav_rate(wav_start) {
            self.rates
                .lock()
                .unwrap()
                .insert(model_id.to_string(), rate);
        }
    }
}

/// The sample rate in a RIFF/WAVE header's `fmt ` chunk, read from the
/// first [`HEADER_WINDOW`] bytes of `bytes`; `None` when it is not there.
pub fn wav_rate(bytes: &[u8]) -> Option<u32> {
    let b = &bytes[..bytes.len().min(HEADER_WINDOW)];
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return None;
    }
    let mut at = 12;
    while at + 8 <= b.len() {
        let id = &b[at..at + 4];
        let len = u32::from_le_bytes(b[at + 4..at + 8].try_into().ok()?) as usize;
        if id == b"fmt " {
            let rate = b.get(at + 12..at + 16)?;
            let rate = u32::from_le_bytes(rate.try_into().ok()?);
            return (rate > 0).then_some(rate);
        }
        at = at.checked_add(8 + len + (len & 1))?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(rate: u32, lead: &[u8]) -> Vec<u8> {
        let mut b = b"RIFF\0\0\0\0WAVE".to_vec();
        b.extend_from_slice(lead);
        b.extend_from_slice(b"fmt \x10\0\0\0\x01\0\x01\0");
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2).to_le_bytes());
        b.extend_from_slice(b"\x02\0\x10\0data\0\0\0\0");
        b
    }

    #[test]
    fn the_rate_is_read_from_the_fmt_chunk_wherever_it_starts() {
        assert_eq!(wav_rate(&header(44_100, b"")), Some(44_100));
        assert_eq!(
            wav_rate(&header(24_000, b"LIST\x03\0\0\0abc\0")),
            Some(24_000),
            "an odd chunk is padded"
        );
        assert_eq!(wav_rate(b"RIFF....WAVE"), None);
        assert_eq!(wav_rate(b"not a wav at all"), None);
        assert_eq!(wav_rate(&header(0, b"")), None);

        let rates = SampleRates::default();
        rates.learn("supertonic", &header(44_100, b""));
        rates.learn("supertonic", b"garbage keeps the old one");
        assert_eq!(rates.get("supertonic"), Some(44_100));
        assert_eq!(rates.get("pocket"), None);
    }
}
