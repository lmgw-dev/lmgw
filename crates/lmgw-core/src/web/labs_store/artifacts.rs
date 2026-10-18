//! `GET /labs/api/artifacts/{id}` (labs-store design §1.4): the file's bytes
//! with the row's type, `X-Content-Type-Options: nosniff` and
//! `Content-Security-Policy: sandbox`, as a Chat attachment is served, so a
//! file opened in a browser runs nothing.

use axum::extract::State;
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use super::super::chat::err_json;
use super::super::chat_extract::ChatPath;
use super::store_failed;
use crate::labs::artifacts;
use crate::state::SharedState;
use crate::store;

pub async fn get(State(state): State<SharedState>, ChatPath(id): ChatPath<i64>) -> Response {
    let row = match store::lab_artifact(&state.db, id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return err_json(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("there is no lab artifact {id}"),
            )
        }
        Err(e) => return store_failed("reading the artifact", e),
    };
    let bytes = match artifacts::read(&state.data_dir, &row.sha256).await {
        Ok(b) => b,
        Err(e) => return err_json(StatusCode::NOT_FOUND, "file_missing", e),
    };
    let mut resp = (StatusCode::OK, bytes).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&row.mime)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
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
