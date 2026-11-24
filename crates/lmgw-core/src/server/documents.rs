//! `POST /v1/documents/convert` (document-conversion design §7.1, §7.5,
//! §7.6): the HTTP half. The body — a multipart form with one `file` part
//! per file, or JSON with the files as base64 — is read under `max_body_mb`,
//! the options are judged ([`options::plan`]), the files converted
//! ([`batch::convert`]) as the caller's tool-pool steps, and the request's
//! row written ([`row`]).
//!
//! A request whose client goes away is dropped here: its running tools are
//! stopped with their process groups, and its row is a 499.

use std::sync::Arc;

use axum::extract::{FromRequest, Multipart, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use base64::Engine as _;
use bytes::Bytes;
use lmgw_api_types::documents::convert::{ConvertAnswer, ConvertFile};
use lmgw_api_types::documents::jobs::JobAccepted;
use serde_json::Value;

use crate::error::GatewayError;
use crate::extract::convert::budget::{answers, upload};
use crate::extract::convert::file::Input;
use crate::extract::convert::options::{self, Refusal};
use crate::extract::convert::{batch, callers, calls, jobs, pool, report};
use crate::proxy::RequestCtx;
use crate::state::SharedState;

pub mod jobs_routes;
pub(crate) mod row;
pub mod send;

/// The route's path, as `CAPABILITY_TABLE` has it.
pub const PATH: &str = "/v1/documents/convert";

/// The `max_body_mb` refusal for this route: the setting and its value, and
/// for a JSON body that base64 makes each file about a third larger.
pub fn body_limit(max_mb: u32, json: bool) -> GatewayError {
    let mut message = format!(
        "request body exceeds max_body_mb ({max_mb} MiB) — raise it under Settings → Network & \
         access"
    );
    if json {
        message.push_str(
            ". The files are base64 in JSON, which makes each about a third larger than the \
             file itself; as multipart/form-data they take their own size",
        );
    }
    GatewayError::Refused {
        status: 413,
        code: "body_limit",
        message,
    }
}

/// [`body_limit`] for a request on this route refused by its declared
/// length before it is read (`server::body_limit_mw`); `None` elsewhere.
/// The path is the request's whole one: the `/v1` group sees its own path
/// with the prefix taken off.
pub fn declared_too_large(req: &Request, max_mb: u32) -> Option<Response> {
    let path = req
        .extensions()
        .get::<axum::extract::OriginalUri>()
        .map_or_else(|| req.uri().path(), |u| u.0.path());
    if path.starts_with(crate::extract::convert::calls::DOCLING_ROUTES) {
        return Some(super::docling::body_limit(max_mb, is_json(req.headers())));
    }
    (path == PATH).then(|| refusal(&body_limit(max_mb, is_json(req.headers()))))
}

pub(crate) fn content_type(headers: &HeaderMap) -> &str {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

pub(crate) fn is_json(headers: &HeaderMap) -> bool {
    content_type(headers)
        .to_ascii_lowercase()
        .starts_with("application/json")
}

/// A refusal as its answer: the one place this route's (and its jobs')
/// errors become a body, so its message is bounded here, whatever it
/// formats (`convert::quoted::outlet`).
fn refusal(e: &GatewayError) -> Response {
    let body = crate::extract::convert::quoted::outlet_openai(e.to_openai_json());
    (e.http_status(), Json(body)).into_response()
}

fn bad(code: &'static str, message: impl Into<String>) -> GatewayError {
    GatewayError::InvalidRequest {
        code,
        message: message.into(),
    }
}

impl From<Refusal> for GatewayError {
    fn from(r: Refusal) -> Self {
        bad(r.code, r.message)
    }
}

/// Whose steps these are in the tool pool's round robin, and whose share
/// they count against: the key's queue in its share (an owner key's is its
/// account's, `convert::callers`), or a queue of the request's own in the
/// keyless share when it has none (auth off), so keyless requests do not
/// queue behind one another.
pub(crate) fn caller(ctx: &RequestCtx) -> pool::Caller {
    match (ctx.principal.key_id(), callers::share_of(&ctx.principal)) {
        (Some(id), Some(share)) => pool::Caller {
            queue: Arc::from(format!("key:{id}")),
            share,
        },
        _ => pool::request_caller(),
    }
}

/// The handler.
pub async fn convert(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    req: Request,
) -> Response {
    let who = caller(&ctx);
    // Boxed: the conversion's future is large, and the gateway's request
    // chains run on a small stack.
    Box::pin(pool::as_caller(who.clone(), handle(state, ctx, who, req))).await
}

/// What a request came to.
enum Done {
    /// Converted now: the status and the answer.
    Now(u16, ConvertAnswer),
    /// Accepted as a job.
    Job(String),
}

/// The refusal of a conversion whose answer share can never fit
/// ([`answers::NeverFits`]).
pub fn never_fits(e: &answers::NeverFits) -> GatewayError {
    GatewayError::Refused {
        status: 503,
        code: answers::NeverFits::CODE,
        message: e.message(),
    }
}

/// The refusal of a body that did not arrive within its read deadline
/// ([`upload::ReadDeadline`]), logged with both names.
pub fn read_late(late: &upload::ReadDeadline) -> GatewayError {
    let message = late.message();
    tracing::info!("a conversion request was refused: {message}");
    GatewayError::Refused {
        status: 408,
        code: upload::ReadDeadline::CODE,
        message,
    }
}

/// The share a request on these routes reserves before its body is read
/// ([`upload::request_share`]), and its body's declared length.
pub fn request_share(state: &SharedState, req: &Request) -> (u64, Option<u64>) {
    let max_body = u64::from(state.snapshot().settings.max_body_mb) << 20;
    let declared = upload::declared(req.headers());
    let bytes = upload::request_share(
        declared,
        is_json(req.headers()),
        max_body,
        answers::global().output_budget(),
    );
    (bytes, declared)
}

async fn handle(state: SharedState, ctx: RequestCtx, who: pool::Caller, req: Request) -> Response {
    let mut row = row::Row::open(&state, &ctx);
    // The answer's share first, before the body is read: a request that
    // waits for one holds nothing else (`budget::answers`).
    let (bytes, _) = request_share(&state, &req);
    let share = match answers::global().hold(&who.share, bytes).await {
        Ok(s) => s,
        Err(e) => {
            let e = never_fits(&e);
            row.finish(503, Some((e.kind().to_string(), e.log_text())))
                .await;
            return refusal(&e);
        }
    };
    let to = send::To::of(&req);
    let (done, record) = Box::pin(answer(&state, &ctx, who, req)).await;
    if let Some(r) = record {
        row.record(r);
    }
    match done {
        Ok(Done::Now(status, body)) => {
            let error = (status >= 400).then(|| failed_row(&body));
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::UNPROCESSABLE_ENTITY);
            // The row is written once the body has ended (verification N-b).
            send::json(code, body, share, to.then(row.when_sent(status, error))).await
        }
        Ok(Done::Job(job)) => {
            row.finish(202, None).await;
            let location = format!("/v1/documents/jobs/{job}");
            (
                StatusCode::ACCEPTED,
                [(header::LOCATION, location)],
                Json(JobAccepted { job }),
            )
                .into_response()
        }
        Err(e) => {
            row.finish(
                e.http_status().as_u16(),
                Some((e.kind().to_string(), e.log_text())),
            )
            .await;
            refusal(&e)
        }
    }
}

/// The row's error for an answer whose files all failed: the first file's
/// code, and each failed file's code by its position and how many failed.
/// No text of the files' own goes into the row (an error message can quote
/// the document, or list thousands of pages; the answer carries it), and
/// file names stay out as they stay out of what the design records about
/// callers, §7.8.
fn failed_row(body: &ConvertAnswer) -> (String, String) {
    let code = |e: &lmgw_api_types::documents::convert::ConvertError| {
        serde_json::to_value(e.code)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "convert_failed".to_string())
    };
    let failed: Vec<(u32, String)> = body
        .documents
        .iter()
        .filter_map(|d| d.error.as_ref().map(|e| (d.index, code(e))))
        .collect();
    let kind = failed
        .first()
        .map_or_else(|| "convert_failed".to_string(), |(_, c)| c.clone());
    let by_file = failed
        .iter()
        .map(|(i, c)| format!("file {i}: {c}"))
        .collect::<Vec<_>>()
        .join(", ");
    (
        kind,
        format!(
            "no file was converted — {} of {} files failed ({by_file})",
            failed.len(),
            body.documents.len()
        ),
    )
}

async fn answer(
    state: &SharedState,
    ctx: &RequestCtx,
    who: pool::Caller,
    req: Request,
) -> (Result<Done, GatewayError>, Option<calls::Record>) {
    let max_mb = state.snapshot().settings.max_body_mb;
    let handover = req
        .extensions()
        .get::<crate::policy::SlotHandover>()
        .cloned();
    let ct = content_type(req.headers()).to_ascii_lowercase();
    let declared = upload::declared(req.headers());
    let max_body = u64::from(max_mb) << 20;
    let late = || Err(read_late(&upload::ReadDeadline::of(declared, max_body)));
    let within = upload::read_deadline(declared, max_body);
    let read = if ct.starts_with("multipart/form-data") {
        tokio::time::timeout(within, read_form(state, req, max_mb))
            .await
            .unwrap_or_else(|_| late())
    } else if ct.starts_with("application/json") {
        tokio::time::timeout(within, read_json(state, req, max_mb))
            .await
            .unwrap_or_else(|_| late())
    } else {
        Err(GatewayError::Refused {
            status: 415,
            code: "unsupported_content_type",
            message: format!(
                "the body is {}; send multipart/form-data with one file part per file, or \
                 application/json with the files as base64",
                if ct.is_empty() {
                    "untyped"
                } else {
                    ct.as_str()
                }
            ),
        })
    };
    let (files, opts) = match read {
        Ok(r) => r,
        Err(e) => return (Err(e), None),
    };
    let platform = report::platform(state.deployment().is_container());
    let reader = crate::extract::convert::model_help::Reader::of(state, ctx);
    let (plan, notes) = match options::plan_for(&opts, platform, &reader) {
        Ok((plan, notes)) => (
            plan.spooling_under(&state.data_dir).with_speech(state, ctx),
            notes,
        ),
        Err(e) => return (Err(e.into()), Some(calls::native(&opts, &[], files.len()))),
    };
    let record = calls::native(&opts, &notes, files.len());
    if files.is_empty() {
        let e = bad(
            "no_files",
            "the request carries no file: send one or more file parts (multipart), or a \
             non-empty files list (JSON)",
        );
        return (Err(e), Some(record));
    }
    if opts.run_async {
        let started = jobs::start(
            state,
            jobs::Start {
                key_id: ctx.principal.key_id(),
                origin: jobs::Origin::Native,
                files,
                decided: Vec::new(),
                deadline: plan.deadline,
                plan,
                notes,
                adapter: None,
                abort_on_error: false,
                caller: who,
                // The request's concurrency slot goes to the job.
                slot: handover.and_then(|h| h.take()),
            },
        )
        .await;
        return (started.map(Done::Job), Some(record));
    }
    let facts = state.doc_tools.current().await;
    let (status, body) = batch::convert(files, &plan, &facts, notes).await;
    (Ok(Done::Now(status, body)), Some(record))
}

type Read = (
    Vec<Input>,
    lmgw_api_types::documents::convert::ConvertOptions,
);

async fn read_form(state: &SharedState, req: Request, max_mb: u32) -> Result<Read, GatewayError> {
    let too_large = |status: StatusCode| status == StatusCode::PAYLOAD_TOO_LARGE;
    let mut mp = Multipart::from_request(req, state)
        .await
        .map_err(|e| bad("invalid_multipart", e.body_text()))?;
    let mut files = Vec::new();
    let mut fields = Vec::new();
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) if too_large(e.status()) => return Err(body_limit(max_mb, false)),
            Err(e) => return Err(bad("invalid_multipart", e.body_text())),
        };
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            let file_name = field.file_name().unwrap_or_default().to_string();
            let bytes = match field.bytes().await {
                Ok(b) => b,
                Err(e) if too_large(e.status()) => return Err(body_limit(max_mb, false)),
                Err(e) => return Err(bad("invalid_multipart", e.body_text())),
            };
            files.push(Input {
                index: u32::try_from(files.len()).unwrap_or(u32::MAX),
                name: file_name,
                bytes: Ok(bytes),
            });
        } else if field.file_name().is_some() {
            return Err(bad(
                "invalid_option",
                format!("the part {name} carries a file; files go in parts named file"),
            ));
        } else {
            let text = match field.text().await {
                Ok(t) => t,
                Err(e) if too_large(e.status()) => return Err(body_limit(max_mb, false)),
                Err(e) => return Err(bad("invalid_multipart", e.body_text())),
            };
            fields.push((name, text));
        }
    }
    Ok((files, options::from_form(fields)?))
}

/// base64 with or without padding, line breaks and spaces left out (a
/// client that wraps its base64 at 76 characters).
pub(crate) fn decode(text: &str) -> Result<Bytes, String> {
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    // A copy without the whitespace only for the text that has some.
    let stripped;
    let clean: &str = if text.bytes().any(|b| b.is_ascii_whitespace()) {
        stripped = text
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect::<String>();
        &stripped
    } else {
        text
    };
    engine
        .decode(clean)
        .map(Bytes::from)
        .map_err(|e| format!("data_base64 is not base64: {e}"))
}

async fn read_json(state: &SharedState, req: Request, max_mb: u32) -> Result<Read, GatewayError> {
    let body = Bytes::from_request(req, state).await.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            body_limit(max_mb, true)
        } else {
            bad("invalid_json", e.body_text())
        }
    })?;
    let parsed = serde_json::from_slice::<Value>(&body)
        .map_err(|e| bad("invalid_json", format!("the body is not JSON: {e}")));
    // The body's bytes are not needed again: one copy of the files fewer
    // while they are decoded.
    drop(body);
    let value = parsed?;
    let Value::Object(mut map) = value else {
        return Err(bad("invalid_json", "the body is not a JSON object"));
    };
    let listed = map.remove("files").unwrap_or(Value::Array(Vec::new()));
    let listed: Vec<ConvertFile> = serde_json::from_value(listed).map_err(|e| {
        bad(
            "invalid_option",
            format!(
                "files: {}; each file is {{\"name\": …, \"data_base64\": …}}",
                crate::extract::convert::quoted::quotes_bounded(&e.to_string())
            ),
        )
    })?;
    let files = listed
        .into_iter()
        .enumerate()
        .map(|(i, f)| Input {
            index: u32::try_from(i).unwrap_or(u32::MAX),
            name: f.name,
            bytes: decode(&f.data_base64),
        })
        .collect();
    Ok((files, options::from_map(map)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Security round A review B1: a body that stops arriving is refused
    /// with 408 at its read deadline, naming both constants (paused time).
    #[tokio::test]
    async fn a_body_that_does_not_arrive_within_its_read_deadline_is_a_408() {
        use futures::StreamExt as _;
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        tokio::time::pause();
        let trickle = futures::stream::once(async {
            Ok::<_, std::io::Error>(Bytes::from_static(b"--example\r\n"))
        })
        .chain(futures::stream::pending());
        let req = Request::builder()
            .method("POST")
            .uri(PATH)
            .header("content-type", "multipart/form-data; boundary=example")
            .header("content-length", "1048576")
            .body(axum::body::Body::from_stream(trickle))
            .unwrap();
        let started = tokio::time::Instant::now();
        let (done, _) = answer(&state, &RequestCtx::default(), pool::request_caller(), req).await;
        let Err(GatewayError::Refused {
            status,
            code,
            message,
            ..
        }) = done
        else {
            panic!("not refused");
        };
        assert_eq!((status, code), (408, "read_deadline"));
        assert!(
            message.contains("64 s")
                && message.contains("UPLOAD_READ_GRACE_S")
                && message.contains("UPLOAD_MIN_RATE_BYTES_PER_S"),
            "{message}"
        );
        assert_eq!(started.elapsed().as_secs(), 64);
    }

    #[test]
    fn base64_reads_with_or_without_padding_and_line_breaks() {
        assert_eq!(decode("aGk=").unwrap(), Bytes::from_static(b"hi"));
        assert_eq!(decode("aGk").unwrap(), Bytes::from_static(b"hi"));
        assert_eq!(decode("aG\nk=\r\n").unwrap(), Bytes::from_static(b"hi"));
        assert!(decode("not base64!").unwrap_err().contains("not base64"));
    }

    #[test]
    fn the_json_limit_says_base64_makes_files_larger() {
        let m = body_limit(8, true).to_string();
        assert!(
            m.contains("max_body_mb (8 MiB)") && m.contains("third larger"),
            "{m}"
        );
        assert!(!body_limit(8, false).to_string().contains("base64"));
    }

    #[test]
    fn the_usage_row_names_codes_by_file_and_never_a_files_own_text() {
        use lmgw_api_types::documents::convert::{
            ConvertError, ConvertStats, ConvertSummary, ConvertedDocument, DocStatus, ErrorCode,
        };
        let failed = |index: u32, code: ErrorCode| ConvertedDocument {
            index,
            name: "example.pdf".into(),
            status: DocStatus::Failed,
            format: None,
            parts: 0,
            content: None,
            structure: None,
            pictures: Vec::new(),
            text: None,
            warnings: Vec::new(),
            stats: ConvertStats::default(),
            error: Some(ConvertError {
                code,
                message: "quoted document text".into(),
                tool: None,
            }),
        };
        let body = ConvertAnswer {
            documents: vec![
                failed(0, ErrorCode::Unreadable),
                failed(1, ErrorCode::NoReader),
            ],
            summary: ConvertSummary::default(),
            tools: Vec::new(),
            warnings: Vec::new(),
        };
        let (kind, message) = failed_row(&body);
        assert_eq!(kind, "unreadable");
        assert!(
            message.contains("2 of 2 files failed (file 0: unreadable, file 1: no_reader)"),
            "{message}"
        );
        assert!(!message.contains("quoted") && !message.contains("example.pdf"));
    }
}
