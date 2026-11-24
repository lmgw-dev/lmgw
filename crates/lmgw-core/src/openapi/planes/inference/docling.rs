//! The docling-serve-compatible routes (document-conversion design §7.4):
//! docling-serve's shapes, under the "docling-serve compatible" tag whose
//! description carries the option table (`tags.rs`).

use lmgw_api_types::documents::docling::{
    ConvertDocumentResponse, ConvertSourcesRequest, DoclingError, HealthCheckResponse, PollQuery,
    TaskFailureResult, TaskStatusResponse, VersionResponse, WsQuery,
};
use lmgw_api_types::openapi_ext::endpoint_group;
use schemars::{Schema, SchemaGenerator};

use super::super::super::registry::{Dialect, DocRoute, Req, Resp, SchemaFn};
use super::super::super::schemas;
use super::base;

const TAG: &str = "docling-serve";

/// `/v1/convert/file`'s form.
fn form(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "DoclingConvertForm",
        schemars::json_schema!({
            "type": "object",
            "required": ["files"],
            "properties": {
                "files": {
                    "type": "array",
                    "items": {"type": "string", "format": "binary"},
                    "description": "One part named files per file; its filename is the \
                        document's."
                },
                "target_type": {
                    "type": "string",
                    "enum": ["inbody", "zip", "presigned_url"],
                    "description": "inbody (the default) or zip; presigned_url is a 400 \
                        target_refused."
                },
                "callbacks": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Not called (a note in x-lmgw-option-notes); poll or use \
                        the status WebSocket."
                }
            },
            "additionalProperties": {"type": "string"},
            "description": "multipart/form-data: the files and docling's options as fields, \
                a list option repeated per value (to_formats=md, to_formats=text), true or \
                false for the switches, an object option as JSON text."
        }),
    )
}

fn convert_answer(g: &mut SchemaGenerator) -> Schema {
    g.subschema_for::<ConvertDocumentResponse>()
}

fn failure(g: &mut SchemaGenerator) -> Schema {
    g.subschema_for::<TaskFailureResult>()
}

const ANSWER: &[(&str, Option<SchemaFn>)] = &[
    ("application/json", Some(convert_answer)),
    ("application/zip", None),
];

const RESULT: &[(&str, Option<SchemaFn>)] = &[
    ("application/json", Some(result_json)),
    ("application/zip", None),
];

fn result_json(g: &mut SchemaGenerator) -> Schema {
    let a = convert_answer(g);
    let f = failure(g);
    schemas::named(
        g,
        "DoclingTaskResult",
        schemars::json_schema!({
            "oneOf": [a, f],
            "description": "One document in the body, or TaskFailureResult for a task that \
                failed as a whole (cancelled, interrupted, or no document exported)."
        }),
    )
}

const SYNC: &str = "Converts the files now with lmgw's own conversion (the same as POST \
    /v1/documents/convert) and answers in docling-serve's shape: one file with the inbody \
    target as ConvertDocumentResponse (document.md_content, text_content; the other outputs \
    null), several files or the zip target as converted_docs.zip (application/zip, \
    Content-Disposition attachment), one <stem>.md / <stem>.txt per converted file at the \
    archive's root as docling-jobkit main writes it; a repeated stem gets _<index>. A failed \
    or skipped file is absent from the zip, as in docling; x-lmgw-documents counts the \
    outcomes. No file exported at all is a 422 TaskFailureResult. The body changes only \
    where output was lost: status partial_success with errors for unread parts and outputs \
    lmgw does not write yet. A one-file failure is HTTP 200 with status failure, as in \
    docling. The request waits until done (no DOCLING_SERVE_MAX_SYNC_WAIT); a client that \
    goes away stops its tools. Bounded by max_body_mb (a 413 in docling's shape). The \
    options and their outcomes: see the tag; x-lmgw-option-notes lists what was not \
    honoured. Each request writes one usage row (documents, docling/<route>) with a record \
    of its route, target and options (Settings › Documents, Who calls the docling route).";

const ASYNC: &str = "Starts the conversion as a task and answers TaskStatusResponse \
    (task_status pending, task_position null: lmgw's tool runs share one pool, which has no \
    queue position). Poll /v1/status/poll/{task_id}, or watch /v1/status/ws/{task_id}, then \
    fetch /v1/result/{task_id}. Tasks do not wait for the GPU hold. A task belongs to its \
    key; another key's poll reads not found. The request, its options and their outcomes \
    are as on the sync route.";

fn route(method: &'static str, path: &'static str, summary: &'static str) -> DocRoute {
    DocRoute {
        dialect: Dialect::DoclingServe,
        endpoints: &[endpoint_group::OTHER],
        ..base(method, path, TAG, summary)
    }
}

pub(super) fn routes() -> Vec<DocRoute> {
    let json = || Req::Json(|g| g.root_schema_for::<ConvertSourcesRequest>());
    let task = || Resp::Json(|g| g.root_schema_for::<TaskStatusResponse>());
    let health = || Resp::Json(|g| g.root_schema_for::<HealthCheckResponse>());
    let not_yet = || Resp::Json(|g| g.root_schema_for::<DoclingError>());
    vec![
        DocRoute {
            description: SYNC,
            request: Req::Multipart(form),
            response: Resp::Download(ANSWER),
            writes: Some(false),
            ..route("POST", "/v1/convert/file", "Convert files (docling-serve)")
        },
        DocRoute {
            description: ASYNC,
            request: Req::Multipart(form),
            response: task(),
            writes: Some(false),
            ..route(
                "POST",
                "/v1/convert/file/async",
                "Convert files as a task (docling-serve)",
            )
        },
        DocRoute {
            description: SYNC,
            request: json(),
            response: Resp::Download(ANSWER),
            writes: Some(false),
            ..route(
                "POST",
                "/v1/convert/source",
                "Convert sources (docling-serve)",
            )
        },
        DocRoute {
            description: ASYNC,
            request: json(),
            response: task(),
            writes: Some(false),
            ..route(
                "POST",
                "/v1/convert/source/async",
                "Convert sources as a task (docling-serve)",
            )
        },
        DocRoute {
            description: "docling-serve's batch route: always a task, for the inbody and zip \
                targets (a storage target is a 400 target_refused). Otherwise as \
                /v1/convert/source/async.",
            request: json(),
            response: task(),
            writes: Some(false),
            ..route(
                "POST",
                "/v1/convert/source/batch",
                "Convert a batch (docling-serve)",
            )
        },
        DocRoute {
            description: "A task's state as TaskStatusResponse: pending (no file done yet), \
                started, success, or failure (no file converted, cancelled, or interrupted by \
                a gateway restart, with error_message and failure). task_meta counts the \
                documents. wait=<seconds> waits that long for the task to complete. A task \
                that does not exist or another key made is a 404 \"Task not found.\".",
            query: Some(|g| g.root_schema_for::<PollQuery>()),
            response: task(),
            ..route(
                "GET",
                "/v1/status/poll/{task_id}",
                "Poll a task (docling-serve)",
            )
        },
        DocRoute {
            description: "The task's state, pushed: docling-serve's WebsocketMessage frames. \
                The key may come as ?api_key= (docling-serve's way; a browser cannot set a \
                header on a WebSocket) or as a header.",
            query: Some(|g| g.root_schema_for::<WsQuery>()),
            response: Resp::WebSocket {
                subprotocol: "",
                frames: "JSON text frames {message, task, error}: connection with the task's \
                    state first, then update with it, then update on every change and on \
                    every message the client sends; the socket closes (1000) once the task \
                    has completed. An unknown task is one error frame \"Task not found.\" and \
                    a close.",
            },
            ..route(
                "GET",
                "/v1/status/ws/{task_id}",
                "Watch a task (docling-serve)",
            )
        },
        DocRoute {
            description: "The task's result in the shape its request asked for (one document \
                in the body, or converted_docs.zip), with x-lmgw-documents and \
                x-lmgw-option-notes. While the task runs, 404 \"Task result not found. Please \
                wait for a completion status.\". A task that failed as a whole answers \
                TaskFailureResult. Results are single use, as docling-serve's default: the \
                task and its stored results are deleted RESULT_REMOVAL_DELAY (300 s) after \
                the first fetch, then the task is not found.",
            response: Resp::Download(RESULT),
            ..route(
                "GET",
                "/v1/result/{task_id}",
                "A task's result (docling-serve)",
            )
        },
        DocRoute {
            description: "docling-serve's chunking routes: not built in lmgw yet, a 501 \
                not_yet that names the route.",
            response: not_yet(),
            ..route("POST", "/v1/chunk/{*rest}", "Chunk documents (not yet)")
        },
        DocRoute {
            description: "docling-serve's capabilities: not built in lmgw yet, a 501 not_yet.",
            response: not_yet(),
            ..route(
                "GET",
                "/v1/capabilities",
                "Deployment capabilities (not yet)",
            )
        },
        DocRoute {
            description: "docling-serve's clear routes: not built in lmgw yet, a 501 not_yet. \
                A native job can be deleted with DELETE /v1/documents/jobs/{id}.",
            response: not_yet(),
            ..route(
                "GET",
                "/v1/clear/{*rest}",
                "Clear converters or results (not yet)",
            )
        },
        DocRoute {
            description: "docling-serve's liveness check: {\"status\": \"ok\"}. No key needed.",
            response: health(),
            ..route("GET", "/health", "Health (docling-serve)")
        },
        DocRoute {
            description: "docling-serve's readiness check: {\"status\": \"ok\"} (lmgw loads \
                no models for the conversion it answers). No key needed.",
            response: health(),
            ..route("GET", "/ready", "Ready (docling-serve)")
        },
        DocRoute {
            description: "lmgw's version and the docling-serve release whose routes it \
                answers. No key needed.",
            response: Resp::Json(|g| g.root_schema_for::<VersionResponse>()),
            ..route("GET", "/version", "Version (docling-serve)")
        },
    ]
}
