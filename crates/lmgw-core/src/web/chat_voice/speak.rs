//! Read-aloud of a stored reply, and stopping a thread's speech (chat-voice
//! design §6.3, §6.4).
//!
//! `POST /chat/api/threads/{id}/messages/{mid}/speak` reads the reply as it
//! is shown — its `content`, then a spoken reply's unheard rest — with the
//! thread's voice (`speech::plan`), and answers SSE: `state` while the TTS
//! is not resident, `voice` once its route is open, a `speech` frame per
//! clause, and `speech_done` — or `speech_error`, also for a thread that
//! cannot speak at all (`tts_not_configured`, `voice_not_found`,
//! `voice_not_configured`, `instructions_required`). It writes one TTS row
//! per speak, labelled as the thread's turns are. Aborting the fetch stops
//! the synthesis at its next await: the read-aloud sees its reader gone
//! (`speech::start`).
//!
//! Refused before any frame: `404` for a thread or a message that is not
//! there, `400 bad_request` for a message that is not a reply, `400
//! empty_message` for a reply with no text.
//!
//! `POST /chat/api/threads/{id}/speech/stop` stops every read-aloud of the
//! thread — a reply read as it streams, a stored one, from any window — and
//! nothing else: the text of a turn goes on (§6.4). It answers `{ok,
//! stopped}`, how many were still speaking; `404` for a thread that is not
//! there.

use std::convert::Infallible;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::chat_threads::SpeechStopped;
use tokio::sync::mpsc;

use crate::state::SharedState;

use super::super::chat::err_json;
use super::super::chat_caller::Caller;
use super::super::chat_extract::ChatPath;
use super::super::chat_repo::ChatRepo;
use super::speech::{self, Feed};

/// `POST /chat/api/threads/{id}/messages/{mid}/speak` (module doc).
pub(crate) async fn speak(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath((id, mid)): ChatPath<(i64, i64)>,
) -> Response {
    let started = Instant::now();
    let repo = ChatRepo::of(id);
    let thread = match repo.thread_as(&state, &caller, id).await {
        Ok(Some(t)) => t,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "not_found", "thread not found"),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    let msg = match repo.message(&state, id, mid).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return err_json(
                StatusCode::NOT_FOUND,
                "not_found",
                "message not found in this thread",
            )
        }
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    if msg.role != "assistant" {
        return err_json(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!("a '{}' message is not read aloud — only replies", msg.role),
        );
    }
    // As shown: the content, then what was never heard of a cut reply.
    let mut text = msg.content;
    if let Some(rest) = msg.voice.and_then(|v| v.unheard) {
        text.push_str(&rest);
    }
    if text.trim().is_empty() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "empty_message",
            "this reply has no text to read aloud",
        );
    }
    let (feed, fed) = mpsc::unbounded_channel();
    let _ = feed.send(Feed::Text(text));
    drop(feed);
    // The page going away drops the reader, and that stops the speech.
    let reader = speech::start(&state, &caller, repo, &thread, (started, None), false, fed);
    let events = futures::stream::unfold(reader, |mut reader| async move {
        let f = reader.recv().await?;
        Some((Ok::<_, Infallible>(f.into_sse()), reader))
    });
    Sse::new(caller.sse(&state, events))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// `POST /chat/api/threads/{id}/speech/stop` (module doc).
pub(crate) async fn stop_speech(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
) -> Response {
    match ChatRepo::of(id).thread_as(&state, &caller, id).await {
        Ok(Some(_)) => {}
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "not_found", "thread not found"),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
    let stopped = state.chat_live.stop_speech(id);
    Json(SpeechStopped {
        ok: true,
        stopped: stopped as u64,
    })
    .into_response()
}
