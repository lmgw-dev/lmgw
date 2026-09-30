//! Audio-lab streaming: the SSE frames audio.cpp emits over the gateway, and
//! the WAV header a raw PCM stream needs before any `<audio>` element will play
//! it. Ported from `assets/audio-lab/audio-lab.js:131-188, 651-717`.
//!
//! The reader loop is the Chat page's ([`super::chat_stream`]) — EventSource
//! cannot POST, so SSE is parsed off a fetch `ReadableStream` by hand. Frame
//! decoding, event folding, base64 and the WAV header are pure functions so
//! they can be unit-tested on the native target.

use futures::StreamExt;
use serde_json::Value;
use wasm_bindgen::JsCast;

/// One meaningful frame of an audio stream. Everything else (keep-alives,
/// `[DONE]`, unknown event names) is dropped by [`parse_record`].
#[derive(Debug, Clone, PartialEq)]
pub enum AudioEvent {
    /// `speech.audio.delta` — a chunk of raw PCM, already base64-decoded.
    Pcm(Vec<u8>),
    /// `transcript.text.delta` — appended to what arrived so far.
    TextDelta(String),
    /// `transcript.text.done` — replaces the transcript when it carries text.
    TextDone(Option<String>),
    Error(String),
}

/// Parse one SSE record (the text between blank lines).
///
/// The event name lives in the `event:` line or in the payload's `type`,
/// depending on the emitter — the payload wins, and both are accepted, exactly
/// as the old island did.
pub fn parse_record(record: &str) -> Option<AudioEvent> {
    let mut event = "message";
    let mut data = String::new();
    for line in record.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.starts_with(':') {
            continue; // keep-alive / comment
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let v: Value = serde_json::from_str(&data).ok()?;
    let kind = v["type"].as_str().unwrap_or(event);
    match kind {
        "speech.audio.delta" => {
            let b64 = v["audio"].as_str().filter(|s| !s.is_empty())?;
            b64_decode(b64).map(AudioEvent::Pcm)
        }
        "transcript.text.delta" => Some(AudioEvent::TextDelta(
            v["delta"]
                .as_str()
                .or_else(|| v["text"].as_str())
                .unwrap_or("")
                .to_string(),
        )),
        "transcript.text.done" => {
            Some(AudioEvent::TextDone(v["text"].as_str().map(str::to_string)))
        }
        "error" => Some(AudioEvent::Error(
            v["error"]["message"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| v["message"].as_str().filter(|s| !s.is_empty()))
                .unwrap_or("stream error")
                .to_string(),
        )),
        _ => None,
    }
}

/// What a stream adds up to: PCM chunks are concatenated and wrapped once at
/// the end, transcript deltas are folded live. A stream can carry both.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StreamAcc {
    pub pcm: Vec<u8>,
    pub text: String,
    pub error: Option<String>,
}

impl StreamAcc {
    pub fn apply(&mut self, ev: AudioEvent) {
        match ev {
            AudioEvent::Pcm(mut bytes) => self.pcm.append(&mut bytes),
            AudioEvent::TextDelta(t) => self.text.push_str(&t),
            // `done` without a `text` field keeps whatever was accumulated.
            AudioEvent::TextDone(Some(t)) => self.text = t,
            AudioEvent::TextDone(None) => {}
            AudioEvent::Error(msg) => self.error = Some(msg),
        }
    }
}

/// Standard-alphabet base64 → bytes. Whitespace is tolerated (SSE data lines
/// can wrap); padding ends the payload; anything else is a decode failure.
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn sextet(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        if c.is_ascii_whitespace() {
            continue;
        }
        acc = (acc << 6) | sextet(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Wrap raw PCM in the canonical 44-byte RIFF/WAVE header.
///
/// A headless PCM stream carries no format at all, so `rate` / `channels` /
/// `bits` are what the user declared in the form — a declaration, not a
/// measurement. Getting them wrong yields audio at the wrong pitch, never an
/// error, which is why the form says so out loud.
pub fn pcm_to_wav(pcm: &[u8], rate: u32, channels: u16, bits: u16) -> Vec<u8> {
    let bytes_per_sample = (bits / 8) as u32;
    let block_align = (channels as u32).wrapping_mul(bytes_per_sample);
    let data_len = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&data_len.wrapping_add(36).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
                                                 // 3 = IEEE float (f32le), 1 = integer PCM.
    out.extend_from_slice(&if bits == 32 { 3u16 } else { 1u16 }.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&rate.wrapping_mul(block_align).to_le_bytes()); // byte rate
    out.extend_from_slice(&(block_align as u16).to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

/// Read an SSE body off an already-sent fetch response, handing every
/// recognized frame to `on_event`. Transport errors (including an abort) end
/// the loop; in-stream `error` frames arrive through `on_event`.
pub async fn read_sse(
    resp: &gloo_net::http::Response,
    mut on_event: impl FnMut(AudioEvent),
) -> Result<(), String> {
    let raw = resp
        .body()
        .ok_or("response had no body")?
        .unchecked_into::<web_sys::ReadableStream>();
    let mut stream = wasm_streams::ReadableStream::from_raw(raw).into_stream();

    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break }; // aborted or network drop
        buf.extend_from_slice(&js_sys::Uint8Array::from(chunk).to_vec());
        // Records end at a blank line; the terminator is ASCII, so byte
        // scanning never splits UTF-8 inside a record.
        while let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
            let record: Vec<u8> = buf.drain(..pos + 2).collect();
            let text = String::from_utf8_lossy(&record);
            if let Some(ev) = parse_record(text.trim()) {
                on_event(ev);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_byte_exact_for_s16le() {
        let pcm = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let wav = pcm_to_wav(&pcm, 24000, 1, 16);
        assert_eq!(wav.len(), 44 + pcm.len());
        assert_eq!(
            &wav[..44],
            &[
                b'R', b'I', b'F', b'F', // "RIFF"
                44, 0, 0, 0, // 36 + 8 data bytes
                b'W', b'A', b'V', b'E', //
                b'f', b'm', b't', b' ', //
                16, 0, 0, 0, // fmt chunk size
                1, 0, // audioFormat = 1 (integer PCM)
                1, 0, // channels
                0xC0, 0x5D, 0x00, 0x00, // 24000 Hz
                0x80, 0xBB, 0x00, 0x00, // byteRate = 24000 * 2
                2, 0, // blockAlign = channels * 2
                16, 0, // bits per sample
                b'd', b'a', b't', b'a', //
                8, 0, 0, 0, // data length
            ]
        );
        assert_eq!(&wav[44..], &pcm[..]);
    }

    #[test]
    fn wav_header_marks_32_bit_as_ieee_float_and_scales_stereo() {
        let wav = pcm_to_wav(&[0; 16], 48000, 2, 32);
        assert_eq!(&wav[20..22], &[3, 0]); // audioFormat = 3 (f32le)
        assert_eq!(&wav[22..24], &[2, 0]); // channels
        assert_eq!(&wav[24..28], &48000u32.to_le_bytes()); // sample rate
        assert_eq!(&wav[28..32], &(48000u32 * 8).to_le_bytes()); // byte rate
        assert_eq!(&wav[32..34], &[8, 0]); // blockAlign = 2 * 4
        assert_eq!(&wav[34..36], &[32, 0]); // bits
    }

    #[test]
    fn base64_round_trips_the_shapes_audio_cpp_sends() {
        assert_eq!(b64_decode("").unwrap(), Vec::<u8>::new());
        assert_eq!(b64_decode("QQ==").unwrap(), b"A");
        assert_eq!(b64_decode("QUI=").unwrap(), b"AB");
        assert_eq!(b64_decode("QUJD").unwrap(), b"ABC");
        assert_eq!(b64_decode("QUJD\nREVG").unwrap(), b"ABCDEF");
        assert_eq!(b64_decode("//8=").unwrap(), vec![0xFF, 0xFF]);
        assert!(b64_decode("not*base64").is_none());
    }

    fn fold(records: &[&str]) -> StreamAcc {
        let mut acc = StreamAcc::default();
        for r in records {
            if let Some(ev) = parse_record(r) {
                acc.apply(ev);
            }
        }
        acc
    }

    #[test]
    fn pcm_deltas_concatenate_in_arrival_order() {
        let acc = fold(&[
            ": keep-alive",
            "event: speech.audio.delta\ndata: {\"audio\": \"QUJD\"}",
            "event: speech.audio.delta\ndata: {\"audio\": \"REVG\"}",
            "data: [DONE]",
        ]);
        assert_eq!(acc.pcm, b"ABCDEF");
        assert!(acc.text.is_empty() && acc.error.is_none());
    }

    #[test]
    fn transcript_deltas_append_and_done_replaces() {
        let acc = fold(&[
            "data: {\"type\": \"transcript.text.delta\", \"delta\": \"hel\"}",
            "data: {\"type\": \"transcript.text.delta\", \"delta\": \"lo wrld\"}",
            "data: {\"type\": \"transcript.text.done\", \"text\": \"hello world\"}",
        ]);
        assert_eq!(acc.text, "hello world");
    }

    #[test]
    fn transcript_delta_falls_back_to_text_and_done_may_be_empty() {
        let acc = fold(&[
            "event: transcript.text.delta\ndata: {\"text\": \"abc\"}",
            "event: transcript.text.done\ndata: {}",
        ]);
        assert_eq!(acc.text, "abc");
    }

    #[test]
    fn payload_type_wins_over_the_event_line() {
        assert_eq!(
            parse_record(
                "event: transcript.text.delta\ndata: {\"type\":\"error\",\"message\":\"boom\"}"
            ),
            Some(AudioEvent::Error("boom".into()))
        );
    }

    #[test]
    fn error_frames_prefer_the_nested_message() {
        let acc = fold(&["event: error\ndata: {\"error\": {\"message\": \"upstream refused\"}}"]);
        assert_eq!(acc.error.as_deref(), Some("upstream refused"));
        let acc = fold(&["event: error\ndata: {}"]);
        assert_eq!(acc.error.as_deref(), Some("stream error"));
    }

    #[test]
    fn noise_frames_are_dropped() {
        assert_eq!(parse_record(": ping"), None);
        assert_eq!(parse_record("data: [DONE]"), None);
        assert_eq!(parse_record("data: not json"), None);
        assert_eq!(parse_record("event: speech.audio.delta\ndata: {}"), None);
        assert_eq!(parse_record("event: something.else\ndata: {\"a\":1}"), None);
    }

    #[test]
    fn multiline_data_lines_are_joined() {
        assert_eq!(
            parse_record("event: transcript.text.delta\ndata: {\"delta\":\ndata: \"hi\"}"),
            Some(AudioEvent::TextDelta("hi".into()))
        );
    }
}
