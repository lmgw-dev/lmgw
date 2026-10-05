//! The binding, at the handshake (chat-voice design §8.1): `?chat_thread=`
//! checked before the 101, so a bound session starts bound.
//!
//! Refusals, flat JSON as realtime's handshake refusals are (§10.2):
//!
//! | Status | Code | Why |
//! |---|---|---|
//! | 403 | `chat_thread_not_allowed` | the principal does not hold `Cap::Admin`, the capability of every `/chat/api` route (the dashboard cookie, or an owner key) |
//! | 400 | `owned_by_thread` | `?model=` beside it: the thread owns the chat model |
//! | 404 | `chat_thread_not_found` | |
//! | 409 | `chat_thread_admin` | an Admin Chat thread (ruling 7) |
//!
//! The capability is checked first, so a key that may not bind learns
//! nothing about the threads. A plain thread with the self-admin toolset
//! attached by hand binds: attaching it is the owner's choice (§8.1, user
//! intent wins); its thread JSON flags it (`voice_resolved.realtime.
//! admin_tools`).
//!
//! What the session starts with is the thread's resolution
//! (`web::chat_voice::bound::voice`): its chat model, its ASR and TTS
//! aliases and voice — realtime's default-model resolution and its policy
//! check are skipped, so a stale `realtime.default_model` cannot fail a
//! bound session. And the stages its connect warm loads (§4.1).

use axum::http::StatusCode;
use axum::response::Response;

use super::super::asr::{AsrResolution, AsrVia};
use super::super::handshake::http_error;
use super::super::resolve::{ChatResolution, Via};
use super::super::session::speech::{self, Speech};
use super::super::warm::Warm;
use crate::principal::Cap;
use crate::proxy::{RequestCtx, StopSignal};
use crate::state::SharedState;
use crate::web::chat_live::VoiceBinding;
use crate::web::chat_voice::bound::{self, VoiceConfig};

/// The thread a session binds to, as the handshake found it.
pub(crate) struct Binding {
    pub thread_id: i64,
    pub title: String,
    pub temporary: bool,
    /// Its turns may dispatch lmgw's admin tools (§8.1's flag).
    pub admin_tools: bool,
    /// What the thread's voice resolved to at the bind.
    pub cfg: VoiceConfig,
    /// Whether its voice turns go to the chat model as audio, judged at the
    /// bind (voice-audio-input design §2.2).
    pub audio_input: bound::Shown,
    /// The stages the connect warm loads (§4.1).
    pub warm: Vec<Warm>,
    /// The thread's one binding; taken by the session's core.
    pub guard: Option<VoiceBinding>,
    /// Raised when another window binds the thread.
    pub taken: StopSignal,
    /// The binding this one took over, raised once its session's journal
    /// drained (`web::chat_live`'s voice binding): this session's journal
    /// writes nothing before it.
    pub fence: Option<StopSignal>,
}

/// The handshake's answer for a bound session: the binding, and the chat,
/// ASR and speech resolutions the session starts with.
pub(crate) struct Bind {
    pub binding: Binding,
    pub chat: ChatResolution,
    pub asr: AsrResolution,
    pub speech: Speech,
}

/// A refusal before the 101 (module doc), in realtime's handshake shape.
fn refuse(status: StatusCode, code: &str, message: String) -> Response {
    http_error(status, "invalid_request_error", code, message)
}

/// Bind thread `id` for the principal of `ctx` (module doc). `model`: a
/// `?model=` was given beside it.
pub(crate) async fn handshake(
    state: &SharedState,
    ctx: &RequestCtx,
    id: i64,
    model: bool,
) -> Result<Bind, Response> {
    let snap = state.snapshot();
    if !ctx.principal.holds(Cap::Admin, &snap) {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "chat_thread_not_allowed",
            format!(
                "binding a session to a chat thread (chat_thread) is the dashboard's: it needs \
                 the admin capability, and the request presented {}",
                ctx.principal.describe()
            ),
        ));
    }
    if model {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "owned_by_thread",
            "model and chat_thread cannot both be given: a session bound to a chat thread \
             answers with the thread's own model — change it in the thread's settings"
                .into(),
        ));
    }
    let Some(thread) = bound::thread(state, id).await else {
        return Err(refuse(
            StatusCode::NOT_FOUND,
            "chat_thread_not_found",
            format!("there is no chat thread {id}"),
        ));
    };
    if bound::is_admin(&thread) {
        return Err(refuse(
            StatusCode::CONFLICT,
            "chat_thread_admin",
            "voice mode is not available in Admin Chat: its replies may change lmgw, and a \
             spoken turn runs without the step of reading it before it is sent"
                .into(),
        ));
    }
    let cfg = bound::voice(&snap, &thread);
    let problem = |stage: &str| {
        cfg.problems
            .iter()
            .find(|p| p.stage == stage)
            .map(|p| p.message.clone())
    };
    let chat = ChatResolution {
        alias: Some(thread.model_alias.clone()),
        via: Via::Alias,
    };
    let asr = AsrResolution {
        alias: cfg.asr.alias.clone(),
        via: AsrVia::Alias,
        missing: cfg.asr.alias.is_none().then(|| {
            problem("asr").unwrap_or_else(|| "the chat thread names no speech-to-text model".into())
        }),
    };
    let speech = speech::for_thread(
        state,
        cfg.tts.alias.as_deref(),
        problem("tts"),
        cfg.voice.name.as_deref(),
    )
    .await;
    let warm = bound::connect_stages(state, &thread).await;
    let audio_input = bound::audio_input(state, &thread).await;
    // Last: every refusal has had its say, and the session it takes over is
    // told only now.
    let bind = state.chat_live.bind_voice(id);
    Ok(Bind {
        binding: Binding {
            thread_id: id,
            title: thread.title.clone(),
            temporary: id < 0,
            admin_tools: bound::thread_ref(&snap, &thread).admin_tools,
            cfg,
            audio_input,
            warm,
            guard: Some(bind.guard),
            taken: bind.taken,
            fence: bind.fence,
        },
        chat,
        asr,
        speech,
    })
}
