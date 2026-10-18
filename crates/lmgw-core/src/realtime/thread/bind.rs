//! The binding (chat-voice design §8.1, client-apps design §1.3, realtime
//! calls design §2.8): `?chat_thread=` or `?chat_folder=`, checked
//! (`check`) and then bound, so a bound session starts bound. The
//! WebSocket upgrade runs both back to back before its 101; a call checks
//! at its POST and checks and binds when its channel opens.
//!
//! Refusals, in the handshake's OpenAI error shape (`{"error": {message,
//! type, param, code}}`, realtime §10.2; review W6-5 corrected "flat"):
//!
//! | Status | Code | Why |
//! |---|---|---|
//! | 403 | `chat_thread_not_allowed` | the principal does not hold `Cap::Chat`, the capability of every `/chat/api` route (the dashboard's cookie, an owner key, a paired device) |
//! | 400 | `owned_by_thread` | `?model=` beside it: the thread owns the chat model |
//! | 404 | `chat_thread_not_found` | no such thread — or, for a device, an Admin Chat thread (client-apps L3) |
//! | 404 | `chat_folder_not_found` | `?chat_folder=` names a folder the caller cannot see (`current` says `not_found`) |
//! | 409 | `not_ongoing`, `folder_no_model` | the folder is no ongoing conversation, or its defaults name no model |
//! | 409 | `chat_thread_admin` | an Admin Chat thread the owner binds (ruling 7) |
//! | 409 | `chat_thread_bound` | `takeover=never`, and another session is bound to the thread: "voice is in use on device 'phone'" (2026-10-07), the holder also as `error.held_by` (calls §2.8) |
//! | 401, 403, 429 | the key's own | a device's key may not use the thread's chat, ASR or TTS alias: realtime §10.2's check |
//! | 500 | `internal` | the store failed to read the thread again once the binding registered, or to read or roll over the folder: lmgw's own failure, never "there is no chat thread" (the branch review's N-4) |
//!
//! The capability is checked first, so a key that may not bind learns
//! nothing about the threads. A plain thread with the self-admin toolset
//! attached by hand binds: attaching it is the owner's choice (§8.1, user
//! intent wins); its thread JSON flags it (`voice_resolved.realtime.
//! admin_tools`).
//!
//! **A folder's current thread** (calls §2.2) is decided by `current`'s
//! rules and bound in one step, under the folder's lock: the check decides,
//! holding the lock; the bind makes a rollover's thread (recording
//! `folder.current` on the feed with its reason, as `current` does) and
//! registers the binding before it lets go. No other client's `current`
//! moves the folder between the decision and the bind.
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
//! **Pending** (calls §2.8, `takeover=ask`, calls only): a thread held when
//! the bind runs is no refusal but [`Outcome::Pending`], naming the holder,
//! with the thread's release signal. The call says `bind_pending`, and binds
//! by [`check_and_bind`] again: with `takeover=always` on the client's
//! `take_over`, with `ask` once the signal wakes.
//!
//! **The owed set at bind** (MCP Tasks design §3.4): the thread's job
//! results no reply answered yet are read once the binding is registered,
//! so a client that binds to speak a result it learned of from the feed
//! (ruling 22, `takeover=never`) has its `response.create` run the
//! continuation.

use axum::http::StatusCode;

use super::super::asr::AsrResolution;
use super::super::resolve::ChatResolution;
use super::super::session::speech::Speech;
use super::super::warm::Warm;
use super::super::Takeover;
use super::check::{self, Checked, Target};
use super::refusal::{refuse, Refused};
use crate::proxy::{RequestCtx, StopSignal};
use crate::state::SharedState;
use crate::store::ChatThread;
use crate::web::chat_live::{TakenBy, VoiceBinding, VoiceRelease};
use crate::web::chat_voice::bound::{self, Caller, VoiceConfig};
use crate::web::folder_pick::Picked;

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

/// What a bind came to.
pub(crate) enum Outcome {
    /// Bound: the session starts.
    Bound(Box<Bind>),
    /// `takeover=ask`, and the thread is held (module doc).
    Pending(Pending),
}

/// A bind under `takeover=ask` that found its thread held (calls §2.8): no
/// session runs until it binds.
pub(crate) struct Pending {
    target: Target,
    held_by: String,
    release: VoiceRelease,
}

impl Pending {
    /// Who holds the thread, as `voice.bound.by` names it: what
    /// `lmgw.session.bind_pending {held_by}` says.
    pub(crate) fn held_by(&self) -> &str {
        &self.held_by
    }

    /// What the call asked to bind: [`check_and_bind`] it again — with
    /// [`Takeover::Always`] on the client's `take_over` (for a folder, its
    /// current thread decided again under its lock), with [`Takeover::Ask`]
    /// once [`Self::released`] resolves.
    pub(crate) fn target(&self) -> Target {
        self.target
    }

    /// The thread it waits for.
    pub(crate) fn thread_id(&self) -> i64 {
        self.release.thread_id()
    }

    /// Resolves once the thread's binding changed: its holder started to end
    /// or went (the thread may be free), or another client took it (a new
    /// holder, for another `bind_pending`). Bind again then.
    pub(crate) async fn released(&mut self) {
        self.release.changed().await;
    }
}

/// [`check::check`] and [`bind`], back to back: what a call runs when its
/// channel opens, and again while it is pending (module doc). `model` is
/// not asked again: the POST refused it.
pub(crate) async fn check_and_bind(
    state: &SharedState,
    ctx: &RequestCtx,
    target: Target,
    takeover: Takeover,
) -> Result<Outcome, Refused> {
    let checked = check::check(state, ctx, target, false, takeover).await?;
    bind(state, ctx, checked, takeover).await
}

/// Bind what `checked` checked, for the principal of `ctx` (module doc):
/// `Always` takes the thread from a session bound to it, `Never` is refused
/// `409 chat_thread_bound` instead, `Ask` comes back
/// [`Outcome::Pending`].
pub(crate) async fn bind(
    state: &SharedState,
    ctx: &RequestCtx,
    checked: Checked,
    takeover: Takeover,
) -> Result<Outcome, Refused> {
    let Checked {
        target,
        caller,
        thread,
        resolution,
        folder,
    } = checked;
    // A folder's rollover makes its thread now, under the folder's lock.
    let (thread, resolution, folder) = match folder {
        Some(picked) => {
            let made = rolled_over(state, ctx, &caller, picked, thread, resolution, takeover);
            let (picked, thread, resolution) = made.await?;
            (thread, resolution, Some(picked))
        }
        None => (thread, resolution, None),
    };
    let id = thread.id;
    // Last: every refusal has had its say, and the session it takes over is
    // told only now — by whom (§1.7). Nothing waits between a folder's
    // decision acted on (its kept thread read current again, or its new
    // thread made) and the registration.
    let named = caller.named();
    let binder = crate::web::chat_live::Binder {
        by: &named,
        device: caller.is_device(),
        level: thread.reach_level(),
        key_id: ctx.principal.key_id(),
    };
    let bind = match takeover {
        Takeover::Always => state.chat_live.bind_voice_as(id, binder),
        Takeover::Never => match state.chat_live.bind_voice_unless_bound(id, binder) {
            Ok(bind) => bind,
            Err(holder) => return Err(check::held(state, ctx, &caller, id, holder).await),
        },
        Takeover::Ask => match state.chat_live.bind_voice_or_wait(id, binder) {
            Ok(bind) => bind,
            Err(waiting) => {
                drop(folder);
                // A thread that left the binder's reach is not there, rather
                // than in use (review F-15, as for `never`).
                let reached = bound::thread(state, id)
                    .await
                    .is_some_and(|t| caller.sees(&state.snapshot(), &t));
                if !reached {
                    return Err(check::not_found(state, ctx, id).await);
                }
                return Ok(Outcome::Pending(Pending {
                    target,
                    held_by: waiting.holder,
                    release: waiting.release,
                }));
            }
        },
    };
    // Bound: the folder's decision has been acted on, and its lock goes.
    drop(folder);
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
        return Err(check::not_found(state, ctx, id).await);
    }
    // The job results the thread holds now that the binding is registered
    // (MCP Tasks design §3.4): one that entered before is owed without being
    // said (the feed said it); one entering after is said by the session,
    // which reads the thread again once it listens for the wakes.
    let tasks = super::tasks::Owed::at_bind(state, id).await;
    let warm = bound::connect_stages(state, &thread).await;
    let audio_input = bound::audio_input(state, &thread).await;
    let snap = state.snapshot();
    let check::Resolution {
        cfg,
        chat,
        asr,
        speech,
    } = resolution;
    Ok(Outcome::Bound(Box::new(Bind {
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
    })))
}

/// A folder's decision acted on, under its lock (module doc): a kept
/// thread read current again; a rollover's thread made, `folder.current`
/// recorded. When the folder's current thread moved since the decision (a
/// delete, a move or an archive, which take no lock) — a kept thread that is
/// no longer current, or a pointer the rollover's write did not find — the
/// folder is decided again and the thread it picks checked again, as
/// `current` decides again.
async fn rolled_over(
    state: &SharedState,
    ctx: &RequestCtx,
    caller: &Caller,
    mut picked: Picked,
    mut thread: ChatThread,
    mut resolution: check::Resolution,
    takeover: Takeover,
) -> Result<(Picked, ChatThread, check::Resolution), Refused> {
    let folder_id = picked.folder().id;
    loop {
        match picked.create(state, caller).await {
            Ok(Some(id)) if id == thread.id => return Ok((picked, thread, resolution)),
            Ok(Some(id)) => {
                let Some(made) = bound::thread(state, id).await else {
                    let why = (StatusCode::INTERNAL_SERVER_ERROR, "internal");
                    let message = format!(
                        "chat thread {id}, which folder {folder_id} just started, could not be read"
                    );
                    tracing::warn!("realtime: binding {message}");
                    return Err(refuse(state, ctx, why, message).await);
                };
                return Ok((picked, made, resolution));
            }
            Ok(None) => {
                picked = match picked.again(state, caller).await {
                    Ok(p) => p,
                    Err(r) => return Err(check::folder_refused(state, ctx, folder_id, r).await),
                };
                thread = check::picked_thread(&picked);
                resolution = check::check_thread(state, ctx, caller, &thread, takeover).await?;
            }
            Err(r) => return Err(check::folder_refused(state, ctx, folder_id, r).await),
        }
    }
}

#[cfg(test)]
mod tests;
