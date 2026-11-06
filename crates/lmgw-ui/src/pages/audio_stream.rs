//! Audio-lab streaming: the SSE frames audio.cpp emits over the gateway, and
//! the WAV header a raw PCM stream needs before any `<audio>` element will play
//! it. Ported from `assets/audio-lab/audio-lab.js:131-188, 651-717`.
//!
//! EventSource cannot POST, so the records are read off the fetch body with
//! the kit's SSE decoder (`lmgw_ui_kit::http::sse`, the Chat's too). Frame
//! decoding, event folding, base64 and the WAV header are pure functions so
//! they can be unit-tested on the native target.

use futures::StreamExt;
use lmgw_ui_kit::http::sse::{Decoder, Record};
use serde_json::Value;
use wasm_bindgen::JsCast;

/// One meaningful frame of an audio stream. Everything else (keep-alives,
/// `[DONE]`, unknown event names) is dropped by [`audio_event`].
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

/// Parse one SSE record's text (between blank lines), as the decoder reads
/// it.
#[cfg(test)]
fn parse_record(record: &str) -> Option<AudioEvent> {
    Decoder::default()
        .push(format!("{record}\n\n").as_bytes())
        .first()
        .and_then(audio_event)
}

/// One decoded record as an audio event.
///
/// The event name lives in the `event:` line or in the payload's `type`,
/// depending on the emitter — the payload wins, and both are accepted, exactly
/// as the old island did.
pub fn audio_event(r: &Record) -> Option<AudioEvent> {
    let data = r.data.as_str();
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let v: Value = serde_json::from_str(data).ok()?;
    let kind = v["type"].as_str().unwrap_or(&r.event);
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

/// Standard-alphabet base64 → bytes, as audio.cpp and the realtime
/// protocol send it: `lmgw-client`'s, which the voice panel runs on.
pub use lmgw_client::base64::decode as b64_decode;

pub use lmgw_ui_kit::wav::pcm_to_wav;

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

    let mut records = Decoder::default();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break }; // aborted or network drop
        for r in records.push(&js_sys::Uint8Array::from(chunk).to_vec()) {
            if let Some(ev) = audio_event(&r) {
                on_event(ev);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
