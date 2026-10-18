//! The Image lab's group and run routes (image-generation design §8,
//! labs-store design §5). The page's form is kept as a group of one cell
//! before anything is sent (`POST /image-lab/api/groups` for a generation,
//! `/groups/edit` for an edit, its picture and mask uploaded with it), so the
//! page holds the group, in flight, before its request goes out. Its one cell
//! is then run (`POST /image-lab/api/groups/{id}/run`): the request built
//! from the stored form and inputs, dispatched in process through the
//! route's own handler, and its answer kept as the run ([`super::kept`]). A
//! run the page stops before the route answered leaves the group with no
//! run, which the history shows as stopped once it is no longer in flight.
//!
//! An edit's picture and mask are the group's input artifacts. The `form`
//! part may name a kept picture (`image`, `mask`: `{id, name}`) instead of an
//! upload, so a re-opened edit or a kept result sent to the edit panel goes
//! out again without the page fetching and uploading it; an `image` or
//! `mask` part of the upload stands in for the ref. A ref names a picture:
//! an artifact of another type is refused.

use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::{Multipart, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::image_lab::{
    ImageGenForm, ImageLabForm, EDITS_ENDPOINT, EDIT_VARIANT, GENERATE_VARIANT,
    GENERATIONS_ENDPOINT,
};
use lmgw_api_types::lab_store::{
    artifact_ref, LabCell, LabFileRef, LabKind, LabMultipartField, LabMultipartFile,
    LabMultipartSummary,
};

use super::kept::{self, no_group, refused, store_failed, Cell};
use super::multipart::{multipart_body, FilePart};
use crate::labs::artifacts;
use crate::labs::record::{self, NewArtifact, NewGroup, RecordError};
use crate::labs::views;
use crate::proxy::{self, RequestCtx};
use crate::state::SharedState;
use crate::store::{self, LabArtifactRow};

/// A form that does not describe a request at all — a missing prompt, a
/// step count that is not a number. Shaped like the gateway's envelope so the
/// page has one error renderer, and 400 because it never left the browser.
fn bad_form(message: String) -> Response {
    refused(StatusCode::BAD_REQUEST, "image_lab_form", message)
}

/// `alias` → a safe file stem for the pictures, runs of unusable characters
/// collapsing to one dash (the page's own rule for its downloads).
fn model_stem(alias: &str) -> String {
    let mut out = String::with_capacity(alias.len());
    let mut dash = false;
    for c in alias.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    if out.is_empty() {
        "image".into()
    } else {
        out
    }
}

/// Keep the new group of `form`: its one cell, its inputs, and answer it
/// (no run yet), or the lab's answer when the store refused or failed.
async fn keep_group(
    state: &SharedState,
    form: ImageLabForm,
    variant: &str,
    inputs: Vec<NewArtifact>,
) -> Response {
    let plan = vec![LabCell {
        model: form.form.model.trim().to_string(),
        variant: variant.into(),
    }];
    let title = form.form.title();
    let named = inputs.len();
    let recorded = record::group(
        &state.db,
        &state.data_dir,
        NewGroup {
            lab: LabKind::Image,
            title,
            plan,
            inputs,
        },
        move |ids| {
            let mut stored = form;
            let mut at = ids.iter().take(named);
            // The picture first, the mask after it, as they were given.
            if let Some(r) = stored.image.as_mut() {
                r.id = *at.next().ok_or("the picture has no row")?;
            }
            if let Some(r) = stored.mask.as_mut() {
                r.id = *at.next().ok_or("the mask has no row")?;
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
    match super::super::labs_store::groups::one(state, LabKind::Image, id).await {
        Ok(Some(g)) => Json(g).into_response(),
        Ok(None) => store_failed("reading the group back", format!("group {id} is gone")),
        Err(_) => store_failed("reading the group back", format!("group {id} did not read")),
    }
}

/// `POST /image-lab/api/groups` — a generation's group: the page's form,
/// checked to build the exact `/v1/images/generations` body, kept with its
/// one cell. Nothing is sent: `/groups/{id}/run` sends it.
pub async fn create_generate(State(state): State<SharedState>, body: Bytes) -> Response {
    let form: ImageGenForm = match serde_json::from_slice(&body) {
        Ok(f) => f,
        Err(e) => return bad_form(format!("the form is not valid JSON: {e}")),
    };
    if let Err(e) = form.generation_body() {
        return bad_form(e);
    }
    let lab_form = ImageLabForm {
        form,
        image: None,
        mask: None,
    };
    keep_group(&state, lab_form, GENERATE_VARIANT, Vec::new()).await
}

/// One picture of an edit: an upload, or a kept artifact the form names.
enum Picture {
    Upload(FilePart),
    Kept(LabFileRef),
}

/// The picture `which` (`image`, `mask`) as the group keeps it: the
/// upload's part, else the form's ref to a kept picture; `None` for
/// neither. A ref to an artifact that is no picture is refused.
async fn picture(
    state: &SharedState,
    which: &str,
    uploads: &mut Vec<FilePart>,
    kept_ref: Option<&LabFileRef>,
) -> Result<Option<(NewArtifact, LabFileRef)>, Response> {
    let chosen = match uploads.iter().position(|p| p.field == which) {
        Some(at) => Picture::Upload(uploads.remove(at)),
        None => match kept_ref {
            Some(r) => Picture::Kept(r.clone()),
            None => return Ok(None),
        },
    };
    match chosen {
        Picture::Upload(part) => {
            let named = LabFileRef {
                id: 0,
                name: part.filename.clone(),
            };
            let new = NewArtifact::Bytes {
                name: part.filename,
                bytes: part.bytes,
                declared_mime: part.content_type,
                data_url_head: None,
            };
            Ok(Some((new, named)))
        }
        Picture::Kept(r) => {
            let row = match store::lab_artifact(&state.db, r.id).await {
                Ok(Some(row)) => row,
                Ok(None) => {
                    return Err(bad_form(format!(
                        "{which}: {} names no artifact the store keeps; pick the picture again",
                        artifact_ref(r.id)
                    )))
                }
                Err(e) => return Err(store_failed("reading the kept picture", e)),
            };
            if !row.mime.starts_with("image/") {
                return Err(bad_form(format!(
                    "{which}: {} is {}, not a picture",
                    artifact_ref(r.id),
                    row.mime
                )));
            }
            // The kept file's own name: the page's label for it is not sent.
            let named = LabFileRef {
                id: 0,
                name: row.name,
            };
            Ok(Some((NewArtifact::Existing { id: r.id }, named)))
        }
    }
}

/// `POST /image-lab/api/groups/edit` — an edit's group: the browser's upload
/// (`form` as JSON, `image`, and optionally `mask`, or kept pictures the form
/// names), the form checked to build the edit's fields, its picture and mask
/// kept as the group's inputs. Nothing is sent: `/groups/{id}/run` sends it.
pub async fn create_edit(State(state): State<SharedState>, mut mp: Multipart) -> Response {
    let mut form: Option<ImageLabForm> = None;
    let mut uploads: Vec<FilePart> = Vec::new();
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return bad_form(format!("invalid multipart body: {e}")),
        };
        let name = field.name().unwrap_or_default().to_string();
        // Dispatched on the **field name**, never on whether a filename could
        // be parsed out of the header: `image` and `mask` are the upload, and
        // a part whose `filename="…\"` the parser hands back as `None` (a
        // trailing backslash is an unterminated escape) is still the picture
        // the owner chose. Read the other way round it was dropped, and the
        // page answered "an edit needs an image to edit".
        match name.as_str() {
            "image" | "mask" => {
                let filename = field
                    .file_name()
                    .map(String::from)
                    // Something has to go in the relayed part's `filename`.
                    // The field name is the honest stand-in: sd-server reads
                    // the bytes, not the name.
                    .unwrap_or_else(|| name.clone());
                let content_type = field.content_type().map(String::from);
                match field.bytes().await {
                    Ok(bytes) => uploads.push(FilePart {
                        field: name,
                        filename,
                        content_type,
                        bytes,
                    }),
                    Err(e) => return bad_form(format!("reading upload: {e}")),
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
    let Some(mut form) = form else {
        return bad_form("no `form` field in the upload".into());
    };
    if let Err(e) = form.form.edit_fields() {
        return bad_form(e);
    }
    let image = match picture(&state, "image", &mut uploads, form.image.as_ref()).await {
        Ok(Some(p)) => p,
        Ok(None) => return bad_form("an edit needs an image to edit — pick one".into()),
        Err(r) => return r,
    };
    let mask = match picture(&state, "mask", &mut uploads, form.mask.as_ref()).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    form.image = Some(image.1);
    form.mask = mask.as_ref().map(|m| m.1.clone());
    let mut inputs = vec![image.0];
    if let Some((new, _)) = mask {
        inputs.push(new);
    }
    keep_group(&state, form, EDIT_VARIANT, inputs).await
}

/// `/v1/images/edits`' handler over the lab's in-process request, with the
/// route's own "no body limit" on it. The HTTP route disables axum's body
/// limit for its multipart (`server::build_router`); a request built here
/// carries no such mark, and axum's multipart reader then stops at its
/// default 2 MB: a kept 1024² PNG is about 2 MB.
async fn edit_unlimited(state: SharedState, req: axum::extract::Request) -> Response {
    use tower::{Layer, ServiceExt};
    let handler = tower::service_fn(move |req: axum::extract::Request| {
        let state = state.clone();
        async move {
            Ok::<_, std::convert::Infallible>(
                proxy::handle_image_edit(state, RequestCtx::default(), req).await,
            )
        }
    });
    match axum::extract::DefaultBodyLimit::disable()
        .layer(handler)
        .oneshot(req)
        .await
    {
        Ok(resp) => resp,
        Err(never) => match never {},
    }
}

/// The stored form or one of its files no longer builds a request: the
/// lab's 500, nothing sent.
fn unbuilt(id: i64, e: impl std::fmt::Display) -> Response {
    refused(
        StatusCode::INTERNAL_SERVER_ERROR,
        "cell_unbuilt",
        format!("group {id} could not be built into a request: {e}"),
    )
}

/// The input `r` names, read back from the store as the part it is sent
/// as: its kept name (sanitised by [`multipart_body`]) and type.
async fn kept_part(
    state: &SharedState,
    inputs: &[LabArtifactRow],
    which: &str,
    r: &LabFileRef,
) -> Result<FilePart, Response> {
    let Some(row) = inputs.iter().find(|a| a.id == r.id) else {
        return Err(refused(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cell_unbuilt",
            format!(
                "{which}: {} is not one of the group's inputs",
                artifact_ref(r.id)
            ),
        ));
    };
    let bytes = artifacts::read(&state.data_dir, &row.sha256)
        .await
        .map_err(|e| refused(StatusCode::NOT_FOUND, "file_missing", e))?;
    Ok(FilePart {
        field: which.into(),
        filename: row.name.clone(),
        content_type: Some(row.mime.clone()),
        bytes: Bytes::from(bytes),
    })
}

/// The 409 of a cell answered or being answered.
fn answered(id: i64) -> Response {
    refused(
        StatusCode::CONFLICT,
        "cell_answered",
        format!("group {id} has its run or is being answered; a re-ask is a new group"),
    )
}

/// `POST /image-lab/api/groups/{id}/run` — the group's one cell: claimed,
/// its request built from the stored form (an edit's picture and mask read
/// back from the store), handed to the route's handler, and the answer kept
/// as its run. 200 with the group whatever the route said (Decision 20).
pub async fn run(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    let Ok(id) = id.parse::<i64>() else {
        return refused(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!("'{id}' is not a group id"),
        );
    };
    let g = match store::lab_group(&state.db, LabKind::Image, id).await {
        Ok(Some(g)) => g,
        Ok(None) => return no_group(id),
        Err(e) => return store_failed("reading the group", e),
    };
    let Some(cell) = views::plan_of(&g).into_iter().next() else {
        return no_group(id);
    };
    // Held until the run is recorded, or the request is dropped first.
    let Some(claim) = state.lab_cells.claim(id, 0) else {
        return answered(id);
    };
    match store::lab_run_exists(&state.db, id, 0, false).await {
        Ok(false) => {}
        Ok(true) => return answered(id),
        Err(e) => return store_failed("reading the group's run", e),
    }
    let inputs: Vec<LabArtifactRow> = match store::lab_artifacts_of(&state.db, &[id]).await {
        Ok(rows) => rows.into_iter().filter(|a| a.run_id.is_none()).collect(),
        Err(e) => return store_failed("reading the group's pictures", e),
    };
    let form: ImageLabForm = match serde_json::from_str(&g.form) {
        Ok(f) => f,
        Err(e) => return unbuilt(id, format!("the stored form does not read: {e}")),
    };
    let model = cell.model.clone();
    let stem = model_stem(&model);
    let format = form.form.output_format.clone();

    if cell.variant != EDIT_VARIANT {
        let body = match form.form.generation_body() {
            Ok(b) => b,
            Err(e) => return unbuilt(id, e),
        };
        let cell = Cell {
            group: g,
            inputs,
            variant: GENERATE_VARIANT,
            endpoint: GENERATIONS_ENDPOINT,
            model,
            request: body.to_string(),
            stem,
            format,
        };
        let started = Instant::now();
        let resp = proxy::handle_image_generation(state.clone(), RequestCtx::default(), body).await;
        return kept::answer(&state, cell, resp, started, claim).await;
    }

    let fields = match form.form.edit_fields() {
        Ok(f) => f,
        Err(e) => return unbuilt(id, e),
    };
    let Some(image) = form.image.as_ref() else {
        return unbuilt(id, "the stored edit names no picture");
    };
    let mut files = match kept_part(&state, &inputs, "image", image).await {
        Ok(p) => vec![p],
        Err(r) => return r,
    };
    if let Some(mask) = form.mask.as_ref() {
        match kept_part(&state, &inputs, "mask", mask).await {
            Ok(p) => files.push(p),
            Err(r) => return r,
        }
    }
    // The summary the card shows: the text fields as sent, each file part by
    // name and size with its kept file. Built from the same `fields` and
    // files the body is, so it cannot describe a request that was not made.
    let refs = [form.image.as_ref(), form.mask.as_ref()];
    let summary = LabMultipartSummary {
        fields: fields
            .iter()
            .map(|(k, v)| LabMultipartField {
                name: k.clone(),
                value: v.clone(),
            })
            .collect(),
        files: files
            .iter()
            .zip(refs.into_iter().flatten())
            .map(|(f, r)| LabMultipartFile {
                name: f.field.clone(),
                filename: f.filename.clone(),
                content_type: f.content_type.clone(),
                bytes: f.bytes.len(),
                artifact: Some(artifact_ref(r.id)),
            })
            .collect(),
    };
    let request = serde_json::to_string(&summary).unwrap_or_else(|_| "{}".into());
    let boundary = format!("lmgwimagelab{}", super::super::rand_hex32());
    let body = multipart_body(&boundary, &fields, &files);
    drop(files);
    let req = match axum::http::Request::builder()
        .method("POST")
        .uri(EDITS_ENDPOINT)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
    {
        Ok(r) => r,
        Err(e) => return unbuilt(id, format!("building the multipart request: {e}")),
    };
    let cell = Cell {
        group: g,
        inputs,
        variant: EDIT_VARIANT,
        endpoint: EDITS_ENDPOINT,
        model,
        request,
        stem,
        format,
    };
    let started = Instant::now();
    let resp = edit_unlimited(state.clone(), req).await;
    kept::answer(&state, cell, resp, started, claim).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stems_collapse_unusable_characters() {
        assert_eq!(model_stem("image/z-image-turbo"), "image-z-image-turbo");
        assert_eq!(model_stem("flux 1.1 pro"), "flux-1.1-pro");
        assert_eq!(model_stem(""), "image");
    }
}
