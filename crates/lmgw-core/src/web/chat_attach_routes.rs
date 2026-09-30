//! Chat attachment routes added with the new kinds (chat-complete design §8):
//! a text-class PDF's Text | Pages choice, and the extracted text for the
//! viewer.

use axum::extract::State;
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::state::SharedState;
use crate::store::SetModeOutcome;

use super::chat::err_json;
use super::chat_extract::{ChatJson, ChatPath};
use super::chat_repo::ChatRepo;

#[derive(Deserialize)]
pub struct ModeReq {
    mode: String,
}

/// `POST /chat/api/attachments/{id}/mode {mode: "text" | "images"}` — how a
/// PDF whose every page has text goes to the model. Drafts only (`409
/// attachment_sent` once sent) and only for text-class PDFs (`422
/// mode_not_applicable`: scanned and hybrid ones are automatic).
pub async fn set_mode(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<ModeReq>,
) -> Response {
    if !matches!(req.mode.as_str(), "text" | "images") {
        return err_json(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "mode must be \"text\" or \"images\"",
        );
    }
    match ChatRepo::of(id).set_mode(&state, id, &req.mode).await {
        Ok(SetModeOutcome::Set) => {
            Json(json!({ "ok": true, "id": id, "mode": req.mode })).into_response()
        }
        Ok(SetModeOutcome::NotFound) => {
            err_json(StatusCode::NOT_FOUND, "not_found", "attachment not found")
        }
        Ok(SetModeOutcome::Sent) => err_json(
            StatusCode::CONFLICT,
            "attachment_sent",
            "this attachment was already sent with a message; its mode can no longer change",
        ),
        Ok(SetModeOutcome::NotTextPdf) => err_json(
            StatusCode::UNPROCESSABLE_ENTITY,
            "mode_not_applicable",
            "only a PDF whose every page has text has a Text | Pages choice; other files are \
             sent automatically",
        ),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// `GET /chat/api/attachments/{id}/text` — what the model reads of the file:
/// a text file as is, a PDF's or office file's extracted text, an audio
/// file's transcript, as `text/plain`. 404 `no_text` for an image or an
/// audio file with no transcript yet.
pub async fn get_text(State(state): State<SharedState>, ChatPath(id): ChatPath<i64>) -> Response {
    let att = match ChatRepo::of(id).attachment(&state, id).await {
        Ok(Some(a)) => a,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "not_found", "attachment not found"),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    let text = match att.kind.as_str() {
        "text" => String::from_utf8_lossy(&att.data).into_owned(),
        _ => match att.extracted {
            Some(t) => t,
            None => {
                return err_json(
                    StatusCode::NOT_FOUND,
                    "no_text",
                    "this attachment has no extracted text",
                )
            }
        },
    };
    let mut resp = (StatusCode::OK, text).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static("sandbox"),
    );
    resp
}
