//! `POST /v1/responses` and its stored-response siblings (api-docs design
//! §4.8), read from `ingress::responses::parse_request` (`responses.rs:146`)
//! and `ResponsesEncoder` (`:756`, `:1304` `snapshot`) — not from OpenAI's
//! own Responses docs. lmgw implements this itself (llama-server has none);
//! see the module doc there for what is faithful and what is refused rather
//! than silently approximated.

use schemars::generate::SchemaGenerator;
use schemars::Schema;
use serde_json::{json, Value};

use super::super::schemas;

fn input_item() -> Value {
    json!({
        "description": "A message ({type:message,role,content}), a prior turn's \
            {type:function_call,call_id,name,arguments} or \
            {type:function_call_output,call_id,output}, an mcp_call echo, or an \
            {type:mcp_approval_response,approval_request_id,approve,reason} verdict on a \
            previous response's pending call.",
    })
}

fn mcp_tool() -> Value {
    json!({
        "type": "object",
        "required": ["type", "server_label"],
        "properties": {
            "type": {"type": "string", "enum": ["mcp"]},
            "server_label": {"type": "string", "description": "Matches a registered MCP \
                server's own tool prefix (or its name when it has none) — lmgw resolves this \
                against its own servers, never a client-supplied server_url."},
            "allowed_tools": {"type": "array", "items": {"type": "string"}},
            "require_approval": {"description": "\"never\" (default) | \"always\" | \
                {never:{tool_names}, always:{tool_names}}."}
        }
    })
}

fn function_tool() -> Value {
    json!({
        "type": "object",
        "required": ["type", "name"],
        "properties": {
            "type": {"type": "string", "enum": ["function"]},
            "name": {"type": "string"},
            "description": {"type": "string"},
            "parameters": {"type": "object", "additionalProperties": true}
        }
    })
}

pub(crate) fn request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "ResponsesRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["model"],
            "properties": {
                "model": {"type": "string"},
                "input": {"description": "A string (one user turn), or an array of items.",
                    "items": input_item()},
                "instructions": {"type": "string", "description": "Becomes the leading \
                    system message; an explicit empty string drops it on a chained call."},
                "tools": {
                    "type": "array",
                    "items": {"oneOf": [function_tool(), mcp_tool()]},
                    "description": "function tools the loop hands back to the client; mcp \
                        tools run server-side on lmgw's own registered MCP servers. Hosted \
                        tools (web_search, file_search, code_interpreter, image_generation, \
                        computer_use) are refused."
                },
                "tool_choice": {"description": "\"auto\" | \"none\" | \"required\" | \
                    {type:function,name}."},
                "stream": {"type": "boolean", "default": false},
                "store": {"type": "boolean", "description": "This stage stores nothing \
                    regardless of the value sent; the response object always answers \
                    store: false so a client can see previous_response_id will not work \
                    before it tries."},
                "metadata": {"description": "Echoed back verbatim on the response object."},
                "temperature": {"type": "number"},
                "top_p": {"type": "number"},
                "top_k": {"type": "integer"},
                "max_output_tokens": {"type": "integer"},
                "max_tool_calls": {"type": "integer"},
                "parallel_tool_calls": {"type": "boolean", "default": true},
                "previous_response_id": {"type": "string", "description": "Refused: this \
                    stage stores nothing, so a chained call would silently lose the earlier \
                    conversation."},
                "reasoning": {
                    "type": "object",
                    "properties": {"effort": {"type": "string"}},
                    "description": "The one reasoning control this dialect reads: \
                        reasoning.effort."
                },
                "text": {
                    "type": "object",
                    "description": "text.format is translated to the chat-completions \
                        response_format (json_schema/json_object) for OpenAI-protocol \
                        upstreams.",
                    "additionalProperties": true
                },
                "include": {"type": "array", "items": {"type": "string"}},
                "background": {"type": "boolean", "description": "Refused: responses always \
                    run synchronously here."},
                "truncation": {"type": "string", "description": "Refused when \"auto\": lmgw \
                    never silently drops conversation history."}
            },
            "additionalProperties": true
        }),
    )
}

fn usage_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "input_tokens": {"type": "integer"},
            "input_tokens_details": {
                "type": "object",
                "properties": {"cached_tokens": {"type": "integer"}}
            },
            "output_tokens": {"type": "integer"},
            "output_tokens_details": {
                "type": "object",
                "properties": {"reasoning_tokens": {"type": "integer"}}
            },
            "total_tokens": {"type": "integer"}
        }
    })
}

/// `ResponsesEncoder::snapshot` (`:1304`). The non-streaming shape — the one
/// this operation documents; `stream: true` instead answers
/// `text/event-stream` with 22 distinct `response.*` event names
/// (`response.created` … `response.completed`/`.incomplete`/`.failed`,
/// `response.output_text.delta`, the `mcp_*`/`function_call_*` tool-call
/// events, …), each `{type, sequence_number, ...}` — prose, not a second
/// schema, for the same reason as the chat route
/// (`v1::chat::response`'s doc comment). `output` items are loosely typed —
/// their shape is exactly what those event names build up.
pub(crate) fn response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "Response",
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "object": {"type": "string", "enum": ["response"]},
                "created_at": {"type": "integer"},
                "status": {"type": "string", "enum": ["in_progress", "completed", "incomplete", "failed"]},
                "model": {"type": "string"},
                "output": {"type": "array", "items": {"type": "object", "additionalProperties": true}},
                "error": {"type": ["object", "null"]},
                "incomplete_details": {"type": ["object", "null"], "properties": {"reason": {"type": "string"}}},
                "usage": usage_schema(),
                "store": {"type": "boolean", "enum": [false]}
            },
            "additionalProperties": true
        }),
    )
}

pub(crate) fn example() -> Value {
    json!({
        "model": "<pick a model>",
        "input": "Say hello in one short sentence."
    })
}

/// One of the 22 distinct `response.*` SSE events `response`'s own doc
/// comment lists (WP4 "Extra" gap — that doc comment explains why this did
/// not exist until now): `{type, sequence_number, ...}`, event-specific
/// fields carried by `additionalProperties` rather than re-deriving all 22
/// shapes here.
pub(crate) fn stream_event(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "ResponseStreamEvent",
        schemars::json_schema!({
            "type": "object",
            "required": ["type", "sequence_number"],
            "properties": {
                "type": {"type": "string", "description": "response.created … \
                    response.completed/.incomplete/.failed, response.output_text.delta, the \
                    mcp_*/function_call_* tool-call events, …"},
                "sequence_number": {"type": "integer"}
            },
            "additionalProperties": true,
            "description": "SSE event: response.<name>, data: <this object>."
        }),
    )
}

/// `GET /v1/responses/{id}/input_items`: this turn's `input` array as sent
/// (`ResponsesRequest::input_items`), for a client that stored `store: true`
/// and wants to see what it originally sent.
pub(crate) fn input_items(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "ResponseInputItems",
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "object": {"type": "string", "enum": ["list"]},
                "data": {"type": "array", "items": {"type": "object", "additionalProperties": true}}
            }
        }),
    )
}

/// `DELETE /v1/responses/{id}`: OpenAI's deletion-confirmation shape.
pub(crate) fn deletion(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "ResponseDeleted",
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "object": {"type": "string", "enum": ["response"]},
                "deleted": {"type": "boolean", "enum": [true]}
            }
        }),
    )
}
