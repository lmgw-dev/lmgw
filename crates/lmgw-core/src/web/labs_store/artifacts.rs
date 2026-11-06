//! `GET /labs/api/artifacts/{id}` (labs-store design §1.4): the file's bytes
//! with the row's type, `X-Content-Type-Options: nosniff` and
//! `Content-Security-Policy: sandbox`, as a Chat attachment is served, so a
//! file opened in a browser runs nothing. Streamed from the disk, whole or
//! the one `Range` a player asks ([`super::super::ranged`]), with the strong
//! `ETag: "sha256:<hex>"` the content-addressed file has.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;

use super::super::chat::err_json;
use super::super::chat_extract::ChatPath;
use super::super::ranged::{self, Head, RangeAsk};
use super::store_failed;
use crate::labs::artifacts;
use crate::state::SharedState;
use crate::store;

pub async fn get(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ask: RangeAsk,
) -> Response {
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
    let path = match artifacts::path(&state.data_dir, &row.sha256) {
        Ok(p) => p,
        Err(e) => return err_json(StatusCode::NOT_FOUND, "file_missing", e),
    };
    let head = Head::typed(&row.mime).with_etag(format!("\"sha256:{}\"", row.sha256));
    // `ranged` sends nosniff + sandbox on every answer it builds.
    match ranged::file(&ask, &path, head).await {
        Ok(r) => r,
        Err(e) => err_json(
            StatusCode::NOT_FOUND,
            "file_missing",
            format!(
                "the lab artifact {} could not be read ({e})",
                path.display()
            ),
        ),
    }
}
