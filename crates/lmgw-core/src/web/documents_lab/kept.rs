//! A Documents lab dispatch kept as its group's run (labs-store design §5,
//! Decision 20): the route's answer read whole, each document's text an
//! output artifact named `lab-artifact:<id>` where it was ([`super::texts`]),
//! and the lab answers 200 with the group, its run inside, whatever the route
//! said. A run the store could not keep (a full disk, the group deleted
//! meanwhile) is answered all the same: no `id`, its texts still inline, and
//! `store_error` says why.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::documents_lab::DocumentsLabAnswer;
use lmgw_api_types::lab_store::{artifact_ref, LabRunView};
use lmgw_api_types::OpenAiError;
use serde_json::Value;

use super::texts::{self, Found};
use crate::labs::record::{self, NewArtifact, NewRun, RunTexts};
use crate::labs::run_facts::failure_of;
use crate::labs::views;
use crate::state::SharedState;
use crate::store::{LabArtifactRow, LabGroupRow, LabRunData, LabRunRow};

/// The lab's own refusal, in the envelope the native route answers in.
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
    tracing::warn!("documents lab: {what} failed: {e}");
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

/// The one cell of a group being run.
pub(super) struct Cell {
    pub group: LabGroupRow,
    /// The group's inputs (the files).
    pub inputs: Vec<LabArtifactRow>,
    pub variant: &'static str,
    pub endpoint: &'static str,
    pub model: String,
    /// The request as kept: the multipart's summary.
    pub request: String,
    /// `output: markdown` was asked: `content` is markdown.
    pub markdown: bool,
    pub docling: bool,
}

/// What the route answered, read.
pub(super) struct Came {
    pub status: StatusCode,
    pub headers: BTreeMap<String, String>,
    pub response_type: Option<String>,
    pub bytes: Bytes,
    pub read_error: Option<String>,
    pub latency_ms: i64,
    /// The failure's code and message when the lab knows them better than
    /// the body says (a cancelled job's state).
    pub said: Option<(String, String)>,
}

/// A failed answer's code and message: the gateway's envelope; the first
/// failed file's code when no file converted (the body is the answer, every
/// file's error in it); docling's `{"detail", "code"}`.
fn failure(text: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(v) = text.and_then(|t| serde_json::from_str::<Value>(t).ok()) else {
        return failure_of(text);
    };
    if v.get("error").is_some() {
        return failure_of(text);
    }
    if let Some(docs) = v.get("documents").and_then(Value::as_array) {
        let failed: Vec<&Value> = docs.iter().filter_map(|d| d.get("error")).collect();
        let code = failed
            .first()
            .and_then(|e| e["code"].as_str())
            .map(String::from);
        let message = format!(
            "no file was converted: {} of {} failed",
            failed.len(),
            docs.len()
        );
        return (code, Some(message));
    }
    if let Some(detail) = v.get("detail") {
        let code = v["code"].as_str().map(String::from);
        let message = detail
            .as_str()
            .map_or_else(|| detail.to_string(), String::from);
        return (code, Some(message));
    }
    failure_of(text)
}

/// The outputs of a zip answer: the archive as it came, then each entry.
async fn zip_outputs(bytes: Bytes, declared: Option<String>) -> Vec<NewArtifact> {
    let whole = bytes.clone();
    let entries = tokio::task::spawn_blocking(move || texts::zip_entries(&whole))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    let mut out = vec![NewArtifact::Bytes {
        name: "converted_docs.zip".into(),
        bytes,
        declared_mime: declared,
        data_url_head: None,
    }];
    match entries {
        Ok(entries) => out.extend(entries.into_iter().map(|(name, b)| NewArtifact::Bytes {
            name,
            bytes: Bytes::from(b),
            declared_mime: Some("text/plain".into()),
            data_url_head: None,
        })),
        Err(e) => tracing::warn!("documents lab: the zip answer did not read: {e}"),
    }
    out
}

/// Record `came` as `cell`'s run: the group with its run.
pub(super) async fn keep(state: &SharedState, cell: Cell, came: Came) -> DocumentsLabAnswer {
    let Came {
        status,
        headers,
        response_type,
        bytes,
        read_error,
        latency_ms,
        said,
    } = came;
    let is_zip = response_type
        .as_deref()
        .is_some_and(|t| t.starts_with("application/zip"));
    let mut outputs = Vec::new();
    let mut found: Vec<Found> = Vec::new();
    let response: Option<Arc<str>> = if is_zip {
        outputs = zip_outputs(bytes.clone(), response_type.clone()).await;
        None
    } else {
        match std::str::from_utf8(&bytes) {
            Ok(text) => {
                found = if cell.docling {
                    texts::docling(text)
                } else {
                    texts::native(text, cell.markdown)
                };
                for f in &mut found {
                    outputs.push(NewArtifact::Bytes {
                        name: f.name.clone(),
                        bytes: std::mem::take(&mut f.bytes),
                        declared_mime: Some(f.mime.into()),
                        data_url_head: None,
                    });
                }
                Some(Arc::from(text))
            }
            Err(_) => {
                outputs.push(NewArtifact::Bytes {
                    name: "response".into(),
                    bytes: bytes.clone(),
                    declared_mime: response_type.clone(),
                    data_url_head: None,
                });
                None
            }
        }
    };
    drop(bytes);
    let (mut error_code, mut error_message) = match said {
        Some((c, m)) => (Some(c), Some(m)),
        None if status.is_success() => (None, None),
        None => failure(response.as_deref()),
    };
    if let Some(e) = read_error {
        error_code.get_or_insert_with(|| "read_failed".into());
        error_message = Some(e);
    }
    let group_id = cell.group.id;
    let data = LabRunData {
        group_id,
        cell: 0,
        variant: cell.variant.into(),
        endpoint: cell.endpoint.into(),
        model_asked: cell.model.clone(),
        answered_by: cell.model.clone(),
        fallback_reason: None,
        upstream_model: None,
        http_status: i64::from(status.as_u16()),
        error_code,
        error_message,
        request: cell.request,
        // The texts' answer is written once they have ids, below.
        response: if found.is_empty() {
            response.as_deref().map(String::from)
        } else {
            None
        },
        response_type,
        headers: serde_json::to_string(&headers).unwrap_or_else(|_| "{}".into()),
        input_tokens: None,
        output_tokens: None,
        latency_ms,
        readable: None,
    };
    let text = response.clone().filter(|_| !found.is_empty());
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
                response: Some(texts::splice(&text, &found, &refs)),
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
            tracing::warn!("documents lab: group {group_id} not kept: {e}");
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
    DocumentsLabAnswer {
        group: views::group_view(cell.group, vec![run], inputs),
        store_error,
        job: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_names_the_envelope_the_first_failed_file_or_doclings_detail() {
        let (c, m) = failure(Some(
            r#"{"error":{"message":"no file","type":"invalid_request_error","code":"no_files"}}"#,
        ));
        assert_eq!(
            (c.as_deref(), m.as_deref()),
            (Some("no_files"), Some("no file"))
        );
        let (c, m) = failure(Some(
            r#"{"documents":[{"error":{"code":"unreadable","message":"x"}},{"error":{"code":"no_reader","message":"y"}}]}"#,
        ));
        assert_eq!(c.as_deref(), Some("unreadable"));
        assert_eq!(m.as_deref(), Some("no file was converted: 2 of 2 failed"));
        let (c, m) = failure(Some(r#"{"detail":"Task not found.","code":"not_found"}"#));
        assert_eq!(
            (c.as_deref(), m.as_deref()),
            (Some("not_found"), Some("Task not found."))
        );
        assert_eq!(failure(Some("plain")).1.as_deref(), Some("plain"));
    }
}
