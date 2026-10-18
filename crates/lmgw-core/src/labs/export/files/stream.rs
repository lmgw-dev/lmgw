//! What a kept audio answer holds besides a playable file: a streamed or raw
//! PCM speech answer becomes a WAV with the PCM format the form held, as
//! the Audio lab page plays it (its `audio_stream` module: the same event
//! names, the same 44-byte header), and a transcription's or a task's text
//! answer, JSON or an event stream, becomes its transcript
//! (`lmgw_api_types::audio_lab::transcript`, which the card's Save entry
//! asks too).

use base64::Engine as _;
use lmgw_api_types::audio_lab::transcript::sse_events as events;

/// How a speech answer that is no audio file carries its PCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PcmSource {
    /// An event stream of `speech.audio.delta` events, each a base64 chunk.
    Sse,
    /// The bytes are the PCM.
    Raw,
}

/// The PCM format a headless stream is wrapped with: a declaration (the
/// form's, or the gateway's rate), never a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Pcm {
    pub rate: u32,
    pub channels: u16,
    pub bits: u16,
}

impl Pcm {
    /// The form's strings, each falling back to the panel's own default
    /// (24 kHz mono 16-bit) when it is no positive number, as the page reads
    /// them (`pages/audio_lab/result.rs`, `Pcm::read`: 0 is no format); `rate` from the gateway's `x-lmgw-sample-rate` wins.
    pub fn read(rate: &str, channels: &str, bits: &str, stated_rate: Option<&str>) -> Self {
        fn positive<T: std::str::FromStr + Default + PartialEq>(s: &str, or: T) -> T {
            s.trim()
                .parse()
                .ok()
                .filter(|v| *v != T::default())
                .unwrap_or(or)
        }
        let rate = stated_rate
            .and_then(|r| r.trim().parse().ok())
            .filter(|r| *r > 0)
            .unwrap_or_else(|| positive(rate, 24_000));
        Self {
            rate,
            channels: positive(channels, 1),
            bits: positive(bits, 16),
        }
    }
}

/// The PCM of a speech event stream: its `speech.audio.delta` chunks in
/// order. A chunk that is no base64 is skipped, as the page skips it.
pub(super) fn sse_pcm(text: &str) -> Vec<u8> {
    let engine = base64::engine::general_purpose::STANDARD;
    let mut pcm = Vec::new();
    for v in events(text) {
        if v["type"] != "speech.audio.delta" {
            continue;
        }
        if let Some(b) = v["audio"].as_str().and_then(|b| engine.decode(b).ok()) {
            pcm.extend_from_slice(&b);
        }
    }
    pcm
}

/// The RIFF chunk size and the data chunk size of `data_len` bytes of PCM:
/// the data size is the real length and the RIFF size adds the 36 header
/// bytes and the pad byte an odd length takes; both saturate at `u32::MAX`
/// (a WAV cannot say more).
fn riff_sizes(data_len: usize) -> (u32, u32) {
    let data = u32::try_from(data_len).unwrap_or(u32::MAX);
    let padded = u32::try_from(data_len.saturating_add(data_len & 1)).unwrap_or(u32::MAX);
    (padded.saturating_add(36), data)
}

/// Raw PCM in the canonical 44-byte RIFF/WAVE header: 32-bit samples are
/// IEEE float (format 3), anything else integer PCM (format 1). A sample
/// takes `bits` rounded up to whole bytes (12-bit PCM is 2 bytes), as WAV
/// containers store it; an odd data length gets the RIFF pad byte, which the
/// data chunk's size does not count.
pub(super) fn pcm_to_wav(pcm: &[u8], f: Pcm) -> Vec<u8> {
    let block_align = u32::from(f.channels) * u32::from(f.bits.div_ceil(8));
    let (riff_len, data_len) = riff_sizes(pcm.len());
    let mut out = Vec::with_capacity(45 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_len.to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&if f.bits == 32 { 3u16 } else { 1u16 }.to_le_bytes());
    out.extend_from_slice(&f.channels.to_le_bytes());
    out.extend_from_slice(&f.rate.to_le_bytes());
    out.extend_from_slice(&f.rate.saturating_mul(block_align).to_le_bytes());
    out.extend_from_slice(&u16::try_from(block_align).unwrap_or(u16::MAX).to_le_bytes());
    out.extend_from_slice(&f.bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    if pcm.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// A kept speech answer `bytes`, carried as `from`, as a WAV.
pub(super) fn wav_of(bytes: &[u8], from: PcmSource, pcm: Pcm) -> Vec<u8> {
    match from {
        PcmSource::Raw => pcm_to_wav(bytes, pcm),
        PcmSource::Sse => pcm_to_wav(&sse_pcm(&String::from_utf8_lossy(bytes)), pcm),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wav_header_is_the_pages_byte_for_byte() {
        let pcm = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let wav = pcm_to_wav(
            &pcm,
            Pcm {
                rate: 24_000,
                channels: 1,
                bits: 16,
            },
        );
        assert_eq!(
            &wav[..44],
            &[
                b'R', b'I', b'F', b'F', 44, 0, 0, 0, b'W', b'A', b'V', b'E', b'f', b'm', b't',
                b' ', 16, 0, 0, 0, 1, 0, 1, 0, 0xC0, 0x5D, 0, 0, 0x80, 0xBB, 0, 0, 2, 0, 16, 0,
                b'd', b'a', b't', b'a', 8, 0, 0, 0,
            ]
        );
        assert_eq!(&wav[44..], &pcm);
        let float = pcm_to_wav(
            &[0; 16],
            Pcm {
                rate: 48_000,
                channels: 2,
                bits: 32,
            },
        );
        assert_eq!(&float[20..22], &[3, 0], "32-bit is IEEE float");
        assert_eq!(&float[32..34], &[8, 0], "block align: 2 × 4");
    }

    #[test]
    fn sizes_saturate_and_an_odd_length_is_padded() {
        assert_eq!(riff_sizes(8), (44, 8));
        assert_eq!(riff_sizes(7), (44, 7), "the pad is in RIFF, not in data");
        assert_eq!(riff_sizes(u32::MAX as usize), (u32::MAX, u32::MAX));
        assert_eq!(
            riff_sizes(u32::MAX as usize - 40),
            (u32::MAX - 3, u32::MAX - 40)
        );
        assert_eq!(riff_sizes(u32::MAX as usize + 10), (u32::MAX, u32::MAX));
        let f = Pcm {
            rate: 8000,
            channels: 1,
            bits: 16,
        };
        let odd = pcm_to_wav(&[1, 2, 3], f);
        assert_eq!(odd.len(), 48, "header + 3 + pad");
        assert_eq!(&odd[4..8], &[40, 0, 0, 0]);
        assert_eq!(&odd[40..44], &[3, 0, 0, 0]);
        assert_eq!(&odd[44..], &[1, 2, 3, 0]);
        assert_eq!(pcm_to_wav(&[1, 2], f).len(), 46, "even: no pad");
    }

    #[test]
    fn a_bit_depth_between_whole_bytes_takes_the_next_whole_byte() {
        let w = pcm_to_wav(
            &[0; 12],
            Pcm {
                rate: 8000,
                channels: 2,
                bits: 12,
            },
        );
        assert_eq!(&w[32..34], &[4, 0], "block align: 2 × 2 bytes");
        assert_eq!(&w[28..32], &32_000u32.to_le_bytes(), "byte rate");
        assert_eq!(&w[34..36], &[12, 0], "the declared bits stay");
        let wide = pcm_to_wav(
            &[],
            Pcm {
                rate: u32::MAX,
                channels: 2,
                bits: 24,
            },
        );
        assert_eq!(
            &wide[28..32],
            &u32::MAX.to_le_bytes(),
            "byte rate saturates"
        );
    }

    #[test]
    fn the_form_declares_the_format_and_the_gateways_rate_wins() {
        let p = Pcm::read("16000", "2", "", None);
        assert_eq!((p.rate, p.channels, p.bits), (16_000, 2, 16));
        assert_eq!(Pcm::read("16000", "1", "16", Some("44100")).rate, 44_100);
        let junk = Pcm::read("fast", "0", "x", Some("none"));
        assert_eq!((junk.rate, junk.channels, junk.bits), (24_000, 1, 16));
    }

    #[test]
    fn a_speech_stream_is_its_deltas_in_order() {
        let sse = ": keep-alive\n\n\
                   event: speech.audio.delta\ndata: {\"audio\": \"QUJD\"}\n\n\
                   data: {\"type\": \"speech.audio.delta\", \"audio\": \"REVG\"}\r\n\r\n\
                   data: {\"type\": \"speech.audio.delta\", \"audio\": \"!!\"}\n\n\
                   data: {\"type\": \"speech.audio.done\"}\n\n\
                   data: [DONE]\n\n";
        assert_eq!(sse_pcm(sse), b"ABCDEF");
        let wav = wav_of(sse.as_bytes(), PcmSource::Sse, Pcm::read("", "", "", None));
        assert_eq!(&wav[44..], b"ABCDEF");
        assert_eq!(
            wav_of(b"xy", PcmSource::Raw, Pcm::read("", "", "", None)).len(),
            46
        );
    }
}
