//! `GET /labs/api/{lab}/export` (labs-store design §1.7): the scope as one
//! file, `Content-Disposition: attachment`, streamed from the temporary file
//! `labs::export` wrote.

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use lmgw_api_types::lab_store::LabExportQuery;
use tokio::io::AsyncReadExt;

use super::super::api_knowledge::attachment as content_disposition;
use super::super::chat::err_json;
use super::super::chat_extract::{ChatPath, ChatQuery};
use super::{lab_of, store_failed};
use crate::labs::export::{self, ExportError};
use crate::state::SharedState;

pub async fn export(
    State(state): State<SharedState>,
    ChatPath(lab): ChatPath<String>,
    ChatQuery(q): ChatQuery<LabExportQuery>,
) -> Response {
    let lab = match lab_of(&lab) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let out = match export::export(&state.db, &state.data_dir, lab, &q).await {
        Ok(f) => f,
        Err(ExportError::BadRequest(m)) => {
            return err_json(StatusCode::BAD_REQUEST, "bad_request", m)
        }
        Err(ExportError::NotFound(m)) => return err_json(StatusCode::NOT_FOUND, "not_found", m),
        Err(ExportError::Failed(m)) => return store_failed("writing the export", m),
    };
    let file = tokio::fs::File::from_std(out.file);
    let body = Body::from_stream(futures::stream::unfold(file, |mut f| async move {
        let mut buf = vec![0u8; 64 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(bytes::Bytes::from(buf)), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    }));
    (
        [
            (header::CONTENT_TYPE, out.mime),
            (header::CONTENT_DISPOSITION, content_disposition(&out.name)),
            (header::CONTENT_LENGTH, out.len.to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response()
}
