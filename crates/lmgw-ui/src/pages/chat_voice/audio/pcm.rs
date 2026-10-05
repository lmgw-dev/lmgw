//! PCM in the page: the formats the voice routes speak, the test tone, and
//! the level a meter shows. Pure, so it is tested natively.
//!
//! Everything here is PCM16 mono: realtime's `pcm16` at 24 kHz both ways, the
//! Chat's `speech` frames at 24 kHz, and dictation's WAV at 16 kHz.

use crate::pages::audio_stream::{b64_encode, pcm_to_wav};

/// The rate of the player, of realtime's audio both ways, and of the Chat's
/// `speech` frames.
pub(crate) const RATE: u32 = 24_000;
/// The rate dictation records at: what the ASR models take natively, a third
/// less to upload than 24 kHz.
pub(crate) const DICTATION_RATE: u32 = 16_000;

/// Samples as little-endian bytes (the wire order of every route here).
pub(crate) fn le_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// A mono PCM16 WAV file of `samples` at `rate`: dictation's upload body
/// (`audio/wav`, chat-voice §5).
pub(crate) fn wav(samples: &[i16], rate: u32) -> Vec<u8> {
    pcm_to_wav(&le_bytes(samples), rate, 1, 16)
}

/// `samples` as realtime's `input_audio_buffer.append` carries them.
pub(crate) fn b64_pcm(samples: &[i16]) -> String {
    b64_encode(&le_bytes(samples))
}

/// Milliseconds of audio in `samples` at `rate` (`truncate.audio_end_ms`).
pub(crate) fn ms_of(samples: u64, rate: u32) -> u64 {
    samples * 1000 / u64::from(rate.max(1))
}

/// A byte stream cut into PCM16 samples, whatever its chunk boundaries: a
/// chunk of odd length keeps its last byte for the next one.
#[derive(Debug, Default)]
pub(crate) struct Pcm16Reader {
    carry: Option<u8>,
}

impl Pcm16Reader {
    /// The whole samples `bytes` completes, as little-endian bytes again
    /// (what the player takes).
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut all = Vec::with_capacity(bytes.len() + 1);
        all.extend(self.carry.take());
        all.extend_from_slice(bytes);
        if all.len() % 2 == 1 {
            self.carry = all.pop();
        }
        all
    }
}

/// The test tone: a rising three-note chime (C5, E5, G5), 0.4 s each with
/// short fades so nothing clicks, at -12 dBFS. Recognisable on any output,
/// gentle on headphones.
pub(crate) fn test_tone(rate: u32) -> Vec<i16> {
    const NOTES: [f64; 3] = [523.25, 659.25, 783.99];
    const NOTE_S: f64 = 0.4;
    const FADE_S: f64 = 0.015;
    const AMP: f64 = 0.25;
    let rate = f64::from(rate);
    let per = (NOTE_S * rate) as usize;
    let fade = (FADE_S * rate).max(1.0);
    let mut out = Vec::with_capacity(per * NOTES.len());
    for f in NOTES {
        for i in 0..per {
            let t = i as f64 / rate;
            let edge = (i as f64).min((per - 1 - i) as f64);
            let env = (edge / fade).min(1.0);
            let v = (2.0 * std::f64::consts::PI * f * t).sin() * AMP * env;
            out.push((v * 32767.0).round() as i16);
        }
    }
    out
}

/// The meter's floor: quieter than this shows an empty bar.
const FLOOR_DB: f64 = -60.0;

/// The meter's fill for an RMS level of `dbfs` ([`meter_level`]'s scale).
pub(crate) fn dbfs_level(dbfs: f64) -> f64 {
    ((dbfs - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// A meter's fill for `samples` (−1…1): their RMS level on a dB scale, from
/// [`FLOOR_DB`] (0) to full scale (1).
pub(crate) fn meter_level(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    let rms = (sum / samples.len() as f64).sqrt();
    if rms <= 0.0 {
        return 0.0;
    }
    dbfs_level(20.0 * rms.log10())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::audio_stream::b64_decode;

    #[test]
    fn a_dictation_wav_is_mono_pcm16_at_its_rate() {
        let wav = wav(&[0, 1, -1, i16::MAX, i16::MIN], DICTATION_RATE);
        assert_eq!(wav.len(), 44 + 10);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[4..8], &(36u32 + 10).to_le_bytes());
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(&wav[20..22], &[1, 0], "integer PCM");
        assert_eq!(&wav[22..24], &[1, 0], "mono");
        assert_eq!(&wav[24..28], &16_000u32.to_le_bytes());
        assert_eq!(&wav[28..32], &32_000u32.to_le_bytes(), "byte rate");
        assert_eq!(&wav[32..34], &[2, 0], "block align");
        assert_eq!(&wav[34..36], &[16, 0], "bits");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(&wav[40..44], &10u32.to_le_bytes());
        assert_eq!(
            &wav[44..],
            &[0, 0, 1, 0, 0xFF, 0xFF, 0xFF, 0x7F, 0x00, 0x80]
        );
    }

    #[test]
    fn realtime_audio_is_base64_of_little_endian_samples() {
        let b64 = b64_pcm(&[1, -2, 300]);
        assert_eq!(
            b64_decode(&b64).unwrap(),
            vec![1, 0, 0xFE, 0xFF, 0x2C, 0x01]
        );
        assert_eq!(b64_pcm(&[]), "");
    }

    #[test]
    fn an_odd_chunk_keeps_its_last_byte_for_the_next() {
        let mut r = Pcm16Reader::default();
        assert_eq!(r.feed(&[1, 2, 3]), vec![1, 2]);
        assert_eq!(r.feed(&[4]), vec![3, 4]);
        assert_eq!(r.feed(&[]), Vec::<u8>::new());
        assert_eq!(r.feed(&[5]), Vec::<u8>::new());
        assert_eq!(r.feed(&[6, 7, 8]), vec![5, 6, 7, 8]);
    }

    #[test]
    fn milliseconds_follow_the_rate() {
        assert_eq!(ms_of(24_000, RATE), 1000);
        assert_eq!(ms_of(960, RATE), 40);
        assert_eq!(ms_of(640, DICTATION_RATE), 40);
    }

    #[test]
    fn the_test_tone_is_three_notes_that_start_and_end_silent() {
        let t = test_tone(RATE);
        assert_eq!(t.len(), 3 * 9600);
        assert_eq!(t[0], 0);
        assert_eq!(*t.last().unwrap(), 0);
        let peak = t.iter().map(|s| s.unsigned_abs()).max().unwrap();
        // -12 dBFS: a quarter of full scale.
        assert!((8000..=8200).contains(&peak), "peak {peak}");
    }

    #[test]
    fn the_meter_reads_decibels_from_its_floor_to_full_scale() {
        assert_eq!(meter_level(&[]), 0.0);
        assert_eq!(meter_level(&[0.0; 128]), 0.0);
        let full = meter_level(&[1.0, -1.0, 1.0, -1.0]);
        assert!((full - 1.0).abs() < 1e-9);
        // -20 dBFS RMS is two thirds of a 60 dB scale.
        let a = 0.1_f32;
        let quiet = meter_level(&[a, -a, a, -a]);
        assert!((quiet - 2.0 / 3.0).abs() < 1e-6, "{quiet}");
        // Below the floor: empty.
        assert_eq!(meter_level(&[1e-5, -1e-5]), 0.0);
        // The same scale by level: dictation's speech threshold reads on it.
        assert!((dbfs_level(-20.0) - quiet).abs() < 1e-6);
        assert!((dbfs_level(-40.0) - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!((dbfs_level(-90.0), dbfs_level(6.0)), (0.0, 1.0));
    }
}
