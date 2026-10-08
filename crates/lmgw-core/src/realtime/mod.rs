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
//! loop), `inbox` (the socket's reader), `conversation` (the items),
//! [`render`] (items → the chat request), `lifecycle` (one response at a
//! time), `responder` (a response's model call), `output` (a response's GA
//! event sequence), `writer` and `liveness` (pings, their round trip, and
//! the end of a session nobody reads), `thread` (a session bound to a Chat
//! thread, chat-voice design §8). Audio in:
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
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;

use crate::ingress::ClientProto;
use crate::policy::SlotHandover;
use crate::principal::Principal;
use crate::proxy::RequestCtx;
use crate::state::SharedState;

pub mod advertise;
pub mod asr;
pub mod audio;
mod audio_in;
pub mod clauses;
mod conversation;
pub(crate) mod expressive;
mod handshake;
pub mod heard;
pub mod ids;
mod inbox;
mod input;
mod lifecycle;
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

/// The handshake's query string. `call_id`, which the Python SDK sometimes
/// appends, belongs to the sideband surface and is ignored.
// `JsonSchema`-derived so the API docs describe the parameter from this
// type rather than restating it. Sideband: realtime design §19.
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
    /// With `chat_thread`: whether this bind may take the thread from a
    /// session already bound to it. Left out, it does — the other session
    /// closes with 4000, naming who took it. `never`: it does not — while
    /// another session is bound to the thread the upgrade is refused, 409
    /// chat_thread_bound naming who holds it ("voice is in use on device
    /// 'phone'"), and nothing is taken over. For a client's own automatic
    /// rebinds (after a conversation moved to a new thread), so it never
    /// takes the voice from another device that followed the same move; a
    /// user's explicit choice to talk here binds without it.
    #[serde(default)]
    takeover: Option<Takeover>,
}

/// `?takeover=`: whether a bind may take its thread from another session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Takeover {
    /// Take the thread over (what a bind without the parameter does).
    Always,
    /// Never: refuse while another session is bound to the thread.
    Never,
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
    if handshake::is_beta_client(&headers) {
        return handshake::http_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "beta_protocol_unsupported",
            "lmgw speaks the GA Realtime protocol, and `OpenAI-Beta: realtime=v1` marks a beta \
             client, whose events are not wire-compatible — use the SDK's GA client \
             (`client.realtime.connect()` in openai-python) and drop the header"
                .into(),
        );
    }
    // With **Require API key** off, an anonymous upgrade is legal — and CORS
    // is permissive, so any page the owner visits could open a voice session
    // against their models. A page announces itself with `Origin`; an SDK
    // client sends none and is unaffected (§10.1).
    if ctx.principal == Principal::Anonymous {
        if let Some(origin) = handshake::foreign_origin(&headers) {
            let e = crate::error::GatewayError::Refused {
                status: 403,
                code: "cross_origin_refused",
                message: format!(
                    "an anonymous realtime session may only be opened from this gateway's own \
                     origin, and this upgrade came from '{origin}' — present an API key (the \
                     openai-insecure-api-key subprotocol in a browser), or open it from the \
                     dashboard"
                ),
            };
            crate::proxy::record_middleware_refusal(&state, &ctx, &e, ClientProto::Realtime).await;
            return (e.http_status(), Json(e.to_openai_json())).into_response();
        }
    }
    let (requested, chat_thread, takeover) = match query {
        Ok(Query(q)) => (q.model, q.chat_thread, q.takeover),
        Err(why) => {
            return handshake::http_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_value",
                format!("the query string did not parse: {}", why.body_text()),
            )
        }
    };

    // A session bound to a chat thread starts bound (chat-voice §8.1): its
    // models are the thread's, and realtime's own resolution is skipped.
    if let Some(id) = chat_thread {
        let takeover = takeover != Some(Takeover::Never);
        let bind = match thread::handshake(&state, &ctx, id, requested.is_some(), takeover).await {
            Ok(b) => b,
            Err(refused) => return refused,
        };
        let init = session::SessionInit {
            running: Some(state.stops.running_at(state.stops.at_or_now(ctx.served_at))),
            limits: Limits::from_settings(&state.snapshot().settings.realtime),
            state,
            ctx,
            requested_model: None,
            chat: bind.chat,
            asr: bind.asr,
            speech: bind.speech,
            slot: slot.and_then(|Extension(h)| h.take()),
            bound: Some(bind.binding),
        };
        return open(ws, init);
    }

    if takeover.is_some() {
        return handshake::http_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_value",
            "takeover applies to a session bound to a chat thread: give chat_thread with it, \
             or leave it out"
                .into(),
        );
    }
    let snap = state.snapshot();
    let chat = match resolve::resolve_chat(&snap, requested.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            // The row names what the client asked for, so "which model did
            // it want" is answerable from Logs.
            crate::proxy::record_request_failure(
                &state,
                &ctx,
                ClientProto::Realtime.as_str(),
                requested.as_deref().unwrap_or_default(),
                None,
                None,
                std::time::Instant::now(),
                &e,
            )
            .await;
            return (e.http_status(), Json(e.to_openai_json())).into_response();
        }
    };
    // No transcription model is named at the handshake: the default chain,
    // which never refuses — a text session needs no ASR (§5.2).
    let asr = match asr::resolve_asr(&state, None).await {
        Ok(a) => a,
        Err(e) => return (e.http_status(), Json(e.to_openai_json())).into_response(),
    };
    // The TTS setting and the voice a session starts with: also never a
    // refusal — a text session never speaks (§5.3).
    let speech = match session::speech::initial(&state).await {
        Ok(s) => s,
        Err(e) => return (e.http_status(), Json(e.to_openai_json())).into_response(),
    };
    if let Err(e) = policy::check_session(&state, &ctx, &chat, &asr, &speech.tts).await {
        return (e.http_status(), Json(e.to_openai_json())).into_response();
    }

    // Everything passed: the session takes the key's slot from the gate and
    // holds it until it ends (§10.3, `SlotHandover` says why the gate cannot).
    let slot = slot.and_then(|Extension(h)| h.take());
    let limits = Limits::from_settings(&snap.settings.realtime);
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
        limits,
        bound: None,
    };
    open(ws, init)
}

/// The 101, and the session on the upgraded socket.
fn open(ws: WebSocketUpgrade, init: session::SessionInit) -> Response {
    let limits = init.limits;
    ws.protocols([REALTIME_PROTOCOL])
        // Explicit, from the settings, so the ceilings are visible ones
        // rather than tungstenite's built-in defaults (§10.4).
        .max_message_size(limits.max_message)
        .max_frame_size(limits.max_frame)
        .on_failed_upgrade(|e| tracing::info!("realtime: the WebSocket upgrade failed: {e}"))
        .on_upgrade(move |socket| session::run(socket, init))
}
