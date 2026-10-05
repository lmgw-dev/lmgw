//! Multipart uploads, shared by `/v1/audio/transcriptions` and
//! `/v1/images/edits` (§6)

use axum::extract::FromRequest;
use axum::http::header;
use bytes::Bytes;

use crate::config::Route;
use crate::error::GatewayError;
use crate::state::SharedState;

/// One buffered field of a `multipart/form-data` request: a form value, or an
/// upload held whole (a clone shares the upload's bytes).
#[derive(Clone)]
pub(super) enum MultipartField {
    Text(String, String),
    File(String, String, Option<String>, Bytes),
}

/// Buffer a multipart body and pick out the `model` field.
///
/// The upload is held in memory because `model` may arrive *after* the file
/// and reqwest builds the form eagerly — peak memory therefore tracks the
/// upload size, which is unbounded by design (§6: no invented body cap).
///
/// The error carries the best-known alias beside it (`"?"` until the `model`
/// field has been seen), so a failure halfway through the upload is still
/// logged against the model the client was asking for.
pub(super) async fn buffer_multipart(
    state: &SharedState,
    req: axum::extract::Request,
) -> Result<(Vec<MultipartField>, String), (String, GatewayError)> {
    let mut mp = match axum::extract::Multipart::from_request(req, state).await {
        Ok(mp) => mp,
        Err(e) => return Err(("?".into(), GatewayError::BadRequest(e.to_string()))),
    };
    let mut fields: Vec<MultipartField> = Vec::new();
    let mut alias = String::new();
    let named = |alias: &String| {
        if alias.is_empty() {
            "?".to_string()
        } else {
            alias.clone()
        }
    };
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                let err = GatewayError::BadRequest(format!("invalid multipart body: {e}"));
                return Err((named(&alias), err));
            }
        };
        let name = field.name().unwrap_or_default().to_string();
        match field.file_name() {
            Some(file_name) => {
                let ct = field.content_type().map(String::from);
                let file_name = file_name.to_string();
                let bytes = match field.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        let err = GatewayError::BadRequest(format!("reading upload: {e}"));
                        return Err((named(&alias), err));
                    }
                };
                fields.push(MultipartField::File(name, file_name, ct, bytes));
            }
            None => {
                let text = match field.text().await {
                    Ok(t) => t,
                    Err(e) => {
                        let err = GatewayError::BadRequest(format!("reading form field: {e}"));
                        return Err((named(&alias), err));
                    }
                };
                if name == "model" {
                    alias = text.clone();
                }
                fields.push(MultipartField::Text(name, text));
            }
        }
    }
    if alias.is_empty() {
        let err = GatewayError::BadRequest("missing 'model' form field".into());
        return Err(("?".into(), err));
    }
    Ok((fields, alias))
}

/// Re-encode buffered fields for one attempt, with `model` rewritten to the
/// concrete upstream id.
///
/// Per attempt because a `Form` is consumed by the send and the §3.2 retry
/// needs a second one. `Bytes` → `reqwest::Body` is a refcount bump, so the
/// upload itself is never copied.
pub(super) fn reencode_multipart(
    fields: &[MultipartField],
    route: &Route,
) -> reqwest::multipart::Form {
    let mut form = reqwest::multipart::Form::new();
    for f in fields {
        match f {
            MultipartField::Text(name, text) => {
                let text = if name == "model" {
                    route.upstream_model.clone()
                } else {
                    text.clone()
                };
                form = form.text(name.clone(), text);
            }
            MultipartField::File(name, file_name, ct, bytes) => {
                let len = bytes.len() as u64;
                let make = || {
                    reqwest::multipart::Part::stream_with_length(
                        reqwest::Body::from(bytes.clone()),
                        len,
                    )
                    .file_name(file_name.clone())
                };
                let part = match ct.as_deref() {
                    Some(ct) => make().mime_str(ct).unwrap_or_else(|_| make()),
                    None => make(),
                };
                form = form.part(name.clone(), part);
            }
        }
    }
    form
}

/// Whether a request's `Content-Type` is `multipart/form-data`.
///
/// The media type is compared case-insensitively and without its parameters,
/// because RFC 9110 says it is: `Multipart/Form-Data; boundary=…` is the same
/// type as the lowercase spelling, and a client that sends it (Java's
/// HttpClient and a few Go libraries do) was reading an empty JSON body error
/// for a perfectly good upload.
pub(super) fn is_multipart_content_type(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.split(';').next().unwrap_or_default().trim())
        .is_some_and(|media| media.eq_ignore_ascii_case("multipart/form-data"))
}
