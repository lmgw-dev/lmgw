//! Image lab: the "test the image routes" playground, the Audio lab's sibling
//! for stable-diffusion.cpp (image-generation design §8). The model picker,
//! the two panels and the result gallery are the SPA's Image lab page; this
//! module is its server (`/image-lab/api/*`).
//!
//! Generation and editing are dispatched **in-process** through the very
//! handlers that serve `POST /v1/images/generations` and `/v1/images/edits`
//! ([`proxy::handle_image_generation`] / [`proxy::handle_image_edit`]), so what
//! the page exercises is the real passthrough — the `resolve_image` guards, the
//! GPU hold, admission, the model rewrite, sd-server's two error bodies
//! normalized into the gateway's one envelope, and the `class = image` log row.
//! Only the auth middleware and the HTTP hop are skipped, the same trade the
//! Audio lab and `local_model_test` make.
//!
//! The request itself is **not** built here: [`ImageGenForm::generation_body`]
//! lives in `lmgw-api-types` because the page's "Request" panel has to show the
//! document this module dispatches, byte for byte. What is built here is the
//! multipart — a browser upload arrives as two files plus a JSON form, and the
//! edits route wants one `multipart/form-data` body with `model` in it.

use axum::body::Body;
use axum::extract::{Multipart, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::image_lab::{ImageGenForm, EDITS_ENDPOINT, GENERATIONS_ENDPOINT};
use serde_json::{json, Value};

use crate::capabilities::exposed::exposed_entries;
use crate::proxy::{self, RequestCtx};
use crate::state::SharedState;

// ---------------------------------------------------------------------------
// Model list
// ---------------------------------------------------------------------------

/// `GET /image-lab/api/models` — everything that can answer an image request,
/// with what the form needs to shape itself.
///
/// Two populations, one list, exactly as the routes see them (§5): the enabled
/// `image/<id>` rows, and every cloud alias whose upstream catalog says it
/// generates images. Both arrive from [`exposed_entries`] — the same join
/// `/v1/models` publishes — so "does this model serve `/v1/images/edits`" is
/// answered here by the capability object the route itself is gated on, not by
/// a second opinion.
///
/// A local row carries three things a cloud alias has not got: the `args` its
/// container was started with (which is where the size defaults come from — the
/// lab invents none), its container's state, and, once that container has been
/// probed, the **capabilities** it reported: the sampler and scheduler lists,
/// the real size bounds, the LoRAs on disk. `None` there is why the page offers
/// a free-text sampler field rather than an empty select.
pub async fn list_models(State(state): State<SharedState>) -> Response {
    let snap = state.snapshot();
    let runtime: Vec<_> = state
        .runtime()
        .list()
        .into_iter()
        .filter(|v| v.class == crate::runtime::Class::Image)
        .collect();

    let mut models: Vec<Value> = Vec::new();
    for e in exposed_entries(&state).await {
        let Some(caps) = e.capabilities.as_ref() else {
            continue;
        };
        if !caps.endpoints.iter().any(|p| p == GENERATIONS_ENDPOINT) {
            continue;
        }
        let row = snap
            .enabled_image_models()
            .find(|m| snap.image_public_name(&m.model_id) == e.name);
        let rt = row.and_then(|m| runtime.iter().find(|r| r.model_id == m.model_id));
        models.push(json!({
            "name": e.name,
            "owner": e.owner,
            "local": row.is_some(),
            "model_id": row.map(|m| m.model_id.clone()),
            "task": caps.task,
            "endpoints": caps.endpoints,
            "edit": caps.endpoints.iter().any(|p| p == EDITS_ENDPOINT),
            // The operator's own words about the pipeline, and the flags its
            // argv carries — `width`, `height`, `steps`, `cfg_scale`,
            // `sampling_method` are the form's defaults when the row sets them.
            "modes": row.map(|m| m.modes()),
            "args": row.map(|m| m.args.clone()),
            "notes": e.notes,
            // Runtime facts. Absent for a cloud alias, which has no container.
            "state": rt.map(|r| r.state.as_str()),
            "warnings": rt.map(|r| r.warnings.clone()).unwrap_or_default(),
            "image_capabilities": rt.and_then(|r| r.image_capabilities.clone()),
        }));
    }
    Json(json!({
        "models": models,
        "models_dir": snap.settings.image.models_dir,
        "hold": snap.settings.hold.active,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// The routes under test
// ---------------------------------------------------------------------------

/// The `x-lmgw-*` headers the gateway stamped on its own answer — which model
/// really served the request (a hold fallback names itself here), which
/// upstream, how long it took. Relayed beside the body because the lab wraps
/// the success case and would otherwise drop them.
fn gateway_headers(h: &HeaderMap) -> Value {
    let mut out = serde_json::Map::new();
    for (name, value) in h {
        let name = name.as_str();
        if let (true, Ok(v)) = (name.starts_with("x-lmgw-"), value.to_str()) {
            out.insert(name.to_string(), json!(v));
        }
    }
    Value::Object(out)
}

/// Run one in-process dispatch and render its answer for the page.
///
/// A success is wrapped — `{endpoint, request, latency_ms, headers, response}`
/// — because the page shows what was sent next to what came back. A failure is
/// **not**: the status, the headers and the body go back exactly as the handler
/// produced them, so what the lab displays is the envelope a client would have
/// received, down to the `code`. A lab that re-wrapped errors would be showing
/// its own error handling rather than the gateway's.
async fn relay(endpoint: &str, request: Value, resp: Response, latency_ms: u64) -> Response {
    let status = resp.status();
    let (parts, body) = resp.into_parts();
    // Unbounded, like the route: a 4096² PNG is the size it is.
    let raw =
        match axum::body::to_bytes(body, usize::MAX).await {
            Ok(b) => b,
            Err(e) => return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": { "message": format!("reading the image response: {e}") } })),
            )
                .into_response(),
        };
    if !status.is_success() {
        return (parts, raw).into_response();
    }
    let parsed: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    Json(json!({
        "endpoint": endpoint,
        "request": request,
        "latency_ms": latency_ms,
        "headers": gateway_headers(&parts.headers),
        "response": parsed,
    }))
    .into_response()
}

/// A form that does not describe a request at all — a missing prompt, a
/// step count that is not a number. Shaped like the gateway's envelope so the
/// page has one error renderer, and 400 because it never left the browser.
fn bad_form(message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(
            json!({ "error": { "message": message, "type": "invalid_request_error",
                               "code": "image_lab_form" } }),
        ),
    )
        .into_response()
}

/// `POST /image-lab/api/generate` — the page's structured form, turned into the
/// exact `/v1/images/generations` body and handed to that route's handler.
pub async fn generate(
    State(state): State<SharedState>,
    Json(form): Json<ImageGenForm>,
) -> Response {
    let body = match form.generation_body() {
        Ok(b) => b,
        Err(e) => return bad_form(e),
    };
    let started = std::time::Instant::now();
    let resp = proxy::handle_image_generation(state, RequestCtx::default(), body.clone()).await;
    let ms = started.elapsed().as_millis() as u64;
    relay(GENERATIONS_ENDPOINT, body, resp, ms).await
}

/// `POST /image-lab/api/edit` — the browser's upload (`form` as JSON, `image`,
/// and optionally `mask`) re-assembled into the one multipart body
/// `/v1/images/edits` accepts, and handed to that route's handler.
///
/// The re-assembly is the point: the edits route is multipart-only (§14, WP2)
/// and reads `model` out of the form to resolve the alias, so the page cannot
/// simply post its JSON and attach files. It uploads the parts; this builds the
/// request a Python client would have built.
pub async fn edit(State(state): State<SharedState>, mut mp: Multipart) -> Response {
    let mut form: Option<ImageGenForm> = None;
    let mut files: Vec<(String, String, Option<String>, Vec<u8>)> = Vec::new();
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
                let file_name = field
                    .file_name()
                    .map(String::from)
                    // Something has to go in the relayed part's `filename`.
                    // The field name is the honest stand-in: sd-server reads
                    // the bytes, not the name.
                    .unwrap_or_else(|| name.clone());
                let ct = field.content_type().map(String::from);
                match field.bytes().await {
                    Ok(b) => files.push((name, file_name, ct, b.to_vec())),
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
    let Some(form) = form else {
        return bad_form("no `form` field in the upload".into());
    };
    let fields = match form.edit_fields() {
        Ok(f) => f,
        Err(e) => return bad_form(e),
    };
    if !files.iter().any(|(n, ..)| n == "image") {
        return bad_form("an edit needs an image to edit — pick one".into());
    }

    // The summary the page shows beside the result: the text fields as sent,
    // plus each file part by name and size. Built from the same `fields` the
    // body is, so it cannot describe a request that was not made.
    let request = json!({
        "fields": fields.iter().map(|(k, v)| json!({"name": k, "value": v})).collect::<Vec<_>>(),
        "files": files
            .iter()
            .map(|(n, f, ct, b)| json!({"name": n, "filename": f, "type": ct, "bytes": b.len()}))
            .collect::<Vec<_>>(),
    });

    let boundary = format!("lmgwimagelab{}", super::rand_hex32());
    let body = multipart_body(&boundary, &fields, &files);
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
        Err(e) => return bad_form(format!("building the multipart request: {e}")),
    };
    let started = std::time::Instant::now();
    let resp = proxy::handle_image_edit(state, RequestCtx::default(), req).await;
    let ms = started.elapsed().as_millis() as u64;
    relay(EDITS_ENDPOINT, request, resp, ms).await
}

/// Serialize one `multipart/form-data` body: the text fields in the order the
/// form lists them, then the file parts.
///
/// Hand-written because the destination is an in-process [`axum::extract::Request`]
/// rather than an outgoing reqwest call — `reqwest::multipart::Form` can only
/// be handed to a `RequestBuilder`, and `proxy::handle_image_edit` takes the
/// request a client would have sent.
fn multipart_body(
    boundary: &str,
    fields: &[(String, String)],
    files: &[(String, String, Option<String>, Vec<u8>)],
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        out.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    for (name, file_name, ct, bytes) in files {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        out.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{name}\"; filename=\"{}\"\r\n",
                // Both characters that can end a quoted string early: a `"`
                // closes it, and a trailing `\` escapes the quote that would
                // have. Dropped rather than escaped, because the name is a
                // label here — the bytes are the upload.
                file_name.replace(['"', '\\'], "")
            )
            .as_bytes(),
        );
        let ct = ct.as_deref().unwrap_or("application/octet-stream");
        out.extend_from_slice(format!("Content-Type: {ct}\r\n\r\n").as_bytes());
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_multipart_carries_every_field_and_file() {
        let fields = vec![
            ("model".to_string(), "image/z".to_string()),
            ("prompt".to_string(), "make it night".to_string()),
        ];
        let files = vec![(
            "image".to_string(),
            "in.png".to_string(),
            Some("image/png".to_string()),
            b"\x89PNGfake".to_vec(),
        )];
        let body = multipart_body("BOUND", &fields, &files);
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with("--BOUND\r\n"));
        assert!(text.contains("name=\"model\"\r\n\r\nimage/z\r\n"));
        assert!(text.contains("name=\"prompt\"\r\n\r\nmake it night\r\n"));
        assert!(text.contains("name=\"image\"; filename=\"in.png\""));
        assert!(text.contains("Content-Type: image/png"));
        assert!(text.ends_with("--BOUND--\r\n"));
    }

    /// Neither a quote nor a backslash in a file name can close the header
    /// and inject a part of its own — a trailing `\` would escape the quote
    /// that ends `filename="…"`, which is the same break-out by another route.
    #[test]
    fn a_quoted_or_escaped_file_name_cannot_break_out_of_its_header() {
        let files = vec![(
            "image".to_string(),
            "a\"; name=\"model".to_string(),
            None,
            vec![1, 2, 3],
        )];
        let body = multipart_body("BOUND", &[], &files);
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("filename=\"a; name=model\""));
        assert_eq!(text.matches("name=\"image\"").count(), 1);

        let files = vec![(
            "image".to_string(),
            "shot\\".to_string(),
            None,
            vec![1, 2, 3],
        )];
        let text = String::from_utf8_lossy(&multipart_body("BOUND", &[], &files)).to_string();
        assert!(text.contains("filename=\"shot\"\r\n"), "{text}");
        assert!(!text.contains('\\'), "{text}");
    }
}
