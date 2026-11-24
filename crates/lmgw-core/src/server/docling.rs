//! The docling-serve-compatible routes over HTTP (document-conversion design
//! §7.4.1): the convert routes (file and source, sync and async, batch),
//! the task routes ([`tasks`]: poll, result, status WebSocket), `/health`,
//! `/ready` and `/version` at the root, and a named 501 for the routes not
//! built yet (`/v1/chunk/*`, `/v1/capabilities`, `/v1/clear/*`).
//!
//! The mapping is `extract::convert::docling_serve`'s; this is the HTTP
//! half: the bodies read under `max_body_mb` (a JSON body's `http` sources
//! fetched once its options are judged, `docling_serve::web`), the answers
//! and their headers (`x-lmgw-option-notes`, `x-lmgw-documents`), the usage
//! row with its `convert_calls` record, and docling's error shape
//! (`{"detail": …}`).
//! docling-serve's `X-Api-Key` is one of the spellings lmgw's resolver
//! takes, so a caller keeps its header and only the key changes; the status
//! WebSocket takes docling's `?api_key=` too (`ws_query_key`).

use axum::extract::{FromRequest, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use lmgw_api_types::documents::docling::{
    ConvertSourcesRequest, DoclingError, HealthCheckResponse, VersionResponse,
};

use super::documents::{self as native, row::Row};
use crate::extract::convert::budget::{answers, upload};
use crate::extract::convert::docling_serve::answer::{self, Answer};
use crate::extract::convert::docling_serve::{self as adapter, sources, web, Parsed, Refused};
use crate::extract::convert::source_fetch::resolve::System;
use crate::extract::convert::{pool, report};
use crate::principal::Cap;
use crate::proxy::RequestCtx;
use crate::state::SharedState;

mod form;
pub mod tasks;

/// `x-lmgw-option-notes`.
pub const OPTION_NOTES: &str = "x-lmgw-option-notes";
/// `x-lmgw-documents`.
pub const DOCUMENTS: &str = "x-lmgw-documents";

/// The WebSocket's path prefix, where docling's `?api_key=` is a credential.
pub const WS_PREFIX: &str = "/v1/status/ws/";

/// An error in docling's shape: the one place these routes' errors become
/// a body, so its `detail` is bounded here (`convert::quoted::outlet_detail`).
pub fn error(status: u16, detail: serde_json::Value, code: Option<&str>) -> Response {
    let code_s = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    (
        code_s,
        Json(DoclingError {
            detail: crate::extract::convert::quoted::outlet_detail(detail),
            code: code.map(String::from),
        }),
    )
        .into_response()
}

fn refused(r: &Refused) -> Response {
    error(r.status, r.detail.clone(), Some(r.code))
}

/// The `max_body_mb` refusal on these routes, in docling's shape.
pub fn body_limit(max_mb: u32, json: bool) -> Response {
    let e = native::body_limit(max_mb, json);
    error(413, e.to_string().into(), Some("body_limit"))
}

/// docling's `?api_key=` on the status WebSocket, read by the principal
/// middleware for that path only (a page cannot set a header on a
/// WebSocket).
pub fn ws_query_key(uri: &axum::http::Uri) -> Option<String> {
    if !uri.path().starts_with(WS_PREFIX) {
        return None;
    }
    let q = uri.query()?;
    url_query_value(q, "api_key").filter(|k| !k.is_empty())
}

fn url_query_value(q: &str, name: &str) -> Option<String> {
    reqwest::Url::parse(&format!("http://h/?{q}"))
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

fn with_header(mut r: Response, name: &'static str, value: Option<&str>) -> Response {
    if let Some(v) = value.and_then(|v| HeaderValue::from_str(v).ok()) {
        r.headers_mut().insert(name, v);
    }
    r
}

/// A conversion answer with its headers, written under its answer `share`
/// (`native::send`).
pub async fn respond(
    a: Answer,
    counted: &str,
    notes: Option<&str>,
    share: answers::Share,
    to: native::send::To,
) -> (u16, Response) {
    let (status, r) = match a {
        Answer::Json(body) => (
            200,
            native::send::json(StatusCode::OK, body, share, to).await,
        ),
        Answer::Zip(bytes) => {
            let (body, deadline) = native::send::body(Bytes::from(bytes), share, to).await;
            let mut r = (
                [
                    (axum::http::header::CONTENT_TYPE, "application/zip"),
                    (
                        axum::http::header::CONTENT_DISPOSITION,
                        "attachment; filename=\"converted_docs.zip\"",
                    ),
                ],
                body,
            )
                .into_response();
            r.headers_mut().insert(
                axum::http::HeaderName::from_static(native::send::WRITE_DEADLINE),
                deadline,
            );
            (200, r)
        }
        Answer::Failure(f) => (
            422,
            native::send::json(StatusCode::UNPROCESSABLE_ENTITY, f, share, to).await,
        ),
    };
    let r = with_header(r, DOCUMENTS, Some(counted));
    (status, with_header(r, OPTION_NOTES, notes))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Sync,
    Async,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Body {
    Form,
    Json,
}

async fn read(
    state: &SharedState,
    route: &'static str,
    body: Body,
    req: Request,
) -> Result<Parsed, (Refused, Option<Parsed>)> {
    let max_mb = state.snapshot().settings.max_body_mb;
    match body {
        Body::Form => form::read(state, route, req, max_mb).await,
        Body::Json => {
            let bytes = Bytes::from_request(req, state).await.map_err(|e| {
                if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    (
                        Refused::new(
                            413,
                            "body_limit",
                            native::body_limit(max_mb, true).to_string(),
                        ),
                        None,
                    )
                } else {
                    (Refused::new(400, "invalid_json", e.body_text()), None)
                }
            })?;
            let parsed: Result<ConvertSourcesRequest, _> = serde_json::from_slice(&bytes);
            drop(bytes);
            let req = parsed.map_err(|e| {
                let mut r = Refused::new(422, "invalid_json", "");
                r.detail = serde_json::json!([{
                    "loc": ["body"],
                    "msg": format!(
                        "the body is not docling's request: {}",
                        crate::extract::convert::quoted::quotes_bounded(&e.to_string())
                    ),
                    "type": "json_invalid"
                }]);
                (r, None)
            })?;
            sources::from_json(route, req).map_err(|(r, p)| (r, Some(p)))
        }
    }
}

/// The bytes a request's files hold now.
fn inputs_len(p: &Parsed) -> u64 {
    p.inputs
        .iter()
        .map(|i| i.bytes.as_ref().map_or(0, |b| b.len() as u64))
        .sum()
}

async fn convert(
    state: SharedState,
    ctx: RequestCtx,
    route: &'static str,
    body: Body,
    mode: Mode,
    req: Request,
) -> Response {
    let who = native::caller(&ctx);
    Box::pin(pool::as_caller(
        who.clone(),
        run(state, ctx, who, route, body, mode, req),
    ))
    .await
}

async fn run(
    state: SharedState,
    ctx: RequestCtx,
    who: pool::Caller,
    route: &'static str,
    body: Body,
    mode: Mode,
    req: Request,
) -> Response {
    let handover = req
        .extensions()
        .get::<crate::policy::SlotHandover>()
        .cloned();
    let mut row = Row::open_as(&state, &ctx, format!("docling{route}"));
    let to = native::send::To::of(&req);
    // The answer's share before the body is read (`budget::answers`).
    let (bytes, declared) = native::request_share(&state, &req);
    let mut share = match answers::global().hold(&who.share, bytes).await {
        Ok(s) => s,
        Err(e) => {
            let code = answers::NeverFits::CODE;
            row.finish(503, Some((code.to_string(), e.message()))).await;
            return error(503, e.message().into(), Some(code));
        }
    };
    let options_at: &[&str] = match body {
        Body::Form => &["body"],
        Body::Json => &["body", "options"],
    };
    let fail = |row: Row, r: Refused| async move {
        let text = r
            .detail
            .as_str()
            .map_or_else(|| r.detail.to_string(), String::from);
        row.finish(r.status, Some((r.code.to_string(), text))).await;
        refused(&r)
    };
    let max_body = u64::from(state.snapshot().settings.max_body_mb) << 20;
    let within = upload::read_deadline(declared, max_body);
    let read = match tokio::time::timeout(within, read(&state, route, body, req)).await {
        Ok(r) => r,
        Err(_) => {
            let late = upload::ReadDeadline::of(declared, max_body);
            let e = native::read_late(&late);
            row.finish(
                408,
                Some((upload::ReadDeadline::CODE.to_string(), e.log_text())),
            )
            .await;
            return error(408, late.message().into(), Some(upload::ReadDeadline::CODE));
        }
    };
    let parsed = match read {
        Ok(p) => p,
        Err((r, p)) => {
            if let Some(p) = p {
                // The options are not judged on a refused request: its route,
                // target and hosts are what is recorded.
                row.record(adapter::record(&p, Vec::new()));
            }
            return fail(row, r).await;
        }
    };
    let platform = report::platform(state.deployment().is_container());
    let reader = crate::extract::convert::model_help::Reader::of(&state, &ctx);
    let mut parsed = parsed;
    if !parsed.web.is_empty() {
        // The options first: a request that answers 422 downloads nothing.
        if let Err((r, record)) = adapter::judge(&parsed, platform, options_at, &reader) {
            row.record(record);
            return fail(row, r).await;
        }
        // The settings as they are now; the fetch is boxed, its future
        // holds the hops and the body.
        let snap = state.snapshot();
        let keyed = !matches!(ctx.principal, crate::principal::Principal::Anonymous);
        let cfg = web::WebConfig::from_settings(&snap.settings, keyed, &System);
        let before = inputs_len(&parsed);
        if let Err(r) = Box::pin(web::fetch_all(&mut parsed, &cfg)).await {
            row.record(adapter::record(&parsed, Vec::new()));
            return fail(row, r).await;
        }
        // The sources downloaded are held beside the upload: counted, never
        // waited for (`Share::grow`).
        share = share.grow(inputs_len(&parsed).saturating_sub(before));
    }
    let p = match adapter::prepare(parsed, platform, options_at, &reader).await {
        Ok(mut p) => {
            p.plan = p
                .plan
                .spooling_under(&state.data_dir)
                .with_speech(&state, &ctx);
            p
        }
        Err((r, record)) => {
            row.record(record);
            return fail(row, r).await;
        }
    };
    row.record(p.record.clone());
    let notes = p.spec.notes.clone();
    match mode {
        Mode::Sync => {
            let spec = p.spec.clone();
            let (docs, seconds) = adapter::convert_now(&state, p).await;
            let (a, counted) = answer::render_off(&spec, docs, seconds).await;
            let status = if matches!(a, Answer::Failure(_)) {
                422
            } else {
                200
            };
            let error =
                (status >= 400).then(|| ("no_documents_exported".to_string(), counted.clone()));
            // The row is written once the body has ended (verification N-b).
            let to = to.then(row.when_sent(status, error));
            let (_, r) = respond(a, &counted, notes.as_deref(), share, to).await;
            r
        }
        Mode::Async => {
            let key = ctx.principal.key_id();
            match adapter::start_job(&state, p, key, who, handover.and_then(|h| h.take())).await {
                Ok(id) => {
                    row.finish(200, None).await;
                    match tasks::status_of(&state, &ctx, &id).await {
                        Some(s) => {
                            with_header(Json(s).into_response(), OPTION_NOTES, notes.as_deref())
                        }
                        None => error(404, "Task not found.".into(), None),
                    }
                }
                Err(e) => {
                    // A named refusal keeps its status and code (the jobs'
                    // bounds: 507, 429); anything else is the 500 it was.
                    let status = e.http_status().as_u16();
                    row.finish(status, Some((e.kind().to_string(), e.log_text())))
                        .await;
                    let code = match &e {
                        crate::error::GatewayError::Refused { code, .. } => code,
                        _ => "internal",
                    };
                    error(status, e.to_string().into(), Some(code))
                }
            }
        }
    }
}

macro_rules! handler {
    ($name:ident, $route:literal, $body:expr, $mode:expr) => {
        pub async fn $name(
            State(state): State<SharedState>,
            axum::Extension(ctx): axum::Extension<RequestCtx>,
            req: Request,
        ) -> Response {
            convert(state, ctx, $route, $body, $mode, req).await
        }
    };
}

handler!(convert_file, "/v1/convert/file", Body::Form, Mode::Sync);
handler!(
    convert_file_async,
    "/v1/convert/file/async",
    Body::Form,
    Mode::Async
);
handler!(convert_source, "/v1/convert/source", Body::Json, Mode::Sync);
handler!(
    convert_source_async,
    "/v1/convert/source/async",
    Body::Json,
    Mode::Async
);
handler!(
    convert_source_batch,
    "/v1/convert/source/batch",
    Body::Json,
    Mode::Async
);

/// The routes not built yet: a named 501.
pub async fn not_yet(req: Request) -> Response {
    let path = req
        .extensions()
        .get::<axum::extract::OriginalUri>()
        .map_or_else(|| req.uri().path().to_string(), |u| u.0.path().to_string());
    error(
        501,
        format!(
            "{path}: docling-serve's chunking, capabilities and clear routes are not built in \
             lmgw yet; convert with /v1/convert/file or /v1/convert/source"
        )
        .into(),
        Some("not_yet"),
    )
}

async fn health() -> Json<HealthCheckResponse> {
    Json(HealthCheckResponse {
        status: "ok".into(),
    })
}

async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        lmgw: env!("CARGO_PKG_VERSION").to_string(),
        docling_serve_compat: adapter::COMPAT.to_string(),
    })
}

/// `/health`, `/ready` and `/version`, at the root where docling clients
/// look for them, `Public` as docling-serve's are.
pub fn root_routes(state: &SharedState) -> Router<SharedState> {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(health))
        .route("/version", get(version))
        .route_layer(crate::server::require(state, Cap::Public))
}
