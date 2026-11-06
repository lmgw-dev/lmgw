//! The Voices lab's shapes (`/voice-lab/api/*`, the `voice_generate` and
//! `voice_sample_save` ops): make a fixed voice for the cloning
//! text-to-speech models.
//!
//! A **round** is one click of Generate: the same text, description, voice
//! and language rendered as several **candidates**, each with its own seed,
//! by one text-to-speech alias over one held route. The owner listens, and
//! the chosen candidate is saved to the voice library as a **sample**: the
//! clip (leading and trailing silence trimmed, loudness untouched) and its
//! exact transcript, which lmgw wrote, so every cloning model can speak with
//! it by name. A sample remembers how it was made ([`VoiceProvenance`]).
//!
//! The gateway builds and reads these types, the dashboard's Voices page
//! reads them, and the API document is generated from them.

use serde::{Deserialize, Serialize};

use crate::audio_lab::Clip;
use crate::ModelLicence;

/// How many candidates a round renders when the request does not say.
pub const DEFAULT_CANDIDATES: u32 = 4;

// ---------------------------------------------------------------------------
// The sentence per language
// ---------------------------------------------------------------------------

/// One language of the sentence table: the built-in sentence, the owner's
/// own when saved, and the short line the cloning check speaks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceSentence {
    /// ISO 639-1 code, lowercase (`en`).
    pub language: String,
    /// The language's English name (`German`), or the code for one lmgw
    /// has no name for.
    pub name: String,
    /// The sentence in effect: the owner's saved one, else the built-in
    /// one, else empty.
    pub text: String,
    /// The built-in sentence; `None` for a language the table lacks, which
    /// only an owner's sentence brings into the list.
    pub builtin: Option<String>,
    /// `text` is the owner's own sentence, saved for this language.
    pub saved: bool,
    /// The built-in line the cloning check speaks; empty without one.
    pub check_line: String,
}

/// `GET /voice-lab/api/sentences`: every language with a built-in or a saved
/// sentence, by code.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceSentences {
    pub languages: Vec<VoiceSentence>,
    /// Why the owner's saved sentences are not in the list: their file
    /// (`sentences.json`) is there and cannot be read. lmgw never writes
    /// over it; saving a sentence is refused until it is fixed or moved.
    #[serde(default)]
    pub error: Option<String>,
}

/// `POST /voice-lab/api/sentences`: save the owner's sentence for a
/// language. An empty `text` drops the saved one, so the built-in sentence
/// is the prefill again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceSentenceSet {
    /// ISO 639-1 code (`de`).
    pub language: String,
    pub text: String,
}

// ---------------------------------------------------------------------------
// What a model takes
// ---------------------------------------------------------------------------

/// What a text-to-speech model does with a voice description (the speech
/// route's `instructions`) — `capabilities.speech.instructions`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum VoiceDescriptionMode {
    /// Not read: a description would be dropped on the way.
    #[default]
    None,
    /// A speaking style for the voice it speaks.
    Style,
    /// The voice is designed from it.
    VoiceDesign,
    /// Sent; the model decides what it does with it (OmniVoice designs a
    /// voice from it, a cloud model nobody described reads it as it reads
    /// instructions).
    Passthrough,
}

/// The lmgw audio row behind a route.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceRow {
    /// The row's model id.
    pub model_id: String,
    /// The audio.cpp family (`qwen3_tts`).
    pub family: String,
    /// The package's variant when it says (`voice_design`).
    pub variant: Option<String>,
}

/// `GET /voice-lab/api/model`'s query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceModelQuery {
    /// The text-to-speech alias.
    pub alias: String,
    /// The language the form is set to, for `language_note`.
    #[serde(default)]
    pub language: Option<String>,
    /// The voice the form names, for `language_note` (some families speak
    /// the language of their voice).
    #[serde(default)]
    pub voice: Option<String>,
}

/// `GET /voice-lab/api/model`: what the Voices lab's form offers for a
/// text-to-speech alias — read from lmgw's catalog, starting nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceModelFacts {
    pub alias: String,
    /// An lmgw audio row (`row` names it); otherwise a cloud alias.
    pub local: bool,
    pub row: Option<VoiceRow>,
    /// What a voice description does.
    pub description: VoiceDescriptionMode,
    /// The model speaks only with a description, and the row has no
    /// default one.
    pub description_required: bool,
    /// The row has a default description of its own, which a request
    /// without one gets.
    pub description_default: bool,
    /// The model speaks a voice it is told (a preset, a shipped voice, a
    /// library clip); `false` for one that designs its voice instead.
    pub takes_voice: bool,
    /// What speaks when no voice is named, when lmgw knows.
    pub default_voice: Option<String>,
    /// A seed reaches the model: each candidate is drawn from its own.
    /// `false` for a cloud model (lmgw sends it none) and for an engine that
    /// takes none.
    pub reads_seed: bool,
    /// The row pins a seed of its own (its default request options): the
    /// candidates' seeds are sent, and what the engine does with both is
    /// its own.
    pub row_pins_seed: bool,
    /// ISO 639-1 codes the model declares it speaks; empty when it declares
    /// none.
    pub languages: Vec<String>,
    /// What happens to the asked language, when it does not reach the model
    /// as set ("does not speak German (de) — not sent").
    pub language_note: Option<String>,
    /// The model clones a voice-library clip (the cloning check's pick).
    pub clones: bool,
    /// Why it would not, or how, in a sentence.
    pub clone_note: Option<String>,
    /// The GPU hold (or a benchmark's lease) pauses the row now.
    pub held: bool,
    /// The alias that answers in its place now, when one does.
    pub fallback: Option<String>,
    /// The licence the row's package states, when it states one.
    /// Information only: never a filter.
    pub licence: Option<ModelLicence>,
}

// ---------------------------------------------------------------------------
// Rounds and candidates
// ---------------------------------------------------------------------------

/// `voice_generate` / `POST /voice-lab/api/rounds`: render candidates.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceGenerateArgs {
    /// The text-to-speech alias, local or cloud.
    pub alias: String,
    /// What every candidate says. Absent or blank: the language's sentence
    /// (the owner's saved one, else the built-in one).
    #[serde(default)]
    pub text: Option<String>,
    /// ISO 639-1 code: picks the sentence, and is sent to a local row that
    /// takes a language, in its own spelling.
    #[serde(default)]
    pub language: Option<String>,
    /// The voice description (or speaking style) for a model that reads one.
    #[serde(default)]
    pub description: Option<String>,
    /// The voice for a model that speaks a voice it is told.
    #[serde(default)]
    pub voice: Option<String>,
    /// How many candidates; 4 when absent. Rendered one after the other.
    #[serde(default)]
    pub count: Option<u32>,
    /// The first candidate's seed; the next get `seed + 1`, `seed + 2`, …
    /// Absent: a random seed per candidate.
    #[serde(default)]
    pub seed: Option<u32>,
}

/// One rendered candidate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceCandidate {
    /// Its number in the round, from 1.
    pub index: u32,
    /// The seed it was sent; `None` when no seed reached the model.
    pub seed: Option<u32>,
    /// Its length, read from its WAV header; `None` for audio that is no
    /// PCM WAV.
    pub duration_ms: Option<u64>,
    pub bytes: u64,
    /// Its content type (`audio/wav`).
    pub mime: String,
    /// Where it plays: `GET` this path.
    pub url: String,
}

/// One round: what was asked, who answered, and its candidates.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceRound {
    /// The round's id, its directory's name.
    pub id: String,
    /// RFC 3339, UTC.
    pub created_at: String,
    /// The alias asked.
    pub alias: String,
    /// The alias that answered: `alias`, or the fallback the GPU hold (or a
    /// busy card) handed the round to.
    pub answered_by: String,
    /// Why a fallback answered (`hold`, `external_vram`, …).
    pub fallback_reason: Option<String>,
    /// The upstream that answered.
    pub upstream: String,
    /// The model name it was sent.
    pub upstream_model: String,
    /// The lmgw audio row behind the answering route, if it is one.
    pub row: Option<VoiceRow>,
    /// The licence the answering row's package states, when it states one.
    pub licence: Option<ModelLicence>,
    /// What every candidate was asked to say.
    pub text: String,
    /// What the engine was sent instead, when lmgw's speech shaping changed
    /// it (inline tags or characters the engine cannot say): what the
    /// candidates say, and the transcript a saved sample gets.
    #[serde(default)]
    pub sent_text: Option<String>,
    pub language: Option<String>,
    /// What happened to the language when it did not reach the model as
    /// set.
    pub language_note: Option<String>,
    pub description: Option<String>,
    pub voice: Option<String>,
    /// Each candidate's seed reached the model.
    pub seeds_sent: bool,
    /// What lmgw's speech shaping changed in the request
    /// (`x-lmgw-speech`'s words), when it changed anything.
    pub shaping: Option<String>,
    /// How many candidates were asked for.
    pub asked: u32,
    pub candidates: Vec<VoiceCandidate>,
    /// Why the round ended before `asked` candidates: the first failure,
    /// or a stop (the page left, Stop, the round discarded).
    pub stopped: Option<String>,
}

/// `GET /voice-lab/api/rounds`: every kept round, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceRounds {
    pub rounds: Vec<VoiceRound>,
    /// What the rounds' audio takes on disk.
    pub bytes: u64,
    /// Where they are kept.
    pub dir: String,
}

// ---------------------------------------------------------------------------
// Samples
// ---------------------------------------------------------------------------

/// `voice_sample_save` / `POST /voice-lab/api/samples`: save a candidate to
/// the voice library.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceSampleSaveArgs {
    /// The round's id.
    pub round: String,
    /// The candidate's number in it, from 1.
    pub candidate: u32,
    /// The voice name it is saved under: what a request's `voice` names.
    /// Letters, digits, space, `.`, `_` and `-`; a trailing `.wav` is
    /// dropped.
    pub name: String,
    /// Replace a sample or clip of that name. Without it, a taken name is
    /// refused (`409 name_taken`) and nothing is written.
    #[serde(default)]
    pub overwrite: bool,
}

/// What trimming took off a saved sample. Only silence is cut; the samples
/// kept are the candidate's own, at their own loudness.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceTrim {
    /// Silence cut from the start, in milliseconds.
    pub lead_ms: u64,
    /// Silence cut from the end.
    pub tail_ms: u64,
    /// What is left.
    pub kept_ms: u64,
    /// Why nothing was cut, when nothing could be (not a PCM WAV, nothing
    /// above the silence floor).
    pub note: Option<String>,
}

/// How a sample was made: kept beside the library by the Voices lab, and
/// shown with the clip. "Regenerate" refills the lab's form from it — GPU
/// output is not bit-identical per seed, so it makes a new candidate rather
/// than restoring this one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceProvenance {
    /// The alias asked.
    pub alias: String,
    /// The alias that answered (a fallback named as one).
    pub answered_by: String,
    pub fallback_reason: Option<String>,
    pub upstream: String,
    pub upstream_model: String,
    /// The lmgw audio row behind the answering route.
    pub row: Option<VoiceRow>,
    pub description: Option<String>,
    pub voice: Option<String>,
    /// The seed the candidate was sent, when one reached the model.
    pub seed: Option<u32>,
    pub language: Option<String>,
    /// What the clip says: its transcript — the text the engine was sent.
    pub text: String,
    /// What the round asked it to say, when lmgw's speech shaping sent the
    /// engine something else (`text`). Regenerate asks this again.
    #[serde(default)]
    pub asked_text: Option<String>,
    /// The round's time, RFC 3339.
    pub generated_at: String,
    /// The save's time, RFC 3339.
    pub saved_at: String,
    /// The licence the answering row's package states, when known.
    pub licence: Option<ModelLicence>,
    /// The round and the candidate it came from.
    pub round: String,
    pub candidate: u32,
    pub trim: VoiceTrim,
    /// The saved clip's SHA-256, hex: a clip replaced by other means no
    /// longer matches it.
    pub sha256: String,
}

/// `voice_sample_save`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceSampleSaved {
    pub ok: bool,
    /// The clip's file name in the library (`anna.wav`).
    pub clip: String,
    /// The voice name a request uses (`anna`).
    pub voice: String,
    /// The library files it replaced (with `overwrite`).
    pub replaced: Vec<String>,
    /// The transcript recorded for it.
    pub transcript: String,
    pub provenance: VoiceProvenance,
    /// The class's voice directory is this library, so `voice` names the
    /// clip on every cloning model; otherwise it is reachable as a
    /// `voice_ref` path only.
    pub library_active: bool,
    /// The outcome in a sentence.
    pub message: String,
}

/// One library clip with how it was made, when the Voices lab made it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceSample {
    pub clip: Clip,
    /// The Voices lab's record of it; `None` for a clip it did not make.
    pub provenance: Option<VoiceProvenance>,
    /// The record still describes the file (its SHA-256 matches). `false`
    /// with a record: the clip was replaced since, by an upload or by hand.
    pub provenance_current: bool,
}

/// `GET /voice-lab/api/samples`: the voice library.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceSamples {
    pub samples: Vec<VoiceSample>,
    /// The class's voice directory is this library: a clip's name is a
    /// voice on every cloning model.
    pub library_active: bool,
    /// Why there is no library (no audio models directory).
    pub error: Option<String>,
    /// Why no sample shows its record: the records' file (`samples.json`)
    /// is there and cannot be read. lmgw never writes over it; saving a
    /// sample is refused until it is fixed or moved.
    #[serde(default)]
    pub records_error: Option<String>,
}

/// `POST /voice-lab/api/clone-check`: speak a test line with a saved sample
/// as the reference voice. The answer is the speech route's own: audio, or
/// its error envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceCloneCheck {
    /// The cloning text-to-speech alias.
    pub alias: String,
    /// The sample: its voice name (`anna`) or file name (`anna.wav`).
    pub sample: String,
    /// What to say; absent or blank: the language's check line.
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_args_refuse_a_misspelt_field() {
        let ok: VoiceGenerateArgs =
            serde_json::from_str(r#"{"alias":"audio/tts","count":2}"#).unwrap();
        assert_eq!(ok.count, Some(2));
        assert!(
            serde_json::from_str::<VoiceGenerateArgs>(r#"{"alias":"a","seeds":[1]}"#)
                .unwrap_err()
                .to_string()
                .contains("seeds")
        );
    }

    #[test]
    fn save_args_default_to_no_overwrite() {
        let a: VoiceSampleSaveArgs =
            serde_json::from_str(r#"{"round":"r","candidate":1,"name":"anna"}"#).unwrap();
        assert!(!a.overwrite);
    }
}
