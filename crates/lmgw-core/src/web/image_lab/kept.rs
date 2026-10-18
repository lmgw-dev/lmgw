//! An Image lab dispatch kept as its group's run (labs-store design §5,
//! Decision 20): the route's answer is read whole, each picture of a 2xx
//! becomes an output artifact named `lab-artifact:<id>` where its
//! `b64_json` was, and the lab answers 200 with the group, its run inside,
//! whatever the route said.
//!
//! The answer is read and kept on a task of its own, which holds the cell's
//! claim until the run is in: a page that stops once the route has answered
//! (or goes away) does not take the picture with it. The group the lab
//! answers is built from what the run route holds, never read back, so a
//! run the store could not keep (a full disk, the group deleted meanwhile)
//! is answered all the same: no `id`, its pictures still inline, and
//! `store_error` says why. An answer is never dropped because the store
//! failed (§1.5).

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::image_lab::ImageLabAnswer;
use lmgw_api_types::lab_store::{artifact_ref, LabRunView};
use lmgw_api_types::OpenAiError;

use super::outputs;
use crate::labs::claims::CellClaim;
use crate::labs::record::{self, NewArtifact, NewRun, RunTexts};
use crate::labs::run_facts::{answer_of, failure_of, gateway_headers};
use crate::labs::views;
use crate::state::SharedState;
use crate::store::{LabArtifactRow, LabGroupRow, LabRunData, LabRunRow};

/// The lab's own refusal, in the envelope the image routes answer in.
pub(super) fn refused(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(OpenAiError::new(
            message.into(),
            "invalid_request_error",
            code,
        )),
    )
        .into_response()
}

/// A store that failed before anything was sent: the lab's 500.
pub(super) fn store_failed(what: &str, e: impl std::fmt::Display) -> Response {
    tracing::warn!("image lab: {what} failed: {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(OpenAiError::new(
            format!("{what} failed: {e}"),
            "api_error",
            "lab_store",
        )),
    )
        .into_response()
}

/// The 404 of a group that is not there (or another lab's), in the
/// envelope.
pub(super) fn no_group(id: i64) -> Response {
    refused(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("the image lab has no group {id}"),
    )
}

/// The one cell of a group being run, with the group as the run route read
/// it: what the answer is built from.
pub(super) struct Cell {
    pub group: LabGroupRow,
    /// The group's inputs (an edit's picture and mask).
    pub inputs: Vec<LabArtifactRow>,
    pub variant: &'static str,
    pub endpoint: &'static str,
    pub model: String,
    /// The request as it is kept: the body sent, or an edit's multipart
    /// summary with its files as refs.
    pub request: String,
    /// What a picture's file is named after: `<stem>-<i>.<ext>`.
    pub stem: String,
    /// The format the form asked for, when the answer does not say.
    pub format: String,
}

/// `png` | `jpeg` | `webp` → the media type a picture of it declares, which
/// stands in only where its bytes are of no kind lmgw recognises.
fn media_type(format: &str) -> String {
    match format {
        "" => "image/png".into(),
        "jpg" => "image/jpeg".into(),
        other => format!("image/{other}"),
    }
}

/// The answer's own `output_format`, else the form's.
fn format_of(text: &str, asked: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Said {
        output_format: Option<String>,
    }
    serde_json::from_str::<Said>(text)
        .ok()
        .and_then(|s| s.output_format)
        .filter(|f| !f.trim().is_empty())
        .unwrap_or_else(|| asked.trim().to_string())
}

/// Read the route's answer `resp` (dispatched at `started`), keep it as the
/// cell's run on a task that holds `claim` until the run is in, and answer
/// the group.
pub(super) async fn answer(
    state: &SharedState,
    cell: Cell,
    resp: Response,
    started: Instant,
    claim: CellClaim,
) -> Response {
    let state = state.clone();
    let kept = tokio::spawn(async move {
        let answer = keep(&state, cell, resp, started).await;
        // Released once the run is in (or could not be).
        drop(claim);
        answer
    });
    match kept.await {
        Ok(a) => Json(a).into_response(),
        Err(e) => store_failed("keeping the answer", e),
    }
}

/// Read and record the answer: the group with its run.
async fn keep(state: &SharedState, cell: Cell, resp: Response, started: Instant) -> ImageLabAnswer {
    let status = resp.status();
    let (parts, body) = resp.into_parts();
    // Unbounded, like the route: a 4096² PNG is the size it is.
    let read = axum::body::to_bytes(body, usize::MAX).await;
    let latency_ms = started.elapsed().as_millis() as i64;
    let (bytes, read_error) = match read {
        Ok(b) => (b, None),
        Err(e) => (Bytes::new(), Some(format!("reading the answer: {e}"))),
    };
    let headers = gateway_headers(&parts.headers);
    let response_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let mut outputs = Vec::new();
    let mut pictures = Vec::new();
    // Shared by the record's splice and an answer the store did not keep.
    let response: Option<Arc<str>> = match std::str::from_utf8(&bytes) {
        Ok(text) => {
            if status.is_success() {
                pictures = outputs::pictures(text);
                let format = format_of(text, &cell.format);
                let ext = match format.as_str() {
                    "" => "png",
                    "jpeg" => "jpg",
                    f => f,
                }
                .to_string();
                for (i, p) in pictures.iter_mut().enumerate() {
                    outputs.push(NewArtifact::Bytes {
                        name: format!("{}-{i}.{ext}", cell.stem),
                        // Taken, not copied: the splice needs its range only.
                        bytes: std::mem::take(&mut p.bytes),
                        declared_mime: Some(media_type(&format)),
                        data_url_head: None,
                    });
                }
            }
            Some(Arc::from(text))
        }
        // No text: kept as an output artifact instead.
        Err(_) => {
            outputs.push(NewArtifact::Bytes {
                name: "response".into(),
                bytes: bytes.clone(),
                declared_mime: response_type.clone(),
                data_url_head: None,
            });
            None
        }
    };
    drop(bytes);
    let (mut error_code, mut error_message) = if status.is_success() {
        (None, None)
    } else {
        failure_of(response.as_deref())
    };
    if let Some(e) = read_error {
        error_code.get_or_insert_with(|| "read_failed".into());
        error_message = Some(e);
    }
    let (upstream_model, input_tokens, output_tokens) = if status.is_success() {
        answer_of(response.as_deref())
    } else {
        (None, None, None)
    };
    let group_id = cell.group.id;
    let data = LabRunData {
        group_id,
        cell: 0,
        variant: cell.variant.into(),
        endpoint: cell.endpoint.into(),
        model_asked: cell.model.clone(),
        answered_by: headers
            .get("x-lmgw-fallback")
            .cloned()
            .unwrap_or_else(|| cell.model.clone()),
        fallback_reason: headers.get("x-lmgw-fallback-reason").cloned(),
        upstream_model,
        http_status: i64::from(status.as_u16()),
        error_code,
        error_message,
        request: cell.request,
        // The pictures' text is written once they have ids, below.
        response: if pictures.is_empty() {
            response.as_deref().map(String::from)
        } else {
            None
        },
        response_type,
        headers: serde_json::to_string(&headers).unwrap_or_else(|_| "{}".into()),
        input_tokens,
        output_tokens,
        latency_ms,
        readable: None,
    };
    let text = response.clone().filter(|_| !pictures.is_empty());
    let request = data.request.clone();
    let recorded = record::run_with(
        &state.db,
        &state.data_dir,
        NewRun {
            data: data.clone(),
            outputs,
            replace_failed: false,
        },
        Vec::new(),
        move |_, out_ids| {
            let text = text?;
            let refs: Vec<String> = out_ids.iter().map(|id| artifact_ref(*id)).collect();
            Some(RunTexts {
                request,
                response: Some(outputs::splice(&text, &pictures, &refs)),
            })
        },
    )
    .await;
    let (run, store_error) = match recorded {
        Ok(r) => {
            let mut data = data;
            if let Some(t) = r.texts {
                data.request = t.request;
                data.response = t.response;
            }
            let outs = r
                .outputs
                .iter()
                .map(|a| views::artifact_view(a, views::api_href(a.id)))
                .collect();
            let row = LabRunRow {
                id: r.id,
                created_at: r.created_at,
                data,
            };
            (views::run_view(&row, outs), None)
        }
        Err(e) => {
            tracing::warn!("image lab: group {group_id} not kept: {e}");
            // The route's answer as it came, its pictures inline.
            let row = LabRunRow {
                id: 0,
                created_at: chrono::Utc::now()
                    .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                    .to_string(),
                data: LabRunData {
                    response: response.as_deref().map(String::from),
                    ..data
                },
            };
            let run = LabRunView {
                id: None,
                ..views::run_view(&row, Vec::new())
            };
            (run, Some(e.to_string()))
        }
    };
    let inputs = cell
        .inputs
        .iter()
        .map(|a| views::artifact_view(a, views::api_href(a.id)))
        .collect();
    ImageLabAnswer {
        group: views::group_view(cell.group, vec![run], inputs),
        store_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answers_own_format_names_the_files() {
        assert_eq!(
            format_of(r#"{"output_format":"webp","data":[]}"#, "png"),
            "webp"
        );
        assert_eq!(format_of(r#"{"data":[]}"#, "jpeg"), "jpeg");
        assert_eq!(format_of("{}", ""), "");
        assert_eq!(media_type(""), "image/png");
        assert_eq!(media_type("jpg"), "image/jpeg");
        assert_eq!(media_type("webp"), "image/webp");
    }
}
