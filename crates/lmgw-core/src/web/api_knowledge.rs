//! `/api/knowledge/*` (chat-complete design §9.5): the Knowledge page's
//! backend — bases, their files, uploads, re-ingest, the search playground,
//! the source viewer, and Resume / Cancel of a base's job. Live job progress
//! is the ordinary jobs feed (`kb_ingest` / `kb_reembed`, key `kb:<id>`).
//!
//! A dashboard backend, not a contract: `Cap::Admin` and an
//! `openapi/exclusions.rs` row each. Every handler is a thin call into
//! [`crate::knowledge::ops`] (or `retrieve` / `read`), answering through
//! [`ops_result`] so failures have `/api`'s one shape.

use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use lmgw_api_types as dto;
use quickdoc_core::retrieve::SearchParams;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::GatewayError;
use crate::knowledge::{self, ops, read, retrieve};
use crate::state::SharedState;

use super::api::ops_result;

pub fn routes(state: &SharedState) -> Router<SharedState> {
    Router::new()
        .route("/api/knowledge/bases", get(list).post(create))
        .route("/api/knowledge/bases/{id}", get(detail))
        .route("/api/knowledge/bases/{id}/settings", post(edit))
        .route("/api/knowledge/bases/{id}/delete", post(delete))
        .route("/api/knowledge/bases/{id}/resume", post(resume))
        .route("/api/knowledge/bases/{id}/cancel", post(cancel))
        // Files are as big as they are; the upload reads its own body under
        // `max_body_mb`, so axum's silent 2 MiB default is off here.
        .route(
            "/api/knowledge/bases/{id}/files",
            get(files)
                .post(upload)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/api/knowledge/files/{id}/delete", post(delete_file))
        .route("/api/knowledge/files/{id}/reingest", post(reingest_file))
        .route("/api/knowledge/files/{id}/text", get(file_text))
        .route("/api/knowledge/files/{id}/original", get(original))
        .route("/api/knowledge/search", post(search))
        // The owner's documents and the money their embedding costs.
        .route_layer(crate::server::require(state, crate::principal::Cap::Admin))
}

async fn list(State(st): State<SharedState>) -> Response {
    ops_result(ops::list(&st).await.map(|b| json!({ "bases": b })))
}

async fn create(State(st): State<SharedState>, Json(req): Json<ops::CreateKb>) -> Response {
    ops_result(ops::create(&st, req).await.map(|v| json!(v)))
}

async fn detail(State(st): State<SharedState>, Path(id): Path<i64>) -> Response {
    ops_result(
        async {
            let base = ops::get(&st, id).await?;
            let files = ops::files(&st, id).await?;
            Ok(json!({ "base": base, "files": files }))
        }
        .await,
    )
}

async fn edit(
    State(st): State<SharedState>,
    Path(id): Path<i64>,
    Json(req): Json<ops::EditKbRequest>,
) -> Response {
    ops_result(ops::edit(&st, id, req).await.map(|v| json!(v)))
}

async fn delete(State(st): State<SharedState>, Path(id): Path<i64>) -> Response {
    ops_result(ops::delete(&st, id).await)
}

async fn resume(State(st): State<SharedState>, Path(id): Path<i64>) -> Response {
    ops_result(ops::resume(&st, id).await)
}

async fn cancel(State(st): State<SharedState>, Path(id): Path<i64>) -> Response {
    ops_result(ops::cancel(&st, id).await)
}

async fn files(State(st): State<SharedState>, Path(id): Path<i64>) -> Response {
    ops_result(ops::files(&st, id).await.map(|f| json!({ "files": f })))
}

async fn delete_file(State(st): State<SharedState>, Path(id): Path<i64>) -> Response {
    ops_result(ops::delete_file(&st, id).await)
}

async fn reingest_file(State(st): State<SharedState>, Path(id): Path<i64>) -> Response {
    ops_result(ops::reingest_file(&st, id).await)
}

fn api_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(dto::ApiError {
            code: code.into(),
            message: message.into(),
        }),
    )
        .into_response()
}

fn body_limit(max_mb: u32) -> Response {
    api_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "body_limit",
        GatewayError::BodyTooLarge { max_mb }.to_string(),
    )
}

/// `POST /api/knowledge/bases/{id}/files` — multipart, one or more files
/// (any field name; a field's file name names the file). The **whole
/// request** is bounded by `max_body_mb` (Settings; `0` = no bound), refused
/// with a named 413 when a declared or streamed body passes it — the chat
/// upload's ceiling and wording.
async fn upload(State(st): State<SharedState>, Path(id): Path<i64>, req: Request) -> Response {
    let max_mb = st.snapshot().settings.max_body_mb;
    let req = if max_mb == 0 {
        req
    } else {
        let max_bytes = max_mb as u64 * 1024 * 1024;
        let declared = req
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if declared.is_some_and(|n| n > max_bytes) {
            return body_limit(max_mb);
        }
        let max = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        let (parts, body) = req.into_parts();
        Request::from_parts(parts, Body::new(http_body_util::Limited::new(body, max)))
    };
    let mut mp = match Multipart::from_request(req, &st).await {
        Ok(m) => m,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, "bad_request", e.body_text()),
    };
    let mut files: Vec<(String, Bytes)> = Vec::new();
    loop {
        match mp.next_field().await {
            Ok(Some(field)) => {
                let name = field
                    .file_name()
                    .or(field.name())
                    .unwrap_or("unnamed")
                    .to_string();
                match field.bytes().await {
                    Ok(b) => files.push((name, b)),
                    Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                        return body_limit(max_mb)
                    }
                    Err(e) => {
                        return api_error(StatusCode::BAD_REQUEST, "bad_request", e.body_text())
                    }
                }
            }
            Ok(None) => break,
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return body_limit(max_mb),
            Err(e) => return api_error(StatusCode::BAD_REQUEST, "bad_request", e.body_text()),
        }
    }
    if files.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "no_files",
            "the upload carried no files — send them as multipart/form-data parts",
        );
    }
    ops_result(ops::upload(&st, id, files).await.map(|o| json!(o)))
}

#[derive(Debug, Deserialize)]
struct TextQuery {
    /// The chunk to highlight (a citation's chunk id).
    #[serde(default)]
    chunk: Option<String>,
    /// What the citation stored besides the id: the passage's position and
    /// the file's sha256 then, for a citation whose chunk id a re-ingest
    /// renamed ([`read::Cited`]).
    #[serde(default)]
    span_start: Option<i64>,
    #[serde(default)]
    span_end: Option<i64>,
    #[serde(default)]
    sha: Option<String>,
}

/// `GET /api/knowledge/files/{id}/text?chunk=<id>[&span_start=&span_end=&sha=]`
/// — the source viewer: the file's extracted text, every chunk's span, and the
/// cited one (by its stored position when its chunk id is gone).
async fn file_text(
    State(st): State<SharedState>,
    Path(id): Path<i64>,
    Query(q): Query<TextQuery>,
) -> Response {
    ops_result(
        read::source(
            &st,
            id,
            q.chunk.as_deref(),
            &read::Cited {
                span_start: q.span_start,
                span_end: q.span_end,
                file_sha: q.sha.filter(|s| !s.is_empty()),
            },
        )
        .await
        .map(|s| json!(s)),
    )
}

/// `GET /api/knowledge/files/{id}/original` — the uploaded bytes, as a
/// download under the file's name.
async fn original(State(st): State<SharedState>, Path(id): Path<i64>) -> Response {
    let file = match knowledge::store::get_file(&st.knowledge.pool, id).await {
        Ok(Some(f)) => f,
        Ok(None) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no knowledge-base file {id}"),
            )
        }
        Err(e) => return ops_result(Err(e.to_string())),
    };
    match knowledge::originals::read(&st.data_dir, &file.sha256).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, file.mime.clone()),
                (header::CONTENT_DISPOSITION, attachment(&file.name)),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => api_error(StatusCode::NOT_FOUND, "not_found", e),
    }
}

/// `attachment` with an ASCII `filename` and the real name as RFC 5987
/// `filename*` — a header value carries no raw UTF-8, and the owner's file
/// names are German as often as not.
pub(super) fn attachment(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchRequest {
    query: String,
    /// The bases to search; omitted or empty searches every base (the
    /// playground is the owner's, so `mcp_visible` does not apply).
    #[serde(default)]
    kb_ids: Vec<i64>,
    /// `0` or omitted: no budget.
    #[serde(default)]
    budget_tokens: Option<usize>,
    /// Stage overrides on top of the owner's search defaults — the same
    /// object the docs playground takes.
    #[serde(default)]
    params: Option<Value>,
}

/// `POST /api/knowledge/search` — the playground: excerpts with their scores,
/// the notes, and every search's per-stage trace.
async fn search(State(st): State<SharedState>, Json(req): Json<SearchRequest>) -> Response {
    ops_result(
        async {
            let ids = if req.kb_ids.is_empty() {
                knowledge::store::list_kbs(&st.knowledge.pool)
                    .await
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|k| k.id)
                    .collect()
            } else {
                req.kb_ids.clone()
            };
            let budget = req.budget_tokens.filter(|b| *b > 0);
            let params: Option<SearchParams> = match &req.params {
                None | Some(Value::Null) => None,
                Some(p) => {
                    let base = crate::quickdoc::query::default_params(&st.snapshot());
                    Some(crate::quickdoc::query::apply_params(&base, p)?)
                }
            };
            let r = retrieve::retrieve(
                &st,
                &ids,
                &req.query,
                &retrieve::Options {
                    budget_tokens: budget,
                    params,
                },
            )
            .await;
            Ok(json!(r))
        }
        .await,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_download_name_survives_as_rfc5987() {
        assert_eq!(
            attachment("Steuer 2025.pdf"),
            "attachment; filename=\"Steuer 2025.pdf\"; filename*=UTF-8''Steuer%202025.pdf"
        );
        let h = attachment("Gebühren \"alt\".pdf");
        assert!(
            h.starts_with("attachment; filename=\"Geb_hren _alt_.pdf\""),
            "{h}"
        );
        assert!(
            h.ends_with("filename*=UTF-8''Geb%C3%BChren%20%22alt%22.pdf"),
            "{h}"
        );
        assert!(axum::http::HeaderValue::from_str(&h).is_ok());
    }
}
