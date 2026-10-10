//! The Audio lab's own extractors: axum's [`Json`] with its rejection answered
//! in the plane's `{error}` shape ([`LabError`](lmgw_api_types::audio_lab::LabError)),
//! as [`ChatJson`](super::super::chat_extract) does for the Chat API. axum's
//! own refusal of a body is plain text; every other failure of a lab route is
//! `{error, code?}`, so a client would have to special-case exactly these.
//! The status stays axum's (400 for bad syntax, 415 for a wrong content type,
//! 422 for a shape that is not the type).

use axum::extract::rejection::JsonRejection;
use axum::extract::FromRequest;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::{extract::Request, Json};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::fail;
use crate::error::GatewayError;

/// A rejection as the lab's `{error}`: axum's status and text.
fn lab_rejected(e: JsonRejection) -> Response {
    fail(e.status(), e.body_text())
}

/// [`Json`], refused as a `LabError`.
pub(crate) struct LabJson<T>(pub T);

impl<T, S> FromRequest<S> for LabJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Self(v)),
            Err(e) => Err(lab_rejected(e)),
        }
    }
}

/// An optional JSON body, refused as a `LabError`: no `Content-Type` means no
/// body (`None`), as axum's `Option<Json<T>>` reads it; any other request is
/// read as JSON and refused when it is not `T`.
pub(crate) struct LabOptJson<T>(pub Option<T>);

impl<T, S> FromRequest<S> for LabOptJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        if !req.headers().contains_key(header::CONTENT_TYPE) {
            return Ok(Self(None));
        }
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Self(Some(v))),
            Err(e) => Err(lab_rejected(e)),
        }
    }
}

/// The body of a route the lab hands to a `/v1` handler, refused the way that
/// handler refuses it: axum's status with the OpenAI error envelope.
pub(super) fn openai_body(body: Result<Json<Value>, JsonRejection>) -> Result<Value, Response> {
    match body {
        Ok(Json(b)) => Ok(b),
        Err(e) => {
            let err = GatewayError::BadRequest(e.to_string());
            Err((e.status(), Json(err.to_openai_json())).into_response())
        }
    }
}
