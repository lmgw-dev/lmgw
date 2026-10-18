//! Which files a kept group's runs made, what each is called and what its
//! manifest line says. Pure: the bytes are read and written by the caller.
//!
//! A file is named `<group id>-<model>-<voice or task>-<first words>.<ext>`:
//! the group id zero-padded so the names sort in the order the groups were
//! made, every part lower-cased with anything but letters, digits, `.` and
//! `_` a dash, the text cut to its first words. A run with several outputs
//! (a separation's stems, a generation of several pictures) adds each
//! output's own id or its place; a name already taken in the export gets
//! `-2`, `-3`, ... before its extension.

use std::collections::HashSet;

use lmgw_api_types::audio_lab::AudioLabForm;
use lmgw_api_types::image_lab::ImageLabForm;
use lmgw_api_types::lab_store::{
    LabArtifactRole, LabArtifactView, LabFileKind, LabFilesInput, LabFilesManifestLine,
    LabGroupView, LabKind, LabRunView, FILES_FORMAT,
};
use serde_json::Value;

use super::stream::{Pcm, PcmSource};
use crate::labs::artifacts;

/// The digits the group id is padded to.
const ID_WIDTH: usize = 6;
/// The bytes the model part of a name keeps.
const MODEL_BYTES: usize = 40;
/// The bytes the voice or task part keeps.
const WHAT_BYTES: usize = 32;
/// The words of the text a name keeps, and the bytes they may take.
const WORDS: usize = 8;
const WORDS_BYTES: usize = 60;

/// Where a file's bytes come from.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Source {
    /// A kept artifact's file, as it is.
    Artifact { sha256: String },
    /// A kept speech stream or raw PCM, written as a WAV.
    Wav {
        sha256: String,
        from: PcmSource,
        pcm: Pcm,
    },
    /// A run's text answer.
    Text(String),
}

/// One file of the export.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Entry {
    pub line: LabFilesManifestLine,
    pub source: Source,
}

/// The names given so far in one export: a second file of a name gets a
/// counter. Compared lower-cased, as a case-insensitive filesystem would.
#[derive(Debug, Default)]
pub(super) struct Namer(HashSet<String>);

impl Namer {
    /// `stem.ext`, or `stem-2.ext`, `stem-3.ext`, ... when that is taken.
    pub fn take(&mut self, stem: &str, ext: &str) -> String {
        let mut name = format!("{stem}.{ext}");
        let mut n = 2;
        while !self.0.insert(name.to_lowercase()) {
            name = format!("{stem}-{n}.{ext}");
            n += 1;
        }
        name
    }
}

/// `s` as a name part of at most `max` bytes: lower-cased, every run of
/// characters that are not letters, digits, `.` or `_` one dash, no dash,
/// dot or underscore at either end. Letters stay whatever their script.
pub(super) fn slug(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        let c = if c.is_alphanumeric() || c == '.' || c == '_' {
            c
        } else {
            '-'
        };
        if c == '-' && (out.is_empty() || out.ends_with('-')) {
            continue;
        }
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out.trim_matches(['-', '.', '_']).to_string()
}

/// The first words of `text` as one name part: each word a [`slug`], as
/// many as fit [`WORDS_BYTES`], at most [`WORDS`].
pub(super) fn words(text: &str) -> String {
    let mut out = String::new();
    for w in text
        .split_whitespace()
        .map(|w| slug(w, WORDS_BYTES))
        .filter(|w| !w.is_empty())
        .take(WORDS)
    {
        let sep = usize::from(!out.is_empty());
        if out.len() + sep + w.len() > WORDS_BYTES {
            if out.is_empty() {
                out = w;
            }
            break;
        }
        if sep == 1 {
            out.push('-');
        }
        out.push_str(&w);
    }
    out
}

/// `<group id>-<model>-<what>-<words of text>`, the parts that are empty
/// left out, the words too when they only repeat `what`.
pub(super) fn stem(group_id: i64, model: &str, what: &str, text: &str) -> String {
    let mut parts = vec![format!("{group_id:0ID_WIDTH$}")];
    let what = slug(what, WHAT_BYTES);
    let text = words(text);
    for p in [slug(model, MODEL_BYTES), what.clone()] {
        if !p.is_empty() {
            parts.push(p);
        }
    }
    if !text.is_empty() && text != what {
        parts.push(text);
    }
    parts.join("-")
}

/// The extension of a file of `mime` that the store named `name`: the
/// type's own, else the name's (`.pcm`, `.sse`, a short ASCII one), else
/// `bin`.
fn ext_for(mime: &str, name: &str) -> String {
    let e = artifacts::ext_of(mime);
    if e != "bin" {
        return e.to_string();
    }
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| (1..=8).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "bin".into())
}

/// The id a run's output adds to the run's name when the run made several:
/// what the store named it after the model (`<model>-vocals.wav` →
/// `vocals`), else its place from 1.
fn output_suffix(model: &str, a: &LabArtifactView) -> String {
    let stem = a.name.rsplit_once('.').map_or(a.name.as_str(), |(s, _)| s);
    // The audio lab names a task's output `<model>-<id>.<ext>`, the model
    // spelled with ASCII letters, digits, `.`, `_` and `-` only.
    let model: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let own = stem
        .strip_prefix(model.as_str())
        .and_then(|s| s.strip_prefix('-'))
        .map(|s| slug(s, WHAT_BYTES))
        .filter(|s| !s.is_empty() && !s.chars().all(|c| c.is_ascii_digit()));
    own.unwrap_or_else(|| (a.ord + 1).to_string())
}

/// What a run's request said, for the manifest: its JSON, else its text.
fn parameters(run: &LabRunView) -> Value {
    serde_json::from_str(&run.request).unwrap_or_else(|_| Value::from(run.request.clone()))
}

fn base_mime(m: &str) -> String {
    m.split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// How a speech run's kept answer `a` becomes a file: an audio file as it
/// is; an event stream or raw PCM as a WAV, as the page plays it.
fn speech_source(run: &LabRunView, a: &LabArtifactView, pcm: Pcm) -> Source {
    let playable = a.mime.starts_with("audio/") && artifacts::ext_of(&a.mime) != "bin";
    let ty = base_mime(run.response_type.as_deref().unwrap_or(&a.mime));
    let from = match ty.as_str() {
        _ if playable => None,
        "text/event-stream" => Some(PcmSource::Sse),
        "audio/pcm" | "audio/l16" | "application/octet-stream" => Some(PcmSource::Raw),
        _ => None,
    };
    match from {
        Some(from) => Source::Wav {
            sha256: a.sha256.clone(),
            from,
            pcm,
        },
        None => Source::Artifact {
            sha256: a.sha256.clone(),
        },
    }
}

/// What a group's runs were asked, read from its form.
struct Asked {
    task: String,
    voice: Option<String>,
    text: Option<String>,
    /// The PCM format of a speech stream.
    pcm: Pcm,
}

fn some(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn asked(lab: LabKind, g: &LabGroupView, run: &LabRunView) -> Asked {
    match lab {
        LabKind::Image => {
            let form = serde_json::from_value::<ImageLabForm>(g.form.clone()).unwrap_or_default();
            Asked {
                task: run.variant.clone(),
                voice: None,
                text: some(&form.form.prompt),
                pcm: Pcm::read("", "", "", None),
            }
        }
        _ => {
            let form = serde_json::from_value::<AudioLabForm>(g.form.clone()).unwrap_or_default();
            let speech = &form.settings.speech;
            let speaking = run.endpoint == "/v1/audio/speech";
            let task = some(&form.task).unwrap_or_else(|| run.variant.clone());
            let text = if speaking {
                some(&speech.input)
            } else {
                // A task's text field, by the names the tasks give it.
                ["text", "input", "prompt", "lyrics"]
                    .iter()
                    .find_map(|k| form.tasks.values.get(*k).and_then(|v| some(v)))
            };
            let pcm = &form.settings.pcm;
            Asked {
                task,
                // A clone's reference clip stands in for a voice name.
                voice: speaking
                    .then(|| {
                        some(&speech.voice).or_else(|| {
                            some(&speech.voice_ref)
                                .map(|r| r.rsplit('/').next().unwrap_or(&r).to_string())
                        })
                    })
                    .flatten(),
                text,
                pcm: Pcm::read(
                    &pcm.rate,
                    &pcm.channels,
                    &pcm.bits,
                    run.headers.get("x-lmgw-sample-rate").map(String::as_str),
                ),
            }
        }
    }
}

/// Every file group `g` of `lab` made, in run order and each run's outputs
/// in order, then its transcript; named through `names`.
pub(super) fn entries(lab: LabKind, g: &LabGroupView, names: &mut Namer) -> Vec<Entry> {
    let inputs: Vec<LabFilesInput> = g
        .artifacts
        .iter()
        .filter(|a| a.role == LabArtifactRole::Input)
        .map(|a| LabFilesInput {
            artifact_id: a.id,
            name: a.name.clone(),
            mime: a.mime.clone(),
            sha256: a.sha256.clone(),
        })
        .collect();
    let mut out = Vec::new();
    for run in &g.runs {
        let asked = asked(lab, g, run);
        let what = asked.voice.as_deref().unwrap_or(&asked.task);
        // A transcription is named after what it transcribed: the title.
        let words = asked.text.as_deref().unwrap_or(&g.title);
        let base = stem(g.id, &run.model_asked, what, words);
        let line = |file: String, kind, mime: String, size| LabFilesManifestLine {
            format: FILES_FORMAT.into(),
            file,
            kind,
            lab,
            group_id: g.id,
            run_id: run.id,
            created_at: run.created_at.clone(),
            endpoint: run.endpoint.clone(),
            model: run.model_asked.clone(),
            answered_by: run.answered_by.clone(),
            task: asked.task.clone(),
            voice: asked.voice.clone(),
            text: asked.text.clone(),
            parameters: parameters(run),
            mime,
            size,
            artifact_id: None,
            sha256: None,
            missing: false,
            inputs: inputs.clone(),
        };
        let outputs: Vec<&LabArtifactView> = run
            .artifacts
            .iter()
            .filter(|a| a.role == LabArtifactRole::Output)
            .collect();
        for a in &outputs {
            let source = if lab == LabKind::Audio && run.endpoint == "/v1/audio/speech" {
                speech_source(run, a, asked.pcm)
            } else {
                Source::Artifact {
                    sha256: a.sha256.clone(),
                }
            };
            let (kind, mime, ext) = match source {
                Source::Wav { .. } => (LabFileKind::Wav, "audio/wav".to_string(), "wav".into()),
                _ => (
                    LabFileKind::Output,
                    a.mime.clone(),
                    ext_for(&a.mime, &a.name),
                ),
            };
            let stem = if outputs.len() > 1 {
                format!("{base}-{}", output_suffix(&run.model_asked, a))
            } else {
                base.clone()
            };
            let mut l = line(names.take(&stem, &ext), kind, mime, a.size);
            l.artifact_id = Some(a.id);
            l.sha256 = Some(a.sha256.clone());
            out.push(Entry { line: l, source });
        }
        let said = lmgw_api_types::audio_lab::transcript::run_transcript(
            run.http_status,
            run.response.as_deref(),
        );
        if let Some(text) = said.filter(|_| lab == LabKind::Audio) {
            let l = line(
                names.take(&base, "txt"),
                LabFileKind::Transcript,
                "text/plain; charset=utf-8".into(),
                text.len() as u64,
            );
            out.push(Entry {
                line: l,
                source: Source::Text(text),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_part_is_lower_case_letters_digits_and_single_dashes() {
        assert_eq!(slug("audio/Kokoro v1.0", 40), "audio-kokoro-v1.0");
        assert_eq!(slug("  ../etc/passwd ", 40), "etc-passwd");
        assert_eq!(slug("a<b>c:d\"e|f?g*h\\i", 40), "a-b-c-d-e-f-g-h-i");
        assert_eq!(slug("Grüße, München!", 40), "grüße-münchen");
        assert_eq!(slug("af_heart", 40), "af_heart");
        assert_eq!(slug("abcdefgh", 5), "abcde", "cut at the byte budget");
        assert_eq!(
            slug("ü".repeat(10).as_str(), 5),
            "üü",
            "never inside a character"
        );
        assert_eq!(slug("---", 10), "");
    }

    #[test]
    fn the_words_are_the_first_few_that_fit() {
        assert_eq!(
            words("Hello there, this is a test."),
            "hello-there-this-is-a-test"
        );
        assert_eq!(
            words("one two three four five six seven eight nine"),
            "one-two-three-four-five-six-seven-eight"
        );
        let long = format!("{} tail", "x".repeat(80));
        assert_eq!(
            words(&long),
            "x".repeat(WORDS_BYTES),
            "one long word is cut"
        );
        assert_eq!(words(" \n "), "");
    }

    #[test]
    fn a_stem_starts_with_the_padded_group_id() {
        assert_eq!(
            stem(42, "audio/kokoro", "af_heart", "Hello there."),
            "000042-audio-kokoro-af_heart-hello-there"
        );
        assert_eq!(stem(7, "", "", ""), "000007");
        assert_eq!(
            stem(1234567, "m", "tts", "tts"),
            "1234567-m-tts",
            "no repeat"
        );
    }

    #[test]
    fn a_taken_name_gets_a_counter_whatever_its_case() {
        let mut n = Namer::default();
        assert_eq!(n.take("000001-a", "wav"), "000001-a.wav");
        assert_eq!(n.take("000001-A", "wav"), "000001-A-2.wav");
        assert_eq!(n.take("000001-a", "wav"), "000001-a-3.wav");
        assert_eq!(
            n.take("000001-a", "txt"),
            "000001-a.txt",
            "another extension"
        );
    }

    #[test]
    fn an_extension_is_the_types_else_the_stored_names() {
        assert_eq!(ext_for("audio/wav", "x.bin"), "wav");
        assert_eq!(ext_for("audio/aac", "m.bin"), "aac");
        assert_eq!(ext_for("audio/opus", "m.bin"), "opus");
        assert_eq!(ext_for("audio/x-wav", "m.bin"), "wav");
        assert_eq!(ext_for("audio/wave", "m.bin"), "wav");
        assert_eq!(ext_for("audio/pcm", "kokoro.pcm"), "pcm");
        assert_eq!(ext_for("application/octet-stream", "response"), "bin");
        assert_eq!(ext_for("application/x-y", "a.tar.gz/../evil"), "bin");
    }
}
