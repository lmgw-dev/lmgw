//! A task's audio as output artifacts (labs-store design §5.1): audio.cpp's
//! `/v1/tasks/run` and `/v1/tasks/stream` answer one JSON document whose
//! audio is base64 text (`audio`, each `named_audio_outputs[i].audio`, an
//! event's `audio` in a stream's `events`). Kept as text, a minute of it is
//! megabytes in `lmgw.sqlite`; each such string becomes a file instead, named
//! `lab-artifact:<id>` in its place, as the Image lab keeps its `b64_json`.
//!
//! The document is read through borrowed `RawValue`s (as the Image lab's
//! pictures are): each string is a slice of the text itself, so its place is
//! its pointer's offset and nothing else is re-serialized. Only a string that
//! comes back byte for byte becomes a file: written without JSON escapes,
//! canonical base64 (padded, no whitespace) of at least one byte. Any other
//! stays inline as the engine wrote it.

use axum::body::Bytes;
use base64::Engine as _;
use lmgw_api_types::systemone::Ordered;
use serde_json::value::RawValue;

/// One audio string in the answer: its JSON string token's byte range, its
/// decoded bytes, and the `id` of the output it belongs to when it says one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Clip {
    pub start: usize,
    pub end: usize,
    pub bytes: Bytes,
    pub id: Option<String>,
}

/// The canonical base64 that `token` (a JSON string token, quotes included)
/// holds, decoded; `None` for any token that would not come back as it is.
fn canonical(token: &str) -> Option<Vec<u8>> {
    let inner = token.strip_prefix('"')?.strip_suffix('"')?;
    if inner.is_empty() || inner.contains('\\') {
        return None;
    }
    let engine = base64::engine::general_purpose::STANDARD;
    let bytes = engine.decode(inner).ok()?;
    (engine.encode(&bytes) == inner && is_audio(&bytes)).then_some(bytes)
}

/// Do these bytes start like an audio file, by the labs' own sniffer? A
/// word such as `none` is canonical base64 too, and under an `audio` key it
/// is still a word.
fn is_audio(bytes: &[u8]) -> bool {
    use crate::extract::sniff::{sniff_artifact, Kind};
    matches!(sniff_artifact(bytes), Ok(s) if s.kind == Kind::Audio)
}

/// Walk `raw` (a slice of the text at `base`), pushing every audio string.
fn walk(raw: &RawValue, base: usize, out: &mut Vec<Clip>) {
    let text = raw.get();
    match text.as_bytes().first() {
        Some(b'{') => {
            let Ok(members) = serde_json::from_str::<Ordered<&RawValue>>(text) else {
                return;
            };
            let id = members
                .iter()
                .find(|(k, _)| *k == "id")
                .and_then(|(_, v)| serde_json::from_str::<String>(v.get()).ok());
            for (key, value) in members.iter() {
                if key == "audio" {
                    if let Some(bytes) = canonical(value.get()) {
                        let start = value.get().as_ptr() as usize - base;
                        out.push(Clip {
                            start,
                            end: start + value.get().len(),
                            bytes: Bytes::from(bytes),
                            id: id.clone(),
                        });
                        continue;
                    }
                }
                walk(value, base, out);
            }
        }
        Some(b'[') => {
            let Ok(items) = serde_json::from_str::<Vec<&RawValue>>(text) else {
                return;
            };
            for item in items {
                walk(item, base, out);
            }
        }
        _ => {}
    }
}

/// Every audio string of the task answer `text`, in order. An answer that
/// does not read as JSON has none.
pub(super) fn clips(text: &str) -> Vec<Clip> {
    let Ok(root) = serde_json::from_str::<&RawValue>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(root, text.as_ptr() as usize, &mut out);
    out
}

/// `text` with each clip's token replaced by the JSON string of its
/// replacement, in order: `refs[i]` for `clips[i]`.
pub(super) fn splice(text: &str, clips: &[Clip], refs: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (c, with) in clips.iter().zip(refs) {
        out.push_str(&text[at..c.start]);
        out.push_str(&serde_json::Value::String(with.clone()).to_string());
        at = c.end;
    }
    out.push_str(&text[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_audio_string_is_found_where_it_is_and_replaced_there_only() {
        // WAV is a RIFF/WAVE header, OGG an Ogg page header; "AAAA" is three zero
        // bytes, no audio.
        let text = r#"{"events": [{"partial_text": {"text": "a"}}, {"audio": "UklGRiQAAABXQVZFZm10IA=="}],
            "result": {"audio": "UklGRiQAAABXQVZFZm10IA==", "sample_rate": 24000,
              "named_audio_outputs": [{"id": "vocals", "audio": "T2dnUwACAAAAAAAAAAA="},
                                      {"id": "drums", "audio": "not base64!"},
                                      {"id": "bass", "audio": "UklGRiQAAABXQVZFZm10IA=="},
                                      {"id": "other", "audio": "AA\/A"}],
              "text": "audio"}}"#;
        let found = clips(text);
        let ids: Vec<Option<&str>> = found.iter().map(|c| c.id.as_deref()).collect();
        assert_eq!(
            ids,
            [None, None, Some("vocals"), Some("bass")],
            "not base64, escaped, or no audio: inline"
        );
        assert!(found[1].bytes.starts_with(b"RIFF"));
        assert!(found[2].bytes.starts_with(b"OggS"));
        let refs: Vec<String> = (1..=4).map(|i| format!("lab-artifact:{i}")).collect();
        let out = splice(text, &found, &refs);
        assert_eq!(
            out,
            r#"{"events": [{"partial_text": {"text": "a"}}, {"audio": "lab-artifact:1"}],
            "result": {"audio": "lab-artifact:2", "sample_rate": 24000,
              "named_audio_outputs": [{"id": "vocals", "audio": "lab-artifact:3"},
                                      {"id": "drums", "audio": "not base64!"},
                                      {"id": "bass", "audio": "lab-artifact:4"},
                                      {"id": "other", "audio": "AA\/A"}],
              "text": "audio"}}"#
        );
    }

    #[test]
    fn a_word_under_an_audio_key_stays_text() {
        // "none" and "mono" are canonical base64 of three bytes.
        let text = r#"{"audio": "none", "mode": {"audio": "mono"}, "ok": true}"#;
        assert!(clips(text).is_empty());
        assert_eq!(splice(text, &[], &[]), text);
        assert!(is_audio(b"ID3\x04\0\0\0\0\0\x10"));
        assert!(is_audio(b"fLaC\0\0\0\x22"));
        assert!(!is_audio(b"none"));
    }

    #[test]
    fn a_midi_answer_becomes_an_artifact_too() {
        let midi = r#""TVRoZAAAAAYAAQACAeA=""#;
        let bytes = canonical(midi).unwrap();
        assert!(bytes.starts_with(b"MThd"));
        // `MThd` with a header length other than 6 is no MIDI file.
        assert_eq!(canonical(r#""TVRoZAAAAAcAAQACAeA=""#), None);
        let text = format!(r#"{{"audio": {midi}, "format": "midi"}}"#);
        let found = clips(&text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].bytes, bytes);
        assert_eq!(
            splice(&text, &found, &["lab-artifact:5".into()]),
            r#"{"audio": "lab-artifact:5", "format": "midi"}"#
        );
    }

    #[test]
    fn only_canonical_base64_comes_back_as_it_was() {
        let wav = r#""UklGRiQAAABXQVZFZm10IA==""#;
        assert_eq!(
            canonical(wav).as_deref(),
            Some(&b"RIFF\x24\0\0\0WAVEfmt "[..])
        );
        assert_eq!(canonical(&wav.replace('=', "")), None, "unpadded");
        assert_eq!(canonical(r#""AAAA""#), None, "canonical, but no audio");
        assert_eq!(canonical(r#""AAA""#), None, "unpadded");
        assert_eq!(canonical(r#""AA AA""#), None, "whitespace");
        assert_eq!(canonical(r#""""#), None, "nothing");
        assert_eq!(canonical("12"), None, "no string");
        assert!(clips("not json").is_empty());
        assert!(clips(r#"{"error": {"message": "x"}}"#).is_empty());
    }
}
