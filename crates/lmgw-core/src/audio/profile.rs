//! A TTS row's speech profile: what its engine accepts by name — native
//! voices, where a voice name goes in the request, the default voice, the
//! language vocabulary — gathered from the spec and the package's own files
//! rather than asked of a running container.
//!
//! Sources, in order of authority:
//! 1. the spec — the row's `model_spec_override` file when it has one, else
//!    the catalog snapshot's family ([`crate::web::audio`]'s cached catalog,
//!    never fetched here), else the spec the package GGUF embeds;
//! 2. the package GGUF's embedded files ([`crate::gguf::embedded`]):
//!    Supertonic's `voice_style_*` sources and `unicode_indexer` (the
//!    characters its engine says, [`super::charset`]), Qwen3-TTS's `config.json`
//!    speakers, languages and variant, Nemotron ASR's language prompts
//!    ([`super::families`]); a SanoTTS or Kroko package's `config.json`
//!    (embedded, else beside the GGUF) for its one language;
//! 3. the row itself (its presets, its default preset) — applied per call in
//!    [`super::voices`], not cached here.
//!
//! The profile is cached in [`ProfileCache`] per row, keyed by everything it
//! is computed from: the row's model id, family, path, task, weight id,
//! spec override and `language` load option, the catalog's fetch time, the
//! selected GGUF's path, length and modification time, and the same of each
//! package file it read from beside the GGUF ([`SpeechProfile::beside`]). A
//! change to any of them computes it again; the computation runs on the
//! blocking pool (a directory walk and a GGUF header read).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::Serialize;
use serde_json::Value;

use super::charset::CharVocab;
use super::families::TakesLanguage;
use super::tags::TagMode;
use super::{families, parse_spec, ModelSpec};
use crate::config::AudioModel;
use crate::gguf::embedded::{read_embedded_index, EmbeddedIndex, MAX_EMBEDDED_FILE};

/// Where an engine reads a voice name from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceField {
    /// The request's `voice` (audio.cpp's `cached_voice_id`) — most
    /// families.
    #[default]
    Voice,
    /// `options.voice_id`: the family declares a `voice_id` request option
    /// and reads that, ignoring `voice` (MagpieTTS,
    /// `magpie_tts/request.cpp`).
    OptionsVoiceId,
}

impl VoiceField {
    /// The request field, as `GET /v1/audio/voices` names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Voice => "voice",
            Self::OptionsVoiceId => "options.voice_id",
        }
    }
}

/// Where a language vocabulary came from — what decides whether a
/// language lmgw only infers (a realtime session's) may be sent at all
/// ([`super::language`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VocabSource {
    /// None known: a language is sent as it came.
    #[default]
    None,
    /// The package's own table (Qwen3-TTS `codec_language_id`, plus `auto`).
    Gguf,
    /// The spec's `language` request option, an enum.
    SpecEnum,
    /// The spec's `languages`, when they are names (`english`), not codes.
    SpecNames,
    /// The spec's `languages` codes, for a family [`families::language_regions`]
    /// lists.
    FamilyTable,
    /// The spec's `languages`, which a family [`families::takes_language`]
    /// lists as taking exactly those (Supertonic's codes, FireRedTTS3's
    /// names): its request vocabulary.
    SpecRequest,
}

/// What a row's engine does with a request's `instructions` (audio-class
/// gap 9a) — `capabilities.speech.instructions`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionsMode {
    /// Not read: dropped on the way, and said so in `x-lmgw-speech`.
    #[default]
    None,
    /// A speaking style for the voice (Qwen3-TTS CustomVoice).
    Style,
    /// The description the voice is designed from — required (Qwen3-TTS
    /// VoiceDesign, MOSS-VoiceGen).
    VoiceDesign,
    /// The engine reads them — its spec declares an instruction option, or
    /// [`families::undeclared_instructions`] says it reads one anyway: sent,
    /// and the engine says what it does with them.
    Passthrough,
}

impl InstructionsMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Style => "style",
            Self::VoiceDesign => "voice_design",
            Self::Passthrough => "passthrough",
        }
    }

    /// The published word back into a mode (`capabilities.speech`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "style" => Some(Self::Style),
            "voice_design" => Some(Self::VoiceDesign),
            "passthrough" => Some(Self::Passthrough),
            _ => None,
        }
    }
}

/// What a row's engine does with a speech request that names no voice at
/// all — no `voice`, no `voice_ref`, no preset: audio.cpp's
/// `build_speech_request` then hands it no speaker
/// ([`families::unvoiced`], live run 3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Unvoiced {
    /// Not known: a voice is always named (realtime refuses to call it
    /// without one, `voice_not_configured`).
    #[default]
    Unknown,
    /// It speaks with a fixed voice of its own — `voice` names it when it is
    /// one the package ships (Kokoro's `af_heart`), `None` when the engine
    /// needs nothing for it (Magpie takes its first speaker).
    EngineDefault { voice: Option<&'static str> },
    /// It speaks with a voice of its own that it samples per request from
    /// its RNG (OmniVoice): a different speaker per request. A seed fixes
    /// the draw for one text, not the speaker across texts (live run 3c),
    /// so realtime, which sends a request per clause, never lets it speak
    /// unnamed (`voice_not_configured`, R5 F1); `/v1/audio/speech`, one
    /// request one speaker, does. A session still sends its one seed with
    /// every clause (`proxy::synthesize::sends_seed`, R4 M1).
    DrawsSpeaker,
    /// It clones from reference audio and refuses without (CosyVoice3):
    /// preflight refuses such a request before anything starts
    /// (`reference_required`).
    NeedsReference,
}

impl Unvoiced {
    /// The engine speaks one fixed voice of its own when none is named —
    /// not one it draws per request ([`Self::DrawsSpeaker`]), which changes
    /// from clause to clause of a realtime answer (R5 F1).
    pub fn fixed_voice(self) -> bool {
        matches!(self, Self::EngineDefault { .. })
    }
}

/// Where a row's engine reads instructions from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionsField {
    /// The request's `instructions`, which audio.cpp hands the engine as
    /// `options.instruction` (`build_speech_request`).
    #[default]
    Instructions,
    /// `options.instruct`: the family's spec declares only that option (Auk,
    /// MOSS-TTS v1.5, MOSS-TTSD), so `instructions` moves there.
    OptionsInstruct,
}

/// The languages a family's request takes, by the spelling it takes them in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LanguageVocab {
    pub source: VocabSource,
    pub entries: Vec<String>,
}

/// What a TTS row's engine accepts by name (module doc).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SpeechProfile {
    pub family: String,
    /// Voices the engine ships, canonical spelling — including ones whose
    /// file the package may lack ([`Self::file_backed`]).
    pub native_voices: Vec<String>,
    /// Native voices the family keeps as `<root>/embeddings/<name>.safetensors`
    /// (a package of the spec lists that file): present only when the file
    /// is (Pocket TTS's `alba` ships with the English package only).
    pub file_backed: Vec<String>,
    pub voice_field: VoiceField,
    /// The voice the engine speaks with when none is named: the spec's
    /// `ui.default_voice`, else its `voice_id` option's default.
    pub default_voice: Option<String>,
    pub language_vocab: LanguageVocab,
    /// The spec's `languages`, as written.
    pub spec_languages: Vec<String>,
    /// The spec declares a `language` request option.
    pub language_option: bool,
    /// The one language a family that speaks or hears its package's
    /// language only has, as an ISO 639-1 code (Pocket TTS, SanoTTS, Kroko
    /// ASR: [`families::package_language`]); `None` for any other family, or
    /// when the package does not say.
    pub package_language: Option<String>,
    /// The family wants a cloned clip's transcript: its spec declares a
    /// `reference_text` request option, or its engine refuses without one
    /// ([`Self::requires_reference_text`]). A library clip without one is
    /// worth a note.
    pub needs_reference_text: bool,
    /// The engine refuses to clone a clip without its transcript
    /// ([`families::clone_requires_transcript`] — what no spec says, read
    /// off audio.cpp's source): speech with an untranscribed library clip is
    /// refused before anything starts (`voice_needs_transcript`,
    /// [`super::transcript`]).
    pub requires_reference_text: bool,
    /// The spec lists the `streaming` mode.
    pub streaming: bool,
    /// The `--task` the package's variant runs, when the package says
    /// (Qwen3-TTS `tts_model_type`).
    pub variant_task: Option<String>,
    /// The variant's own name (`voice_design`), for a message about it.
    pub variant: Option<String>,
    /// What the engine does with `instructions` (gap 9a).
    pub instructions: InstructionsMode,
    /// Where it reads them.
    pub instructions_field: InstructionsField,
    /// What it does with inline tags (gap 9b).
    pub inline_tags: TagMode,
    /// The tags a [`TagMode::Fixed`] family renders, in its spelling.
    pub tags: Vec<String>,
    /// The engine takes a request's `seed`: the spec declares the option,
    /// or [`families::reads_seed`] says it reads one anyway (Qwen3-TTS).
    pub reads_seed: bool,
    /// What it does with a request that names no voice
    /// ([`families::unvoiced`]).
    pub unvoiced: Unvoiced,
    /// The characters its engine says, for a family that refuses one its
    /// package lacks ([`families::char_vocabulary`]): read from the
    /// package (`char_vocab`). `None` for every other family, and for one
    /// whose package does not have it (a problem then): its `input` goes as
    /// it came.
    #[serde(skip)]
    pub char_vocab: Option<Arc<CharVocab>>,
    /// The package files read from beside the GGUF, or looked for there:
    /// the cache computes the profile again when one of them changes, is
    /// added or goes ([`ProfileCache`]). Each is kept with its stamp, taken
    /// where the file was first looked at, before it was read: an edit
    /// during the computation then shows. Only [`Self::note_beside`] adds
    /// to it, so a path never goes without its stamp.
    #[serde(skip)]
    pub(crate) beside: Beside,
    /// What the facts above were read from, for a log line or a dashboard.
    pub sources: Vec<String>,
    /// Facts that could not be read, and why (a corrupt GGUF, an embedded
    /// file over the bound). Logged once per computation.
    pub problems: Vec<String>,
    /// What the engine does to a text that its package's vocabulary cannot
    /// follow (`char_vocab`): the facts above are all used, so these are no
    /// problems of the profile. Logged once per computation.
    pub notes: Vec<String>,
}

/// The package files a profile read from beside its GGUF, each with its
/// stamp. The list is private: [`SpeechProfile::note_beside`] is the only
/// way in, so a path never goes without its stamp.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Beside(Vec<(PathBuf, FileStamp)>);

impl Beside {
    /// The noted paths, in order.
    #[cfg(test)]
    pub(crate) fn paths(&self) -> Vec<&Path> {
        self.0.iter().map(|(p, _)| p.as_path()).collect()
    }

    /// The stamp noted for `path`, if it was noted.
    #[cfg(test)]
    pub(crate) fn stamp_of(&self, path: &Path) -> Option<FileStamp> {
        self.0.iter().find(|(p, _)| p == path).map(|(_, s)| *s)
    }
}

impl SpeechProfile {
    /// Notes `path`, a package file beside the GGUF the profile reads or
    /// looks for, and its stamp as it is now ([`Self::beside`]).
    pub(crate) fn note_beside(&mut self, path: PathBuf) {
        let stamp = stamp(&path);
        self.beside.0.push((path, stamp));
    }

    /// Whether every package file noted in [`Self::beside`] still has its
    /// stamp.
    pub(crate) fn beside_unchanged(&self) -> bool {
        self.beside.0.iter().all(|(path, was)| stamp(path) == *was)
    }

    /// `name` as a native voice, compared without case: its canonical
    /// spelling. Engines differ (Qwen3 lowercases, Magpie compares exactly),
    /// so lmgw accepts any case and sends the spelling the package uses.
    pub fn native(&self, name: &str) -> Option<&str> {
        self.native_voices
            .iter()
            .find(|v| v.eq_ignore_ascii_case(name))
            .map(String::as_str)
    }
}

/// Compute a profile from a row, the spec chosen for it (override or
/// catalog; `None` falls back to the GGUF's embedded spec) and the row's
/// package GGUF. Blocking: reads the GGUF header.
pub fn compute(row: &AudioModel, spec: Option<&ModelSpec>, gguf: Option<&Path>) -> SpeechProfile {
    let mut p = SpeechProfile {
        family: row.family.clone(),
        ..SpeechProfile::default()
    };
    let index = gguf.and_then(|g| match read_embedded_index(g) {
        Ok(i) => Some(i),
        Err(e) => {
            p.problems.push(format!(
                "{}: {e}",
                g.file_name().unwrap_or_default().to_string_lossy()
            ));
            None
        }
    });
    let embedded_spec: Option<Value> = index
        .as_ref()
        .and_then(|i| i.model_spec_json.as_deref())
        .and_then(|j| serde_json::from_str(j).ok());
    let fallback = spec
        .is_none()
        .then(|| embedded_spec.as_ref().map(parse_spec))
        .flatten();
    let spec = spec.or(fallback.as_ref());
    if let Some(s) = spec {
        p.sources.push(if fallback.is_some() {
            "the spec embedded in the package GGUF".into()
        } else {
            "the catalog spec".into()
        });
        from_spec(&mut p, s);
    }
    // What no spec declares: an engine that reads `instruction` anyway
    // (OmniVoice, MOSS-VoiceGen), from the family table.
    if let Some(mode) = families::undeclared_instructions(&row.family) {
        p.instructions = mode;
        p.instructions_field = InstructionsField::Instructions;
    }
    if let Some(v) = &embedded_spec {
        voice_styles(&mut p, v);
    }
    if let Some(i) = &index {
        if families::reads_qwen3_config(&row.family) {
            qwen3_config(&mut p, i);
        }
        if families::reads_prompt_dictionary(&row.family) {
            prompts::prompt_dictionary(&mut p, i);
        }
    }
    package_language::read(&mut p, row, index.as_ref(), gguf);
    char_vocab::read(&mut p, embedded_spec.as_ref(), index.as_ref(), gguf);
    // What no spec says: the tags a tokenizer renders, and for a Qwen3
    // package the GGUF could not tell, the row's own task (a VoiceDesign
    // row runs `vdes`).
    if let Some(vocab) = families::tag_vocabulary(&row.family) {
        p.inline_tags = TagMode::Fixed;
        p.tags = vocab.iter().map(|t| t.to_string()).collect();
    } else if families::free_form_tags(&row.family) {
        p.inline_tags = TagMode::Free;
    }
    if families::reads_seed(&row.family) {
        p.reads_seed = true;
    }
    if families::reads_qwen3_config(&row.family) && p.variant.is_none() {
        p.instructions = if row.task == "vdes" {
            InstructionsMode::VoiceDesign
        } else {
            InstructionsMode::Passthrough
        };
    }
    // What no spec says: an engine that refuses a clip without its
    // transcript — OmniVoice declares no options at all (after the Qwen3
    // config, whose variant decides for Qwen3-TTS).
    if families::clone_requires_transcript(&row.family, p.variant.as_deref()) {
        p.requires_reference_text = true;
        p.needs_reference_text = true;
    }
    p.native_voices.sort();
    p.native_voices.dedup();
    p.unvoiced = match families::unvoiced(&row.family, p.variant.as_deref()) {
        // A built-in default the package lacks would be refused by the
        // engine: not one to count on.
        Some(Unvoiced::EngineDefault { voice: Some(v) })
            if !p.native_voices.is_empty() && p.native(v).is_none() =>
        {
            Unvoiced::Unknown
        }
        Some(u) => u,
        None => Unvoiced::Unknown,
    };
    p
}

/// Natives, default, languages and flags from a spec.
fn from_spec(p: &mut SpeechProfile, s: &ModelSpec) {
    // Magpie: a `voice_id` request option with named values, read instead of
    // `voice`. A numeric value is an index, not a name.
    if let Some(o) = s.options.request.iter().find(|o| o.name == "voice_id") {
        let named: Vec<String> = o.values.iter().filter(|v| !is_index(v)).cloned().collect();
        if !named.is_empty() {
            p.voice_field = VoiceField::OptionsVoiceId;
            p.native_voices.extend(named);
            p.default_voice = o
                .default
                .as_ref()
                .and_then(Value::as_str)
                .filter(|d| !is_index(d))
                .map(str::to_string);
        }
    }
    p.native_voices.extend(s.builtin_voices.iter().cloned());
    if let Some(d) = s.default_voice.as_ref().filter(|d| !d.trim().is_empty()) {
        p.default_voice = Some(d.clone());
    }
    p.file_backed = s
        .builtin_voices
        .iter()
        .filter(|v| {
            let file = format!("embeddings/{v}.safetensors");
            s.packages
                .iter()
                .flat_map(|pkg| pkg.files.iter())
                .any(|f| f == &file || f.ends_with(&format!("/{file}")))
        })
        .cloned()
        .collect();
    p.spec_languages = s.languages.clone();
    p.language_option = s.options.request.iter().any(|o| o.name == "language");
    p.needs_reference_text = s.options.request.iter().any(|o| o.name == "reference_text");
    p.streaming = s.modes.iter().any(|m| m == "streaming");
    p.language_vocab = spec_vocab(s);
    // `instructions` reach the engine as `options.instruction`; a family
    // that declares only `instruct` gets them moved there.
    let declares = |name: &str| s.options.request.iter().any(|o| o.name == name);
    p.reads_seed = declares("seed");
    if declares("instruction") {
        p.instructions = InstructionsMode::Passthrough;
    } else if declares("instruct") {
        p.instructions = InstructionsMode::Passthrough;
        p.instructions_field = InstructionsField::OptionsInstruct;
    }
}

/// The spec's language vocabulary: the `language` option's enum, else
/// `languages` for a family that takes exactly those, else `languages` when
/// they are names, else `languages` for a family the regions table lists —
/// or none.
fn spec_vocab(s: &ModelSpec) -> LanguageVocab {
    if let Some(o) = s
        .options
        .request
        .iter()
        .find(|o| o.name == "language" && !o.values.is_empty())
    {
        return LanguageVocab {
            source: VocabSource::SpecEnum,
            entries: o.values.clone(),
        };
    }
    if families::takes_language(&s.family) == Some(TakesLanguage::SpecLanguages)
        && !s.languages.is_empty()
    {
        return LanguageVocab {
            source: VocabSource::SpecRequest,
            entries: s.languages.clone(),
        };
    }
    let names = !s.languages.is_empty()
        && s.languages
            .iter()
            .all(|l| l.len() > 3 && l.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    if names {
        return LanguageVocab {
            source: VocabSource::SpecNames,
            entries: s.languages.clone(),
        };
    }
    if families::language_regions(&s.family).is_some() && !s.languages.is_empty() {
        return LanguageVocab {
            source: VocabSource::FamilyTable,
            entries: s.languages.clone(),
        };
    }
    LanguageVocab::default()
}

fn is_index(v: &str) -> bool {
    !v.is_empty() && v.chars().all(|c| c.is_ascii_digit())
}

/// Supertonic: its embedded spec's sources name one file per built-in voice,
/// `voice_style_<name>` (`model:voice_styles/<name>.json`).
fn voice_styles(p: &mut SpeechProfile, spec: &Value) {
    let Some(sources) = spec.get("sources").and_then(Value::as_array) else {
        return;
    };
    let mut found = Vec::new();
    for src in sources {
        for group in ["files", "optional_files"] {
            if let Some(files) = src.get(group).and_then(Value::as_object) {
                found.extend(
                    files
                        .keys()
                        .filter_map(|k| k.strip_prefix("voice_style_"))
                        .filter(|n| !n.is_empty())
                        .map(str::to_string),
                );
            }
        }
    }
    if !found.is_empty() {
        p.sources
            .push("the voice styles the package GGUF embeds".into());
        p.native_voices.extend(found);
    }
}

/// Qwen3-TTS: speakers (CustomVoice only — the other variants have no
/// speaker table that a request may name), languages and variant from the
/// embedded `config.json`.
fn qwen3_config(p: &mut SpeechProfile, index: &EmbeddedIndex) {
    let raw = match index.file("config.json") {
        Ok(Some(raw)) => raw,
        Ok(None) => return,
        Err(e) => {
            p.problems.push(format!("config.json: {e}"));
            return;
        }
    };
    let Ok(cfg) = serde_json::from_slice::<Value>(&raw) else {
        p.problems.push("config.json: not JSON".into());
        return;
    };
    let model_type = cfg.get("tts_model_type").and_then(Value::as_str);
    p.variant_task = model_type
        .and_then(families::qwen3_variant_task)
        .map(str::to_string);
    if let Some(mode) = model_type.and_then(families::qwen3_variant_instructions) {
        p.variant = model_type.map(str::to_string);
        p.instructions = mode;
        p.instructions_field = InstructionsField::Instructions;
    }
    let talker = cfg.get("talker_config");
    let keys = |k: &str| -> Vec<String> {
        talker
            .and_then(|t| t.get(k))
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    };
    if model_type == Some("custom_voice") {
        p.native_voices.extend(keys("spk_id"));
    }
    let mut languages = keys("codec_language_id");
    if !languages.is_empty() {
        languages.sort();
        languages.push("auto".into());
        p.language_vocab = LanguageVocab {
            source: VocabSource::Gguf,
            entries: languages,
        };
    }
    p.sources
        .push("the config.json the package GGUF embeds".into());
}

// ---------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------

/// Everything a profile is computed from (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfileKey {
    family: String,
    path: String,
    task: String,
    weight_id: Option<String>,
    spec_override: Option<String>,
    catalog: Option<String>,
    gguf: Option<(PathBuf, u64, Option<SystemTime>)>,
    /// The row's `language` load option (Pocket TTS's package language).
    load_language: Option<String>,
}

/// The profiles computed so far: one per row (by model id), replaced when
/// its key changes — so the map never holds more than one entry per row.
#[derive(Debug, Default)]
pub struct ProfileCache {
    rows: Mutex<HashMap<String, Cached>>,
    computed: std::sync::atomic::AtomicU64,
}

/// A row's profile, with its key; the stamps of the files it read from
/// beside its GGUF are the profile's own ([`SpeechProfile::beside`]).
type Cached = (ProfileKey, Arc<SpeechProfile>);

/// A file's length and modification time; `None` when it is not there.
pub(crate) type FileStamp = Option<(u64, Option<SystemTime>)>;

/// The [`FileStamp`] of `path` as it is now.
fn stamp(path: &Path) -> FileStamp {
    std::fs::metadata(path)
        .ok()
        .map(|m| (m.len(), m.modified().ok()))
}

impl ProfileCache {
    /// How many profiles were computed (cache misses) — for tests.
    pub fn computed(&self) -> u64 {
        self.computed.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// The profile of `row`, from the cache or computed on the blocking pool.
/// `models_dir` is the audio class's; `catalog` the cached snapshot's spec
/// for the row's family and its fetch time.
pub async fn profile_of(
    cache: &Arc<ProfileCache>,
    models_dir: &str,
    row: &AudioModel,
    catalog: Option<(String, ModelSpec)>,
) -> Arc<SpeechProfile> {
    let cache = cache.clone();
    let models_dir = models_dir.to_string();
    let row = row.clone();
    let model_id = row.model_id.clone();
    let computed =
        tokio::task::spawn_blocking(move || profile_blocking(&cache, &models_dir, &row, catalog))
            .await;
    match computed {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("audio: the speech profile of '{model_id}' could not be computed: {e}");
            Arc::new(SpeechProfile::default())
        }
    }
}

fn profile_blocking(
    cache: &ProfileCache,
    models_dir: &str,
    row: &AudioModel,
    catalog: Option<(String, ModelSpec)>,
) -> Arc<SpeechProfile> {
    let root = super::files::row_root(models_dir, row);
    let gguf = super::files::row_gguf(
        Path::new(models_dir),
        &root,
        row.weight_id.as_deref(),
        &row.family,
    )
    .map(|(path, len)| {
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        (path, len, mtime)
    });
    let spec_override = row
        .model_spec_override
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let key = ProfileKey {
        family: row.family.clone(),
        path: row.path.clone(),
        task: row.task.clone(),
        weight_id: row.weight_id.clone(),
        spec_override: spec_override.clone(),
        catalog: catalog.as_ref().map(|(at, _)| at.clone()),
        gguf: gguf.clone(),
        load_language: package_language::load_option(row).map(str::to_string),
    };
    // Cloned out, so the files are not looked at under the lock.
    let cached = cache.rows.lock().unwrap().get(&row.model_id).cloned();
    if let Some((k, p)) = cached {
        if k == key && p.beside_unchanged() {
            return p;
        }
    }
    let over = spec_override
        .as_deref()
        .and_then(|o| override_spec(models_dir, o, &row.family));
    let spec = over.or(catalog.map(|(_, s)| s));
    let profile = Arc::new(compute(
        row,
        spec.as_ref(),
        gguf.as_ref().map(|g| g.0.as_path()),
    ));
    for note in &profile.notes {
        tracing::warn!("audio: speech profile of '{}': {note}", row.model_id);
    }
    for problem in &profile.problems {
        tracing::warn!(
            "audio: speech profile of '{}': {problem} — the facts it carries are not used",
            row.model_id
        );
    }
    cache
        .computed
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    cache
        .rows
        .lock()
        .unwrap()
        .insert(row.model_id.clone(), (key, profile.clone()));
    profile
}

/// A row's `model_spec_override`: a `<family>.json` file, or a directory
/// holding one, relative to the models dir — the spec the engine itself
/// uses for the row.
fn override_spec(models_dir: &str, rel: &str, family: &str) -> Option<ModelSpec> {
    let mut path = PathBuf::from(models_dir).join(rel.trim_start_matches('/'));
    if path.is_dir() {
        path = path.join(format!("{family}.json"));
    }
    let len = std::fs::metadata(&path).ok()?.len();
    if len > MAX_EMBEDDED_FILE {
        tracing::warn!(
            "audio: the spec override {} is {len} bytes, more than the {MAX_EMBEDDED_FILE} lmgw \
             reads for a profile; using the catalog spec",
            path.display()
        );
        return None;
    }
    let raw = std::fs::read(&path).ok()?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    Some(parse_spec(&v))
}

/// Nemotron ASR's language prompts, from the package's own processor config.
mod prompts;

/// The one language of a package that speaks or hears only its own.
mod package_language;

/// The characters an engine that refuses the rest says, from its package.
mod char_vocab;

#[cfg(test)]
mod tests;
