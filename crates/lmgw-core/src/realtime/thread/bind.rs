//! The binding, at the handshake (chat-voice design §8.1, client-apps design
//! §1.3): `?chat_thread=` checked before the 101, so a bound session starts
//! bound.
//!
//! Refusals, in the handshake's OpenAI error shape (`{"error": {message,
//! type, param, code}}`, realtime §10.2; review W6-5 corrected "flat"):
//!
//! | Status | Code | Why |
//! |---|---|---|
//! | 403 | `chat_thread_not_allowed` | the principal does not hold `Cap::Chat`, the capability of every `/chat/api` route (the dashboard's cookie, an owner key, a paired device) |
//! | 400 | `owned_by_thread` | `?model=` beside it: the thread owns the chat model |
//! | 404 | `chat_thread_not_found` | no such thread — or, for a device, an Admin Chat thread (client-apps L3) |
//! | 409 | `chat_thread_admin` | an Admin Chat thread the owner binds (ruling 7) |
//! | 409 | `chat_thread_bound` | `takeover=never`, and another session is bound to the thread: "voice is in use on device 'phone'" (2026-10-07) |
//! | 401, 403, 429 | the key's own | a device's key may not use the thread's chat, ASR or TTS alias: realtime §10.2's check |
//! | 500 | `internal` | the store failed to read the thread again once the binding registered: lmgw's own failure, never "there is no chat thread" (the branch review's N-4) |
//!
//! The capability is checked first, so a key that may not bind learns
//! nothing about the threads. A plain thread with the self-admin toolset
//! attached by hand binds: attaching it is the owner's choice (§8.1, user
//! intent wins); its thread JSON flags it (`voice_resolved.realtime.
//! admin_tools`).
//!
//! What the session starts with is the thread's resolution
//! (`web::chat_voice::bound::voice`): its chat model, its ASR and TTS
//! aliases and voice — realtime's default-model resolution is skipped, so a
//! stale `realtime.default_model` cannot fail a bound session. And the
//! stages its connect warm loads (§4.1).
//!
//! **A device binds as its key** (client-apps design §1.3). Before the 101
//! the thread's chat, ASR and TTS aliases pass realtime §10.2's check
//! against the key (scope and budget; the gate's `admit_session` already ran
//! expiry, the rate windows and the session's concurrency slot), and every
//! model call the session makes is checked and counted as an unbound
//! session's is (§10.3). Each of a device's refusals here writes its request
//! row, as realtime's own handshake refusals do. The owner's bind is the
//! dashboard's own request and keeps today's behaviour: no alias check, no
//! row for a refusal.
//!
//! The bind names its binder (§1.7): a session it takes over is told
//! "voice mode moved to device 'phone'" — or "… to the dashboard".
//!
//! **The owed set at bind** (MCP Tasks design §3.4): the thread's job
//! results no reply answered yet are read once the binding is registered,
//! so a client that binds to speak a result it learned of from the feed
//! (ruling 22, `takeover=never`) has its `response.create` run the
//! continuation.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::super::asr::{AsrResolution, AsrVia};
use super::super::handshake::http_error;
use super::super::resolve::{ChatResolution, Via};
use super::super::session::speech::{self, Speech};
use super::super::warm::Warm;
use crate::ingress::ClientProto;
use crate::principal::Cap;
use crate::proxy::{RequestCtx, StopSignal};
use crate::state::SharedState;
use crate::web::chat_live::{TakenBy, VoiceBinding};
use crate::web::chat_voice::bound::{self, Caller, VoiceConfig};

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
    /// Who did (client-apps design §1.7).
    pub taken_by: TakenBy,
    /// The binding this one took over, raised once its session's journal
    /// drained (`web::chat_live`'s voice binding): this session's journal
    /// writes nothing before it.
    pub fence: Option<StopSignal>,
    /// The thread's job results as the bind found them (MCP Tasks design
    /// §3.4): the ones no reply answered are owed a continuation; taken by
    /// the session's core.
    pub tasks: super::tasks::Owed,
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
/// A device's writes its request row first, as realtime's own handshake
/// refusals do (client-apps design §1.3); the owner's is the dashboard's own
/// request, and writes none (chat-voice §8.8, NIT 12).
async fn refuse(
    state: &SharedState,
    ctx: &RequestCtx,
    (status, code): (StatusCode, &'static str),
    message: String,
) -> Response {
    if Caller::of(ctx).is_device() {
        let e = crate::error::GatewayError::Refused {
            status: status.as_u16(),
            code,
            message: message.clone(),
        };
        crate::proxy::record_middleware_refusal(state, ctx, &e, ClientProto::Realtime).await;
    }
    let kind = if status.is_server_error() {
        "server_error"
    } else {
        "invalid_request_error"
    };
    http_error(status, kind, code, message)
}

/// Bind thread `id` for the principal of `ctx` (module doc). `model`: a
/// `?model=` was given beside it. `takeover`: a session already bound to
/// the thread is taken over; `false` (`takeover=never`): the bind is
/// refused instead, `409 chat_thread_bound`.
pub(crate) async fn handshake(
    state: &SharedState,
    ctx: &RequestCtx,
    id: i64,
    model: bool,
    takeover: bool,
) -> Result<Bind, Response> {
    let snap = state.snapshot();
    if !ctx.principal.holds(Cap::Chat, &snap) {
        let message = format!(
            "binding a session to a chat thread (chat_thread) needs the chat capability — the \
             dashboard, an owner key or a paired device — and the request presented {}",
            ctx.principal.describe()
        );
        let why = (StatusCode::FORBIDDEN, "chat_thread_not_allowed");
        return Err(refuse(state, ctx, why, message).await);
    }
    let caller = Caller::of(ctx);
    if model {
        let message = "model and chat_thread cannot both be given: a session bound to a chat \
                       thread answers with the thread's own model — change it in the thread's \
                       settings"
            .to_string();
        let why = (StatusCode::BAD_REQUEST, "owned_by_thread");
        return Err(refuse(state, ctx, why, message).await);
    }
    // An Admin Chat thread does not exist for a device (client-apps L3), nor
    // one with the self-admin toolset unless the device may use lmgw's admin
    // tools: the 404 of a thread that is not there.
    let Some(thread) = bound::thread(state, id)
        .await
        .filter(|t| caller.sees(&snap, t))
    else {
        let why = (StatusCode::NOT_FOUND, "chat_thread_not_found");
        return Err(refuse(state, ctx, why, format!("there is no chat thread {id}")).await);
    };
    if bound::is_admin(&thread) {
        let message = "voice mode is not available in Admin Chat: its replies may change lmgw, \
                       and a spoken turn runs without the step of reading it before it is sent"
            .to_string();
        let why = (StatusCode::CONFLICT, "chat_thread_admin");
        return Err(refuse(state, ctx, why, message).await);
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
    // A device binds as its key (module doc): realtime §10.2's check for the
    // thread's chat, ASR and TTS aliases, each refusal's row written by the
    // check itself.
    if caller.is_device() {
        let checked = super::super::policy::check_session(state, ctx, &chat, &asr, &speech.tts);
        if let Err(e) = checked.await {
            return Err((e.http_status(), axum::Json(e.to_openai_json())).into_response());
        }
    }
    let warm = bound::connect_stages(state, &thread).await;
    let audio_input = bound::audio_input(state, &thread).await;
    // Last: every refusal has had its say, and the session it takes over is
    // told only now — by whom (§1.7).
    let named = caller.named();
    let binder = crate::web::chat_live::Binder {
        by: &named,
        device: caller.is_device(),
        level: thread.reach_level(),
        key_id: ctx.principal.key_id(),
    };
    let bind = if takeover {
        state.chat_live.bind_voice_as(id, binder)
    } else {
        match state.chat_live.bind_voice_unless_bound(id, binder) {
            Ok(bind) => bind,
            Err(holder) => {
                // The thread as it is now (review F-15): one that left the
                // binder's reach since the read above is not there, rather
                // than in use — the binder is not named to a device that may
                // no longer see the thread.
                let reached = bound::thread(state, id)
                    .await
                    .is_some_and(|t| caller.sees(&state.snapshot(), &t));
                if !reached {
                    let why = (StatusCode::NOT_FOUND, "chat_thread_not_found");
                    let message = format!("there is no chat thread {id}");
                    return Err(refuse(state, ctx, why, message).await);
                }
                let why = (StatusCode::CONFLICT, "chat_thread_bound");
                let message = format!(
                    "voice is in use on {holder}: this bind asked not to take it over \
                     (takeover=never)"
                );
                return Err(refuse(state, ctx, why, message).await);
            }
        }
    };
    // The thread as it is now that the binding is registered (review
    // W4-3): an attach of the self-admin toolset that landed after the read
    // above found no binding to stop, nor did the device's admin-tools
    // switch turned off meanwhile. Any later one finds this one. A store
    // that failed to say is lmgw's own failure, not a thread that went (the
    // branch review's N-4): its live flags stay as they are.
    let now = match bound::thread_checked(state, id).await {
        Ok(now) => now,
        Err(e) => {
            drop(bind);
            let why = (StatusCode::INTERNAL_SERVER_ERROR, "internal");
            let message = format!("chat thread {id} could not be read: {e}");
            tracing::warn!("realtime: binding {message}");
            return Err(refuse(state, ctx, why, message).await);
        }
    };
    let level = now.as_ref().map_or(0, |t| t.reach_level());
    if level != thread.reach_level() {
        // Its live flags follow, and a binding that lost its reach stops.
        state
            .chat_live
            .self_admin_changed(&state.snapshot(), id, level);
    }
    // A thread deleted since the read is not there for the owner either
    // (its live events already went to the owner alone): nothing to bind.
    let reached = now.is_some_and(|t| caller.sees(&state.snapshot(), &t));
    if !reached {
        drop(bind);
        let why = (StatusCode::NOT_FOUND, "chat_thread_not_found");
        return Err(refuse(state, ctx, why, format!("there is no chat thread {id}")).await);
    }
    // The job results the thread holds now that the binding is registered
    // (MCP Tasks design §3.4): one that entered before is owed without being
    // said (the feed said it); one entering after is said by the session,
    // which reads the thread again once it listens for the wakes.
    let tasks = super::tasks::Owed::at_bind(state, id).await;
    Ok(Bind {
        binding: Binding {
            thread_id: id,
            title: thread.title.clone(),
            temporary: id < 0,
            // Its turns may change lmgw: the thread's flag, and a binder
            // whose own level is `full` (a device's, the pre-merge review's
            // P-3; the owner's always is) — the one rule every re-read
            // applies too (review G-14).
            admin_tools: bound::thread_ref(&snap, &thread, &caller).admin_tools,
            cfg,
            audio_input,
            warm,
            guard: Some(bind.guard),
            taken: bind.taken,
            taken_by: bind.taken_by,
            fence: bind.fence,
            tasks,
        },
        chat,
        asr,
        speech,
    })
}
