//! Speech shaping: one pure function from a client's `/v1/audio/speech`
//! body to the body a local audio.cpp row's engine understands, plus a
//! report of what it changed.
//!
//! Called on the **final** route (after admission, which may still swap to
//! a fallback). An lmgw audio row gets all of it ([`shape_speech`]); any
//! other route only the expressive half ([`shape_remote`]): its inline tags
//! stripped and its instructions sent as they came, unless the owner's
//! `capabilities_override` on the alias says what the model takes. The HTTP
//! route and the realtime path ([`crate::proxy::synthesize`]) call the same
//! functions, so a voice name, a language, an instruction or a tag means
//! the same on both. `input` is touched only for its inline tags
//! ([`super::tags`]) and, on an lmgw audio row whose engine refuses a
//! character its package lacks, for those characters ([`super::charset`]):
//! each is replaced by an equivalent it says, or dropped. Any other route
//! gets them as the client sent them.

use axum::http::HeaderValue;
use serde_json::{Map, Value};

use super::charset::fit;
use super::language::map_language;
use super::profile::{InstructionsField, InstructionsMode, SpeechProfile, VoiceField};
use super::tags::{shape_tags, TagMode};
use super::voices::{RowVoices, VoiceKind};
use crate::config::AudioModel;
use listed::listed;
pub use listed::HEADER_CODEPOINTS;

/// One change shaping made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShapeChange {
    /// The request's `voice` named a native voice of a family that reads
    /// `options.voice_id`, and went there (in the package's spelling).
    VoiceToOptions { name: String },
    /// A preset's (or the row's default preset's) `voice_id` named such a
    /// voice and was copied to `options.voice_id`.
    PresetToOptions { name: String },
    /// The request's `voice` named a native voice in another case, and was
    /// sent in the package's spelling.
    VoiceSpelling { from: String, to: String },
    /// The request's `language` was sent in the family's vocabulary.
    Language { from: String, to: String },
    /// `instructions` went to `options.instruct`, the only field the family
    /// reads them from.
    InstructionsToOptions,
    /// `instructions` were dropped: the model reads none (or the client's
    /// own `options.instruct` already says it).
    InstructionsDropped,
    /// The row's default request options describe the voice under the
    /// other synonym (`key`: `instruct` for a request's `instruction`, or
    /// the reverse), and the request's text went there too, so it replaces
    /// the row's ([`over_row_description`]).
    InstructionsOverRow { key: &'static str },
    /// Inline tags of `input` were rewritten into the family's spelling
    /// (`mapped`) or removed (`stripped`); tags kept as written are no
    /// change.
    Tags { mapped: u32, stripped: u32 },
    /// Characters of `input` the row's engine cannot say
    /// ([`super::charset::fit`]): `replaced` by an equivalent it says,
    /// `dropped` where none is — codepoints, each once, in the order the
    /// text has them.
    Chars {
        replaced: Vec<u32>,
        dropped: Vec<u32>,
    },
}

/// What [`shape_speech`] changed; empty when the body went as it came.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShapeReport {
    pub changes: Vec<ShapeChange>,
}

impl ShapeReport {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// The report as `x-lmgw-speech`'s value (`voice=options.voice_id;
    /// language=de->german`), `None` when nothing changed. Names that are not
    /// plain header text are left out of the value, never mangled into it.
    pub fn header_value(&self) -> Option<HeaderValue> {
        if self.is_empty() {
            return None;
        }
        let plain = |s: &str| {
            !s.is_empty()
                && s.len() <= 64
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        let parts: Vec<String> = self
            .changes
            .iter()
            .map(|c| match c {
                ShapeChange::VoiceToOptions { .. } => "voice=options.voice_id".to_string(),
                ShapeChange::PresetToOptions { .. } => "voice=preset->options.voice_id".into(),
                ShapeChange::VoiceSpelling { to, .. } if plain(to) => format!("voice={to}"),
                ShapeChange::VoiceSpelling { .. } => "voice=spelling".into(),
                ShapeChange::Language { from, to } if plain(from) && plain(to) => {
                    format!("language={from}->{to}")
                }
                ShapeChange::Language { .. } => "language=mapped".into(),
                ShapeChange::InstructionsToOptions => "instructions=options.instruct".into(),
                ShapeChange::InstructionsDropped => "instructions=dropped".into(),
                ShapeChange::InstructionsOverRow { key } => {
                    format!("instructions=also:options.{key}")
                }
                ShapeChange::Tags { mapped, stripped } => {
                    let mut n = Vec::new();
                    if *mapped > 0 {
                        n.push(format!("mapped:{mapped}"));
                    }
                    if *stripped > 0 {
                        n.push(format!("stripped:{stripped}"));
                    }
                    format!("tags={}", n.join(","))
                }
                ShapeChange::Chars { replaced, dropped } => {
                    let mut n = Vec::new();
                    if !replaced.is_empty() {
                        n.push(format!("replaced:{}", listed(replaced)));
                    }
                    if !dropped.is_empty() {
                        n.push(format!("dropped:{}", listed(dropped)));
                    }
                    format!("chars={}", n.join(","))
                }
            })
            .collect();
        HeaderValue::from_str(&parts.join("; ")).ok()
    }
}

/// Shape `body` (a `/v1/audio/speech` JSON object, `model` already
/// rewritten) for `row`, whose engine `profile` and `voices` describe.
pub fn shape_speech(
    profile: &SpeechProfile,
    voices: &RowVoices,
    row: &AudioModel,
    body: &mut Map<String, Value>,
) -> ShapeReport {
    let mut report = ShapeReport::default();
    shape_voice(profile, voices, row, body, &mut report);
    shape_language(profile, body, &mut report);
    shape_expressive(&Expressive::of(profile), body, &mut report);
    over_row_description(row, body, &mut report);
    shape_chars(profile, body, &mut report);
    report
}

/// `input` in the characters the row's engine says, when its package has a
/// vocabulary ([`super::charset`]) — after the tags are shaped, on the text
/// that is sent.
fn shape_chars(profile: &SpeechProfile, body: &mut Map<String, Value>, report: &mut ShapeReport) {
    if let Some(change) = body.get_mut("input").and_then(|t| fit_text(profile, t)) {
        report.changes.push(change);
    }
}

/// `text`, when it is a string, in the characters the row's engine says
/// ([`super::charset::fit`]): the change, or `None` when its package has no
/// vocabulary or the engine says every character. `input` here, a task's
/// `text` on `/v1/tasks/*` (`crate::proxy::audio`).
pub fn fit_text(profile: &SpeechProfile, text: &mut Value) -> Option<ShapeChange> {
    let vocab = profile.char_vocab.as_deref()?;
    let f = fit(text.as_str()?, vocab)?;
    *text = Value::String(f.text);
    Some(ShapeChange::Chars {
        replaced: f.replaced,
        dropped: f.dropped,
    })
}

/// The expressive half of shaping (audio-class gap 9) on a route that is
/// not an lmgw audio row: `rules` are the owner's override on its alias, or
/// [`Expressive::remote`].
pub fn shape_remote(rules: &Expressive, body: &mut Map<String, Value>) -> ShapeReport {
    let mut report = ShapeReport::default();
    shape_expressive(rules, body, &mut report);
    report
}

/// What a route does with `instructions` and inline tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expressive {
    pub instructions: InstructionsMode,
    pub field: InstructionsField,
    pub tags: TagMode,
    /// The tags a [`TagMode::Fixed`] route renders, in its spelling.
    pub vocab: Vec<String>,
    /// `instructions` is lmgw's assumption, not something the route
    /// declared: a remote route nobody described ([`Self::remote`]).
    /// Shaping sends its instructions all the same; it takes no delivery
    /// cues (`super::cues`), since a cloud TTS ignored every phrasing of
    /// one that was tried.
    pub assumed: bool,
}

impl Expressive {
    /// A local row's, from its profile.
    pub fn of(profile: &SpeechProfile) -> Self {
        Self {
            instructions: profile.instructions,
            field: profile.instructions_field,
            tags: profile.inline_tags,
            vocab: profile.tags.clone(),
            assumed: false,
        }
    }

    /// A remote route nobody described: instructions go as they came (an
    /// OpenAI TTS reads them; one that does not says so itself), and every
    /// inline tag is stripped, since lmgw cannot know it would be heard
    /// rather than read out. What the instructions do is assumed, so it
    /// takes no delivery cues.
    pub fn remote() -> Self {
        Self {
            instructions: InstructionsMode::Passthrough,
            assumed: true,
            ..Self::default()
        }
    }

    /// The `capabilities.speech.instructions` word the route declares,
    /// `None` when it is only assumed ([`Self::assumed`]) — what
    /// [`lmgw_api_types::realtime::takes_cues`] goes by.
    pub fn declared_instructions(&self) -> Option<&'static str> {
        (!self.assumed).then(|| self.instructions.as_str())
    }

    /// [`Self::remote`], with what the owner's `capabilities_override`
    /// declares under `capabilities.speech` (`instructions`, `inline_tags`,
    /// `tags`) in place of the defaults. A word that names no mode is
    /// ignored here; the override's own validation refuses it at save time.
    pub fn from_override(speech: Option<&Value>) -> Self {
        let mut r = Self::remote();
        let Some(s) = speech else {
            return r;
        };
        if let Some(m) = s
            .get("instructions")
            .and_then(Value::as_str)
            .and_then(InstructionsMode::parse)
        {
            r.instructions = m;
            r.assumed = false;
        }
        if let Some(m) = s
            .get("inline_tags")
            .and_then(Value::as_str)
            .and_then(TagMode::parse)
        {
            r.tags = m;
        }
        if let Some(list) = s.get("tags").and_then(Value::as_array) {
            r.vocab = list
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }
        r
    }
}

/// `instructions` where the route reads them (gap 9a), inline tags the way
/// it renders them (gap 9b).
fn shape_expressive(rules: &Expressive, body: &mut Map<String, Value>, report: &mut ShapeReport) {
    if body.contains_key("instructions") {
        let said = body
            .get("instructions")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.trim().is_empty());
        match (rules.instructions, rules.field) {
            // Removed even when empty: audio.cpp hands any `instructions` to
            // the engine as `options.instruction`, and a family that
            // validates its option names (Kokoro) refuses an unknown one.
            (InstructionsMode::None, _) => {
                body.remove("instructions");
                if said {
                    report.changes.push(ShapeChange::InstructionsDropped);
                }
            }
            (_, InstructionsField::OptionsInstruct) => {
                let text = body.remove("instructions");
                let own = body
                    .get("options")
                    .and_then(|o| o.get("instruct"))
                    .is_some();
                if said {
                    let moved =
                        !own && set_option_value(body, "instruct", text.unwrap_or_default());
                    report.changes.push(if moved {
                        ShapeChange::InstructionsToOptions
                    } else {
                        ShapeChange::InstructionsDropped
                    });
                }
            }
            _ => {}
        }
    }
    let shaped = body
        .get("input")
        .and_then(Value::as_str)
        .and_then(|input| shape_tags(input, rules.tags, &rules.vocab));
    if let Some((input, n)) = shaped {
        body.insert("input".into(), Value::String(input));
        if n.mapped + n.stripped > 0 {
            report.changes.push(ShapeChange::Tags {
                mapped: n.mapped,
                stripped: n.stripped,
            });
        }
    }
}

/// A description the request sends under one synonym, written under the
/// other too where the row's defaults say something else there (R2).
///
/// audio.cpp makes the request's `instructions` the engine's
/// `options.instruction` (`build_speech_request`), then merges the row's
/// `default_request_options` under the request's options key by key — a
/// request key replaces only the same key (`apply_default_request_options`).
/// The engines that read both synonyms — Qwen3-TTS, MOSS-VoiceGen, MOSS-TTS
/// v1.5, MOSS-TTSD, dots — refuse a request whose `instruction` and
/// `instruct` are both set and differ (`find_option_match`: "conflicting
/// option values"). So a style or a description sent over a row's default
/// under the other key failed every request. Written under both, the two
/// agree and the request's replaces the row's, as a request's own option
/// does everywhere else. A key the request sets itself is left as it came.
fn over_row_description(row: &AudioModel, body: &mut Map<String, Value>, report: &mut ShapeReport) {
    let option = |key: &str| body.get("options").and_then(|o| o.get(key)).cloned();
    // What the engine gets from the request under each key, as audio.cpp
    // builds it: `instructions`, when present, is `instruction`.
    let instruction = body
        .get("instructions")
        .cloned()
        .or_else(|| option("instruction"));
    let instruct = option("instruct");
    let text = |v: &Option<Value>| {
        v.as_ref()
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    let sides = [
        (text(&instruction), instruct.is_some(), "instruct"),
        (text(&instruct), instruction.is_some(), "instruction"),
    ];
    for (sent, request_has_key, key) in sides {
        let Some(sent) = sent.filter(|_| !request_has_key) else {
            continue;
        };
        let row_says = row
            .default_request_options
            .get(key)
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty());
        if row_says.is_some_and(|d| d != sent) && set_option_value(body, key, Value::String(sent)) {
            report
                .changes
                .push(ShapeChange::InstructionsOverRow { key });
        }
    }
}

/// The request's `language` (and an `options.language`) in the family's
/// vocabulary ([`super::language`], audio-class gap 7): `de` → `german` for
/// Qwen3, `en` → `en-us` for Kokoro; anything that maps to nothing is sent
/// as it came.
fn shape_language(
    profile: &SpeechProfile,
    body: &mut Map<String, Value>,
    report: &mut ShapeReport,
) {
    let mut map = |v: &mut Value| {
        let Some(asked) = v.as_str() else {
            return;
        };
        if let Some(to) = map_language(asked, &profile.family, &profile.language_vocab) {
            report.changes.push(ShapeChange::Language {
                from: asked.to_string(),
                to: to.clone(),
            });
            *v = Value::String(to);
        }
    };
    if let Some(v) = body.get_mut("language") {
        map(v);
    }
    if let Some(v) = body
        .get_mut("options")
        .and_then(Value::as_object_mut)
        .and_then(|o| o.get_mut("language"))
    {
        map(v);
    }
}

/// Native voices by name (audio-class gap 1). audio.cpp's own precedence
/// for a request's `voice` decides first: a preset of the row, then a clip
/// of the voice library, then the name as a voice id. Only that last case is
/// lmgw's to shape:
/// - a native voice named in another case is sent in the package's
///   spelling (Magpie compares exactly; Qwen3 lowercases either way);
/// - for a family that reads `options.voice_id` instead of `voice`
///   (Magpie), the name moves there — `voice` alone is ignored by it;
/// - a preset (or, with no `voice` at all, the row's default preset) whose
///   `voice_id` names such a voice gets it copied to `options.voice_id`, for
///   the same reason.
///
/// A client's own `options.voice_id` always wins, and a default preset
/// never overrides the row's own `default_request_options.voice_id`.
fn shape_voice(
    profile: &SpeechProfile,
    voices: &RowVoices,
    row: &AudioModel,
    body: &mut Map<String, Value>,
    report: &mut ShapeReport,
) {
    let to_options = profile.voice_field == VoiceField::OptionsVoiceId;
    let client_option = body
        .get("options")
        .and_then(|o| o.get("voice_id"))
        .is_some();
    let named = body
        .get("voice")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(name) = named else {
        if body.contains_key("voice_ref")
            || !to_options
            || client_option
            || row.default_request_options.contains_key("voice_id")
        {
            return;
        }
        let id = match &row.default_voice_preset {
            // A preset's name — or, on a row without presets, a voice id
            // itself (`check_default_voice_preset`).
            Some(Value::String(preset)) => match row.voice_presets.get(preset) {
                Some(p) => p.get("voice_id").and_then(Value::as_str),
                None => Some(preset.as_str()),
            },
            Some(Value::Object(o)) => o.get("voice_id").and_then(Value::as_str),
            _ => None,
        };
        if let Some(c) = id.and_then(|id| present_native(voices, id)) {
            if set_option(body, c) {
                report
                    .changes
                    .push(ShapeChange::PresetToOptions { name: c.into() });
            }
        }
        return;
    };
    match voices.exact(&name).map(|e| e.kind) {
        Some(VoiceKind::Preset) => {
            if !to_options || client_option {
                return;
            }
            let id = row
                .voice_presets
                .get(&name)
                .and_then(|p| p.get("voice_id"))
                .and_then(Value::as_str);
            if let Some(c) = id.and_then(|id| present_native(voices, id)) {
                if set_option(body, c) {
                    report
                        .changes
                        .push(ShapeChange::PresetToOptions { name: c.into() });
                }
            }
        }
        Some(VoiceKind::Library | VoiceKind::Embedding) => {}
        Some(VoiceKind::Native) | None => {
            let Some(c) = present_native(voices, &name) else {
                return;
            };
            if to_options {
                if !client_option && set_option(body, c) {
                    body.remove("voice");
                    report
                        .changes
                        .push(ShapeChange::VoiceToOptions { name: c.into() });
                }
            } else if c != name {
                body.insert("voice".into(), Value::String(c.into()));
                report.changes.push(ShapeChange::VoiceSpelling {
                    from: name.clone(),
                    to: c.into(),
                });
            }
        }
    }
}

/// `name` as a native voice the row has (not one whose file is missing),
/// compared without case: its canonical spelling.
fn present_native<'a>(voices: &'a RowVoices, name: &str) -> Option<&'a str> {
    voices
        .entries
        .iter()
        .find(|e| e.kind == VoiceKind::Native && e.id.eq_ignore_ascii_case(name))
        .map(|e| e.id.as_str())
}

/// `options.voice_id = id`, creating `options`; `false` when `options` is
/// there and not an object (left as the client sent it).
fn set_option(body: &mut Map<String, Value>, id: &str) -> bool {
    set_option_value(body, "voice_id", Value::String(id.to_string()))
}

/// `options.<key> = value`, the same way.
fn set_option_value(body: &mut Map<String, Value>, key: &str, value: Value) -> bool {
    let options = body
        .entry("options")
        .or_insert_with(|| Value::Object(Map::new()));
    match options.as_object_mut() {
        Some(o) => {
            o.insert(key.into(), value);
            true
        }
        None => false,
    }
}

/// The codepoints of the header's `chars=` part, bounded where it says so.
mod listed;

#[cfg(test)]
mod tests;
