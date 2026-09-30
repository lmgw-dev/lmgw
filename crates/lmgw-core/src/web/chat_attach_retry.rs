//! Retrying a transcription that failed at upload (chat-complete design §8).
//!
//! An audio draft for a model that cannot hear it is transcribed when it is
//! uploaded. A transient failure then (GPU hold, the alias briefly down) must
//! not pin the draft: the send retries once before its gate, and
//! `POST /chat/api/attachments/{id}/transcribe` does the same on request.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use serde_json::{json, Value};

use crate::state::SharedState;
use crate::store::ChatAttachmentMeta;

use super::chat::err_json;
use super::chat_attach_gate::{native_audio, Caps};
use super::chat_attach_ingest::apply_transcript;
use super::chat_extract::ChatPath;
use super::chat_repo::ChatRepo;

/// What a retry found.
pub(super) enum Retried {
    NotFound,
    /// Already sent with a message: its transcript is part of what was sent.
    Sent,
    NotAudio,
    NoStt,
    /// Transcribed (or already was): the attachment's metadata now.
    Done(Value),
    /// The speech-to-text call failed again, with this message.
    Failed(String),
}

/// Transcribe draft `id` with the configured alias and store the result.
pub(super) async fn retry_transcript(state: &SharedState, id: i64) -> Result<Retried, String> {
    let repo = ChatRepo::of(id);
    let att = match repo.attachment(state, id).await {
        Ok(Some(a)) => a,
        Ok(None) => return Ok(Retried::NotFound),
        Err(e) => return Err(e.to_string()),
    };
    if att.message_id.is_some() {
        return Ok(Retried::Sent);
    }
    if att.kind != "audio" {
        return Ok(Retried::NotAudio);
    }
    if att.meta.get("transcript_alias").is_some() && att.extracted.is_some() {
        return Ok(Retried::Done(att.meta));
    }
    let stt = state.snapshot().settings.chat_stt_alias.clone();
    if stt.is_empty() {
        return Ok(Retried::NoStt);
    }
    let text = match crate::proxy::transcribe(
        state,
        &stt,
        Bytes::from(att.data.clone()),
        &att.name,
        &att.mime,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return Ok(Retried::Failed(e.to_string())),
    };
    let mut meta = if att.meta.is_object() {
        att.meta.clone()
    } else {
        json!({})
    };
    let mut extracted = None;
    apply_transcript(&mut meta, &stt, &text, &mut extracted);
    repo.set_extracted(state, id, &text, &meta)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Retried::Done(meta))
}

/// The send's second chance: every audio draft that carries a transcription
/// error, whose model cannot hear it, is transcribed once more (when a
/// speech-to-text alias is set). Success clears the error in `atts`; failure
/// puts the new error there, so the gate that follows names it.
pub(super) async fn retry_failed(state: &SharedState, atts: &mut [ChatAttachmentMeta], caps: Caps) {
    for a in atts.iter_mut() {
        let failed = a.kind == "audio"
            && a.message_id.is_none()
            && a.meta.get("transcript_error").is_some()
            && a.meta.get("transcript_alias").is_none()
            && native_audio(caps, &a.mime).is_none();
        if !failed {
            continue;
        }
        match retry_transcript(state, a.id).await {
            Ok(Retried::Done(meta)) => {
                a.extracted_tokens = crate::store::attachment_tokens(&meta);
                a.meta = meta;
            }
            Ok(Retried::Failed(e)) => a.meta["transcript_error"] = Value::String(e),
            Ok(_) => {}
            Err(e) => tracing::warn!(attachment = a.id, "chat: transcription retry: {e}"),
        }
    }
}

/// `POST /chat/api/attachments/{id}/transcribe` — transcribe an audio draft
/// again (after a failure at upload). `409 attachment_sent` for a sent one,
/// `422 not_audio`, `422 stt_not_set`, or `422 transcription_failed` with the
/// speech-to-text call's own message; `200 {ok, id, meta}` otherwise.
pub async fn transcribe(State(state): State<SharedState>, ChatPath(id): ChatPath<i64>) -> Response {
    let unprocessable =
        |code: &'static str, msg: String| err_json(StatusCode::UNPROCESSABLE_ENTITY, code, msg);
    match retry_transcript(&state, id).await {
        Ok(Retried::Done(meta)) => Json(json!({ "ok": true, "id": id, "meta": meta })).into_response(),
        Ok(Retried::NotFound) => err_json(StatusCode::NOT_FOUND, "not_found", "attachment not found"),
        Ok(Retried::Sent) => err_json(
            StatusCode::CONFLICT,
            "attachment_sent",
            "this attachment was already sent with a message; it can no longer be transcribed again",
        ),
        Ok(Retried::NotAudio) => unprocessable("not_audio", "only an audio file has a transcript".into()),
        Ok(Retried::NoStt) => unprocessable(
            "stt_not_set",
            "no speech-to-text model is set (Settings → Chat)".into(),
        ),
        Ok(Retried::Failed(e)) => unprocessable("transcription_failed", e),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e),
    }
}
