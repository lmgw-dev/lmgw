//! What an answer adds up to, for the run in flight and for a kept run
//! alike: a JSON body's tracks, text and timelines, a whole audio file, or
//! an event stream's PCM wrapped in a WAV. The players' object URLs are
//! handed back with the result, so whoever shows it revokes them. A kept
//! task answer names its audio as `lab-artifact:<id>` where the base64 was:
//! its track plays the kept file.

use lmgw_api_types::lab_store::{artifact_ref_id, LabArtifactView};
use serde_json::Value;

use super::super::audio_stream::{b64_decode, parse_record, pcm_to_wav, StreamAcc};
use super::{blob_url, track_stem, RunResult, Track};

/// A result and the object URLs its players point at.
#[derive(Default)]
pub(super) struct Read {
    pub result: RunResult,
    pub urls: Vec<String>,
    /// The audio bytes it carried, when it carried any.
    pub bytes: Option<usize>,
    /// The engine's own `timing` object, when the JSON had one.
    pub timing: Value,
}

impl Read {
    /// One base64 track of a JSON answer, or the kept file a
    /// `lab-artifact:<id>` among `kept` names; its size, 0 when it is no
    /// audio. Short strings are never audio: the old island used the same
    /// guard so a stray `data` field cannot produce a broken player.
    fn push_track(
        &mut self,
        stem: &str,
        id: &str,
        b64: Option<&str>,
        rate: Option<f64>,
        kept: &[LabArtifactView],
    ) -> usize {
        if let Some(a) = b64
            .and_then(artifact_ref_id)
            .and_then(|ref_id| kept.iter().find(|a| a.id == ref_id))
        {
            self.result.tracks.push(Track {
                id: id.to_string(),
                url: a.href.clone(),
                name: a.name.clone(),
                size: a.size as usize,
                sample_rate: rate,
            });
            return a.size as usize;
        }
        let Some(b64) = b64.filter(|s| s.len() >= 64) else {
            return 0;
        };
        let Some(bytes) = b64_decode(b64) else {
            return 0;
        };
        let n = bytes.len();
        if let Some(url) = blob_url(&bytes, "audio/wav") {
            self.urls.push(url.clone());
            self.result.tracks.push(Track {
                id: id.to_string(),
                url,
                name: if id.is_empty() {
                    format!("{stem}.wav")
                } else {
                    format!("{stem}-{id}.wav")
                },
                size: n,
                sample_rate: rate,
            });
        }
        n
    }
}

/// A complete JSON answer (a transcription, a task's result, speech's JSON
/// envelope): its tracks, text and timelines. `model` names the files;
/// `kept` are a kept run's files, which its `lab-artifact:<id>` refs name.
pub(super) fn from_json(j: &Value, model: &str, kept: &[LabArtifactView]) -> Read {
    let stem = track_stem(model);
    // `/v1/tasks/stream` wraps the same document as `{events, result}`.
    let r = if j["result"].is_object() {
        &j["result"]
    } else {
        j
    };
    let timing = if r["timing"].is_object() {
        r["timing"].clone()
    } else {
        j["timing"].clone()
    };
    let mut read = Read {
        result: RunResult {
            json: Some(j.clone()),
            events: j["events"].as_array().cloned(),
            text: r["text"].as_str().unwrap_or("").to_string(),
            language: r["language"].as_str().unwrap_or("").to_string(),
            segments: r["segments"].as_array().cloned().unwrap_or_default(),
            speaker_turns: r["speaker_turns"].as_array().cloned().unwrap_or_default(),
            words: r["words"].as_array().cloned().unwrap_or_default(),
            ..Default::default()
        },
        timing,
        ..Default::default()
    };
    // A single `audio` (+ `sample_rate`) for one-output tasks,
    // `named_audio_outputs` for separation's stem list; `data` / `b64_json`
    // cover `/v1/audio/speech`'s json envelope.
    let single = r["audio"]
        .as_str()
        .or_else(|| j["data"].as_str())
        .or_else(|| j["b64_json"].as_str());
    let mut bytes = read.push_track(&stem, "", single, r["sample_rate"].as_f64(), kept);
    for n in r["named_audio_outputs"].as_array().into_iter().flatten() {
        bytes += read.push_track(
            &stem,
            n["id"].as_str().unwrap_or(""),
            n["audio"].as_str(),
            n["sample_rate"].as_f64(),
            kept,
        );
    }
    if bytes > 0 {
        read.bytes = Some(bytes);
    }
    read
}

/// The extension a whole audio file of `mime` is downloaded with.
pub(super) fn ext_of(mime: &str) -> &'static str {
    if mime.contains("wav") {
        "wav"
    } else if mime.contains("mpeg") {
        "mp3"
    } else if mime.contains("ogg") {
        "ogg"
    } else if mime.contains("flac") {
        "flac"
    } else {
        "bin"
    }
}

/// A whole audio file: one player over its bytes.
pub(super) fn from_binary(bytes: &[u8], mime: &str, model: &str) -> Read {
    let mut read = Read {
        bytes: Some(bytes.len()),
        ..Default::default()
    };
    if let Some(url) = blob_url(bytes, mime) {
        read.urls.push(url.clone());
        read.result.tracks.push(Track {
            id: String::new(),
            url,
            name: format!("{}.{}", track_stem(model), ext_of(mime)),
            size: bytes.len(),
            sample_rate: None,
        });
    }
    read
}

/// The PCM format a stream is wrapped with: the strings the form held, each
/// falling back to the panel's own default when it is no positive number (0
/// is no rate, channel count or bit depth: core's export reads them the same
/// way, `labs::export::files::stream::Pcm::read`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Pcm {
    pub rate: u32,
    pub channels: u16,
    pub bits: u16,
}

impl Pcm {
    pub fn read(rate: &str, channels: &str, bits: &str) -> Self {
        fn positive<T: std::str::FromStr + Default + PartialEq>(s: &str, or: T) -> T {
            s.trim()
                .parse()
                .ok()
                .filter(|v| *v != T::default())
                .unwrap_or(or)
        }
        Self {
            rate: positive(rate, 24000),
            channels: positive(channels, 1),
            bits: positive(bits, 16),
        }
    }
}

/// What a stream's accumulated PCM and text add up to: the PCM as one WAV.
pub(super) fn from_stream(acc: &StreamAcc, pcm: Pcm, model: &str) -> Read {
    let mut read = Read {
        result: RunResult {
            text: acc.text.clone(),
            ..Default::default()
        },
        ..Default::default()
    };
    if !acc.pcm.is_empty() {
        let wav = pcm_to_wav(&acc.pcm, pcm.rate, pcm.channels, pcm.bits);
        if let Some(url) = blob_url(&wav, "audio/wav") {
            read.urls.push(url.clone());
            read.result.tracks.push(Track {
                id: String::new(),
                url,
                name: format!("{}.wav", track_stem(model)),
                size: wav.len(),
                sample_rate: Some(f64::from(pcm.rate)),
            });
        }
        read.bytes = Some(acc.pcm.len());
    }
    read
}

/// A kept event stream's text folded as the live reader folds it: every
/// record between blank lines.
pub(super) fn fold_sse(text: &str) -> StreamAcc {
    let mut acc = StreamAcc::default();
    let text = text.replace("\r\n", "\n");
    for record in text.split("\n\n") {
        if let Some(ev) = parse_record(record) {
            acc.apply(ev);
        }
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kept_stream_folds_as_the_live_one_did() {
        let text = "data: {\"type\":\"speech.audio.delta\",\"audio\":\"AAAA\"}\r\n\r\n\
                    : keep-alive\n\n\
                    data: {\"type\":\"transcript.text.delta\",\"delta\":\"hel\"}\n\n\
                    data: {\"type\":\"transcript.text.delta\",\"delta\":\"lo\"}\n\n\
                    data: [DONE]\n\n";
        let acc = fold_sse(text);
        assert_eq!(acc.pcm, [0, 0, 0]);
        assert_eq!(acc.text, "hello");
        assert_eq!(acc.error, None);
    }

    #[test]
    fn a_kept_tasks_audio_plays_its_kept_files() {
        let kept: Vec<LabArtifactView> = serde_json::from_value(serde_json::json!([
            {"id": 7, "run_id": 2, "role": "output", "ord": 0, "name": "audio-sep.wav",
             "mime": "audio/wav", "size": 4410, "sha256": "ab", "data_url_head": null,
             "href": "/labs/api/artifacts/7", "created_at": ""},
            {"id": 8, "run_id": 2, "role": "output", "ord": 1, "name": "audio-sep-vocals.wav",
             "mime": "audio/wav", "size": 2205, "sha256": "cd", "data_url_head": null,
             "href": "/labs/api/artifacts/8", "created_at": ""}
        ]))
        .unwrap();
        let j = serde_json::json!({
            "audio": "lab-artifact:7", "sample_rate": 22050,
            "named_audio_outputs": [{"id": "vocals", "audio": "lab-artifact:8"},
                                    {"id": "gone", "audio": "lab-artifact:9"}]
        });
        let read = from_json(&j, "audio/sep", &kept);
        let t = &read.result.tracks;
        assert_eq!(t.len(), 2, "a ref to no kept file is no track");
        assert_eq!(t[0].url, "/labs/api/artifacts/7");
        assert_eq!(t[0].sample_rate, Some(22050.0));
        assert_eq!(
            (t[1].id.as_str(), t[1].name.as_str()),
            ("vocals", "audio-sep-vocals.wav")
        );
        assert_eq!(read.bytes, Some(4410 + 2205));
        assert!(read.urls.is_empty(), "kept files need no object URL");
    }

    #[test]
    fn the_pcm_format_falls_back_to_the_panels_defaults() {
        assert_eq!(
            Pcm::read("44100", "2", "32"),
            Pcm {
                rate: 44100,
                channels: 2,
                bits: 32
            }
        );
        assert_eq!(
            Pcm::read("", "x", ""),
            Pcm::read("0", "0", "0"),
            "0 is no format either"
        );
        assert_eq!(
            Pcm::read("", "x", ""),
            Pcm {
                rate: 24000,
                channels: 1,
                bits: 16
            }
        );
        assert_eq!(ext_of("audio/mpeg"), "mp3");
        assert_eq!(ext_of("application/octet-stream"), "bin");
    }
}
