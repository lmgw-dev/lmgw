//! The route registry's shape (api-docs design §4.2): one [`DocRoute`] per
//! documented `(method, path)`, the enums its fields draw from, and
//! [`all_routes`], which concatenates every plane into the one list
//! `build.rs` assembles from.
//!
//! A `DocRoute` is deliberately not the route's `CAPABILITY_TABLE` row: the
//! capability is read *from* that table by `build.rs` (§4.1 "never
//! repeated"), so a `DocRoute` cannot itself say something the table
//! disagrees with.

use schemars::SchemaGenerator;
use serde_json::Value;

/// A schema-generating function: `|g| g.root_schema_for::<SomeType>()`, or a
/// hand-written `json_schema!{..}` for a shape with no Rust type of its own.
pub(crate) type SchemaFn = fn(&mut SchemaGenerator) -> schemars::Schema;

/// One documented operation on the routes plane (§4.2). Op documentation is
/// [`super::ops::table::OpDoc`], not this — the two planes share almost no
/// fields once an op's `x-lmgw-tool` merge and its `writes` default are
/// accounted for, and a struct trying to be both would need more `Option`s
/// than either does.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DocRoute {
    /// As in `CAPABILITY_TABLE`: the HTTP verb, or `*` for an `any(..)` route.
    pub method: &'static str,
    /// Exactly the `CAPABILITY_TABLE` path, `{*id}` kept — [`super::params`]
    /// is what turns that into the OpenAPI-legal `{id}` plus
    /// `x-lmgw-wildcard`.
    pub path: &'static str,
    /// A [`super::tags::TAGS`] id.
    pub tag: &'static str,
    pub summary: &'static str,
    /// `""` is allowed when `tool` supplies the prose instead (§4.7 merge).
    pub description: &'static str,
    /// A self-admin tool sharing this code path — `x-lmgw-tool`, and (when
    /// `description` is `""`) where the description comes from.
    pub tool: Option<&'static str>,
    /// The handler's `Query<T>` struct, if it has one.
    pub query: Option<SchemaFn>,
    /// Which `{name}` path params are `Path<i64>` rather than `Path<String>`
    /// in the handler — everything else is a string.
    pub path_ints: &'static [&'static str],
    pub request: Req,
    pub response: Resp,
    pub dialect: Dialect,
    /// `/v1/models` `lmgw.endpoints` groups this route belongs to (§4.11):
    /// `"openai"` | `"anthropic"` | `"other"`.
    pub endpoints: &'static [&'static str],
    /// `x-lmgw-model-task`, for the tester's `ModelPicker` (§6.6).
    pub model_task: Option<&'static str>,
    /// `x-lmgw-confirm-note`: an extra sentence the confirm click shows.
    pub confirm_note: Option<&'static str>,
    /// `None` means the method rule (§4.2); `Some` is `DocRoute`'s own
    /// override for the inference-plane POSTs that only compute an answer.
    pub writes: Option<bool>,
    pub example: Option<fn() -> Value>,
}

/// A request body's shape (§4.2).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Req {
    /// No request body (a `GET`, a `DELETE`).
    None,
    Json(SchemaFn),
    Multipart(SchemaFn),
    /// A raw body of the given mime type (e.g. `POST /api/docs/import`).
    Raw(&'static str),
    /// A JSON body (object or array) that may equally arrive as
    /// newline-delimited JSON text — `application/x-ndjson`, one value per
    /// line — for a handler that reads raw bytes and decodes either
    /// (`POST /api/agents/runs/{job_id}/events`, `ledger::decode_body`).
    JsonOrNdjson(SchemaFn),
    /// `POST /mcp`, `POST /mcp/admin`: JSON-RPC 2.0, documented by hand in
    /// `v1::mcp` rather than schema-generated.
    JsonRpc,
}

/// A response body's shape (§4.2).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Resp {
    Json(SchemaFn),
    /// An op's `{ok, message?, id?}` shape (§4.7) — api-types `OpOutcome`
    /// (`models.rs:396`), extra keys allowed. Not schema-generated so every
    /// such op does not each need its own trivial `SchemaFn`.
    OpOutcome,
    /// No DTO exists (§4.6 rule 4) — the reason shown on the page.
    Untyped(&'static str),
    /// A download or a media type with no useful schema; the content types it
    /// may answer with.
    Binary(&'static [&'static str]),
    /// `text/event-stream`, `{event name: schema fn}` (`x-lmgw-sse-events`).
    Sse(&'static [(&'static str, SchemaFn)]),
    /// Both `application/json` and `text/event-stream` on the one 200 (WP4
    /// "Extra" gap): a route whose caller picks streaming with a request
    /// field (`stream`/`stream_format`) rather than a different path or verb
    /// — `POST /v1/chat/completions`, `/v1/completions`, `/v1/messages`,
    /// `/v1/responses` — so `Resp::Json` alone was documenting only the
    /// non-streaming half. `json` is the plain body's schema; `events` is the
    /// SSE side, rendered exactly like [`Resp::Sse`]'s (`x-lmgw-sse-events`),
    /// on the same response rather than a second one — OpenAPI has one
    /// response per status code, and both shapes answer the same `200`.
    JsonOrSse {
        json: SchemaFn,
        events: &'static [(&'static str, SchemaFn)],
    },
    /// `POST /api/session/login`'s 302.
    Redirect,
    NoContent,
    /// This document itself (`/api/openapi.json`, `/v1/openapi.json`).
    Doc,
}

/// Which dialect a route speaks (§4.2, `x-lmgw-dialect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    OpenAi,
    Anthropic,
    LlamaCpp,
    JsonRpc,
    Dashboard,
}

impl Dialect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => lmgw_api_types::openapi_ext::dialect::OPENAI,
            Self::Anthropic => lmgw_api_types::openapi_ext::dialect::ANTHROPIC,
            Self::LlamaCpp => lmgw_api_types::openapi_ext::dialect::LLAMACPP,
            Self::JsonRpc => lmgw_api_types::openapi_ext::dialect::JSONRPC,
            Self::Dashboard => lmgw_api_types::openapi_ext::dialect::DASHBOARD,
        }
    }

    /// The `default` response's error envelope (§4.3, §4.8) — the schema
    /// component name `schemas::error_ref` resolves to.
    pub fn error_schema_name(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAiError",
            Self::Anthropic => "AnthropicError",
            Self::LlamaCpp => "LlamaCppError",
            Self::JsonRpc => "JsonRpcError",
            Self::Dashboard => "ApiError",
        }
    }
}

/// A tag's group (§4.3) — `tags.rs`'s [`super::tags::TagDef::group`], and
/// `x-lmgw-group`'s value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Group {
    Inference,
    DashboardApi,
    Ops,
    AgentRuntime,
}

impl Group {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inference => "Inference",
            Self::DashboardApi => "Dashboard API",
            Self::Ops => "Ops",
            Self::AgentRuntime => "Agent runtime",
        }
    }
}

/// Every plane's routes, concatenated in plane-declaration order.
pub(crate) fn all_routes() -> Vec<DocRoute> {
    let mut routes = Vec::new();
    routes.extend(super::planes::inference::routes());
    routes.extend(super::planes::dashboard::routes());
    routes.extend(super::planes::usage::routes());
    routes.extend(super::planes::docs::routes());
    routes.extend(super::planes::agents::routes());
    routes.extend(super::planes::session::routes());
    routes
}
