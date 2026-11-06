//! The `default`-response error envelopes : one
//! schema per [`Dialect`], registered into `g` the first time a route of that
//! dialect is built ([`super::super::schemas::error_ref`]) rather than up
//! front — a document with no routes of some dialect never grows a
//! component nothing points at.
//!
//! Shapes read from the code that actually builds them, not from provider
//! docs: [`crate::error::GatewayError::to_openai_json`],
//! [`crate::error::GatewayError::to_anthropic_json`],
//! `proxy::tokenize::llama_error`, the JSON-RPC error frame
//! `mcp::ingress.rs` sends (`rpc_err`), and api-types `ApiError`, which is
//! already `JsonSchema`-derived so the dashboard plane just needs its
//! own `$ref`.

use schemars::generate::SchemaGenerator;
use schemars::Schema;

use super::super::registry::Dialect;
use super::super::schemas;

/// The error component for `dialect`, registered on first use — safe to call
/// once per operation that needs it: [`schemas::named`] no-ops on a second
/// call with identical content.
///
/// Each helper below hard-codes its own component name as a literal (so the
/// name sits next to the schema it names, not in a lookup table three files
/// away) — the `debug_assert!` is what keeps that literal honest against
/// [`Dialect::error_schema_name`], which is what `Resp::Redirect`/`NoContent`
/// use for the *ref*'s name and is otherwise nothing this module reads.
pub(crate) fn register(g: &mut SchemaGenerator, dialect: Dialect) -> Schema {
    let schema = match dialect {
        Dialect::OpenAi => openai_error(g),
        Dialect::Anthropic => anthropic_error(g),
        Dialect::LlamaCpp => llamacpp_error(g),
        Dialect::JsonRpc => jsonrpc_error(g),
        Dialect::Dashboard => api_error(g),
        Dialect::Lab => g.subschema_for::<lmgw_api_types::audio_lab::LabError>(),
    };
    debug_assert_eq!(
        schema
            .as_value()
            .get("$ref")
            .and_then(serde_json::Value::as_str),
        Some(format!("#/components/schemas/{}", dialect.error_schema_name()).as_str()),
        "the literal component name in v1::errors must match Dialect::error_schema_name()"
    );
    schema
}

/// `GatewayError::to_openai_json`: `error.code` is the
/// gateway's own machine word ([`crate::error::GatewayError::kind`], e.g.
/// `gpu_hold`, `not_found`), not an HTTP status — `param` names the format
/// field for the two structured-output codes and is `null` otherwise
/// (structured-output design §8).
fn openai_error(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "OpenAiError",
        schemars::json_schema!({
            "type": "object",
            "required": ["error"],
            "properties": {
                "error": {
                    "type": "object",
                    "required": ["message", "type", "code"],
                    "properties": {
                        "message": {"type": "string"},
                        "type": {"type": "string", "description": "e.g. invalid_request_error, \
                            authentication_error, not_found_error, rate_limit_error, api_error."},
                        "param": {"type": ["string", "null"], "description": "The request \
                            field the error is about: response_format (chat completions, \
                            completions) or text.format (/v1/responses) for the codes \
                            response_format_unsupported and invalid_json_schema; null for every \
                            other error."},
                        "code": {"type": "string", "description": "lmgw's own machine-readable \
                            error code (e.g. gpu_hold, gpu_benchmark, gpu_deferred, gpu_setup — \
                            a 503: the local model could not start on the graphics card, and \
                            the message says what on the computer lmgw runs on is not ready \
                            (fixed there, not by retrying), engine_download — a 502: the engine \
                            the local model runs in could not be downloaded, \
                            response_format_unsupported — the model or its upstream cannot \
                            produce the asked structured output type, invalid_json_schema — \
                            the upstream rejected the schema, its message kept, \
                            budget_unpriced — a budgeted caller's request on a cloud model \
                            with no price on file for what it is billed in, flex without a flex \
                            price among them), not the HTTP status."}
                    }
                }
            }
        }),
    )
}

/// `GatewayError::to_anthropic_json`.
fn anthropic_error(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AnthropicError",
        schemars::json_schema!({
            "type": "object",
            "required": ["type", "error"],
            "properties": {
                "type": {"type": "string", "enum": ["error"]},
                "error": {
                    "type": "object",
                    "required": ["type", "message"],
                    "properties": {
                        "type": {"type": "string", "description": "e.g. invalid_request_error, \
                            not_found_error, permission_error, overloaded_error, api_error."},
                        "message": {"type": "string"}
                    }
                }
            }
        }),
    )
}

/// `proxy::tokenize::llama_error`: `lmgw_code` is
/// [`crate::error::GatewayError::kind`], kept alongside llama.cpp's own
/// `type` word so a client already checking for `gpu_hold` still can. Not
/// used by the gate layer's own 401/403/429 or a declared-Content-Length 413,
/// which stay in the shared `/v1` OpenAI dialect.
fn llamacpp_error(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "LlamaCppError",
        schemars::json_schema!({
            "type": "object",
            "required": ["error"],
            "properties": {
                "error": {
                    "type": "object",
                    "required": ["code", "message", "type", "lmgw_code"],
                    "properties": {
                        "code": {"type": "integer", "description": "The HTTP status, repeated \
                            in the body as llama.cpp does."},
                        "message": {"type": "string"},
                        "type": {
                            "type": "string",
                            "enum": [
                                "invalid_request_error",
                                "authentication_error",
                                "permission_error",
                                "not_found_error",
                                "not_supported_error",
                                "unavailable_error",
                                "server_error"
                            ]
                        },
                        "lmgw_code": {"type": "string", "description": "lmgw's own machine-readable error code, \
                            e.g. gpu_hold, gpu_benchmark, gpu_deferred, gpu_setup, \
                            engine_download."}
                    }
                }
            }
        }),
    )
}

/// The JSON-RPC 2.0 error frame `mcp::ingress.rs`'s `rpc_err` sends —
/// `{jsonrpc, id, error: {code, message}}`. `id` echoes the request's id (or
/// `null` when it could not be read); its type is left unconstrained, as
/// JSON-RPC's own spec leaves it (string, number or null).
fn jsonrpc_error(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "JsonRpcError",
        schemars::json_schema!({
            "type": "object",
            "required": ["jsonrpc", "error"],
            "properties": {
                "jsonrpc": {"type": "string", "enum": ["2.0"]},
                "id": {},
                "error": {
                    "type": "object",
                    "required": ["code", "message"],
                    "properties": {
                        "code": {"type": "integer"},
                        "message": {"type": "string"}
                    }
                }
            }
        }),
    )
}

/// The dashboard plane's uniform error body (api-types `ApiError`),
/// already `JsonSchema`-derived; the ops plane (`Dialect::Dashboard`,
/// `build.rs`'s `op_operation`) refers to it.
fn api_error(g: &mut SchemaGenerator) -> Schema {
    g.subschema_for::<lmgw_api_types::ApiError>()
}
