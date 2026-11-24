//! `POST /documents-lab/api/groups/{id}/run` (WP12): the group's one cell,
//! claimed, its multipart built from the stored form and the kept files and
//! handed to the route's handler in process, as a client's request: under
//! the route's `max_body_mb` (its own 413 when the files pass it), the
//! options judged by the route, the conversion in the tool pool, the usage
//! row written. The answer is kept as the run ([`super::kept`]); an async
//! request's 202 is answered at once with its job, and its run is kept when
//! the job ends ([`super::job`]).
//!
//! A sync request the page stops (or whose page goes away) before the route
//! answered is dropped with the handler: its tools are stopped, and the group
//! has no run.

use std::collections::BTreeMap;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::documents::convert::Output;
use lmgw_api_types::documents::jobs::JobAccepted;
use lmgw_api_types::documents_lab::{DocumentsLabAnswer, DocumentsLabForm, DocumentsLabRoute};
use lmgw_api_types::lab_store::{
    artifact_ref, LabKind, LabMultipartField, LabMultipartFile, LabMultipartSummary,
};

use super::body::{multipart_stream, PartFile};
use super::kept::{self, refused, store_failed, Came, Cell};
use crate::labs::run_facts::gateway_headers;
use crate::labs::{artifacts, views};
use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::store::{self, LabArtifactRow};

fn no_group(id: i64) -> Response {
    refused(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("the documents lab has no group {id}"),
    )
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

fn answered(id: i64) -> Response {
    refused(
        StatusCode::CONFLICT,
        "cell_answered",
        format!("group {id} has its run or is being answered; a re-run is a new group"),
    )
}

/// The route's handler over the lab's in-process request, with the route's
/// bounds: a body past `max_body_mb` is the route's own 413 (what
/// `server::body_limit_mw` answers for its declared length), and axum's
/// default multipart limit is off as on the route (`server::build_router`).
async fn dispatch(
    state: &SharedState,
    ctx: RequestCtx,
    docling: bool,
    req: axum::extract::Request,
) -> Response {
    use tower::{Layer, ServiceExt};
    let max_mb = state.snapshot().settings.max_body_mb;
    let len = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if max_mb > 0 && len > u64::from(max_mb) * 1024 * 1024 {
        if let Some(r) = crate::server::documents::declared_too_large(&req, max_mb) {
            return r;
        }
    }
    let st = state.clone();
    let handler = tower::service_fn(move |req: axum::extract::Request| {
        let state = st.clone();
        let ctx = ctx.clone();
        async move {
            let ctx = axum::Extension(ctx);
            let r = if docling {
                crate::server::docling::convert_file(State(state), ctx, req).await
            } else {
                crate::server::documents::convert(State(state), ctx, req).await
            };
            Ok::<_, std::convert::Infallible>(r)
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

/// The kept files the form names, in its order, as the parts they are sent
/// as (`file` on the native route, `files` on docling's): their place on the
/// disk and size, never their bytes.
async fn parts(
    state: &SharedState,
    form: &DocumentsLabForm,
    inputs: &[LabArtifactRow],
    field: &str,
) -> Result<Vec<PartFile>, String> {
    let mut out = Vec::with_capacity(form.files.len());
    for f in &form.files {
        let Some(row) = inputs.iter().find(|a| a.id == f.id) else {
            return Err(format!(
                "{} ({}) is not one of the group's files",
                artifact_ref(f.id),
                f.name
            ));
        };
        let path = artifacts::path(&state.data_dir, &row.sha256)?;
        out.push(PartFile {
            field: field.into(),
            filename: row.name.clone(),
            content_type: Some(row.mime.clone()),
            path,
            size: row.size.max(0) as u64,
        });
    }
    Ok(out)
}

/// Read an answer whole.
async fn read(resp: Response, started: Instant) -> (Came, Option<String>) {
    let status = resp.status();
    let (parts, body) = resp.into_parts();
    let read = axum::body::to_bytes(body, usize::MAX).await;
    let latency_ms = started.elapsed().as_millis() as i64;
    let (bytes, read_error) = match read {
        Ok(b) => (b, None),
        Err(e) => (Bytes::new(), Some(format!("reading the answer: {e}"))),
    };
    let mut headers: BTreeMap<String, String> = gateway_headers(&parts.headers);
    let location = parts
        .headers
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    if let Some(l) = &location {
        headers.insert("location".into(), l.clone());
    }
    let response_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    (
        Came {
            status,
            headers,
            response_type,
            bytes,
            read_error,
            latency_ms,
            said: None,
        },
        location,
    )
}

/// The handler. The lab's requests are the owner's who runs them (security
/// round A, L7): their jobs are the owner's, never keyless ones another
/// keyless caller could read with auth off.
pub async fn run(
    State(state): State<SharedState>,
    ctx: Option<axum::Extension<RequestCtx>>,
    Path(id): Path<String>,
) -> Response {
    let ctx = ctx.map(|axum::Extension(c)| c).unwrap_or_default();
    let Ok(id) = id.parse::<i64>() else {
        return refused(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!("'{id}' is not a group id"),
        );
    };
    let g = match store::lab_group(&state.db, LabKind::Documents, id).await {
        Ok(Some(g)) => g,
        Ok(None) => return no_group(id),
        Err(e) => return store_failed("reading the group", e),
    };
    if views::plan_of(&g).is_empty() {
        return no_group(id);
    }
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
        Err(e) => return store_failed("reading the group's files", e),
    };
    let form: DocumentsLabForm = match serde_json::from_str(&g.form) {
        Ok(f) => f,
        Err(e) => return unbuilt(id, format!("the stored form does not read: {e}")),
    };
    let (fields, _) = match form.settings.fields() {
        Ok(f) => f,
        Err(e) => return unbuilt(id, e),
    };
    let docling = form.settings.route == DocumentsLabRoute::Docling;
    let field = if docling { "files" } else { "file" };
    let files = match parts(&state, &form, &inputs, field).await {
        Ok(p) => p,
        Err(e) => return refused(StatusCode::NOT_FOUND, "file_missing", e),
    };
    // The summary the card shows and the curl is built from: the fields as
    // sent, each file part by name and size with its kept file.
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
            .zip(&form.files)
            .map(|(f, r)| LabMultipartFile {
                name: f.field.clone(),
                filename: f.filename.clone(),
                content_type: f.content_type.clone(),
                bytes: f.size as usize,
                artifact: Some(artifact_ref(r.id)),
            })
            .collect(),
    };
    let request = serde_json::to_string(&summary).unwrap_or_else(|_| "{}".into());
    let boundary = format!("lmgwdocumentslab{}", super::super::rand_hex32());
    let streamed = multipart_stream(&boundary, &fields, files);
    let endpoint = form.settings.endpoint();
    let req = match axum::http::Request::builder()
        .method("POST")
        .uri(endpoint)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(header::CONTENT_LENGTH, streamed.len)
        .body(streamed.body)
    {
        Ok(r) => r,
        Err(e) => return unbuilt(id, format!("building the multipart request: {e}")),
    };
    let cell = Cell {
        group: g,
        inputs,
        variant: form.settings.variant(),
        endpoint,
        model: form.settings.model().into(),
        request,
        markdown: form.settings.options.output == Output::Markdown,
        docling,
    };
    let started = Instant::now();
    let principal = ctx.principal.clone();
    let resp = Box::pin(dispatch(&state, ctx, docling, req)).await;
    if resp.status() == StatusCode::ACCEPTED && !docling {
        return accepted(&state, principal, cell, resp, started, claim).await;
    }
    // The answer is read and kept on a task of its own, which holds the
    // claim until the run is in: a page that stops once the route has
    // answered does not take the answer with it.
    let st = state.clone();
    let kept = tokio::spawn(async move {
        let (came, _) = read(resp, started).await;
        let answer = kept::keep(&st, cell, came).await;
        drop(claim);
        answer
    });
    match kept.await {
        Ok(a) => Json(a).into_response(),
        Err(e) => store_failed("keeping the answer", e),
    }
}

/// An async request's 202: the job's id answered now with the group in
/// flight, its run kept by [`super::job::follow`] once the job has ended.
async fn accepted(
    state: &SharedState,
    principal: crate::principal::Principal,
    cell: Cell,
    resp: Response,
    started: Instant,
    claim: crate::labs::claims::CellClaim,
) -> Response {
    let (came, _) = read(resp, started).await;
    let job = serde_json::from_slice::<JobAccepted>(&came.bytes)
        .ok()
        .map(|j| j.job);
    let Some(job) = job else {
        // A 202 that names no job: kept as the answer it is.
        let answer = kept::keep(state, cell, came).await;
        drop(claim);
        return Json(answer).into_response();
    };
    let mut group = views::group_view(cell.group.clone(), Vec::new(), Vec::new());
    group.in_flight = true;
    group.artifacts = cell
        .inputs
        .iter()
        .map(|a| views::artifact_view(a, views::api_href(a.id)))
        .collect();
    super::job::follow(
        state.clone(),
        principal,
        cell,
        came,
        job.clone(),
        started,
        claim,
    );
    Json(DocumentsLabAnswer {
        group,
        store_error: None,
        job: Some(job),
    })
    .into_response()
}
