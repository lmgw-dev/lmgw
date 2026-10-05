//! Which voice the TTS alias speaks with (realtime design §5.3).
//!
//! `audio.output.voice` is resolved in order:
//! 1. a name the TTS model knows — its `GET /v1/audio/voices` list, the row's
//!    presets, a voice its package ships (an lmgw row's natives, matched
//!    without case), or a clip of the voice library, which audio.cpp
//!    resolves by name when the class's `voice_dir` is the library; for an
//!    OpenAI TTS (a cloud one), OpenAI's built-in names, sent lowercase as
//!    they are — never the owner's substitute for them (R3 D2);
//! 2. a name mapped by the setting `realtime.voice_map`;
//! 3. an OpenAI built-in name (`alloy` … `cedar`, and `marin` for a session
//!    that names none): the setting `realtime.default_voice`, then the TTS
//!    row's `default_voice_preset`, then — for a TTS that designs its voice
//!    from the speech instructions — no voice at all ([`designed`], R2) —
//!    and for a TTS whose engine speaks with a fixed voice of its own when
//!    none is named (Magpie, Kokoro, Supertonic), no voice at all either
//!    ([`engine_default`], R3). Both skip a `default_voice` they are not
//!    known to have (R3 N2). A TTS that draws a new speaker per request
//!    when none is named (OmniVoice) takes only a voice it knows, and with
//!    none is missing ([`draws_speaker`], R5 F1): each clause is a request,
//!    and its voice changed within an answer, seed or not. A TTS that
//!    clones from reference audio takes only one of its clips (R4 D5).
//!    These last two take a `default_voice` nothing rules out,
//!    provisionally, while lmgw cannot see the engine's voices — the
//!    class's `voice_dir` is not the library and no response of the
//!    session has read the list (R5, R6);
//! 4. `{id}`: a voice-library clip;
//! 5. anything else is `error {code: "voice_not_found"}` on the update that
//!    named it.
//!
//! **Never a call without a voice.** At least one engine (Pocket TTS, §3.1)
//! refuses to speak without one, so a chain that ends with nothing to speak
//! with is recorded as missing, and an audio response says so before it
//! starts (`voice_not_configured`). Three calls are sent without `voice`:
//! the row's own inline default preset — then the engine has one — a
//! voice-design TTS's, which designs the voice from the description and
//! reads no speaker (Qwen3-TTS VoiceDesign, MOSS-VoiceGen), and one whose
//! engine's source shows it speaks with a fixed voice of its own then
//! (`audio::families::unvoiced`).
//!
//! **Nothing is read from the model to resolve a voice** (WP3 review M1).
//! The handshake and every `session.update` resolve against what is known
//! without a request — the row's presets, the voice library, a list an
//! earlier response read — because asking the model meant starting it: an
//! admission on the session core, which froze the session for a cold start,
//! evicted others, or under the GPU hold refused a good name as
//! `voice_not_found`; and even a running container that does not answer
//! hung the handshake or the core. So a name only the model's list could
//! confirm — rule 1 with the list unread, or a `voice_map` or
//! `default_voice` target the known facts do not show — is accepted
//! **provisionally**: echoed, logged as unverified, and checked at the first
//! clause of the response that speaks with it, on that response's held
//! route (`responder::speech`), where an unknown one fails the response
//! with `voice_not_found` and the session stays open. The list read there is
//! remembered for the session's later updates.
//!
//! **Once the list is read, it decides** (package B review 5). A response's
//! first clause reads the model's list, and the session keeps it: the
//! session's voice is resolved again against it at once, so a name the list
//! does not show is no longer provisional:
//! - the session's own name is [`VoiceOutcome::NotFound`]: a later audio
//!   response is refused with `voice_not_found`, and a later
//!   `session.update` naming such a voice is refused the same way;
//! - a target the **owner's settings** named — `voice_map`,
//!   `default_voice`, the row's `default_voice_preset` — is
//!   [`VoiceOutcome::Missing`] (B2 review 4): the owner's misconfiguration
//!   is `voice_not_configured`, said by audio responses only, never a
//!   refusal of the client's `session.update` (a text session never trips
//!   over it).
//!
//! **Neither is for good** (B2 review 5): before each response the session
//! resolves its voice again when the settings changed, and while it is
//! `NotFound` or `Missing` it forgets the list it read, so the response's
//! first clause reads it again (`session::speech`). Rule 3's clip and
//! known-voice checks still go by that list ([`VoiceFacts::seen`], R6):
//! lmgw has seen the engine's voices, so a `default_voice` it lacks is
//! refused before the next response starts, not after its TTS was admitted.
//!
//! **A fallback speaks its own voice** ([`for_fallback`], §9.2): when the
//! GPU hold or admission answers with the TTS alias's fallback, the voice is
//! resolved once more, against the fallback.

use serde_json::Value;

use super::protocol::{ErrorObject, Voice};
use crate::audio::profile::Unvoiced;
use crate::config::RealtimeSettings;
use crate::error::GatewayError;
use crate::runtime::Class;
use crate::state::SharedState;

mod fallback;

pub(crate) use fallback::{for_fallback, Engine};

/// OpenAI's built-in voice names (§2.2).
pub const BUILTIN_VOICES: [&str; 10] = [
    "alloy", "ash", "ballad", "coral", "echo", "sage", "shimmer", "verse", "marin", "cedar",
];

/// What a TTS model is known to speak, gathered without a request to it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VoiceFacts {
    /// The model's own list, once a response has read it on its held route;
    /// `None` until then, and again while the session's voice is not found
    /// or missing, so the next response reads it afresh (module doc).
    pub listed: Option<Vec<String>>,
    /// The list a response of this session last read, kept when
    /// [`Self::listed`] is forgotten (R6): lmgw has seen the engine's
    /// voices, so the rules that need it to ([`Self::sees_voices`]) decide
    /// before a response starts. Dropped with another TTS alias, or when
    /// the owner changes what it was read under ([`list_key`]).
    pub seen: Option<Vec<String>>,
    /// The row's preset names (an lmgw audio row).
    pub presets: Vec<String>,
    /// Those of them that load a clip (`voice_ref`): with the voice
    /// library's clips and the names its list shows, the only voices an
    /// engine that clones from reference audio speaks
    /// ([`Unvoiced::NeedsReference`], R4 D5).
    pub clip_presets: Vec<String>,
    /// The row's `default_voice_preset`.
    pub default_preset: Option<Value>,
    /// The voice library's clip names, when the model answers to them: an
    /// lmgw audio row, with the class's `voice_dir` on the library — or
    /// none at all, with no `voice_dir`: then it answers to no clip (R6).
    /// `None` otherwise: a voice dir mounted for the engine alone, which
    /// lmgw cannot list, or no lmgw audio row.
    pub library: Option<Vec<String>>,
    /// The voices an lmgw audio row's package ships, in its own spelling
    /// ([`crate::audio::profile`]): Magpie's `Jason`, Supertonic's `F1`,
    /// Qwen3's `ryan`. Matched without case — lmgw sends the package's
    /// spelling (`crate::audio::shape`).
    pub native: Vec<String>,
    /// The TTS designs its voice from the speech instructions
    /// (`expressive::SpeechFacts::designs`): an OpenAI name nobody gave a
    /// voice to stand for is sent as no voice ([`designed`], R2).
    pub designs: bool,
    /// The TTS is an OpenAI one — the OpenAI protocol on a generic upstream,
    /// a cloud TTS ([`Engine::OpenAi`]): OpenAI's built-in names are its own
    /// voices, as they are for such a fallback (R3 D2).
    pub openai: bool,
    /// What an lmgw row's engine does with no voice named
    /// ([`crate::audio::profile::Unvoiced`], R3): one that speaks with a
    /// fixed voice of its own is sent none where nobody named one
    /// ([`engine_default`]); one that draws a speaker per request is never
    /// sent none ([`draws_speaker`], R5 F1); one that clones from reference
    /// audio is never sent an inline preset without a clip.
    pub unvoiced: Unvoiced,
    /// The voice-library clips an lmgw row's engine refuses to clone for
    /// want of a transcript (`crate::audio::transcript::untranscribed`):
    /// the Chat refuses to speak with one before anything starts
    /// (`voice_needs_transcript`). Empty for any other engine.
    pub untranscribed: Vec<String>,
}

impl VoiceFacts {
    /// Whether the facts show `name` as a voice of the model.
    pub fn knows(&self, name: &str) -> bool {
        let has = |v: &[String]| v.iter().any(|x| x == name);
        self.listed.as_deref().is_some_and(has)
            || has(&self.presets)
            || self.library.as_deref().is_some_and(has)
            || self.native.iter().any(|x| x.eq_ignore_ascii_case(name))
            || (self.openai && is_builtin(name))
    }

    /// The TTS speaks without being named a voice: it designs one from the
    /// speech instructions, or its engine has a fixed one of its own (R3) —
    /// not one it draws per request (R5 F1).
    fn needs_no_voice(&self) -> bool {
        self.designs || self.unvoiced.fixed_voice()
    }

    /// The model's list as a response of this session last read it:
    /// [`Self::listed`], or once that is forgotten for a fresh read,
    /// [`Self::seen`] (R6).
    fn last_read(&self) -> Option<&[String]> {
        self.listed.as_deref().or(self.seen.as_deref())
    }

    /// The list last read shows `name` ([`Self::last_read`]).
    fn read_shows(&self, name: &str) -> bool {
        self.last_read()
            .is_some_and(|l| l.iter().any(|x| x == name))
    }

    /// `name` is a clip this model clones from: a voice-library clip it
    /// answers to, a preset of its row that loads one (R4 D5), or a name
    /// its list last showed — a clip of a voice dir only the engine sees
    /// (R5, R6) — but never a preset of its row that loads none.
    fn clip(&self, name: &str) -> bool {
        let has = |v: &[String]| v.iter().any(|x| x == name);
        !self.clipless_preset(name)
            && (self.library.as_deref().is_some_and(has)
                || has(&self.clip_presets)
                || self.read_shows(name))
    }

    /// `name` is a preset of its row that loads no clip (no `voice_ref`):
    /// no voice for a TTS that clones from reference audio (R3 N4), whatever
    /// else shows the name.
    fn clipless_preset(&self, name: &str) -> bool {
        let has = |v: &[String]| v.iter().any(|x| x == name);
        has(&self.presets) && !has(&self.clip_presets)
    }

    /// lmgw sees every voice the engine takes by name: the class's
    /// `voice_dir` is the voice library or empty ([`Self::library`]), or a
    /// response of this session has read the model's own list — forgotten
    /// for a fresh read or not (R6). Otherwise the engine's clips lie in a
    /// voice dir mounted for it alone (`extra_run_args`, a supported setup),
    /// and any name may be one of them (R4 review, R5).
    fn sees_voices(&self) -> bool {
        self.library.is_some() || self.last_read().is_some()
    }

    /// The voice library as a place lmgw looked, for a message: none when
    /// the class's `voice_dir` is not the library; when it answers to no
    /// clip at all, saying so — the library dir may still hold some (R6).
    fn library_item(&self) -> Option<&'static str> {
        match self.library.as_deref()? {
            [] => Some(
                "a voice-library clip (it answers to none: the library has none, or the audio \
                 class's voice_dir is empty)",
            ),
            _ => Some("a voice-library clip"),
        }
    }

    /// Whether the owner's `default_voice` `name` is one to send this model
    /// (rule 3, module doc). A TTS that needs a voice named — one that
    /// clones from reference audio (R4 D5), or draws its speaker per
    /// request (R5 F1) — takes only one it has while lmgw sees its voices:
    /// a clip, or for the second any voice it is known to have or its list
    /// last showed. Any other name ends in a refusal after the response
    /// started. While lmgw cannot see them it is sent any name but a preset
    /// of its row that loads no clip, provisionally, as rule 3 sends any
    /// model's (R5: a clip in a voice dir mounted for the engine alone was
    /// refused before). One that designs its voice or speaks a fixed one of
    /// its own takes only a voice it is known to have (R3 N2); any other is
    /// sent it, verified or not.
    fn takes_default(&self, name: &str) -> bool {
        match self.unvoiced {
            Unvoiced::NeedsReference => {
                self.clip(name) || !(self.sees_voices() || self.clipless_preset(name))
            }
            Unvoiced::DrawsSpeaker => {
                self.knows(name) || self.read_shows(name) || !self.sees_voices()
            }
            _ => self.knows(name) || !self.needs_no_voice(),
        }
    }

    /// Why `name` is no clip of this model, by what lmgw looked at: said
    /// when a TTS that clones from reference audio passes over the owner's
    /// `default_voice` (R4 D5) — the voice library only when it is the
    /// class's `voice_dir`, the model's list only once read (R5, R6).
    fn no_clip(&self, name: &str) -> String {
        if self.clipless_preset(name) {
            return "it is a preset of its row that loads none (no voice_ref)".into();
        }
        let mut not: Vec<&str> = self.library_item().into_iter().collect();
        if self.last_read().is_some() {
            not.push("in its voice list");
        }
        not.push("a preset of its row with a voice_ref");
        format!("not {}", not.join(", nor "))
    }

    /// `name` in the spelling the model takes: an OpenAI built-in name
    /// lowercased for an OpenAI TTS, which knows no other; anything else as
    /// it came (an lmgw row's natives are spelled by shaping).
    fn spelling(&self, name: &str) -> String {
        if self.openai && is_builtin(name) {
            name.to_ascii_lowercase()
        } else {
            name.to_string()
        }
    }
}

/// One of OpenAI's built-in voice names, in any case.
fn is_builtin(name: &str) -> bool {
    BUILTIN_VOICES.contains(&name.to_ascii_lowercase().as_str())
}

/// A voice to speak with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakVoice {
    /// The `voice` the engine is sent; `None` only for the row's inline
    /// default preset, which speaks when the field is left out.
    pub send: Option<String>,
    /// The voice's name — `lmgw.resolved.voice`.
    pub name: String,
    pub via: VoiceVia,
    /// The facts show the model has it; `false`: accepted provisionally,
    /// checked at the first clause (module doc).
    pub verified: bool,
}

/// Which rule of the chain (module doc) answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceVia {
    Model,
    VoiceMap,
    DefaultVoice,
    DefaultPreset,
    Library,
    /// A voice-design TTS's own voice, designed from the description.
    Designed,
    /// The voice the TTS's engine speaks with when none is named (R3).
    EngineDefault,
}

impl VoiceVia {
    /// The knob that named the voice, for a log line or an error.
    pub fn knob(self) -> &'static str {
        match self {
            Self::Model => "audio.output.voice",
            Self::VoiceMap => "the setting realtime.voice_map",
            Self::DefaultVoice => "the setting realtime.default_voice",
            Self::DefaultPreset => "the TTS row's default_voice_preset",
            Self::Library => "audio.output.voice {id}",
            Self::Designed => "the voice-design TTS, which designs it from the speech instructions",
            Self::EngineDefault => "the TTS model's own voice: its engine speaks without one named",
        }
    }
}

/// What a session's voice resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceOutcome {
    Resolved(SpeakVoice),
    /// Nothing to speak with, and why — said by an audio response
    /// (`voice_not_configured`).
    Missing(String),
    /// The model's list, read by a response, shows it does not have the
    /// voice, and why — said by an audio response (`voice_not_found`;
    /// module doc).
    NotFound(String),
}

impl VoiceOutcome {
    pub fn name(&self) -> Option<String> {
        match self {
            Self::Resolved(v) => Some(v.name.clone()),
            Self::Missing(_) | Self::NotFound(_) => None,
        }
    }
}

/// One pass of the chain over what is known.
#[derive(Debug, PartialEq)]
enum Decision {
    Outcome(VoiceOutcome),
    NotFound(String),
}

/// Gather `alias`'s [`VoiceFacts`] without a request to the model (module
/// doc): its row's presets, the voices its package ships and the voice
/// library, read on the blocking pool — a directory listing must not hold an
/// async worker (review m9). Read afresh whenever a session's TTS alias or
/// voice changes, so a clip or a preset added meanwhile is seen. `label`:
/// what the log line starts with (`realtime <session id>`, `chat thread
/// <id>`) — `None` at a session's handshake, before it has an id.
pub async fn facts(state: &SharedState, alias: &str, label: Option<&str>) -> VoiceFacts {
    let snap = state.snapshot();
    let (mut facts, row) = row_facts(&snap, alias);
    if !row {
        facts.openai = snap
            .resolve(alias)
            .is_ok_and(|r| Engine::of(r.upstream.protocol, r.upstream.kind) == Engine::OpenAi);
        return facts;
    }
    if let Some(m) = audio_row(&snap, alias) {
        let speech = crate::audio::voices::row_speech(state, m).await;
        facts.unvoiced = speech.profile.unvoiced;
        facts.untranscribed =
            crate::audio::transcript::untranscribed(m, &speech.profile, &speech.voices);
        facts.native = speech
            .voices
            .entries
            .iter()
            .filter(|e| e.kind == crate::audio::voices::VoiceKind::Native)
            .map(|e| e.id.clone())
            .collect();
    }
    let st = state.clone();
    facts.library = tokio::task::spawn_blocking(move || {
        // No voice_dir: lmgw knows the engine answers to no clip at all —
        // its own list is the model's whole one (`Synthesis::voice_names`),
        // so rule 3 decides before a response starts (R6).
        crate::web::library_voices(&st).or_else(|| {
            crate::web::voice_dir_is_library(&st.snapshot().settings.audio).then(Vec::new)
        })
    })
    .await
    .unwrap_or_else(|e| {
        match label {
            Some(label) => tracing::warn!("{label}: reading the voice library failed: {e}"),
            None => tracing::warn!(
                "realtime: reading the voice library failed at a session's handshake, before \
                 the session had an id: {e}"
            ),
        }
        None
    });
    facts
}

/// What the owner configured that decides the list `alias`'s model shows
/// and the `default_voice` rule 3 checks against it — `default_voice`, the
/// audio class's `voice_dir`, an lmgw row's container configuration
/// (`vram::residency::resident_key`: the model, its image and run args) —
/// as one comparable string. A session keeps the list a response read
/// ([`VoiceFacts::seen`]) while this stays the same (R6): an owner who adds
/// a clip to a mounted dir and names it is heard at once, while a snapshot
/// published for anything else — a learned residency or peak, a
/// benchmark's lease — is no reason to start the TTS just to read it again.
pub(crate) fn list_key(snap: &crate::config::Snapshot, alias: &str) -> String {
    let audio = &snap.settings.audio;
    let row = audio_row(snap, alias).map(|m| crate::vram::residency::resident_key(m, audio));
    serde_json::json!([snap.settings.realtime.default_voice, audio.voice_dir, row]).to_string()
}

/// What `alias`'s row in `snap` says — its presets and default preset —
/// without the voice library (a directory listing, which [`facts`] reads off
/// the async workers); `true` when it is an lmgw audio row. Cheap: what
/// the session reads again when the settings change (B2 review 5).
pub fn row_facts(snap: &crate::config::Snapshot, alias: &str) -> (VoiceFacts, bool) {
    let Some(m) = audio_row(snap, alias) else {
        return (VoiceFacts::default(), false);
    };
    let facts = VoiceFacts {
        presets: m.voice_presets.keys().cloned().collect(),
        clip_presets: m
            .voice_presets
            .iter()
            .filter(|(_, p)| p.get("voice_ref").is_some())
            .map(|(name, _)| name.clone())
            .collect(),
        default_preset: m.default_voice_preset.clone(),
        ..VoiceFacts::default()
    };
    (facts, true)
}

/// The lmgw audio row `alias` names, if it names one.
fn audio_row<'a>(
    snap: &'a crate::config::Snapshot,
    alias: &str,
) -> Option<&'a crate::config::AudioModel> {
    let route = snap.resolve(alias).ok()?;
    let t = crate::vram::classify(&route).filter(|t| t.class == Class::Audio)?;
    snap.audio_models.iter().find(|m| m.model_id == t.model_id)
}

/// Resolve `requested` for the TTS alias `alias` against `facts` (module
/// doc) — without a request, so a name only the model's list could confirm
/// comes back unverified. `Err`: the name is not a voice this model can
/// speak (`voice_not_found`).
pub fn resolve(
    alias: Option<&str>,
    requested: &Voice,
    facts: &VoiceFacts,
    settings: &RealtimeSettings,
) -> Result<VoiceOutcome, ErrorObject> {
    let Some(alias) = alias else {
        return Ok(VoiceOutcome::Missing("no TTS alias to speak with".into()));
    };
    match decide(alias, requested, facts, settings) {
        Decision::Outcome(o) => Ok(o),
        Decision::NotFound(why) => {
            Err(ErrorObject::invalid("voice_not_found", why)
                .with_param("session.audio.output.voice"))
        }
    }
}

fn decide(
    alias: &str,
    requested: &Voice,
    facts: &VoiceFacts,
    settings: &RealtimeSettings,
) -> Decision {
    let speak = |send: &str, via| named(alias, send, via, facts);
    match requested {
        Voice::Name(n) => {
            if facts.knows(n) {
                return speak(&facts.spelling(n), VoiceVia::Model);
            }
            if let Some(m) = settings.voice_map.get(n).filter(|m| !m.trim().is_empty()) {
                return speak(&facts.spelling(m.trim()), VoiceVia::VoiceMap);
            }
            if is_builtin(n) {
                return default_voice(alias, n, facts, settings);
            }
            if facts.listed.is_none() {
                // Only the model's list could say: provisional (module doc).
                return speak(n, VoiceVia::Model);
            }
            Decision::NotFound(unknown(alias, requested))
        }
        Voice::Id { id } => {
            if facts.library.as_deref().is_some_and(|l| l.contains(id)) {
                return speak(id, VoiceVia::Library);
            }
            if facts.knows(id) {
                return speak(id, VoiceVia::Model);
            }
            Decision::NotFound(match facts.library.as_deref() {
                Some([]) => format!(
                    "voice {{id: '{id}'}}: TTS model '{alias}' answers to no voice-library clip \
                     — the library has none, or the audio class's voice_dir is empty"
                ),
                Some(_) => format!("voice {{id: '{id}'}}: the voice library has no clip '{id}'"),
                None => format!(
                    "voice {{id: '{id}'}} names a voice-library clip, and TTS model '{alias}' \
                     does not answer to the library's clips (it is not an lmgw audio model, or \
                     the audio class's voice_dir is not the library)"
                ),
            })
        }
    }
}

/// A voice named `send` by `via`: verified when the facts show it,
/// provisional while the model's list is unread, and once the list is read
/// and nothing shows it, not found — or, for a name the owner's settings
/// gave, missing (module doc).
fn named(alias: &str, send: &str, via: VoiceVia, facts: &VoiceFacts) -> Decision {
    let verified = facts.knows(send);
    if !verified && facts.listed.is_some() {
        let why = lacks(alias, send, via);
        return match via {
            VoiceVia::Model | VoiceVia::Library => Decision::NotFound(why),
            VoiceVia::VoiceMap
            | VoiceVia::DefaultVoice
            | VoiceVia::DefaultPreset
            | VoiceVia::Designed
            | VoiceVia::EngineDefault => Decision::Outcome(VoiceOutcome::Missing(why)),
        };
    }
    Decision::Outcome(VoiceOutcome::Resolved(SpeakVoice {
        send: Some(send.to_string()),
        name: send.to_string(),
        via,
        verified,
    }))
}

/// Why the model lacks `send`, which `via` named — for a name the owner's
/// settings gave, saying which one to fix.
pub(crate) fn lacks(alias: &str, send: &str, via: VoiceVia) -> String {
    let fix = match via {
        VoiceVia::Model | VoiceVia::Library => String::new(),
        _ => format!(" — fix {}", via.knob()),
    };
    format!(
        "voice '{send}' (named by {}) is not a voice of TTS model '{alias}': not in its voice \
         list, presets or voice library (GET /v1/audio/voices?model={alias}){fix}",
        via.knob()
    )
}

/// The row's inline default preset: sent as no `voice` at all, so the row's
/// own preset speaks — named by its voice id, or the clip it clones.
fn inline_preset(o: &serde_json::Map<String, Value>) -> SpeakVoice {
    let name = o
        .get("voice_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            o.get("voice_ref").and_then(Value::as_str).map(|r| {
                let file = r.rsplit('/').next().unwrap_or(r);
                file.rsplit_once('.').map_or(file, |(n, _)| n).to_string()
            })
        })
        .unwrap_or_else(|| "default_voice_preset".into());
    SpeakVoice {
        send: None,
        name,
        via: VoiceVia::DefaultPreset,
        verified: true,
    }
}

/// The voice of a TTS that designs it from the speech instructions (R2):
/// sent as no voice — Qwen3-TTS VoiceDesign and MOSS-VoiceGen read no
/// speaker — and named `designed` in `lmgw.resolved.voice`.
fn designed() -> SpeakVoice {
    SpeakVoice {
        send: None,
        name: DESIGNED.into(),
        via: VoiceVia::Designed,
        verified: true,
    }
}

/// `lmgw.resolved.voice` of a designed voice ([`designed`]).
pub const DESIGNED: &str = "designed";

/// The voice of a TTS whose engine speaks with a fixed one of its own when
/// none is named (R3, [`Unvoiced::fixed_voice`]): sent as no voice —
/// Magpie takes its first, Kokoro `af_heart`, Supertonic `M1` — and named
/// `engine_default` in `lmgw.resolved.voice`. Not OmniVoice's: it draws a
/// new speaker per request ([`draws_speaker`], R5 F1).
fn engine_default() -> SpeakVoice {
    SpeakVoice {
        send: None,
        name: ENGINE_DEFAULT.into(),
        via: VoiceVia::EngineDefault,
        verified: true,
    }
}

/// `lmgw.resolved.voice` of an engine's own voice ([`engine_default`]).
pub const ENGINE_DEFAULT: &str = "engine_default";

/// Why a TTS that draws its speaker per request (OmniVoice,
/// [`Unvoiced::DrawsSpeaker`]) has no voice: nothing configured names one
/// it knows (R5 F1). On `/v1/audio/speech` one request is one speaker; a
/// session sends one request per clause, and the clause's text picks the
/// speaker even under the session's seed — live run 3c heard 222, 118 and
/// 222 Hz within one answer. A voice that changes mid-answer is worse than
/// a refusal, so the session says so before `response.created`
/// (`voice_not_configured`).
fn draws_speaker(alias: &str, configured: &str, facts: &VoiceFacts) -> String {
    let skipped = if configured.is_empty() {
        String::new()
    } else {
        // What lmgw looked at: the library only when it is the class's
        // voice_dir, the list only once read (R5, R6).
        let mut not = vec!["a preset of its row"];
        not.extend(facts.library_item());
        if facts.last_read().is_some() {
            not.push("in its voice list");
        }
        format!(
            "; the setting realtime.default_voice ('{configured}') is not a voice it is known to \
             have — not {}",
            not.join(", nor ")
        )
    };
    format!(
        "TTS model '{alias}' draws a new speaker for every request when it is named no voice, \
         and a session sends one request per clause: its voice would change within an \
         answer{skipped} — configure a voice clip or preset for it: realtime.default_voice \
         naming one of its clips or a preset of its row, the row's default_voice_preset, or a \
         voice the model knows sent as the voice (GET /v1/audio/voices?model={alias})"
    )
}

/// Rule 3: an OpenAI name the model does not have — the owner's
/// substitute for it, else a designed voice for a TTS that designs one.
///
/// A TTS that designs its voice passes over a `default_voice` it is not
/// known to have (live run 3, N2): the owner's setting names a voice of
/// another model — Pocket's `alba` — and a session that switched to a
/// voice-design TTS was refused `voice_not_configured` for it. One it
/// knows (a preset, a library clip, its list) is still sent. So does a TTS
/// whose engine speaks with a fixed voice of its own, and with nothing else
/// configured it speaks with that one ([`engine_default`], R3). One that
/// draws its speaker per request (OmniVoice) passes it over too, but with
/// nothing else configured its voice is missing ([`draws_speaker`], R5 F1).
///
/// A TTS that clones from reference audio (CosyVoice3) takes the owner's
/// `default_voice` only when it is one of its clips — a voice-library clip,
/// a preset with a `voice_ref`, or a name its list shows (R4 D5). Pocket's
/// `alba` was sent to it provisionally: the response was created, the
/// container started, and only then was it refused `voice_not_configured`.
/// Skipped, the chain goes on to the row's default preset, and with none it
/// is missing before anything starts. Both rules hold only while lmgw sees
/// the engine's voices ([`VoiceFacts::sees_voices`]): with the class's
/// `voice_dir` elsewhere and the list unread, the default is sent
/// provisionally, as to any model — the R4 review found `anna.wav` in such a
/// dir refused as "no clip" (R5). Once a response read the list, the
/// session goes by it even after forgetting it for a fresh read: R5's review
/// found every later response sending the default provisionally again,
/// admitted and then refused (R6).
fn default_voice(
    alias: &str,
    name: &str,
    facts: &VoiceFacts,
    settings: &RealtimeSettings,
) -> Decision {
    let configured = settings.default_voice.trim();
    if !configured.is_empty() && facts.takes_default(configured) {
        // Checked like any name: unverified until the facts show it (m10).
        return named(alias, configured, VoiceVia::DefaultVoice, facts);
    }
    match &facts.default_preset {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            named(alias, s.trim(), VoiceVia::DefaultPreset, facts)
        }
        // An engine that clones from reference audio refuses an inline
        // preset without a clip (R3 N4): said before the response starts.
        Some(Value::Object(o))
            if facts.unvoiced == Unvoiced::NeedsReference && !o.contains_key("voice_ref") =>
        {
            Decision::Outcome(VoiceOutcome::Missing(format!(
                "TTS model '{alias}' clones its voice from reference audio, and the row's inline \
                 default_voice_preset loads none (no voice_ref) — give it a clip, or send a \
                 voice-library clip as the voice"
            )))
        }
        Some(Value::Object(o)) => Decision::Outcome(VoiceOutcome::Resolved(inline_preset(o))),
        _ if facts.designs => Decision::Outcome(VoiceOutcome::Resolved(designed())),
        _ if facts.needs_no_voice() => Decision::Outcome(VoiceOutcome::Resolved(engine_default())),
        _ if facts.unvoiced == Unvoiced::DrawsSpeaker => Decision::Outcome(VoiceOutcome::Missing(
            draws_speaker(alias, configured, facts),
        )),
        _ if facts.unvoiced == Unvoiced::NeedsReference && !configured.is_empty() => {
            Decision::Outcome(VoiceOutcome::Missing(format!(
                "TTS model '{alias}' clones its voice from reference audio, and the setting \
                 realtime.default_voice ('{configured}') is not one of its clips — {} — set it \
                 to one, give the row a default_voice_preset with a voice_ref, or send one of its \
                 clips as the voice",
                facts.no_clip(configured)
            )))
        }
        _ => Decision::Outcome(VoiceOutcome::Missing(format!(
            "'{name}' is an OpenAI voice name that TTS model '{alias}' does not have, and \
             neither the setting realtime.default_voice nor the TTS row's default_voice_preset \
             names a voice to speak with instead — set one, or send a voice the model knows \
             (GET /v1/audio/voices?model={alias})"
        ))),
    }
}

fn unknown(alias: &str, requested: &Voice) -> String {
    format!(
        "voice '{}' is not a voice of TTS model '{alias}': not in its voice list, presets or \
         voice library, not mapped by realtime.voice_map, and not an OpenAI voice name",
        echo(requested)
    )
}

/// The requested voice as a string — what `response.audio.output.voice`
/// echoes: `@openai/agents`' schema wants a string there, `{id}` included.
pub fn echo(requested: &Voice) -> String {
    match requested {
        Voice::Name(n) => n.clone(),
        Voice::Id { id } => id.clone(),
    }
}

/// The log line (§5.1: every substitution is logged).
pub fn log_resolution(session_id: &str, requested: &Voice, o: &VoiceOutcome) {
    let asked = echo(requested);
    let v = match o {
        VoiceOutcome::Missing(why) | VoiceOutcome::NotFound(why) => {
            return tracing::info!("realtime {session_id}: no voice for '{asked}' — {why}")
        }
        VoiceOutcome::Resolved(v) => v,
    };
    let unverified = if v.verified {
        ""
    } else {
        " — not verified yet: the TTS model's voice list is read at the first spoken clause"
    };
    if v.via == VoiceVia::Model || v.via == VoiceVia::Library {
        tracing::info!("realtime {session_id}: voice '{}'{unverified}", v.name)
    } else {
        tracing::info!(
            "realtime {session_id}: voice '{asked}' → '{}' ({}){unverified}",
            v.name,
            v.via.knob()
        )
    }
}

/// `voice_not_found` from the first clause: the model does not have the
/// voice the session settled on (module doc).
pub fn not_found(message: String) -> GatewayError {
    GatewayError::Refused {
        status: 400,
        code: "voice_not_found",
        message,
    }
}

/// `voice_not_configured` from the first clause: nothing the TTS alias's
/// fallback can speak with (§9.2).
pub fn not_configured(message: String) -> GatewayError {
    GatewayError::Refused {
        status: 400,
        code: "voice_not_configured",
        message,
    }
}

/// A voice error of [`not_found`] or [`not_configured`] in the session's
/// shape: a request error about the voice `param` named — the session's
/// `session.audio.output.voice`, or a `response.create`'s own
/// `response.audio.output.voice` (package B review 6) — not a permission one.
/// An empty `param` names none: a bound session's voice is its thread's.
pub fn voice_error(e: &GatewayError, param: &str) -> Option<ErrorObject> {
    match e {
        GatewayError::Refused {
            code: code @ ("voice_not_found" | "voice_not_configured"),
            message,
            ..
        } => {
            let error = ErrorObject::invalid(code, message.clone());
            Some(if param.is_empty() {
                error
            } else {
                error.with_param(param)
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
