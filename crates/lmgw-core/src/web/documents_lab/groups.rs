//! `POST /documents-lab/api/groups` (WP12): the page's form and its uploads
//! kept as a group of one cell before anything is sent, so the page holds
//! the group, in flight, before the request goes out.
//!
//! The upload is multipart: `form` (a [`DocumentsLabForm`] as JSON) and one
//! `file` part per new file. The form lists every file in request order; a
//! file with `id` 0 is the next `file` part, any other names a file the store
//! keeps (a re-run's), which the new group names again without an upload.
//! The files are the group's input artifacts in that order, and the stored
//! form names each by its artifact.
//!
//! Each `file` part is spooled to disk as its bytes arrive (never held whole
//! in memory), and the bytes are counted against the route's `max_body_mb`
//! as they come: the part that passes it is answered with the route's own
//! 413 envelope at once, and nothing of the upload is kept (the spool files
//! are removed as they drop). The count covers the new files and the kept
//! ones the form names again, as the route counts one request.

use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::documents_lab::{DocumentsLabForm, DocumentsLabRoute};
use lmgw_api_types::lab_store::{artifact_ref, LabCell, LabKind};

use super::kept::{refused, store_failed};
use crate::labs::artifacts::{Spool, Spooled};
use crate::labs::record::{self, NewArtifact, NewGroup, RecordError};
use crate::state::SharedState;
use crate::store;

/// A form that describes no request: 400, nothing kept.
fn bad_form(message: impl Into<String>) -> Response {
    refused(StatusCode::BAD_REQUEST, "documents_lab_form", message)
}

/// One upload part.
struct Upload {
    name: String,
    content_type: Option<String>,
    file: Spooled,
}

/// The route's own 413 for a request past `max_body_mb`: the native route's
/// envelope, or docling's for the docling route.
fn too_large(max_mb: u32, docling: bool) -> Response {
    if docling {
        return crate::server::docling::body_limit(max_mb, false);
    }
    let e = crate::server::documents::body_limit(max_mb, false);
    (e.http_status(), Json(e.to_openai_json())).into_response()
}

/// One `file` part onto a spool file, counting it into `total`. `Err` is
/// the answer to give: the 413 once `total` passes `max_mb` (0 = no limit).
async fn spool_part(
    state: &SharedState,
    field: &mut axum::extract::multipart::Field<'_>,
    total: &mut u64,
    max_mb: u32,
    docling: bool,
) -> Result<Spooled, Response> {
    let mut spool = Spool::create(&state.data_dir)
        .await
        .map_err(|e| store_failed("spooling an upload", e))?;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                *total += chunk.len() as u64;
                if max_mb > 0 && *total > u64::from(max_mb) * 1024 * 1024 {
                    return Err(too_large(max_mb, docling));
                }
                spool
                    .write(&chunk)
                    .await
                    .map_err(|e| store_failed("spooling an upload", e))?;
            }
            Ok(None) => break,
            Err(e) => return Err(bad_form(format!("reading upload: {e}"))),
        }
    }
    spool
        .finish()
        .await
        .map_err(|e| store_failed("spooling an upload", e))
}

/// The handler.
pub async fn create(State(state): State<SharedState>, mut mp: Multipart) -> Response {
    let mut form: Option<DocumentsLabForm> = None;
    let mut uploads: Vec<Upload> = Vec::new();
    let max_mb = state.snapshot().settings.max_body_mb;
    let limit = u64::from(max_mb) * 1024 * 1024;
    let mut total = 0u64;
    loop {
        let mut field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return bad_form(format!("invalid multipart body: {e}")),
        };
        // By the field name, as the Image lab reads its parts: a `file`
        // whose name the parser could not read is still the file.
        match field.name().unwrap_or_default() {
            "file" => {
                let name = field.file_name().unwrap_or("file").to_string();
                let content_type = field.content_type().map(String::from);
                // The page sends the form first, so the route is known here;
                // a part before it is judged by the native route's envelope.
                let docling = form
                    .as_ref()
                    .is_some_and(|f| f.settings.route == DocumentsLabRoute::Docling);
                match spool_part(&state, &mut field, &mut total, max_mb, docling).await {
                    Ok(file) => uploads.push(Upload {
                        name,
                        content_type,
                        file,
                    }),
                    Err(r) => return r,
                }
            }
            "form" => {
                let text = match field.text().await {
                    Ok(t) => t,
                    Err(e) => return bad_form(format!("reading the form field: {e}")),
                };
                match serde_json::from_str(&text) {
                    Ok(f) => form = Some(f),
                    Err(e) => return bad_form(format!("the form field is not valid JSON: {e}")),
                }
            }
            _ => {}
        }
    }
    let Some(form) = form else {
        return bad_form("no `form` field in the upload");
    };
    if form.files.is_empty() {
        return bad_form("pick one or more files to convert");
    }
    if let Err(e) = form.settings.fields() {
        return bad_form(e);
    }
    let fresh = form.files.iter().filter(|f| f.id == 0).count();
    if fresh != uploads.len() {
        return bad_form(format!(
            "the form lists {fresh} new file(s) and the upload carries {}",
            uploads.len()
        ));
    }
    let mut uploads = uploads.into_iter();
    let mut kept_bytes = 0u64;
    let mut inputs = Vec::with_capacity(form.files.len());
    for f in &form.files {
        if f.id == 0 {
            let Some(u) = uploads.next() else {
                return bad_form("an upload is missing");
            };
            inputs.push(NewArtifact::Spooled {
                name: u.name,
                file: u.file,
                declared_mime: u.content_type,
            });
            continue;
        }
        // A kept file: still there, and the new group's input as it is.
        match store::lab_artifact(&state.db, f.id).await {
            Ok(Some(row)) => {
                kept_bytes += row.size.max(0) as u64;
                inputs.push(NewArtifact::Existing { id: f.id });
            }
            Ok(None) => {
                return bad_form(format!(
                    "{} ({}) is not kept any more; add the file again",
                    artifact_ref(f.id),
                    f.name
                ))
            }
            Err(e) => return store_failed("reading a kept file", e),
        }
    }
    if limit > 0 && total + kept_bytes > limit {
        return too_large(max_mb, form.settings.route == DocumentsLabRoute::Docling);
    }
    keep(&state, form, inputs).await
}

/// Keep the group and answer it (no run yet).
async fn keep(state: &SharedState, form: DocumentsLabForm, inputs: Vec<NewArtifact>) -> Response {
    let plan = vec![LabCell {
        model: form.settings.model().into(),
        variant: form.settings.variant().into(),
    }];
    let recorded = record::group(
        &state.db,
        &state.data_dir,
        NewGroup {
            lab: LabKind::Documents,
            title: form.title(),
            plan,
            inputs,
        },
        move |ids| {
            let mut stored = form;
            if ids.len() != stored.files.len() {
                return Err("the files and their rows differ in number".into());
            }
            for (f, id) in stored.files.iter_mut().zip(ids) {
                f.id = *id;
            }
            let text = serde_json::to_string(&stored).map_err(|e| e.to_string())?;
            Ok((text, None))
        },
    )
    .await;
    let id = match recorded {
        Ok((id, _)) => id,
        Err(RecordError::Refused(m)) => return bad_form(m),
        Err(RecordError::Taken(m) | RecordError::Failed(m)) => {
            return store_failed("keeping the group", m)
        }
    };
    match super::super::labs_store::groups::one(state, LabKind::Documents, id).await {
        Ok(Some(g)) => Json(g).into_response(),
        Ok(None) => store_failed("reading the group back", format!("group {id} is gone")),
        Err(r) => r,
    }
}
