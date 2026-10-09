//! A realtime session bound to a Chat thread (chat-voice design §8): the
//! dashboard's realtime mode.
//!
//! **The binding is decided at the handshake** (`bind`): `GET
//! /v1/realtime?chat_thread=<id>`, checked before the 101 — the Chat
//! capability (`Cap::Chat`: the dashboard, an owner key, a paired device),
//! the thread, and no Admin Chat; for a device, its key's check of the
//! thread's aliases (client-apps design §1.3). A bound session starts
//! bound: `session.created` carries the thread's resolution, realtime's
//! default-model resolution is skipped, and the connect warm is the
//! thread's Admit group (§4.2). One bound session per thread: a second bind
//! takes over, and the older session closes with a reason naming the binder
//! (`web::chat_live`'s voice binding, client-apps §1.7); the newer session's
//! journal writes only once the older one's has drained.
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
//! **Late MCP task results** (MCP Tasks design §3.4, `tasks`): each result
//! that enters the thread is said as `lmgw.task.done`, and a
//! `response.create` with no new words answers the results no reply
//! answered yet (a continuation) instead of being refused `empty_turn`.
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

pub(crate) mod approvals;
mod bind;
pub(crate) mod hearing;
mod hooks;
pub(crate) mod journal;
mod owned;
pub(crate) mod reply;
/// The session's own voice following its thread's (personality-profiles
/// design D21).
pub(crate) mod reshape;
mod stages;
/// Late MCP task results in the thread (MCP Tasks design §3.4, §4.2).
pub(crate) mod tasks;
pub(crate) mod turn;
mod verdict;

use journal::Journal;

pub(crate) use bind::{handshake, Binding};
pub(in crate::realtime) use owned::{check_create, check_update, refuse_item};

/// The close code a session taken over by another window closes with:
/// application-defined (RFC 6455 §7.4.2), so a page tells it from a
/// network close. Shared with clients (`lmgw-api-types`).
pub(crate) use lmgw_api_types::realtime::CLOSE_TAKEN_OVER;

/// The close code a device's session closes with when its thread left its
/// reach ([`out_of_reach_reason`]): its own, not a revocation's 4003, since
/// the key is still good. Shared with clients (`lmgw-api-types`).
pub(crate) use lmgw_api_types::realtime::CLOSE_OUT_OF_REACH;

/// The close reason a session taken over gets when the binder has no name.
pub(crate) const TAKEN_OVER: &str = "voice mode moved to another window";

/// The reason a session taken over is given (client-apps design §1.7):
/// "voice mode moved to device 'phone'", "… to the dashboard" — cut at a
/// character boundary to the 123 bytes a close frame's reason may hold.
pub(crate) fn taken_over_reason(by: Option<&str>) -> String {
    let Some(by) = by else {
        return TAKEN_OVER.to_string();
    };
    let mut text = format!("voice mode moved to {by}");
    if text.len() > 123 {
        let mut end = 120;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

/// The reason a device's session closes with when its thread left its reach
/// (client-apps design L3, review W3-1), with [`CLOSE_OUT_OF_REACH`]: "chat
/// thread 7 is out of reach for this key". Neutral, like every other L3 refusal (review W4-18):
/// it does not say why, so a device does not learn that the self-admin
/// toolset was attached. A close frame's reason may hold 123 bytes; this
/// one is at most 70.
pub(crate) fn out_of_reach_reason(thread_id: i64) -> String {
    format!("chat thread {thread_id} is out of reach for this key")
}

/// What a bound session's spoken turn in flight is told when its thread is
/// not there for its binder any more — deleted, or (for a device) out of
/// its reach — before the session's close comes: for a device the close's
/// own neutral words, the same for a deleted thread as for one hidden from
/// it (review W4-18; client-apps design §1.6's close-code note,
/// 2026-10-07); for the owner, that it is gone. Its code is
/// `chat_thread_not_found` in both.
pub(crate) fn not_there(thread_id: i64, device: bool) -> String {
    if device {
        out_of_reach_reason(thread_id)
    } else {
        format!("chat thread {thread_id} is gone (deleted, or a temporary chat kept or discarded)")
    }
}

/// What a bound session's core keeps of its binding.
pub(crate) struct Bound {
    pub thread_id: i64,
    /// Raised when another window binds the thread (module doc).
    pub taken: StopSignal,
    /// Who did, once `taken` is raised (client-apps design §1.7).
    pub taken_by: crate::web::chat_live::TakenBy,
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
    /// The approval items it showed and the client's answers
    /// (client-apps design §6.4).
    pub approvals: approvals::Approvals,
    /// The thread's job results no reply answered, and the newest message
    /// read: what a continuation answers, and what `lmgw.task.done` said
    /// (MCP Tasks design §3.4).
    pub tasks: tasks::Owed,
    /// A turn was committed with no automatic response owed to it — the
    /// client's `input_audio_buffer.commit`, or turn detection's with
    /// `create_response` off — since the client's last `response.create`:
    /// the next create answers it, and is no continuation (MCP Tasks design
    /// §3.4, `lifecycle::bound`).
    pub committed: bool,
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
    /// The session ends because its key was revoked (client-apps design
    /// §1.6): its binding's `voice.ended` says so.
    pub(crate) fn revoked(&self) {
        self._binding.revoked();
    }

    /// The session ends because its thread left its device's reach (the
    /// self-admin toolset was attached, client-apps design L3, review W3-1),
    /// not because another window took it over.
    pub(crate) fn out_of_reach(&self) -> bool {
        self._binding.out_of_reach()
    }

    /// The session left its loop and ends now (review F-5): a
    /// `takeover=never` bind no longer counts its binding.
    pub(crate) fn closing(&self) {
        self._binding.closing();
    }

    pub(crate) fn new(b: &mut Binding) -> Self {
        Self {
            thread_id: b.thread_id,
            taken: b.taken.clone(),
            taken_by: b.taken_by.clone(),
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
            approvals: Default::default(),
            tasks: std::mem::take(&mut b.tasks),
            committed: false,
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
/// checked and normalized as a client's `session.update` would be. The
/// voice, the TTS and the style follow the thread from then on, each
/// response's re-read taking a change (`reshape`).
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
    let mut patch = reshape::voice_patch(cfg);
    let audio = patch
        .entry("audio")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("the voice patch's audio is an object");
    audio.insert(
        "input".into(),
        json!({
            "transcription": {
                "model": cfg.asr.alias,
                "language": cfg.language.value,
            },
            "turn_detection": turn_detection,
        }),
    );
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
