//! The POST's body (calls design §2.1): the offer as `application/sdp`, or
//! `multipart/form-data` with the part `sdp` and an optional `session`, a
//! JSON object the session applies as its first update (§2.2). Other parts
//! are ignored; anything else is 415. The body is bounded by `max_body_mb`
//! (`server::body_limit_mw`), whose trip is its 413. The `session` part is
//! a session event, so `realtime.max_message_mb` bounds it as it bounds a
//! WebSocket's `session.update` (0: no bound): read up to the bound, and
//! refused past it naming the setting.

use axum::extract::multipart::{Field, MultipartError};
use axum::extract::{FromRequest, Multipart, Request};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{Map, Value};

use super::{refuse, refuse_param};
use crate::state::SharedState;

/// How the offer comes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Shape {
    /// `application/sdp`: the offer alone.
    Sdp,
    /// `multipart/form-data`: `sdp` and `session`.
    Multipart,
}

/// The body read.
pub(super) struct Posted {
    pub sdp: String,
    /// The `session` part, when there is one.
    pub session: Option<Map<String, Value>>,
}

/// The shape `headers` announce, or the 415.
pub(super) fn shape(headers: &HeaderMap) -> Result<Shape, Response> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let mime = ct.split(';').next().unwrap_or_default().trim();
    if mime.eq_ignore_ascii_case("application/sdp") {
        Ok(Shape::Sdp)
    } else if mime.eq_ignore_ascii_case("multipart/form-data") {
        Ok(Shape::Multipart)
    } else {
        Err(refuse(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            format!(
                "POST /v1/realtime/calls takes the SDP offer as application/sdp, or as \
                 multipart/form-data with the parts sdp and session; this request's \
                 Content-Type is '{ct}'"
            ),
        ))
    }
}

/// Read the body in `shape`, or its refusal.
pub(super) async fn read(
    state: &SharedState,
    req: Request,
    shape: Shape,
) -> Result<Posted, Response> {
    match shape {
        Shape::Sdp => {
            let bytes = match axum::body::Bytes::from_request(req, state).await {
                Ok(b) => b,
                Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                    return Err(too_large(state))
                }
                Err(e) => return Err(unreadable(&e.body_text())),
            };
            let sdp = String::from_utf8(bytes.to_vec())
                .map_err(|_| invalid_sdp("the offer is not UTF-8 text"))?;
            Ok(Posted { sdp, session: None })
        }
        Shape::Multipart => multipart(state, req).await,
    }
}

async fn multipart(state: &SharedState, req: Request) -> Result<Posted, Response> {
    let mut form = Multipart::from_request(req, state)
        .await
        .map_err(|e| unreadable(&e.body_text()))?;
    let (mut sdp, mut session) = (None, None);
    loop {
        let field = match form.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(field_failed(state, &e)),
        };
        match field.name().unwrap_or_default() {
            "sdp" => {
                sdp = Some(match field.text().await {
                    Ok(t) => t,
                    Err(e) => return Err(field_failed(state, &e)),
                })
            }
            "session" => session = Some(session_field(state, field).await?),
            _ => {}
        }
    }
    let Some(sdp) = sdp else {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "missing_sdp",
            "the multipart body has no part named sdp: send the SDP offer in it".into(),
        ));
    };
    Ok(Posted { sdp, session })
}

/// A multipart read that failed: the body over `max_body_mb`, or a body
/// that does not parse.
fn field_failed(state: &SharedState, e: &MultipartError) -> Response {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        too_large(state)
    } else {
        unreadable(&e.body_text())
    }
}

/// The `session` part read up to `realtime.max_message_mb` (module doc),
/// then [`session_part`].
async fn session_field(
    state: &SharedState,
    mut field: Field<'_>,
) -> Result<Map<String, Value>, Response> {
    let mb = state.snapshot().settings.realtime.max_message_mb;
    let bound = (mb > 0).then_some((mb as usize) << 20);
    let mut read = Vec::new();
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                if bound.is_some_and(|b| read.len() + chunk.len() > b) {
                    return Err(refuse_param(
                        StatusCode::BAD_REQUEST,
                        "invalid_value",
                        format!(
                            "the session part is over realtime.max_message_mb ({mb} MiB), which \
                             bounds a session event on a WebSocket and on a call alike"
                        ),
                        "session",
                    ));
                }
                read.extend_from_slice(&chunk);
            }
            Ok(None) => return session_part(&read),
            Err(e) => return Err(field_failed(state, &e)),
        }
    }
}

/// The `session` part: any JSON object, which the session reads as its
/// first update (§2.2); anything else is 400 `invalid_value`.
fn session_part(bytes: &[u8]) -> Result<Map<String, Value>, Response> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(o)) => Ok(o),
        Ok(_) => Err(not_an_object("it is JSON, but not an object")),
        Err(e) => Err(not_an_object(&format!("it does not parse: {e}"))),
    }
}

fn not_an_object(why: &str) -> Response {
    refuse_param(
        StatusCode::BAD_REQUEST,
        "invalid_value",
        format!(
            "the session part must be a JSON object, the session as session.update takes it, \
             and {why}"
        ),
        "session",
    )
}

/// 400 `invalid_sdp`, quoting why.
pub(super) fn invalid_sdp(why: &str) -> Response {
    refuse(
        StatusCode::BAD_REQUEST,
        "invalid_sdp",
        format!("the SDP offer does not parse: {why}"),
    )
}

/// A body that could not be read as its shape says.
fn unreadable(why: &str) -> Response {
    refuse(
        StatusCode::BAD_REQUEST,
        "bad_request",
        format!("the body could not be read: {why}"),
    )
}

/// The body-limit refusal (`max_body_mb`), in the shape every `/v1` route
/// gives it.
fn too_large(state: &SharedState) -> Response {
    let e = crate::error::GatewayError::BodyTooLarge {
        max_mb: state.snapshot().settings.max_body_mb,
    };
    (e.http_status(), Json(e.to_openai_json())).into_response()
}
