//! What a thread's voice resolves to (chat-voice design §2.3): each field in
//! the order thread → `chat_*` setting → `realtime.*` setting → none, and the
//! facts the page shows beside each choice — where it came from, whether it
//! is served on this machine, whether it runs on the CPU, and what the GPU
//! hold (or, for a CPU row too, a benchmark run's lease) would answer with
//! instead.
//!
//! Every Chat voice route and the bound realtime session use this one
//! function, so the thread JSON's `voice_resolved` is what they do. It reads
//! the snapshot only: nothing is started, probed or fetched, and the GPU
//! hold's *state* is the page's to read (the titlebar's `vram` frame) — this
//! says what the hold *would* do.
//!
//! The ASR chain runs the other way round from realtime's on purpose (§2.1):
//! the Chat prefers its own `chat_stt_alias` and falls back to
//! `realtime.asr_alias`; realtime prefers its own and falls back to the
//! Chat's (`realtime/asr.rs`). Either one configured serves both.
//!
//! **A voice belongs to one TTS model** (WP1 review M1), so the voice is not
//! a chain of its own: [`voice`] takes a name only from a level at least as
//! specific as the one that chose the TTS model, or from one whose model is
//! the same alias. `realtime.default_voice` is never turned into a name: it
//! is reported as what realtime's chain starts from (`inherits`), so callers
//! send no voice and realtime's own rule 3 decides, with its known-voice
//! checks (`realtime/voice.rs`).

use serde::Serialize;

use crate::config::{SelfAdmin, Settings, Snapshot};
use crate::runtime::{Class, Placement};
use crate::store::{ChatThread, ThreadVoice, TurnDetection};

#[cfg(test)]
mod tests;

/// The two languages — the one the user speaks and the one replies are in
/// — and a turn's of them.
mod languages;
pub(crate) use languages::{language, reply_language, turn_language};

/// Which level of the chain a value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    /// The thread's own override.
    Thread,
    /// Settings → Chat → Voice.
    Chat,
    /// Settings → Realtime.
    Realtime,
    /// The TTS row's own description of its voice — a speech style only:
    /// what a voice-design row designs from where the owner-wide styles
    /// stand back for it (chat-voice design §6.1; `speech::resolve_shown`).
    Row,
    /// The reply language only: none is set for it, so it is the language
    /// the user speaks, as resolved (chat-voice design §2.1, 2026-10-05).
    SpeechIn,
}

/// A thread's voice, resolved — `voice_resolved` in the thread JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct VoiceConfig {
    pub asr: Stage,
    pub tts: Stage,
    pub voice: VoiceName,
    pub speech_style: SpeechStyle,
    /// The language the user speaks, ISO 639-1: what the ASR is told where
    /// it takes a language, and what the prompt says the user speaks
    /// (chat-voice design §2.1, split 2026-10-05); `value: None` = none —
    /// the ASR detects.
    pub language: Sourced<Option<String>>,
    /// The language replies are in, ISO 639-1: what the model is asked to
    /// answer in and the TTS speaks (§2.1, added 2026-10-05) — the
    /// thread's, Settings', else `language` (`source: speech_in`);
    /// `value: None` = none, the reply follows the user, as before.
    pub reply_language: Sourced<Option<String>>,
    /// Where a stage's model does not take its language as set — the ASR
    /// the spoken one, the TTS the reply's (`super::language::notes`):
    /// filled by `speech::resolve_shown`, which reads the rows' speech
    /// facts; empty here.
    pub language_notes: Vec<super::language::LanguageNote>,
    pub read_aloud: Sourced<bool>,
    pub turn_detection: Sourced<TurnDetection>,
    /// Whether a voice turn goes to the chat model as audio, and why not
    /// (voice-audio-input design §2.3, `super::audio_input`): filled by
    /// `speech::resolve_shown`, since the verdict resolves the chat model's
    /// route and looks up its capabilities; `None` here.
    pub audio_input: Option<super::audio_input::Shown>,
    /// The thread's TTS seed; `None` until its first use draws one.
    pub seed: Option<u32>,
    /// What blocks a voice feature now: a stage with no alias, or one whose
    /// alias no longer resolves.
    pub problems: Vec<Problem>,
    pub realtime: RealtimeAvailability,
}

/// A value and the level it came from; `source: None` when no level sets
/// it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Sourced<T> {
    pub value: T,
    pub source: Option<Source>,
}

/// One speech stage (ASR or TTS).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Stage {
    /// `None`: nothing configured at any level.
    pub alias: Option<String>,
    pub source: Option<Source>,
    /// What the levels below the thread give — the alias the thread speaks
    /// or transcribes with once its own override is cleared. The page's
    /// choice for an emptied box; equal to `alias` when the thread sets
    /// none.
    pub inherited: Option<String>,
    /// `false` for an alias served off this machine (a provider, or a server
    /// elsewhere); `None` when the alias does not resolve.
    pub local: Option<bool>,
    /// A model lmgw starts itself (a container of its own): the GPU hold and
    /// a benchmark run's lease apply to it. `false` for an alias off this
    /// machine and for a local server lmgw does not run.
    pub managed: bool,
    /// An audio row that runs on the CPU: no VRAM, and the GPU hold does not
    /// stop it — a benchmark run's lease does (`Snapshot::gpu_block_for`).
    pub cpu: bool,
    /// What answers instead while the row is blocked: the row's usable hold
    /// fallback. A GPU row is blocked by the GPU hold and by a benchmark
    /// run's lease, a CPU row by the lease only (`cpu` tells the page which);
    /// the swap is `Snapshot::resolve_for_request`'s. `None` for an alias off
    /// this machine, or a row with none (a block then refuses).
    pub fallback: Option<Fallback>,
    /// A hold fallback the row names but that cannot stand in, and why (a
    /// block then refuses, naming it).
    pub fallback_unusable: Option<FallbackUnusable>,
}

/// A hold fallback, and whether it is local (a usable one never is).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Fallback {
    pub alias: String,
    pub local: bool,
}

/// A named hold fallback that cannot answer: `why` is the clause the hold's
/// refusal names it with ("does not resolve", "is itself a local model", …).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct FallbackUnusable {
    pub alias: String,
    pub why: &'static str,
}

/// The voice (module doc).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct VoiceName {
    /// The voice to ask for: the thread's own, or Settings → Chat's when it
    /// was chosen for the model the thread speaks with. `None`: no voice is
    /// named, and realtime's chain decides (realtime §5.3).
    pub name: Option<String>,
    /// Where `name` came from. With no name, `realtime` when
    /// `realtime.default_voice` is what realtime's chain starts from, else
    /// `None`.
    pub source: Option<Source>,
    /// `realtime.default_voice`, when no name is named: realtime's chain
    /// applies it, checked against the model when it speaks. Never sent as
    /// the voice.
    pub inherits: Option<String>,
    /// Why a voice set at a less specific level is not used here, in the
    /// page's words.
    pub note: Option<String>,
}

/// The speech instructions; `""` is none. [`resolve`] gives the levels'
/// choice; the thread JSON shows the one the speech uses
/// (`speech::resolve_shown`), which may be the row's own (`source: row`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SpeechStyle {
    pub text: String,
    pub source: Source,
}

/// Something that blocks a voice feature, by stage.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Problem {
    /// `asr` or `tts`.
    pub stage: &'static str,
    /// `not_configured` or `unresolved`; `voice_needs_transcript` for a
    /// voice clip the TTS cannot clone without a transcript (the thread
    /// JSON's only, `speech::resolve_shown`).
    pub code: &'static str,
    pub message: String,
}

/// Whether realtime mode may bind this thread (§8.1), and the self-admin
/// chip's fact (§9.4).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct RealtimeAvailability {
    pub ok: bool,
    /// The refusal's machine code — the bind's own 409 code
    /// (`chat_thread_admin`) — so the page keys on it, not on the prose.
    pub code: Option<&'static str>,
    pub reason: Option<String>,
    /// The thread carries the self-admin toolset while `self_admin` is
    /// `full`: its tools can change lmgw. Shown, never a refusal.
    pub admin_tools: bool,
}

/// The first set value of a chain: `(value, source)`. Empty text counts as
/// unset at every level.
fn first<const N: usize>(levels: [(Option<&str>, Source); N]) -> Option<(&str, Source)> {
    levels
        .into_iter()
        .find_map(|(v, src)| v.map(str::trim).filter(|v| !v.is_empty()).map(|v| (v, src)))
}

/// The ASR alias the Chat transcribes with for `thread`: its override, then
/// `chat_stt_alias`, then `realtime.asr_alias`. Dictation, the bound session
/// and audio attachments all go by it (§2.1).
pub(crate) fn asr_alias(snap: &Snapshot, thread: &ChatThread) -> Option<String> {
    asr_chain(snap, thread).map(|(a, _)| a.to_string())
}

fn asr_chain<'a>(snap: &'a Snapshot, thread: &'a ChatThread) -> Option<(&'a str, Source)> {
    first([
        (thread.voice.asr_alias.as_deref(), Source::Thread),
        (Some(snap.settings.chat_stt_alias.as_str()), Source::Chat),
        (
            Some(snap.settings.realtime.asr_alias.as_str()),
            Source::Realtime,
        ),
    ])
}

/// The Chat's own ASR alias below the thread: `chat_stt_alias`, then
/// `realtime.asr_alias`.
fn chat_asr(s: &Settings) -> Option<(&str, Source)> {
    first([
        (Some(s.chat_stt_alias.as_str()), Source::Chat),
        (Some(s.realtime.asr_alias.as_str()), Source::Realtime),
    ])
}

/// The Chat's own TTS alias below the thread: `chat_tts_alias`, then
/// `realtime.tts_alias` — the model `chat_voice` is chosen for.
fn chat_tts(s: &Settings) -> Option<(&str, Source)> {
    first([
        (Some(s.chat_tts_alias.as_str()), Source::Chat),
        (Some(s.realtime.tts_alias.as_str()), Source::Realtime),
    ])
}

/// The owner-wide speech style below the thread: Settings → Chat's, else
/// Settings → Realtime's (`""` there is none).
pub(crate) fn owner_style(s: &Settings) -> SpeechStyle {
    if s.chat_speech_style.trim().is_empty() {
        SpeechStyle {
            text: s.realtime.speech_instructions.trim().to_string(),
            source: Source::Realtime,
        }
    } else {
        SpeechStyle {
            text: s.chat_speech_style.trim().to_string(),
            source: Source::Chat,
        }
    }
}

/// Resolve `thread`'s voice against the snapshot (module doc).
pub(crate) fn resolve(snap: &Snapshot, thread: &ChatThread) -> VoiceConfig {
    let s = &snap.settings;
    let v = &thread.voice;
    let mut problems = Vec::new();
    let asr = stage(
        snap,
        "asr",
        asr_chain(snap, thread),
        chat_asr(s),
        &mut problems,
    );
    let tts_chosen = first([(v.tts_alias.as_deref(), Source::Thread)]).or_else(|| chat_tts(s));
    let tts = stage(snap, "tts", tts_chosen, chat_tts(s), &mut problems);
    let voice = voice(s, v, tts_chosen);
    // A thread's own `""` is "none for this thread", so it is a value here,
    // unlike an empty setting, which falls through.
    let speech_style = match v.speech_style.as_deref() {
        Some(t) => SpeechStyle {
            text: t.trim().to_string(),
            source: Source::Thread,
        },
        None => owner_style(s),
    };
    let language = language(s, v);
    let reply_language = reply_language(s, v, &language);
    let read_aloud = match v.read_aloud {
        Some(on) => Sourced {
            value: on,
            source: Some(Source::Thread),
        },
        None => Sourced {
            value: s.chat_read_aloud,
            source: Some(Source::Chat),
        },
    };
    let turn_detection = match v.turn_detection {
        Some(t) => Sourced {
            value: t,
            source: Some(Source::Thread),
        },
        None => Sourced {
            value: TurnDetection::parse(&s.chat_turn_detection).unwrap_or_default(),
            source: Some(Source::Chat),
        },
    };
    VoiceConfig {
        asr,
        tts,
        voice,
        speech_style,
        language,
        reply_language,
        language_notes: Vec::new(),
        read_aloud,
        turn_detection,
        audio_input: None,
        seed: v.seed,
        problems,
        realtime: realtime_availability(snap, thread),
    }
}

/// The voice for a thread whose TTS alias came from `tts` (module doc): the
/// thread's own always; Settings → Chat's when the TTS is the Chat's own
/// (no thread override, or one naming the same alias); otherwise none, and
/// `realtime.default_voice` is reported as what realtime's chain starts
/// from.
fn voice(s: &Settings, v: &ThreadVoice, tts: Option<(&str, Source)>) -> VoiceName {
    if let Some((n, src)) = first([(v.voice.as_deref(), Source::Thread)]) {
        return VoiceName {
            name: Some(n.to_string()),
            source: Some(src),
            inherits: None,
            note: None,
        };
    }
    let mut note = None;
    if let Some((n, src)) = first([(Some(s.chat_voice.as_str()), Source::Chat)]) {
        let chat_model = chat_tts(s).map(|(a, _)| a);
        let same_model = match tts {
            Some((alias, Source::Thread)) => chat_model == Some(alias),
            _ => true,
        };
        if same_model {
            return VoiceName {
                name: Some(n.to_string()),
                source: Some(src),
                inherits: None,
                note: None,
            };
        }
        note = Some(format!(
            "the voice '{n}' set in Settings → Chat was chosen for {}; this thread's \
             text-to-speech model does not use it",
            chat_model.map_or("another text-to-speech model".to_string(), |m| format!(
                "the text-to-speech model '{m}'"
            )),
        ));
    }
    let inherits = first([(Some(s.realtime.default_voice.as_str()), Source::Realtime)]);
    VoiceName {
        name: None,
        source: inherits.map(|(_, src)| src),
        inherits: inherits.map(|(d, _)| d.to_string()),
        note,
    }
}

/// One stage's facts, and its problem when it has one. `inherited`: what
/// the levels below the thread give.
fn stage(
    snap: &Snapshot,
    which: &'static str,
    chosen: Option<(&str, Source)>,
    inherited: Option<(&str, Source)>,
    problems: &mut Vec<Problem>,
) -> Stage {
    let what = if which == "asr" {
        "speech-to-text"
    } else {
        "text-to-speech"
    };
    let mut out = Stage {
        alias: None,
        source: None,
        inherited: inherited.map(|(a, _)| a.to_string()),
        local: None,
        managed: false,
        cpu: false,
        fallback: None,
        fallback_unusable: None,
    };
    let Some((alias, source)) = chosen else {
        problems.push(Problem {
            stage: which,
            code: "not_configured",
            message: format!(
                "no {what} model is set: choose one in Settings → Chat → Voice, in Settings → \
                 Realtime, or in this thread's voice settings"
            ),
        });
        return out;
    };
    out.alias = Some(alias.to_string());
    out.source = Some(source);
    let route = match snap.resolve(alias) {
        Ok(r) => r,
        Err(e) => {
            problems.push(Problem {
                stage: which,
                code: "unresolved",
                message: format!(
                    "the {what} model '{alias}' ({}) does not resolve: {e}",
                    source_label(source)
                ),
            });
            return out;
        }
    };
    out.local = Some(snap.is_local_upstream(route.upstream.id));
    if let Some(target) = crate::vram::classify(&route) {
        out.managed = true;
        out.cpu = target.class == Class::Audio
            && snap.placement(target.class, &target.model_id) == Placement::Cpu;
        // A CPU row too: the GPU hold leaves it alone, but a benchmark run's
        // lease swaps it to this fallback exactly as the hold swaps a GPU row
        // (WP7 review M2), and the page says so before anything is recorded.
        match snap.fallback_route(&target) {
            crate::config::FallbackRoute::Usable { alias, route } => {
                out.fallback = Some(Fallback {
                    local: snap.is_local_upstream(route.upstream.id),
                    alias,
                });
            }
            crate::config::FallbackRoute::Unusable { alias, why } => {
                out.fallback_unusable = Some(FallbackUnusable { alias, why });
            }
            crate::config::FallbackRoute::None => {}
        }
    }
    out
}

/// Where a setting sits, for a sentence.
fn source_label(s: Source) -> &'static str {
    match s {
        Source::Thread => "this thread's voice settings",
        Source::Chat => "Settings → Chat → Voice",
        Source::Realtime => "Settings → Realtime",
        Source::Row => "the model's own description",
        Source::SpeechIn => "the language you speak",
    }
}

/// Whether `thread`'s turns may dispatch lmgw's own admin tools (§8.1): the
/// self-admin toolset attached by hand, and `self_admin` at `full`.
pub(crate) fn admin_tools(snap: &Snapshot, thread: &ChatThread) -> bool {
    snap.settings.self_admin == SelfAdmin::Full
        && thread
            .mcp_tools
            .iter()
            .any(|m| m.server_label == crate::mcp::exec::SELF_ADMIN_LABEL)
}

/// §8.1: realtime mode is refused for Admin Chat (ruling 7), and only for
/// it; a plain thread with the self-admin toolset attached is allowed and
/// flagged.
fn realtime_availability(snap: &Snapshot, thread: &ChatThread) -> RealtimeAvailability {
    let admin = thread.kind == super::super::agentchat::ADMIN_KIND;
    let admin_tools = admin_tools(snap, thread);
    RealtimeAvailability {
        ok: !admin,
        code: admin.then_some("chat_thread_admin"),
        reason: admin.then(|| "Voice mode is not available in Admin Chat".to_string()),
        admin_tools,
    }
}
