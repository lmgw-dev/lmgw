//! The Chat API's own extractors: axum's [`Json`], [`Query`] and [`Path`],
//! with their rejections answered in the surface's flat
//! [`ApiError`](lmgw_api_types::ApiError) shape (chat-complete design ground
//! rules, review R1 item d).
//!
//! axum answers a body or query it cannot read with a plain-text 400 / 415 /
//! 422 of its own — a `/folders/{id}/delete` without `threads`, a search with
//! `?folder=x`, a thread id that is not a number. Every other refusal of
//! `/chat/api` is `{code, message}`, so a client would have to special-case
//! exactly these. Swapping the extractor type is all a handler does to opt in.

use axum::body::Bytes;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use serde::de::DeserializeOwned;

use super::chat::err_json;

/// A rejection as the flat `ApiError`: its own status, a code named after it,
/// and axum's text as the message.
fn rejected(status: StatusCode, text: String) -> Response {
    let code = match status {
        StatusCode::PAYLOAD_TOO_LARGE => "body_limit",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
        _ => "bad_request",
    };
    err_json(status, code, text)
}

/// [`Json`], refused as an `ApiError`.
pub(super) struct ChatJson<T>(pub T);

impl<T, S> FromRequest<S> for ChatJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Self(v)),
            Err(e) => Err(json_rejected(e)),
        }
    }
}

fn json_rejected(e: JsonRejection) -> Response {
    rejected(e.status(), e.body_text())
}

/// An optional JSON body (chat-voice design §6.4: regenerate and continue
/// took none, and still accept none): an empty body, or one of only
/// whitespace, is `T::default()`; anything else is read as JSON whatever
/// its content type, and refused as an `ApiError` when it is not `T`.
pub(super) struct ChatOptJson<T>(pub T);

impl<T, S> FromRequest<S> for ChatOptJson<T>
where
    T: DeserializeOwned + Default,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let body = Bytes::from_request(req, state)
            .await
            .map_err(|e| rejected(e.status(), e.body_text()))?;
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Self(T::default()));
        }
        serde_json::from_slice(&body).map(Self).map_err(|e| {
            rejected(
                StatusCode::BAD_REQUEST,
                format!("the body is not the JSON this route reads: {e}"),
            )
        })
    }
}

/// [`Query`], refused as an `ApiError`.
pub(super) struct ChatQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for ChatQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(v)) => Ok(Self(v)),
            Err(e) => Err(query_rejected(e)),
        }
    }
}

fn query_rejected(e: QueryRejection) -> Response {
    rejected(e.status(), e.body_text())
}

/// [`Path`], refused as an `ApiError`.
pub(super) struct ChatPath<T>(pub T);

impl<T, S> FromRequestParts<S> for ChatPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Path::<T>::from_request_parts(parts, state).await {
            Ok(Path(v)) => Ok(Self(v)),
            Err(e) => Err(path_rejected(e)),
        }
    }
}

fn path_rejected(e: PathRejection) -> Response {
    rejected(e.status(), e.body_text())
}
