//! The rest of the resource: `GET` and `DELETE /v1/responses/{id}`, and
//! `GET /v1/responses/{id}/input_items` (openai egress design §6).
//!
//! An id is looked up in lmgw's own store first, then among the responses an
//! upstream made natively ([`crate::store::NativeResponse`]): such an id is
//! forwarded to that upstream, query string and all, and its answer relayed
//! as sent. An id in neither place is lmgw's 404. Both lookups read as the
//! caller ([`super::reader`]): another key's response answers exactly as an
//! unknown id does.

use axum::body::Body;
use axum::extract::Path;
use axum::http::{header, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::egress::apply_bearer_auth;
use crate::error::GatewayError;
use crate::ingress::responses as wire;
use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::store::{self, NativeResponse};

fn error(e: GatewayError) -> Response {
    (e.http_status(), axum::Json(wire::serialize_error(&e))).into_response()
}

fn not_found(id: &str) -> Response {
    error(GatewayError::NotFound(format!(
        "no stored response '{id}' — it may have been evicted, or storing was off \
         when it was created"
    )))
}

/// Where `id` is, for this caller.
enum Found {
    Stored(store::StoredResponse),
    Native(NativeResponse),
    Nowhere,
}

async fn find(state: &SharedState, ctx: &RequestCtx, id: &str) -> Result<Found, Response> {
    let reader = super::reader(ctx);
    if let Some(r) = store::get_response(&state.db, id, reader)
        .await
        .map_err(error)?
    {
        return Ok(Found::Stored(r));
    }
    Ok(
        match store::get_native_response(&state.db, id, reader)
            .await
            .map_err(error)?
        {
            Some(n) => Found::Native(n),
            None => Found::Nowhere,
        },
    )
}

/// `GET /v1/responses/{id}` — the response object exactly as it was served,
/// or as its upstream serves it now.
pub async fn get_response(
    state: SharedState,
    ctx: RequestCtx,
    Path(id): Path<String>,
    query: Option<&str>,
) -> Response {
    match find(&state, &ctx, &id).await {
        Err(resp) => resp,
        Ok(Found::Stored(r)) => match serde_json::from_str::<Value>(&r.body) {
            Ok(v) => (StatusCode::OK, axum::Json(v)).into_response(),
            Err(e) => error(GatewayError::Internal(format!(
                "stored response {id} is unreadable: {e}"
            ))),
        },
        Ok(Found::Native(n)) => forward(&state, &n, Method::GET, None, query)
            .await
            .unwrap_or_else(|r| r),
        Ok(Found::Nowhere) => not_found(&id),
    }
}

/// `DELETE /v1/responses/{id}`.
///
/// Deletes the one response, not its chain — that is what the API means, and
/// the Responses tab is where a whole conversation is dropped. Its descendants
/// keep their `chain_id`, so the conversation still lists and evicts as a unit.
/// A native response is deleted by its upstream, and forgotten here once that
/// succeeded, or once the upstream says it has no such response.
pub async fn delete_response(
    state: SharedState,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> Response {
    match find(&state, &ctx, &id).await {
        Err(resp) => resp,
        Ok(Found::Stored(_)) => {
            match store::delete_response(&state.db, &id, super::reader(&ctx)).await {
                Ok(true) => (
                    StatusCode::OK,
                    axum::Json(json!({"id": id, "object": "response", "deleted": true})),
                )
                    .into_response(),
                Ok(false) => not_found(&id),
                Err(e) => error(e),
            }
        }
        Ok(Found::Native(n)) => match forward(&state, &n, Method::DELETE, None, None).await {
            Ok(resp) => {
                // 404: the upstream no longer has it either.
                if resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND {
                    if let Err(e) = store::delete_native_response(&state.db, &id).await {
                        tracing::warn!("forgetting native response {id}: {e}");
                    }
                }
                resp
            }
            Err(resp) => resp,
        },
        Ok(Found::Nowhere) => not_found(&id),
    }
}

/// `GET /v1/responses/{id}/input_items` — the `input` this response was given.
pub async fn list_input_items(
    state: SharedState,
    ctx: RequestCtx,
    Path(id): Path<String>,
    query: Option<&str>,
) -> Response {
    let r = match find(&state, &ctx, &id).await {
        Err(resp) => return resp,
        Ok(Found::Stored(r)) => r,
        Ok(Found::Native(n)) => {
            return forward(&state, &n, Method::GET, Some("input_items"), query)
                .await
                .unwrap_or_else(|r| r)
        }
        Ok(Found::Nowhere) => return not_found(&id),
    };
    let items: Vec<Value> = serde_json::from_str(&r.input_items).unwrap_or_default();
    let first = items.first().and_then(|i| i.get("id")).cloned();
    let last = items.last().and_then(|i| i.get("id")).cloned();
    (
        StatusCode::OK,
        axum::Json(json!({
            "object": "list",
            "data": items,
            "first_id": first,
            "last_id": last,
            // Everything is returned in one page: these are one turn's items on
            // a single-user gateway, not a cursor-worthy collection.
            "has_more": false,
        })),
    )
        .into_response()
}

/// `method` on `{base}/responses/{id}[/{sub}][?query]` of the upstream that
/// made `n`, its answer relayed as sent: status, content type and body, a
/// stream (`GET …?stream=true`) included. The send is bounded by the
/// upstream's `request_timeout` where one is set. `Err` is lmgw's own answer
/// when the forward could not be made.
async fn forward(
    state: &SharedState,
    n: &NativeResponse,
    method: Method,
    sub: Option<&str>,
    query: Option<&str>,
) -> Result<Response, Response> {
    let snap = state.snapshot();
    let Some(up) = snap.upstreams.get(&n.upstream_id).filter(|u| u.enabled) else {
        return Err(error(GatewayError::NotFound(format!(
            "response '{}' was made by an upstream that is disabled now, so it cannot be \
             reached through this gateway",
            n.id
        ))));
    };
    let mut url = reqwest::Url::parse(up.base()).map_err(|e| {
        error(GatewayError::Internal(format!(
            "upstream '{}' has an unusable base URL: {e}",
            up.name
        )))
    })?;
    {
        let Ok(mut path) = url.path_segments_mut() else {
            return Err(error(GatewayError::Internal(format!(
                "upstream '{}' has a base URL that takes no path",
                up.name
            ))));
        };
        path.pop_if_empty().push("responses").push(&n.id);
        if let Some(sub) = sub {
            path.push(sub);
        }
    }
    url.set_query(query.filter(|q| !q.is_empty()));
    let rb = apply_bearer_auth(state.http.request(method, url), up);
    let sent = match up.request_timeout() {
        Some(d) => tokio::time::timeout(d, rb.send())
            .await
            .map_err(|_| error(GatewayError::Timeout))?,
        None => rb.send().await,
    };
    let resp = sent.map_err(|e| error(GatewayError::from(e)))?;
    let mut out = Response::builder().status(resp.status().as_u16());
    if let Some(ct) = resp.headers().get(header::CONTENT_TYPE) {
        out = out.header(header::CONTENT_TYPE, ct.clone());
    }
    Ok(out
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response()))
}
