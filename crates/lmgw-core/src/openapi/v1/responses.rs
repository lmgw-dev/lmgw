//! `POST /v1/responses` and its stored-response siblings, read from
//! `ingress::responses::parse_request` and `ResponsesEncoder` — not from
//! OpenAI's own Responses docs. lmgw implements this itself (llama-server has none);
//! see the module doc there for what is faithful and what is refused rather
//! than silently approximated.

use schemars::generate::SchemaGenerator;
use schemars::Schema;
use serde_json::{json, Value};

use super::super::schemas;

fn input_item() -> Value {
    json!({
        "description": "A message ({type:message,role,content}), a prior turn's \
            {type:function_call,call_id,name,arguments} (call_id as lmgw gave it: a Gemini \
            model's carries its thought signature and runs to hundreds of characters; echo \
            it exactly, and do not send it to another provider directly) or \
            {type:function_call_output,call_id,output} (output a string, or an array of \
            {type:input_text,text} and {type:input_image,image_url} items: a base64 data: \
            image goes on as an image where the upstream's tool results take one and as a \
            named placeholder elsewhere, another URL as a resource naming it, and any other \
            item as its JSON text), an mcp_call echo, or an \
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
                against its own servers, never against a client-supplied server_url."},
            "allowed_tools": {
                "oneOf": [
                    {"type": "array", "items": {"type": "string"}},
                    {"type": "object", "properties": {
                        "tool_names": {"type": "array", "items": {"type": "string"}}}}
                ],
                "description": "Tool names, as a list or {tool_names}; a name matches the \
                    exposed or the server's own spelling (docs__query or query). An empty \
                    list leaves nothing. read_only is refused: lmgw does not read MCP tool \
                    annotations."
            },
            "require_approval": {"description": "\"never\" (default) | \"always\" | \
                {never:{tool_names}, always:{tool_names}}. read_only is refused, as in \
                allowed_tools."}
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
                "store": {"type": "boolean", "description": "Whether lmgw keeps this \
                    response, so a later call can continue it with previous_response_id \
                    (default true). Storing also needs the gateway's response store, which \
                    is on by default: a request can turn storing off, never on. The \
                    response object's store says whether this response was kept, so a \
                    client can see up front whether previous_response_id will work. On an \
                    upstream that implements the Responses API itself the body goes there \
                    as sent, and storing is the provider's."},
                "metadata": {"description": "Echoed back verbatim on the response object."},
                "temperature": {"type": "number"},
                "top_p": {"type": "number"},
                "top_k": {"type": "integer"},
                "max_output_tokens": {"type": "integer"},
                "max_tool_calls": {"type": "integer"},
                "parallel_tool_calls": {"type": "boolean", "default": true},
                "previous_response_id": {"type": "string", "description": "Continues a \
                    stored response: its conversation, tool calls and results included, \
                    goes ahead of this call's input, and mcp_approval_response items settle \
                    the calls it stopped at. 400 while the gateway's response store is \
                    switched off; 404 when no stored response has this id (it was evicted, \
                    or created with storing off)."},
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
                    run synchronously."},
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
        "required": [
            "input_tokens", "input_tokens_details", "output_tokens", "output_tokens_details",
            "total_tokens"
        ],
        "properties": {
            "input_tokens": {"type": "integer"},
            "input_tokens_details": {
                "type": "object",
                "required": ["cached_tokens"],
                "properties": {
                    "cached_tokens": {"type": "integer"},
                    "cache_write_tokens": {"type": "integer"}
                },
                "description": "Tokens read from and written to the prompt cache, both \
                    part of input_tokens; 0 when the upstream reported none (cache_write_tokens \
                    is lmgw's own addition and absent from a verbatim upstream body)."
            },
            "output_tokens": {"type": "integer"},
            "output_tokens_details": {
                "type": "object",
                "required": ["reasoning_tokens"],
                "properties": {"reasoning_tokens": {"type": "integer"}}
            },
            "total_tokens": {"type": "integer"}
        }
    })
}

/// The usage object, or `null` (a native upstream's response that has not
/// finished, or reports none).
fn usage_nullable() -> Value {
    let mut usage = usage_schema();
    usage["type"] = json!(["object", "null"]);
    usage
}

/// `ResponsesEncoder::snapshot`. The non-streaming shape — the one
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
            "required": [
                "id", "object", "created_at", "status", "model", "output", "error",
                "incomplete_details"
            ],
            "properties": {
                "id": {"type": "string"},
                "object": {"type": "string", "enum": ["response"]},
                "created_at": {"type": "integer"},
                "status": {"type": "string", "enum": ["queued", "in_progress", "completed", "incomplete", "failed", "cancelled"]},
                "model": {"type": "string"},
                "output": {"type": "array", "items": {"type": "object", "additionalProperties": true}},
                "error": {"type": ["object", "null"]},
                "incomplete_details": {"type": ["object", "null"], "required": ["reason"],
                    "properties": {"reason": {"type": "string"}}},
                "usage": usage_nullable(),
                "store": {"type": "boolean", "description": "Whether lmgw stored this response, so a later \
                    request can continue from it with previous_response_id. Sent by lmgw's own \
                    translation; a verbatim upstream body may leave it out."}
            },
            "description": "An upstream that speaks the Responses API natively is relayed as it \
                sends the body: such a body may carry status queued (background), a null or \
                missing usage and no store. lmgw's own translation always sends usage and store.",
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
/// comment lists (that doc comment explains why it is a separate
/// schema): `{type, sequence_number, ...}`, event-specific
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
/// (`ResponsesRequest::input_items`), for a client that wants to see what it
/// originally sent.
pub(crate) fn input_items(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "ResponseInputItems",
        schemars::json_schema!({
            "type": "object",
            "required": ["object", "data"],
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
            "required": ["id", "object", "deleted"],
            "properties": {
                "id": {"type": "string"},
                "object": {"type": "string", "enum": ["response"]},
                "deleted": {"type": "boolean", "enum": [true]}
            }
        }),
    )
}
