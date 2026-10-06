//! A realtime session bound to a Chat thread (chat-voice design §8): the
//! dashboard's realtime mode.
//!
//! **The binding is decided at the handshake** (`bind`): `GET
//! /v1/realtime?chat_thread=<id>`, checked before the 101 — the dashboard's
//! own capability (`Cap::Admin`), the thread, and no Admin Chat. A bound
//! session starts bound: `session.created` carries the thread's resolution,
//! realtime's default-model resolution and its policy check are skipped,
//! and the connect warm is the thread's Admit group (§4.2). One bound
//! session per thread: a second bind takes over, and the older session
//! closes with a reason (`web::chat_live`'s voice binding); the newer
//! session's journal writes only once the older one's has drained.
//!
//! **The thread owns** the chat model, the instructions, the tools, the
//! transcription model, the TTS, the voice, the speech instructions, the tag
//! hint, the language, the seed and the conversation (`owned`): a
//! `session.update` that changes one of them, the overrides of a
//! `response.create`, and `conversation.item.create` / `.delete` are refused
//! with `owned_by_thread`. The client owns turn detection, the barge-in and
//! echo knobs, the output modality and the response and buffer events.
//!
//! **A bound turn** (§8.2) is the Chat's own turn (`turn`): the thread's
//! model, prompt with the voice block (§8.5), sampling, reasoning,
//! attachments, knowledge bases and MCP tools apply as in text, and its
//! frames are relayed as `lmgw.chat.frame`. **The journal** (`journal`,
//! §8.3) writes each response's user message and finalizes its reply with
//! what was heard (`reply`), in a fixed order and under the thread's
//! conditional write; it drains before the session's task ends (§8.6).
//!
//! **A turn the model hears** (voice-audio-input design): the verdict
//! ([`Bound::audio_input`], judged at the bind, at each `speech_started`
//! and after each response, `verdict`) decides at the commit whether a turn
//! goes to the chat model as audio; such a turn is in [`Bound::hearing`]
//! until its transcript is in (`hearing`).
//!
//! An unbound session behaves exactly as before: nothing here runs for it,
//! and it never sends an `lmgw.*` event.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::protocol::{ChatThreadRef, Session};
use super::transcribe::Facts;
use super::warm::ModelState;
use crate::config::RealtimeSettings;
use crate::proxy::StopSignal;
use crate::store::TurnDetection;
use crate::web::chat_live::VoiceBinding;

mod bind;
pub(crate) mod hearing;
mod hooks;
pub(crate) mod journal;
mod owned;
pub(crate) mod reply;
mod stages;
pub(crate) mod turn;
mod verdict;

use journal::Journal;

pub(crate) use bind::{handshake, Binding};
pub(in crate::realtime) use owned::{check_create, check_update, refuse_item};

/// The close code a session taken over by another window closes with:
/// application-defined (RFC 6455 §7.4.2), so a page tells it from a
/// network close.
pub(crate) const CLOSE_TAKEN_OVER: u16 = 4000;

/// The close reason a session taken over gets.
pub(crate) const TAKEN_OVER: &str = "voice mode moved to another window";

/// What a bound session's core keeps of its binding.
pub(crate) struct Bound {
    pub thread_id: i64,
    /// Raised when another window binds the thread (module doc).
    pub taken: StopSignal,
    /// The session this one took over, until its journal drained: the
    /// journal waits for it (`web::chat_live`'s voice binding).
    pub fence: Option<StopSignal>,
    /// The thread's history writes (§8.3); started with the session's loop.
    pub journal: Option<Journal>,
    /// Where the connect warm says its outcomes (`lmgw.model.state`).
    pub states: Option<mpsc::UnboundedSender<ModelState>>,
    /// The committed turns handed to the journal — written, or queued to be:
    /// a turn owed again after a cut is never written twice.
    pub submitted: HashSet<String>,
    /// How each committed turn was transcribed.
    pub asr: HashMap<String, Facts>,
    /// Why a turn a response went with as audio has no transcript: its
    /// transcription was attempted and failed — what its user row says once
    /// a model heard it (voice-audio-input design §3.2). Kept past its
    /// response's transcript only when the attempt carried the audio, for a
    /// response that answers it after a cut, and until a response that
    /// answers it has played (`lifecycle::hearing`, `bound_ended`).
    pub asr_errors: HashMap<String, String>,
    /// The launched responses whose reply slot is open, and what is known
    /// of each.
    pub responses: HashMap<u64, Served>,
    /// Each finished response's reply item and its generation: a truncate
    /// of it after its response ended re-cuts the reply.
    pub replies: HashMap<String, u64>,
    /// Each launched response's id, by generation: its turn's frames name
    /// it, the late ones after a cancel too.
    pub response_ids: HashMap<u64, String>,
    /// The `speech_started` warm's last stages (`stages`).
    pub stages: std::sync::Arc<stages::Stages>,
    /// The models whose server refused a heard turn's audio this session,
    /// or failed on it, and why (voice-audio-input design §3.5): the
    /// verdict's session row, so later turns to them go as their transcript
    /// until voice mode is entered again. lmgw's own refusals
    /// (`audio_input_unsupported`, a context guard) never enter it.
    pub refused: crate::web::chat_voice::bound::Refusals,
    /// Whether the next turn goes to the chat model as audio, and why not
    /// (voice-audio-input design §2.2): the thread's verdict as last judged
    /// (`verdict`), with the setting it was judged under. A commit reads it,
    /// with [`Self::refused`] on top. A prediction, not a permission: only
    /// `fit_route` lets the audio leave.
    pub audio_input: crate::web::chat_voice::bound::Shown,
    /// The last verdict judging started (`verdict`): an older one that
    /// comes back later is not taken.
    pub verdicts: u64,
    /// Where a verdict judged off the core comes back to.
    pub verdict_tx: Option<mpsc::UnboundedSender<(u64, crate::web::chat_voice::bound::Shown)>>,
    /// The audio turns still being transcribed, and the responses that
    /// heard them (`hearing`, voice-audio-input design §3.1).
    pub hearing: hearing::Hearing,
    /// The responses whose hold ended with nothing released — a veto, or a
    /// cut nobody heard (`lifecycle::held`): what they still say is dropped,
    /// not relayed, until their responder's last word.
    pub unreleased: HashSet<u64>,
    /// Holds the thread's one binding for the session's life.
    _binding: VoiceBinding,
}

/// What a bound response answered and spoke with (§8.7's models and cold
/// stages), as the responder and the speaker say it.
#[derive(Debug, Default)]
pub(crate) struct Served {
    pub chat: Option<String>,
    pub chat_answered_by: Option<String>,
    pub chat_cold: bool,
    pub tts: Option<String>,
    pub tts_answered_by: Option<String>,
    pub voice: Option<String>,
    pub tts_cold: bool,
    /// When the TTS route's opening said `loading`, until it opened.
    pub tts_loading: Option<std::time::Instant>,
    /// Its first clause was an announcement (§8.4); `None` before one.
    pub first_announced: Option<bool>,
    /// How its turns reached the chat model, and why the transcript
    /// (voice-audio-input design §5); `None` with audio input off.
    pub input: Option<(crate::store::InputPath, Option<String>)>,
}

impl Bound {
    pub(crate) fn new(b: &mut Binding) -> Self {
        Self {
            thread_id: b.thread_id,
            taken: b.taken.clone(),
            fence: b.fence.take(),
            journal: None,
            states: None,
            submitted: HashSet::new(),
            asr: HashMap::new(),
            asr_errors: HashMap::new(),
            responses: HashMap::new(),
            replies: HashMap::new(),
            response_ids: HashMap::new(),
            stages: Default::default(),
            refused: Default::default(),
            audio_input: b.audio_input.clone(),
            verdicts: 0,
            verdict_tx: None,
            hearing: Default::default(),
            unreleased: HashSet::new(),
            _binding: b.guard.take().expect("a binding is taken once"),
        }
    }
}

/// Resolves when `taken` is raised; never for an unbound session.
pub(in crate::realtime) async fn taken(taken: Option<&StopSignal>) {
    match taken {
        Some(t) => t.raised().await,
        None => std::future::pending().await,
    }
}

/// A bound session's session object, shaped by what its thread chose
/// (§8.1): its transcription model and language, its voice, its TTS and
/// its own speech style, and its turn detection as the session's starting
/// point (the client's to change). Through realtime's own merge, so it is
/// checked and normalized as a client's `session.update` would be.
pub(in crate::realtime) fn shape_session(
    session: &mut Session,
    b: &Binding,
    settings: &RealtimeSettings,
) {
    let cfg = &b.cfg;
    let turn_detection = match cfg.turn_detection.value {
        TurnDetection::SemanticVad => json!({"type": "semantic_vad"}),
        TurnDetection::ServerVad => json!({"type": "server_vad"}),
        TurnDetection::PushToTalk => Value::Null,
    };
    let style = match cfg.speech_style.source {
        crate::web::chat_voice::bound::Source::Thread => json!(cfg.speech_style.text),
        _ => Value::Null,
    };
    let patch = json!({
        "type": "realtime",
        "audio": {
            "input": {
                "transcription": {
                    "model": cfg.asr.alias,
                    "language": cfg.language.value,
                },
                "turn_detection": turn_detection,
            },
            "output": {
                "voice": cfg.voice.name.as_deref().unwrap_or(super::merge::DEFAULT_VOICE),
            },
        },
        "lmgw": {"tts_model": cfg.tts.alias, "speech_instructions": style},
    });
    let Value::Object(patch) = patch else {
        return;
    };
    match super::merge::apply_update(session, &patch, settings) {
        Ok(shaped) => *session = shaped,
        // Only a stored value out of range could: the session keeps
        // realtime's defaults, and says so.
        Err(e) => tracing::warn!(
            "realtime {}: chat thread {}'s voice settings did not apply to the session: {}",
            session.id.as_deref().unwrap_or("?"),
            b.thread_id,
            e.message
        ),
    }
}

/// `session.lmgw.resolved.chat_thread` (§8.1).
pub(in crate::realtime) fn set_resolved(session: &mut Session, b: &Binding) {
    let lmgw = session.lmgw.get_or_insert_with(Default::default);
    let resolved = lmgw.resolved.get_or_insert_with(Default::default);
    resolved.chat_thread = Some(ChatThreadRef {
        id: b.thread_id,
        title: b.title.clone(),
        temporary: b.temporary,
        admin_tools: b.admin_tools,
    });
}
