//! `GET /v1/realtime`: OpenAI's GA Realtime protocol over a WebSocket,
//! answered by a cascade of aliases lmgw already serves (realtime design).
//!
//! This module is the route and the handshake. Everything that can fail
//! cheaply fails **before the 101**, as an ordinary HTTP error the client's
//! SDK surfaces (§10.2): the upgrade itself (426), a beta client (400), an
//! anonymous page from another origin (403), an unknown model (404), and the
//! key's scope and budget for the session's aliases. Only then is the
//! connection upgraded and handed to [`session::run`].
//!
//! Children: [`protocol`] (the GA event types), [`merge`] (the session object
//! and `session.update`), [`resolve`] (which alias answers), [`ids`],
//! `handshake` (header rules and limits), `policy`, `session` (the core's
//! loop), `link` (the transport seam: the link's reader, its sending half
//! and its closes, and the WebSocket behind them), `conversation` (the items),
//! [`render`] (items → the chat request), `lifecycle` (one response at a
//! time), `responder` (a response's model call), `output` (a response's GA
//! event sequence), `writer` and `liveness` (pings, their round trip, and
//! the end of a session nobody reads), `thread` (a session bound to a Chat
//! thread, chat-voice design §8), `calls` (WebRTC calls: the media plane and
//! a call's Opus in and out, calls design §3 to §5). Audio in:
//! [`asr`] (which ASR alias transcribes), `audio_in` (the input buffer and
//! its turn detector), `input` (the buffer's events in the core),
//! `transcribe` (an ASR call per committed turn), `scorer` (Smart Turn on
//! the blocking pool, §6.3) and `warm` (background starts, §9.1). Audio out: `tts` (which TTS alias speaks), `voice` (with
//! which voice), the responder's speech (clauses → TTS) and the writer's
//! paced queue.
//!
//! The pure compute the session calls — no tokio, no I/O — sits beside them:
//! [`audio`] (PCM and WAV, resampling, Silero), [`turn`] (`server_vad`, the
//! barge-in gate, Smart Turn and `semantic_vad`'s rule), [`clauses`] (the chat stream cut for TTS), [`heard`]
//! (what the listener heard) and [`pacing`] (the paced output clock).

use axum::extract::rejection::QueryRejection;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Extension;
use serde::Deserialize;

use crate::policy::SlotHandover;
use crate::proxy::RequestCtx;
use crate::state::SharedState;

pub mod advertise;
pub mod asr;
pub mod audio;
mod audio_in;
pub mod calls;
pub mod clauses;
mod conversation;
pub(crate) mod expressive;
mod handshake;
pub mod heard;
pub mod ids;
mod input;
mod lifecycle;
mod link;
mod liveness;
mod mcp_tools;
pub mod merge;
mod output;
pub mod pacing;
mod policy;
pub mod protocol;
pub mod render;
pub mod resolve;
pub(crate) mod responder;
mod scorer;
mod session;
#[cfg(test)]
mod test_fixtures;
mod thread;
mod transcribe;
mod tts;
pub mod turn;
pub(crate) mod voice;
pub(crate) mod warm;
mod writer;

pub use handshake::{subprotocol_key, Limits, REALTIME_PROTOCOL};
pub use scorer::ScoreHook;
#[doc(hidden)]
pub use thread::turn::audio::{heard_response_for_tests, HeardForTests};
pub use tts::is_tts_alias;

/// The route's full path — what `principal_mw` scopes the subprotocol
/// credential to (§10.1).
pub const PATH: &str = "/v1/realtime";

pub use calls::route::{is_hangup, PATH as CALLS_PATH};

/// The handshake's query string.
// `JsonSchema`-derived so the API docs describe the parameter from this
// type rather than restating it. Shared with the WebRTC calls' POST
// (realtime calls design §2.2), which reads the same parameters with the
// same rules ([`RealtimeQuery::binding`], [`RealtimeQuery::sideband`]).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub(crate) struct RealtimeQuery {
    /// The chat model: a `realtime.model_map` name, an OpenAI Realtime name
    /// answered by `realtime.default_model`, or a chat alias — in that
    /// order. Optional — a session may name its model in its first
    /// `session.update` instead.
    #[serde(default)]
    model: Option<String>,
    /// A voice mode bound to a Chat thread: bind the session to this thread
    /// (negative for a temporary one). The thread then owns the chat model,
    /// prompt, tools, speech models, voice and conversation, and the session
    /// sends lmgw's `lmgw.*` extension events. Needs the chat capability
    /// (the dashboard's cookie, an owner key or a paired device's key),
    /// refused beside `model`, and refused for an Admin Chat thread (a
    /// device is told there is no such thread). A device binds as its key:
    /// the thread's aliases pass its scope and budget first.
    #[serde(default)]
    chat_thread: Option<i64>,
    /// A voice mode bound to an ongoing folder's current thread: the bind
    /// decides the thread by the rules of `POST
    /// /chat/api/folders/{id}/current` and binds it in one step, under the
    /// folder's lock — the current thread, or a new one from the folder's
    /// defaults when there is none, when it is out of the caller's reach,
    /// when it idled past the folder's idle minutes, or with `new=true`
    /// (unless it has no message yet). A new thread is recorded as
    /// `folder.current` on the Chat feed with its reason. The thread bound
    /// is reported in `session.lmgw.resolved.chat_thread`, and is then as
    /// with `chat_thread`. Refused beside `chat_thread` (400 invalid_value)
    /// and beside `model` (400 owned_by_thread); 404 chat_folder_not_found
    /// for a folder the caller cannot see, 409 not_ongoing for one that is
    /// no ongoing conversation, 409 folder_no_model for one whose defaults
    /// name no model, then the thread's own refusals.
    #[serde(default)]
    chat_folder: Option<i64>,
    /// With `chat_folder`: start a new conversation — a new thread unless
    /// the folder's current one has no message yet. Refused without
    /// `chat_folder` (400 invalid_value).
    #[serde(default)]
    new: Option<bool>,
    /// With `chat_thread` or `chat_folder`: whether this bind may take the
    /// thread from a session already bound to it. Left out, it does — the
    /// other session closes (a WebSocket with 4000), naming who took it.
    /// `never`: it does not — while another session is bound to the thread
    /// the request is refused, 409 chat_thread_bound naming who holds it
    /// ("voice is in use on device 'phone'", and as `error.held_by`), and
    /// nothing is taken over; a WebRTC call whose thread is taken between
    /// its POST and its channel's open ends with lmgw.session.closed
    /// {kind: thread_bound}. For a client's own automatic rebinds (after a
    /// conversation moved to a new thread), so it never takes the voice
    /// from another device that followed the same move; a user's explicit
    /// choice to talk binds without it. `ask` is a WebRTC call's own (POST
    /// /v1/realtime/calls): a thread held when the call's channel opens is
    /// asked about, lmgw.session.bind_pending; on GET /v1/realtime it is
    /// 400 invalid_value. Without `chat_thread` or `chat_folder`, 400
    /// invalid_value.
    #[serde(default)]
    takeover: Option<Takeover>,
    /// OpenAI's sideband connection: a server-side WebSocket attached to a
    /// WebRTC call's session. lmgw serves none, so it is refused, 400
    /// sideband_unsupported, rather than opening an unrelated session.
    #[serde(default)]
    call_id: Option<String>,
}

/// `?takeover=`: whether a bind may take its thread from another session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Takeover {
    /// Take the thread over (what a bind without the parameter does).
    Always,
    /// Never: refuse while another session is bound to the thread.
    Never,
    /// Ask first (WebRTC calls only, realtime calls design §2.8): a thread
    /// held when the call's channel opens makes the call wait,
    /// `lmgw.session.bind_pending`, for the client's
    /// `lmgw.session.take_over` or the holder's end.
    Ask,
}

impl RealtimeQuery {
    /// `?call_id=` refused (realtime calls design §2.7): with calls served,
    /// a sideband attach that silently opened an unrelated session would be
    /// a trap.
    pub(crate) fn sideband(&self) -> Result<(), Response> {
        match &self.call_id {
            None => Ok(()),
            Some(_) => Err(handshake::http_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "sideband_unsupported",
                "lmgw serves no sideband connection to a call: call_id attaches a server-side \
                 WebSocket to a WebRTC call's session in OpenAI's API, which lmgw does not \
                 serve — leave call_id out to open a session of its own"
                    .into(),
            )),
        }
    }

    /// What the query binds, and how: `None` for an unbound session. The
    /// parameter rules (realtime calls design §2.5), each 400
    /// `invalid_value`: `chat_folder` beside `chat_thread`, `new` without
    /// `chat_folder`, `takeover` with neither. `model` beside a target is
    /// the bind's own check (`owned_by_thread`, after the capability).
    pub(crate) fn binding(&self) -> Result<Option<(thread::Target, Takeover)>, thread::Refused> {
        let invalid = |message: &str| {
            thread::Refused::new(StatusCode::BAD_REQUEST, "invalid_value", message.into())
        };
        let target = match (self.chat_thread, self.chat_folder) {
            (Some(_), Some(_)) => {
                return Err(invalid(
                    "chat_thread and chat_folder cannot both be given: bind a thread, or a \
                     folder's current thread",
                ))
            }
            (Some(id), None) => Some(thread::Target::Thread(id)),
            (None, Some(id)) => Some(thread::Target::Folder {
                id,
                new: self.new.unwrap_or(false),
            }),
            (None, None) => None,
        };
        if self.new.is_some() && self.chat_folder.is_none() {
            return Err(invalid(
                "new applies to a session bound to a folder's current thread: give chat_folder \
                 with it, or leave it out",
            ));
        }
        match (target, self.takeover) {
            (None, Some(_)) => Err(invalid(
                "takeover applies to a session bound to a chat thread: give chat_thread or \
                 chat_folder with it, or leave it out",
            )),
            (None, None) => Ok(None),
            (Some(t), takeover) => Ok(Some((t, takeover.unwrap_or(Takeover::Always)))),
        }
    }
}

/// `GET /v1/realtime` — the handshake.
pub(crate) async fn upgrade(
    State(state): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    slot: Option<Extension<SlotHandover>>,
    query: Result<Query<RealtimeQuery>, QueryRejection>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    // axum's own rejection is a 400 that does not say what is missing (§10.2).
    let ws = match ws {
        Ok(ws) => ws,
        Err(why) => return handshake::upgrade_required(&why.body_text()),
    };
    let door = handshake::Door::Upgrade;
    if let Some(refused) = handshake::door(&state, &ctx, &headers, door).await {
        return refused;
    }
    let query = match handshake::query(query) {
        Ok(q) => q,
        Err(refused) => return refused,
    };
    let binding = match query.binding() {
        Ok(b) => b,
        Err(refused) => return refused.into_response(),
    };
    let requested = query.model;

    // A session bound to a chat thread starts bound (chat-voice §8.1): its
    // models are the thread's, and realtime's own resolution is skipped.
    // The checks and the bind run back to back, before the 101 (realtime
    // calls design §2.8).
    if let Some((target, takeover)) = binding {
        if takeover == Takeover::Ask {
            return handshake::http_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_value",
                "takeover=ask applies to WebRTC calls: a WebSocket session binds before its \
                 upgrade, with no channel to ask on — leave takeover out to take the thread \
                 over, or send never"
                    .into(),
            );
        }
        let checked = thread::check(&state, &ctx, target, requested.is_some(), takeover);
        let checked = match checked.await {
            Ok(c) => c,
            Err(refused) => return refused.into_response(),
        };
        let bind = match thread::bind(&state, &ctx, checked, takeover).await {
            Ok(thread::Outcome::Bound(b)) => *b,
            // Only `ask` waits, and it is refused above.
            Ok(thread::Outcome::Pending(p)) => {
                tracing::error!(
                    "realtime: a WebSocket bind of chat thread {} came back pending",
                    p.thread_id()
                );
                return thread::Refused::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "the bind came back pending on a WebSocket".into(),
                )
                .into_response();
            }
            Err(refused) => return refused.into_response(),
        };
        let limits = Limits::from_settings(&state.snapshot().settings.realtime);
        let init = session::SessionInit {
            running: Some(state.stops.running_at(state.stops.at_or_now(ctx.served_at))),
            transport: link::Transport::WebSocket(limits),
            state,
            ctx,
            requested_model: None,
            chat: bind.chat,
            asr: bind.asr,
            speech: bind.speech,
            slot: slot.and_then(|Extension(h)| h.take()),
            bound: Some(bind.binding),
            call: None,
        };
        return open(ws, limits, init);
    }

    let handshake::Unbound { chat, asr, speech } =
        match handshake::unbound(&state, &ctx, requested.as_deref()).await {
            Ok(u) => u,
            Err(refused) => return refused,
        };

    // Everything passed: the session takes the key's slot from the gate and
    // holds it until it ends (§10.3, `SlotHandover` says why the gate cannot).
    let slot = slot.and_then(|Extension(h)| h.take());
    let limits = Limits::from_settings(&state.snapshot().settings.realtime);
    let init = session::SessionInit {
        // Before the 101 (`SessionInit::running`).
        running: Some(state.stops.running_at(state.stops.at_or_now(ctx.served_at))),
        state: state.clone(),
        ctx,
        requested_model: requested,
        chat,
        asr,
        speech,
        slot,
        transport: link::Transport::WebSocket(limits),
        bound: None,
        call: None,
    };
    open(ws, limits, init)
}

/// The 101, bounded by `limits` (the session's transport's), and the
/// session on the upgraded socket.
fn open(ws: WebSocketUpgrade, limits: Limits, init: session::SessionInit) -> Response {
    ws.protocols([REALTIME_PROTOCOL])
        // Explicit, from the settings, so the ceilings are visible ones
        // rather than tungstenite's built-in defaults (§10.4).
        .max_message_size(limits.max_message)
        .max_frame_size(limits.max_frame)
        .on_failed_upgrade(|e| tracing::info!("realtime: the WebSocket upgrade failed: {e}"))
        .on_upgrade(move |socket| {
            let (sink, reader) = link::ws::split(socket, &limits);
            session::run(sink, reader, init)
        })
}
