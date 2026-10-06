//! `POST /tokenize`: llama.cpp's own token endpoint, for llama.cpp clients
//! (api-docs design §5.3).
//!
//! A thin adapter onto a llama-server backend: the client's object goes to
//! the backend's own `/tokenize` with `model` rewritten to the upstream's, and
//! the answer — token ids, with pieces when asked — comes back verbatim. Only
//! a llama-server can return ids of the tokenizer it actually runs; every
//! other backend is refused with `501` and pointed at `POST /v1/count_tokens`,
//! which counts on all of them. Ids from a tokenizer the backend may not use
//! would be a hidden approximation (§10 choice 6).
//!
//! Registered here and merged at the router's root rather than in
//! `server.rs`: it is not under `/v1`, and every route `server.rs` registers
//! is mounted there (§0 finding 6, `tests/it/route_walk.rs`).

use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::config::Protocol;
use crate::egress::with_timeout;
use crate::error::GatewayError;
use crate::gate::{Opened, RouteCheck};
use crate::ingress::ClientProto;
use crate::principal::Cap;
use crate::state::SharedState;

use super::*;

/// What a `/tokenize` without `model` is told. llama-server's router mode
/// requires the field for the same reason lmgw does.
const MODEL_REQUIRED: &str = "'model' is required: lmgw routes /tokenize by model name, as \
                              llama-server's router mode does — see GET /v1/models";

/// The route, with the `/v1` JSON routes' body bound and capability
/// (principals §3.2: `Inference`).
pub fn routes(state: &SharedState) -> Router<SharedState> {
    Router::new()
        .route("/tokenize", post(tokenize))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::server::body_limit_mw,
        ))
        .layer(DefaultBodyLimit::disable())
        .route_layer(crate::server::require(state, Cap::Inference))
}

/// The handler (module doc). Like the other counters a cheap success writes
/// no request-log row; a failure — a key refused by its alias scope included
/// (api-docs design §5.4) — and a count that had to bring its model up write
/// one, and so does a client that went away first ([`Unanswered`]). No budget
/// applies (§12 entry 15).
///
/// The body is read whatever its `Content-Type` says, as llama-server reads
/// it (review R1 #6): clients that post JSON without the header, or with
/// `text/plain`, work against llama-server and must against lmgw too.
async fn tokenize(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    // The log row's `ingress_proto` has no llama.cpp spelling; `/tokenize` is
    // an OpenAI-family route like the rest of lmgw's own counters.
    let mut row = counter_row(&state, ClientProto::OpenaiChat, &ctx, &Value::Null);
    let body = match read_body(&state, body) {
        Ok(b) => b,
        Err(e) => {
            row.failed(&e).await;
            return llama_error_of(&e);
        }
    };
    let Some(alias) = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        row.failed(&GatewayError::BadRequest(MODEL_REQUIRED.into()))
            .await;
        return llama_error(StatusCode::BAD_REQUEST, MODEL_REQUIRED, "bad_request");
    };
    row.alias(&alias);
    if let Some(e) = counter_policy(&state, &ctx, &alias) {
        row.failed(&e).await;
        return llama_error_of(&e);
    }
    let Opened {
        route,
        hold,
        headers,
    } = match admit_counter(
        &state,
        &alias,
        RouteCheck::Text("/tokenize"),
        false,
        Some(&mut row),
    )
    .await
    {
        Ok(o) => o,
        Err((headers, e)) => {
            row.failed(&e).await;
            return headers.stamp(llama_error_of(&e));
        }
    };
    // llama_cpp implies kind llama_server (llama.cpp egress design, decision 11).
    if route.upstream.protocol != Protocol::LlamaCpp {
        let message = format!(
            "model '{alias}' is served by {}/{} upstream '{}', which cannot return token ids; \
             POST /v1/count_tokens counts tokens on every backend",
            route.upstream.protocol.as_str(),
            route.upstream.kind.as_str(),
            route.upstream.name,
        );
        row.answered(501, Some(("unsupported", message.clone())))
            .await;
        return headers.stamp(llama_error(
            StatusCode::NOT_IMPLEMENTED,
            &message,
            "unsupported",
        ));
    }

    let timeout = route.upstream.request_timeout();
    let sent = crate::vram::send_local(hold.as_ref(), &route, None, |r| {
        let mut forwarded = body.clone();
        forwarded["model"] = json!(r.upstream_model);
        Ok(with_timeout(
            crate::egress::llama_cpp::count::tokenize_request(&state.http, &r.upstream, &forwarded),
            timeout,
        ))
    })
    .await;
    let resp = match sent {
        Ok(r) => r,
        Err(e) => {
            row.failed(&e).await;
            return headers.stamp(llama_error_of(&e));
        }
    };
    // Relayed as the backend answered — its ids, and its own errors too: a
    // llama.cpp client reads llama.cpp's refusals already.
    let status = resp.status();
    let content_type = resp.headers().get(header::CONTENT_TYPE).cloned();
    let bytes = resp.bytes().await;
    // Read to the end: the container no longer has to stay up for it.
    drop(hold);
    let bytes = match bytes {
        Ok(b) => b,
        Err(e) => {
            let e = GatewayError::from(e);
            row.failed(&e).await;
            return headers.stamp(llama_error_of(&e));
        }
    };
    // Except the upstream refusing lmgw's own credential (review R1 #4): that
    // is the gateway's configuration, not the client's request, and every
    // other route answers it with a 502 ([`GatewayError::http_status`]).
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        let e =
            crate::egress::for_protocol(route.upstream.protocol).map_error(status.as_u16(), &bytes);
        row.failed(&e).await;
        return headers.stamp(llama_error_of(&e));
    }
    // The backend's own refusal, relayed as it is, is a failed count all the
    // same: its row says what the backend said.
    if !status.is_success() {
        let e =
            crate::egress::for_protocol(route.upstream.protocol).map_error(status.as_u16(), &bytes);
        row.answered(status.as_u16(), Some((e.kind(), e.to_string())))
            .await;
    } else {
        row.answered(status.as_u16(), None).await;
    }
    let mut out = Response::builder().status(status.as_u16());
    if let Some(ct) = content_type {
        out = out.header(header::CONTENT_TYPE, ct);
    }
    headers.stamp(
        out.body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response()),
    )
}

/// The client's object, from whatever bytes it posted: the body limit's
/// `413` when the stream tripped it (`body_limit_mw`'s `Limited` wrapper), a
/// `400` when the bytes are not a JSON value.
fn read_body(
    state: &SharedState,
    body: Result<Bytes, BytesRejection>,
) -> Result<Value, GatewayError> {
    let bytes = body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            GatewayError::BodyTooLarge {
                max_mb: state.snapshot().settings.max_body_mb,
            }
        } else {
            GatewayError::BadRequest(e.body_text())
        }
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|e| GatewayError::BadRequest(format!("the body is not JSON: {e}")))
}

/// A [`GatewayError`] in llama.cpp's error shape ([`llama_error`]).
fn llama_error_of(e: &GatewayError) -> Response {
    llama_error(e.http_status(), &e.to_string(), e.code())
}

/// llama.cpp's error shape (api-docs design §5.3):
/// `{"error": {"code": <status>, "message", "type", "lmgw_code"}}`.
///
/// `type` is llama.cpp's word for the status. `lmgw_code` is lmgw's own
/// [`GatewayError::code`], so a client that checks for `gpu_hold` still can.
///
/// Three refusals are answered before this route's handler runs, by layers
/// every `/v1` route shares, and so in the `/v1` OpenAI dialect instead
/// (review R1 #5; §5.3): the gate's 401 and 429 (credential, rate), its 403
/// (a principal without the `Inference` capability), and the 413 for a body
/// whose declared `Content-Length` is over `max_body_mb`. A body that trips
/// the limit without declaring its length is refused here, in this shape.
pub(crate) fn llama_error(status: StatusCode, message: &str, lmgw_code: &str) -> Response {
    let kind = match status.as_u16() {
        400 | 413 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        501 => "not_supported_error",
        429 | 503 => "unavailable_error",
        _ => "server_error",
    };
    (
        status,
        Json(json!({"error": {
            "code": status.as_u16(),
            "message": message,
            "type": kind,
            "lmgw_code": lmgw_code,
        }})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_of(resp: Response) -> (u16, Value) {
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn llama_error_shape_by_status() {
        for (status, kind) in [
            (400, "invalid_request_error"),
            (413, "invalid_request_error"),
            (422, "invalid_request_error"),
            (401, "authentication_error"),
            (403, "permission_error"),
            (404, "not_found_error"),
            (501, "not_supported_error"),
            (429, "unavailable_error"),
            (503, "unavailable_error"),
            (500, "server_error"),
            (502, "server_error"),
        ] {
            let resp = llama_error(StatusCode::from_u16(status).unwrap(), "why", "code_x");
            let (got, body) = body_of(resp).await;
            assert_eq!(got, status);
            assert_eq!(
                body,
                json!({"error": {"code": status, "message": "why", "type": kind,
                                 "lmgw_code": "code_x"}}),
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn a_hold_keeps_its_own_code() {
        let e = GatewayError::GpuHold {
            model: "m".into(),
            detail: "held".into(),
        };
        let (status, body) = body_of(llama_error_of(&e)).await;
        assert_eq!(status, 503);
        assert_eq!(body["error"]["code"], 503);
        assert_eq!(body["error"]["type"], "unavailable_error");
        assert_eq!(body["error"]["lmgw_code"], "gpu_hold");
    }
}
