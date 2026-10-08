//! lmgw's own measurement of what an audio request sends (billable-units
//! design §4.1–§4.3): the quantities it holds exactly, before any answer.
//!
//! **Never estimated.** A duration comes from a WAV's header and data size
//! and from nothing else — never from a byte count and a bitrate — and a
//! character count from the text as sent, never from tokens. What cannot be
//! measured that way is `None`.
//!
//! A measurement rides in `MediaOutcome::measured`, which exists only for a
//! 2xx answer, so a refused request never carries one (§4.1). What the
//! provider reports wins over it ([`super::usage::Reported::quantities`]).

use serde_json::Value;

use crate::pricing::Quantities;
use crate::realtime::audio::pcm::wav_duration_ms;

use super::MultipartField;

/// The name of an upload's audio field (OpenAI's `file`).
const AUDIO_FIELD: &str = "file";

/// An upload's input audio: the `file` field's length in milliseconds when
/// it is a PCM16 or float32 WAV, read from its header and data size with
/// nothing decoded, and no cap beyond the body limit that already admitted
/// it (§9.1). The length is floored to the millisecond (whole frames ×
/// 1000 / rate), while a provider's reported seconds are rounded to it
/// (`usage::duration_ms`): the two rules part by under a millisecond.
///
/// `None` for anything that cannot be measured exactly:
/// - any other container — WebM, Ogg, MP3, M4A, FLAC: no demuxer in v1
///   (§4.2, Q4), so such an upload is priced per minute only when its
///   provider reports the duration;
/// - a WAV whose `data` chunk does not state its size ([`sizes_declared`]):
///   the reader takes the rest of the file as audio then, which would count
///   any chunk after it;
/// - a form with no `file`, or two.
pub(super) fn upload(fields: &[MultipartField]) -> Quantities {
    let mut files = fields.iter().filter_map(|f| match f {
        MultipartField::File(name, _, _, bytes) if name == AUDIO_FIELD => Some(bytes),
        _ => None,
    });
    let audio_in_ms = match (files.next(), files.next()) {
        (Some(bytes), None) if sizes_declared(bytes) => wav_duration_ms(bytes).ok(),
        _ => None,
    };
    Quantities {
        audio_in_ms,
        ..Default::default()
    }
}

/// Whether a WAV has a `data` chunk and every one states its size, walking
/// the RIFF chunks as `realtime::audio::pcm` does. A streaming writer
/// leaves the size 0 or `0xFFFFFFFF` (an RF64 file always does: its real
/// size is in `ds64`), and the reader, which decodes dictation as well and
/// stays lenient, then takes the rest of the file as audio — a trailing
/// `LIST` or `id3 ` chunk included. Measurement cannot tell such a chunk
/// from samples, so it measures nothing then.
fn sizes_declared(bytes: &[u8]) -> bool {
    let mut pos = 12usize;
    let mut seen = false;
    while let Some(head) = bytes.get(pos..pos + 8) {
        let size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
        if &head[..4] == b"data" {
            if size == 0 || size == u32::MAX {
                return false;
            }
            seen = true;
        }
        let padded = size as usize + (size & 1) as usize;
        pos = pos.saturating_add(8).saturating_add(padded);
    }
    seen
}

/// A speech request's input text: the characters of `input` as sent —
/// after shaping took the tags and cue its route does not read, so what the
/// TTS reads (§4.3) — as Unicode scalar values (§9.2). `None` when the body
/// has no text `input`.
pub(crate) fn speech_chars(body: &Value) -> Option<u64> {
    body.get("input")
        .and_then(Value::as_str)
        .map(|t| t.chars().count() as u64)
}

/// [`speech_chars`] as the request's quantities.
pub(super) fn speech(body: &Value) -> Quantities {
    Quantities {
        chars_in: speech_chars(body),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use serde_json::json;

    use super::*;
    use crate::realtime::audio::pcm::write_wav_pcm16_mono;

    fn file(name: &str, bytes: Vec<u8>) -> MultipartField {
        MultipartField::File(
            name.into(),
            "in.wav".into(),
            Some("audio/wav".into()),
            Bytes::from(bytes),
        )
    }

    fn wav(ms: usize, rate: u32) -> Vec<u8> {
        let samples = vec![0i16; ms * rate as usize / 1000];
        write_wav_pcm16_mono(&samples, rate).unwrap()
    }

    #[test]
    fn a_wav_upload_is_measured_from_its_header_and_data() {
        let fields = [
            MultipartField::Text("model".into(), "whisper".into()),
            file("file", wav(1_500, 16_000)),
        ];
        assert_eq!(upload(&fields).audio_in_ms, Some(1_500));
        assert_eq!(
            upload(&[file("file", wav(27_391, 24_000))]).audio_in_ms,
            Some(27_391)
        );
        assert_eq!(upload(&fields).chars_in, None);
        assert_eq!(upload(&fields).requests, None, "the row writer's to say");
    }

    #[test]
    fn anything_but_one_wav_file_is_unmeasured() {
        let mp3 = file("file", b"ID3\x03\x00\x00\x00fake-mp3".to_vec());
        assert_eq!(upload(&[mp3]).audio_in_ms, None, "no demuxer (Q4)");
        assert_eq!(
            upload(&[file("file", b"RIFFaudio".to_vec())]).audio_in_ms,
            None
        );
        assert_eq!(
            upload(&[file("image", wav(1_000, 16_000))]).audio_in_ms,
            None
        );
        assert_eq!(upload(&[]).audio_in_ms, None);
        let two = [
            file("file", wav(1_000, 16_000)),
            file("file", wav(2_000, 16_000)),
        ];
        assert_eq!(upload(&two).audio_in_ms, None, "which one did it hear?");
    }

    /// `wav` with its `data` chunk's size set to `size`, and `tail` after it.
    fn with_data_size(wav: &[u8], size: u32, tail: &[u8]) -> Vec<u8> {
        let mut out = wav.to_vec();
        let at = out.windows(4).position(|w| w == b"data").unwrap() + 4;
        out[at..at + 4].copy_from_slice(&size.to_le_bytes());
        out.extend_from_slice(tail);
        out
    }

    /// A trailing chunk after a sized `data` is not audio; after a
    /// streaming-size one the reader would count it, so nothing is measured.
    #[test]
    fn a_wav_that_does_not_state_its_data_size_is_unmeasured() {
        let one_s = wav(1_000, 16_000);
        let size = 16_000 * 2;
        // A LIST chunk of 3 200 bytes: 100 ms, were it read as 16 kHz PCM16.
        let mut list = b"LIST".to_vec();
        list.extend_from_slice(&3_192u32.to_le_bytes());
        list.extend_from_slice(b"INFO");
        list.resize(3_200, 0);
        let tagged = with_data_size(&one_s, size, &list);
        assert_eq!(upload(&[file("file", tagged)]).audio_in_ms, Some(1_000));
        for streaming in [0, u32::MAX] {
            let open = with_data_size(&one_s, streaming, &list);
            assert_eq!(
                wav_duration_ms(&open).ok(),
                Some(1_100),
                "the lenient reader counts the LIST chunk's bytes as samples"
            );
            assert_eq!(
                upload(&[file("file", open)]).audio_in_ms,
                None,
                "{streaming}"
            );
        }
        let mut rf64 = with_data_size(&one_s, u32::MAX, b"");
        rf64[..4].copy_from_slice(b"RF64");
        assert_eq!(upload(&[file("file", rf64)]).audio_in_ms, None);
        assert!(
            !sizes_declared(b"RIFF\x04\x00\x00\x00WAVE"),
            "no data chunk"
        );
    }

    #[test]
    fn speech_counts_unicode_scalar_values_of_the_input() {
        assert_eq!(
            speech_chars(&json!({"input": "The quick brown fox."})),
            Some(20)
        );
        assert_eq!(speech_chars(&json!({"input": "Grüße, 東京!"})), Some(10));
        assert_eq!(speech_chars(&json!({"input": ""})), Some(0));
        assert_eq!(speech_chars(&json!({"voice": "alloy"})), None);
        assert_eq!(speech_chars(&json!({"input": 3})), None);
        assert_eq!(speech(&json!({"input": "abc"})).chars_in, Some(3));
    }
}
