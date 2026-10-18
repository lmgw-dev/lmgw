//! The bind's checks (realtime calls design §2.8): everything a bind to a
//! Chat thread, or to a folder's current thread, may be refused for — run
//! without binding anything.
//!
//! The WebSocket upgrade runs them right before the bind (`bind`), back to
//! back, before its 101. A call runs them at its POST, mutating nothing,
//! and again when its channel opens, right before the bind: seconds have
//! passed, and reach, scope or budget may have moved.
//!
//! In order: the chat capability; `model` beside the target; the target —
//! the thread's reach for the caller, or for `?chat_folder=` the folder's
//! decision by `current`'s rules under the folder's lock
//! (`web::folder_pick`), which [`Checked`] holds until the bind; then the
//! thread's own checks, on the thread the decision picked (for a rollover the
//! thread the folder's defaults make, before it is created): no Admin Chat,
//! its voice configuration, a device's scope and budget over its aliases;
//! last, under `takeover=never`, another session holding it (the 409).

use axum::http::StatusCode;

use super::super::asr::{AsrResolution, AsrVia};
use super::super::resolve::{ChatResolution, Via};
use super::super::session::speech::{self, Speech};
use super::super::Takeover;
use super::refusal::{refuse, Refused};
use crate::principal::Cap;
use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::store::ChatThread;
use crate::web::chat_voice::bound::{self, Caller, VoiceConfig};
use crate::web::folder_pick::{self, Pick, Picked};

/// What a bind binds: `?chat_thread=` or `?chat_folder=` with `new`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    /// This thread (negative: a temporary one).
    Thread(i64),
    /// This ongoing folder's current thread, decided at the bind; `new`: a
    /// new conversation is asked for.
    Folder { id: i64, new: bool },
}

impl Target {
    /// The query parameter that named it.
    pub(crate) fn param(self) -> &'static str {
        match self {
            Self::Thread(_) => "chat_thread",
            Self::Folder { .. } => "chat_folder",
        }
    }
}

/// What the thread's voice resolves to, as the session starts with it.
pub(super) struct Resolution {
    pub cfg: VoiceConfig,
    pub chat: ChatResolution,
    pub asr: AsrResolution,
    pub speech: Speech,
}

/// A bind that passed its checks (module doc): what `bind` binds.
pub(crate) struct Checked {
    pub(super) target: Target,
    pub(super) caller: Caller,
    /// The thread the bind takes: stored, or for a folder's rollover the
    /// one its defaults make, not created yet (id 0).
    pub(super) thread: ChatThread,
    pub(super) resolution: Resolution,
    /// A folder's decision, holding the folder's lock until the bind.
    pub(super) folder: Option<Picked>,
}

/// The checks for `target`, for the principal of `ctx` (module doc).
/// `model`: a `?model=` was given beside it. Nothing is bound and nothing
/// is made; a folder's lock is held by the answer until it drops.
pub(crate) async fn check(
    state: &SharedState,
    ctx: &RequestCtx,
    target: Target,
    model: bool,
    takeover: Takeover,
) -> Result<Checked, Refused> {
    let snap = state.snapshot();
    let param = target.param();
    if !ctx.principal.holds(Cap::Chat, &snap) {
        let message = format!(
            "binding a session to a chat thread ({param}) needs the chat capability — the \
             dashboard, an owner key or a paired device — and the request presented {}",
            ctx.principal.describe()
        );
        let why = (StatusCode::FORBIDDEN, "chat_thread_not_allowed");
        return Err(refuse(state, ctx, why, message).await);
    }
    let caller = Caller::of(ctx);
    if model {
        let message = match target {
            Target::Thread(_) => {
                "model and chat_thread cannot both be given: a session bound to a chat thread \
                 answers with the thread's own model — change it in the thread's settings"
            }
            Target::Folder { .. } => {
                "model and chat_folder cannot both be given: a session bound to a folder's \
                 current thread answers with the thread's own model — change it in the \
                 folder's defaults"
            }
        };
        let why = (StatusCode::BAD_REQUEST, "owned_by_thread");
        return Err(refuse(state, ctx, why, message.to_string()).await);
    }
    let (thread, folder) = match target {
        Target::Thread(id) => (reached(state, ctx, &caller, id).await?, None),
        Target::Folder { id, new } => {
            let picked = match folder_pick::pick(state, &caller, id, new).await {
                Ok(p) => p,
                Err(r) => return Err(folder_refused(state, ctx, id, r).await),
            };
            (picked_thread(&picked), Some(picked))
        }
    };
    let resolution = check_thread(state, ctx, &caller, &thread, takeover).await?;
    Ok(Checked {
        target,
        caller,
        thread,
        resolution,
        folder,
    })
}

/// Thread `id` as the caller reaches it: an Admin Chat thread does not
/// exist for a device (client-apps L3), nor one with the self-admin toolset
/// unless the device may use lmgw's admin tools — the 404 of a thread that
/// is not there.
async fn reached(
    state: &SharedState,
    ctx: &RequestCtx,
    caller: &Caller,
    id: i64,
) -> Result<ChatThread, Refused> {
    match bound::thread(state, id)
        .await
        .filter(|t| caller.sees(&state.snapshot(), t))
    {
        Some(t) => Ok(t),
        None => Err(not_found(state, ctx, id).await),
    }
}

/// The 404 of a thread that is not there for the caller.
pub(super) async fn not_found(state: &SharedState, ctx: &RequestCtx, id: i64) -> Refused {
    let why = (StatusCode::NOT_FOUND, "chat_thread_not_found");
    refuse(state, ctx, why, format!("there is no chat thread {id}")).await
}

/// The thread a folder's decision picked: its current thread, or the one
/// its defaults make.
pub(super) fn picked_thread(picked: &Picked) -> ChatThread {
    match picked.pick() {
        Pick::Keep(t) => (**t).clone(),
        Pick::New { thread, .. } => (**thread).clone(),
    }
}

/// A folder's refusal in the handshake's shape (§2.2): `current`'s, with
/// the folder's 404 naming what was not found.
pub(super) async fn folder_refused(
    state: &SharedState,
    ctx: &RequestCtx,
    id: i64,
    r: folder_pick::Refusal,
) -> Refused {
    let (why, message) = match r {
        folder_pick::Refusal::NotFound => (
            (StatusCode::NOT_FOUND, "chat_folder_not_found"),
            format!("there is no chat folder {id}"),
        ),
        folder_pick::Refusal::NotOngoing(m) => ((StatusCode::CONFLICT, "not_ongoing"), m),
        folder_pick::Refusal::NoModel(m) => ((StatusCode::CONFLICT, "folder_no_model"), m),
        folder_pick::Refusal::Internal(e) => {
            let message = format!("chat folder {id} could not be read: {e}");
            tracing::warn!("realtime: binding {message}");
            ((StatusCode::INTERNAL_SERVER_ERROR, "internal"), message)
        }
    };
    refuse(state, ctx, why, message).await
}

/// The thread's own checks (module doc), on `thread` as found or as a
/// folder's defaults make it.
pub(super) async fn check_thread(
    state: &SharedState,
    ctx: &RequestCtx,
    caller: &Caller,
    thread: &ChatThread,
    takeover: Takeover,
) -> Result<Resolution, Refused> {
    let snap = state.snapshot();
    if bound::is_admin(thread) {
        let message = "voice mode is not available in Admin Chat: its replies may change lmgw, \
                       and a spoken turn runs without the step of reading it before it is sent"
            .to_string();
        let why = (StatusCode::CONFLICT, "chat_thread_admin");
        return Err(refuse(state, ctx, why, message).await);
    }
    let cfg = bound::voice(&snap, thread);
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
    // A device binds as its key (`bind`'s module doc): realtime §10.2's
    // check for the thread's chat, ASR and TTS aliases, each refusal's row
    // written by the check itself.
    if caller.is_device() {
        let checked = super::super::policy::check_session(state, ctx, &chat, &asr, &speech.tts);
        if let Err(e) = checked.await {
            return Err(Refused::gateway(&e));
        }
    }
    // `takeover=never`: another session holding the thread is the 409 now
    // (a call's POST binds nothing, §2.8); a thread a rollover makes is
    // held by nobody.
    if takeover == Takeover::Never && thread.id != 0 {
        if let Some(holder) = state
            .chat_live
            .voice_held_against(thread.id, ctx.principal.key_id())
        {
            return Err(held(state, ctx, caller, thread.id, holder).await);
        }
    }
    Ok(Resolution {
        cfg,
        chat,
        asr,
        speech,
    })
}

/// Thread `id` is held by `holder` against a bind that may not take it: the
/// 409 naming the holder (`error.held_by`, §2.8), or — for a thread that
/// left the binder's reach since it was read (review F-15) — the 404 of a
/// thread that is not there, so a device is not told who uses a thread it
/// may no longer see.
pub(super) async fn held(
    state: &SharedState,
    ctx: &RequestCtx,
    caller: &Caller,
    id: i64,
    holder: String,
) -> Refused {
    let reached = bound::thread(state, id)
        .await
        .is_some_and(|t| caller.sees(&state.snapshot(), &t));
    if !reached {
        return not_found(state, ctx, id).await;
    }
    let why = (StatusCode::CONFLICT, "chat_thread_bound");
    let message = format!(
        "voice is in use on {holder}: this bind asked not to take it over (takeover=never)"
    );
    let mut refused = refuse(state, ctx, why, message).await;
    refused.held_by = Some(holder);
    refused
}
