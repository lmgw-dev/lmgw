//! The docling-serve-compatible routes (document-conversion design §7.4):
//! docling-serve's own request and answer shapes, copied from docling's
//! `docling/datamodel/service/{requests,responses,tasks}.py` and docling's
//! `base_models.py` as read on 2026-11-18, so a strict docling client keeps
//! parsing them (D16). lmgw's own notes travel in headers
//! (`x-lmgw-option-notes`, `x-lmgw-documents`), never in these bodies.
//!
//! The options object stays a JSON map: every option docling knows is read
//! by name and given an outcome, and one lmgw does not know is a note, never
//! a refusal (§7.4.2).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `POST /v1/convert/source`, `/v1/convert/source/async` and
/// `/v1/convert/source/batch`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConvertSourcesRequest {
    /// docling's `ConvertDocumentsOptions`, by option name.
    pub options: Map<String, Value>,
    /// `{"kind": "file", "base64_string", "filename"}`; `kind: "http"`
    /// (`url`, `headers`) is refused unless the owner lets lmgw fetch web
    /// addresses (not built yet); `s3` and the other storage kinds are
    /// refused, as lmgw fetches nothing from a place a caller chose.
    pub sources: Vec<DoclingSource>,
    /// docling-serve's older spelling of the file sources.
    pub file_sources: Vec<DoclingSource>,
    /// docling-serve's older spelling of the http sources.
    pub http_sources: Vec<DoclingSource>,
    /// `{"kind": "inbody"}` (the default) or `{"kind": "zip"}`; a target
    /// that would make lmgw write to a place the caller chose (`put`, `s3`,
    /// `presigned_url`, …) is refused.
    pub target: Option<DoclingTarget>,
    /// The batch route's list of targets (one is served).
    pub targets: Option<Vec<DoclingTarget>>,
    /// Not called: lmgw makes no call to an address a caller chose; poll the
    /// task or use its WebSocket.
    pub callbacks: Vec<Value>,
}

/// One source.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DoclingSource {
    /// `file`, `http`, `s3`, …; absent in the older `file_sources` and
    /// `http_sources` lists.
    pub kind: Option<String>,
    pub base64_string: Option<String>,
    pub filename: Option<String>,
    pub url: Option<String>,
    pub headers: Option<Map<String, Value>>,
    /// The fields of the other kinds (bucket, credentials), never read.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// A target.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DoclingTarget {
    pub kind: String,
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// docling's `ConversionStatus`: a document's outcome, and a task's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ConversionStatus {
    Pending,
    Started,
    Failure,
    Success,
    PartialSuccess,
    Skipped,
}

/// docling's `ExportDocumentResponse`: the outputs a request asked for; the
/// others are `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExportDocumentResponse {
    pub filename: String,
    pub md_content: Option<String>,
    /// A DoclingDocument; not produced by lmgw yet (always `null`).
    pub json_content: Option<Value>,
    pub html_content: Option<String>,
    pub text_content: Option<String>,
    pub doctags_content: Option<String>,
    pub doclang_content: Option<String>,
}

/// docling's `ErrorItem`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ErrorItem {
    /// `document_backend`, `model`, `doc_assembler`, `user_input`,
    /// `pipeline`.
    pub component_type: String,
    pub module_name: String,
    pub error_message: String,
    /// docling's `FailureCategory`: `policy`, `timeout`, `backend_failure`,
    /// `unknown`, ….
    pub category: String,
    /// The 1-based page it concerns, `null` for the whole document.
    pub page_no: Option<u32>,
}

/// docling's `ConvertDocumentResponse`: one document, in the body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConvertDocumentResponse {
    pub document: ExportDocumentResponse,
    pub status: ConversionStatus,
    pub errors: Vec<ErrorItem>,
    /// Seconds.
    pub processing_time: f64,
    /// docling's profiling timings; lmgw sends none (`{}`).
    pub timings: Map<String, Value>,
    /// docling's confidence scores; lmgw computes none (`null`).
    pub confidence: Option<Value>,
}

/// docling's `TaskType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum TaskType {
    Convert,
    Chunk,
}

/// docling's `TaskProcessingMeta`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskProcessingMeta {
    pub num_docs: u32,
    pub num_processed: u32,
    pub num_succeeded: u32,
    pub num_partially_succeeded: u32,
    pub num_failed: u32,
}

/// docling's `TaskStatusResponse`: the async routes' answer, the poll's, and
/// the WebSocket's `task`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskStatusResponse {
    pub task_id: String,
    pub task_type: TaskType,
    /// `pending`, `started`, `success` or `failure`.
    pub task_status: ConversionStatus,
    /// lmgw has no task queue of its own (tool runs share one pool), so
    /// always `null`.
    pub task_position: Option<u32>,
    pub task_meta: Option<TaskProcessingMeta>,
    pub error_message: Option<String>,
    pub failure: Option<PublicFailureInfo>,
}

/// docling's `PublicFailureInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PublicFailureInfo {
    /// docling's `FailureCategory`.
    pub category: String,
    pub message: String,
    pub retryable: bool,
    /// `admission`, `source_enumeration`, `execution`, `orchestration`.
    pub phase: String,
    pub details: Map<String, Value>,
}

/// docling's `TaskFailureResult`: the result of a task that failed as a
/// whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskFailureResult {
    /// Always `TaskFailureResult`.
    pub kind: String,
    pub failure: PublicFailureInfo,
}

/// docling's `MessageKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MessageKind {
    Connection,
    Update,
    Error,
}

/// docling's `WebsocketMessage`: one frame of `/v1/status/ws/{task_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WebsocketMessage {
    pub message: MessageKind,
    pub task: Option<TaskStatusResponse>,
    pub error: Option<String>,
}

/// `GET /health` and `GET /ready`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HealthCheckResponse {
    /// `ok`.
    pub status: String,
}

/// `GET /version`: lmgw's version and the docling-serve release whose
/// routes it answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VersionResponse {
    pub lmgw: String,
    #[serde(rename = "docling-serve-compat")]
    pub docling_serve_compat: String,
}

/// An error on these routes, in FastAPI's shape: `detail` is a sentence, or
/// for a wrong option type a list of `{loc, msg, type}` as docling's
/// validation answers it. `code` is lmgw's name for a refusal (a strict
/// docling client reads `detail` only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DoclingError {
    pub detail: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// `GET /v1/status/poll/{task_id}`'s query.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PollQuery {
    /// Seconds to wait for the task to complete before answering.
    pub wait: f64,
}

/// `WS /v1/status/ws/{task_id}`'s query: docling-serve's WebSocket clients
/// send their key here (a browser cannot set a header on a WebSocket).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WsQuery {
    pub api_key: Option<String>,
}
