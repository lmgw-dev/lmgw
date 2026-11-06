//! The answer of `POST /v1/audio/alignments`: a transcript the client
//! submitted, laid over the clip it submitted it with, word by word.
//!
//! Every forced-alignment family answers in its own dialect — seconds, or
//! samples, or both; a sample rate or none; punctuation kept on the word or
//! stripped — and lmgw reads each of them into this one shape, so a client
//! (a story player highlighting a line while it plays) does not
//! carry a reader per model. The gateway builds it; the API document is
//! generated from it.

use serde::{Deserialize, Serialize};

/// A forced alignment: the submitted transcript and where each of its words
/// is spoken in the clip.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioAlignment {
    /// The transcript as the client submitted it (the multipart `text`
    /// field), not what the model echoes. `char_start` and `char_end` of the
    /// words index into this string.
    pub text: String,
    /// The language as the model reported it (`en`, `de`; the model's own
    /// spelling). Absent when the model says none.
    ///
    /// A model may report the language it assumes rather than the one it was
    /// asked to align in: CrisperWhisper's aligner answers its default, `en`,
    /// to any `language` (checked 2026-10-22 with `de`, `german` and none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// The rate, in Hz, the `start_sample` and `end_sample` of the words
    /// count in: the model's own figure when its answer states one, else
    /// derived from the words' seconds and samples (they are what the
    /// samples count in, even where the engine resampled the clip), else the
    /// uploaded WAV's header. Absent when none of the three works; the
    /// seconds are always there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    /// The words in the order the model answered them, which is the
    /// transcript's order.
    pub words: Vec<AlignedWord>,
    /// How long the engine took, as it reports it (audio.cpp's `timing`
    /// object, which both aligners send). Absent when the answer has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<AudioTiming>,
}

/// The engine's own account of one inference. Each figure is absent when the
/// engine leaves it out.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioTiming {
    /// Wall-clock milliseconds of the inference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<f64>,
    /// Milliseconds of audio the engine processed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_duration_ms: Option<f64>,
    /// Real-time factor: `wall_ms` over `audio_duration_ms`; below 1 is
    /// faster than real time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtf: Option<f64>,
}

/// One aligned word.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AlignedWord {
    /// The word as the model reported it, punctuation and all (CrisperWhisper
    /// keeps the comma on `Nuova,`, Qwen3's aligner strips it).
    pub word: String,
    /// Where the word starts in the clip, in seconds. Taken from the model,
    /// or computed from `start_sample` and `sample_rate` when it sends only
    /// samples.
    pub start: f64,
    /// Where the word ends in the clip, in seconds; as `start`.
    pub end: f64,
    /// `start` in samples of `sample_rate`: as the model answered, or
    /// computed from the seconds when the rate is known. Absent when the
    /// model sends none and the rate is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_sample: Option<u64>,
    /// `end` in samples; as `start_sample`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_sample: Option<u64>,
    /// How sure the model is, when it says (several families send `0`
    /// for every word, which means "not computed", not "unsure").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// Where the word starts in `text`, counted in Unicode scalar values
    /// (not bytes, not UTF-16 units). The span is matched on letters and
    /// digits only, case-insensitively, in order: `Nuova,` and `Nuova` both
    /// cover the five letters of `Nuova` in the transcript. Absent, with
    /// `char_end`, for a word that could not be found in the transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub char_start: Option<usize>,
    /// One past the word's last matched letter or digit in `text`, in the
    /// same units as `char_start`: the span is `[char_start, char_end)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub char_end: Option<usize>,
}
