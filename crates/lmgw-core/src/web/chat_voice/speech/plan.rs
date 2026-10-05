//! How a thread speaks (chat-voice design §6.1): realtime's speech, resolved
//! for the thread as if a session had asked for it.
//!
//! - **The alias** is the thread's resolved TTS (`resolve`: thread →
//!   Settings → Chat → Voice → Settings → Realtime); none at any level is
//!   `tts_not_configured`.
//! - **The voice** goes through realtime's chain (realtime §5.3) with what is
//!   known of the model without a request ([`voice_of`]): the thread's voice
//!   name, or none named — realtime's `default_voice`, then the row's own,
//!   decide. A name the chain finds nothing for is `voice_not_configured`,
//!   one the model is known to lack `voice_not_found`; one only the model's
//!   list could confirm is spoken provisionally and checked at the first
//!   clause (`responder::speech`'s settle). A voice-library clip whose
//!   engine refuses to clone it without a transcript, and that has none, is
//!   `voice_needs_transcript` ([`super::clip`]).
//! - **The style** is realtime's precedence ([`style_of`]): the thread's own
//!   speech style is the session level — the owner's choice for this
//!   thread, which a voice-design row's own description does not override —
//!   and the owner-wide style (Settings → Chat's, else
//!   `realtime.speech_instructions`) is the setting, which stands back for a
//!   voice-design row that describes itself, as realtime's own setting does:
//!   a style is not a voice. A voice-design row with no description anywhere
//!   is `instructions_required`. The thread JSON shows the style used
//!   ([`resolve_shown`]).
//! - **The language** — the thread's reply language, the one the model
//!   answers in (split 2026-10-05) — is a request (changed 2026-10-04):
//!   every clause is sent it wherever the TTS row takes a language, in the row's spelling,
//!   and where it does not, the plan logs why
//!   (`audio::language::tts_fit`, `chat_voice::language`). It also picks
//!   the announcements' words ([`Announce`]).
//! - **The seed** is the thread's own, drawn and stored on first use with a
//!   conditional write ([`ChatRepo::draw_seed`]): two first uses keep one
//!   seed, so one thread keeps one voice on a row whose voice comes from its
//!   seed. It is unpinned in `sends_seed`'s sense: only such a row gets it.
//! - **No writer** (`progress: None`, `ahead: None`): read-aloud is unpaced,
//!   and its rows carry the label the thread's model turns carry
//!   (`speech_proto`: `chat`, or `admin` for an Admin Chat thread).
//! - **The longest pause** is `realtime.longest_pause_ms`, the one owner-wide
//!   value: the Chat has no voice setting of its own for it (a bound
//!   realtime session's own `lmgw.longest_pause_ms` replaces it for that
//!   session's turns).

use crate::audio::language::SpeechLanguage;
use crate::config::Snapshot;
use crate::proxy::synthesize::SessionSeed;
use crate::realtime::clauses::Announce;
use crate::realtime::expressive::{self, Asked, SpeechFacts, Style};
use crate::realtime::protocol::{ErrorObject, Voice};
use crate::realtime::responder::Speech;
use crate::realtime::voice::{self, VoiceFacts, VoiceOutcome};
use crate::realtime::warm::{Speaks, Warm};
use crate::state::SharedState;
use crate::store::ChatThread;

use super::super::resolve::{owner_style, SpeechStyle};

use super::super::super::chat_repo::ChatRepo;
use super::super::resolve::{resolve, Source, VoiceConfig};
use super::super::speech_proto;

/// A thread's speech, ready to speak with.
pub(crate) struct Plan {
    pub speech: Speech,
    pub announce: Announce,
    /// What the TTS does with speech instructions and inline tags — what a
    /// voice turn's tag hint is made from (§8.5).
    pub expressive: SpeechFacts,
}

/// Why a thread cannot speak: the `speech_error` frame's `code` and
/// `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub code: String,
    pub message: String,
}

impl Refusal {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

/// The voice the thread asks `alias` for, and what is known of the model
/// without a request: its voice facts (whether it designs its voice
/// included) and how it takes instructions and tags.
pub(crate) struct ThreadVoice {
    pub requested: Voice,
    pub facts: VoiceFacts,
    pub expressive: SpeechFacts,
    pub outcome: Result<VoiceOutcome, ErrorObject>,
}

/// Resolve the thread's voice for `alias` through realtime's chain (module
/// doc). `label`: what the log lines start with.
pub(crate) async fn voice_of(
    state: &SharedState,
    snap: &Snapshot,
    cfg: &VoiceConfig,
    alias: &str,
    label: &str,
) -> ThreadVoice {
    let requested = Voice::Name(
        cfg.voice
            .name
            .clone()
            .unwrap_or_else(|| crate::realtime::merge::DEFAULT_VOICE.to_string()),
    );
    let mut facts = voice::facts(state, alias, Some(label)).await;
    let expressive = expressive::facts(state, alias).await;
    // A row that designs its voice is sent none where nobody named one
    // (realtime R2), as a session's is.
    facts.designs = expressive.designs();
    let outcome = voice::resolve(Some(alias), &requested, &facts, &snap.settings.realtime);
    ThreadVoice {
        requested,
        facts,
        expressive,
        outcome,
    }
}

/// The speech instructions the thread's clauses carry (module doc): the
/// thread's own style at the session level, the owner-wide one as the
/// setting.
pub(crate) fn style_of(snap: &Snapshot, cfg: &VoiceConfig, facts: &SpeechFacts) -> Style {
    let owner = owner_style(&snap.settings);
    let session = match cfg.speech_style.source {
        Source::Thread => Some(cfg.speech_style.text.as_str()),
        Source::Chat | Source::Realtime | Source::Row | Source::SpeechIn => None,
    };
    expressive::resolve_style(
        facts,
        &Asked {
            response: None,
            session,
            setting: &owner.text,
        },
    )
}

/// [`resolve`] as the thread JSON shows it (§2.3): its `speech_style` the
/// one the thread's speech uses — the row's own description (`source:
/// row`) where a voice-design row describes itself and the thread sets no
/// style of its own ([`style_of`]). Only the row's speech facts can tell,
/// so this reads them (its cached profile, its voice files). And a voice
/// clip its TTS cannot clone without a transcript is a `tts` problem
/// (`voice_needs_transcript`, [`super::clip::problem`]): the page refuses
/// read-aloud and voice mode with it before the press. Where a stage's model
/// does not take its language as set, `language_notes` says so
/// (`chat_voice::language`); `audio_input` says whether a voice turn goes to
/// the chat model as audio (`chat_voice::audio_input`).
pub(crate) async fn resolve_shown(state: &SharedState, thread: &ChatThread) -> VoiceConfig {
    let snap = state.snapshot();
    let mut cfg = resolve(&snap, thread);
    if let Some(alias) = cfg.tts.alias.clone() {
        let facts = expressive::facts(state, &alias).await;
        let style = style_of(&snap, &cfg, &facts);
        if style.source == Some(expressive::Source::Row) {
            cfg.speech_style = SpeechStyle {
                text: style.text.unwrap_or_default(),
                source: Source::Row,
            };
        }
        let label = format!("chat thread {}", thread.id);
        if let Some(p) = super::clip::problem(state, &snap, &cfg, &alias, &label).await {
            cfg.problems.push(p);
        }
    }
    cfg.language_notes = super::super::language::notes(state, &snap, &cfg).await;
    cfg.audio_input = Some(super::super::audio_input::shown(state, thread).await);
    cfg
}

/// How `thread` speaks (module doc) — drawing its seed on first use — or
/// why it cannot.
pub(crate) async fn plan(
    state: &SharedState,
    repo: ChatRepo,
    thread: &ChatThread,
) -> Result<Plan, Refusal> {
    let snap = state.snapshot();
    let cfg = resolve(&snap, thread);
    let label = format!("chat thread {}", thread.id);
    let Some(alias) = cfg.tts.alias.clone() else {
        let message = cfg
            .problems
            .iter()
            .find(|p| p.stage == "tts")
            .map(|p| p.message.clone())
            .unwrap_or_else(|| "no text-to-speech model is set (Settings → Chat → Voice)".into());
        return Err(Refusal::new("tts_not_configured", message));
    };
    let v = voice_of(state, &snap, &cfg, &alias, &label).await;
    let voice = match v.outcome {
        Ok(VoiceOutcome::Resolved(voice)) => voice,
        Ok(VoiceOutcome::Missing(why)) => return Err(Refusal::new("voice_not_configured", why)),
        Ok(VoiceOutcome::NotFound(why)) => return Err(Refusal::new("voice_not_found", why)),
        Err(e) => {
            let code = e.code.unwrap_or_else(|| "voice_not_found".into());
            return Err(Refusal::new(&code, e.message));
        }
    };
    if let Some(refusal) = super::clip::refusal(&alias, &voice, &v.facts) {
        return Err(refusal);
    }
    tracing::debug!(
        "{label}: TTS '{alias}' speaks voice '{}'{}",
        voice.name,
        if voice.verified {
            ""
        } else {
            " (checked at the first clause)"
        }
    );
    let style = style_of(&snap, &cfg, &v.expressive);
    if v.expressive.designs() && style.text.is_none() {
        return Err(Refusal::new(
            "instructions_required",
            format!(
                "the text-to-speech model '{alias}' designs its voice from a description, and \
                 none is set — give this thread a speech style, set one in Settings → Chat → \
                 Voice, or give the row a default under its default request options \
                 (`instruct`)"
            ),
        ));
    }
    let seed = thread_seed(state, repo, thread.id, &cfg, &label).await;
    // The reply language: the voice speaks what the model answers in
    // (chat-voice design §2.1, split 2026-10-05).
    let language = cfg.reply_language.value.clone();
    if let Some(code) = language.as_deref() {
        // The voice the engine speaks: the one sent, else its own default —
        // what the page's note judges too (`language::tts_note`).
        if let Some(note) =
            super::super::language::tts_note(state, &snap, &alias, code, voice.send.as_deref())
                .await
        {
            super::super::language::log_once(&label, &alias, code, &note);
        }
    }
    Ok(Plan {
        expressive: v.expressive,
        announce: Announce::for_language(language.as_deref()),
        speech: Speech {
            label,
            proto: speech_proto(thread),
            alias,
            requested: v.requested,
            voice,
            facts: v.facts,
            speed: None,
            language: language.map(SpeechLanguage::Request),
            instructions: style.send,
            dropped: style.dropped,
            seed: Some(SessionSeed {
                value: seed,
                pinned: false,
            }),
            ahead: None,
            longest_pause_ms: snap.settings.realtime.longest_pause_ms,
            progress: None,
        },
    })
}

/// The thread's TTS seed (module doc): its own, or one drawn and stored now
/// on its first use — a read-aloud's, or the press's warm's, so the warm
/// warms the voice the read-aloud then speaks with (review m7).
pub(crate) async fn thread_seed(
    state: &SharedState,
    repo: ChatRepo,
    thread_id: i64,
    cfg: &VoiceConfig,
    label: &str,
) -> u32 {
    if let Some(seed) = cfg.seed {
        return seed;
    }
    let drawn = rand::random::<u32>();
    repo.draw_seed(state, thread_id, drawn)
        .await
        .unwrap_or_else(|e| {
            // Spoken with all the same; the next use draws again.
            tracing::warn!("{label}: its TTS seed could not be stored: {e}");
            drawn
        })
}

impl Plan {
    /// The TTS stage as this speech will speak, for a warm: its alias,
    /// instructions, voice, language and seed.
    pub(crate) fn warm(&self) -> Warm {
        let s = &self.speech;
        Warm::Voice {
            alias: s.alias.clone(),
            instructions: s.instructions.clone(),
            speaks: Some(Speaks {
                voice: s.voice.send.clone(),
                language: s.language.clone(),
                seed: s.seed,
            }),
        }
    }
}
