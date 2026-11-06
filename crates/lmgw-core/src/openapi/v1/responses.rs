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
            previous response's pending call. On an upstream that implements the Responses \
            API itself (the OpenAI platform, a compatible row flagged for it) every other \
            item the API defines goes there as sent: an {type:item_reference,id} (or an \
            untyped {id}), a hosted tool's record (web_search_call, file_search_call, \
            code_interpreter_call, image_generation_call, computer_call and its output, …), \
            and content parts lmgw's loop has no reading for (input_file, an input_image by \
            file_id). lmgw's own loop refuses each of them by name (400 unsupported). For a \
            client key, an item_reference id and any input item's own id (a reasoning \
            item's rs_…, a message's msg_…, a hosted call's) pass only when a native answer \
            given to that key carried the item, and a file_id anywhere in the body never \
            does (lmgw does not proxy /v1/files, so no client key made one): either answers \
            404 as an unknown previous_response_id does. The owner key passes.",
    })
}

/// A tool the upstream runs itself: forwarded natively, refused by the loop.
fn hosted_tool() -> Value {
    json!({
        "type": "object",
        "required": ["type"],
        "properties": {
            "type": {"type": "string", "not": {"enum": ["function", "mcp"]},
                "description": "web_search, file_search, code_interpreter, \
                    image_generation, computer_use_preview, shell, … as the upstream's own \
                    Responses API defines them."}
        },
        "additionalProperties": true,
        "description": "A hosted tool: goes with the body to an upstream that implements \
            the Responses API itself, which runs it; lmgw's own loop refuses it (400 \
            unsupported), and so does a request that also needs the loop (mcp tools lmgw \
            serves, a previous_response_id lmgw stored), naming both. For a client key, a \
            tool that names something stored under the provider account (a file_search \
            vector_store_ids entry, a code_interpreter container id or file_ids, a shell \
            container_reference or skill_id) answers 404 as an unknown id does; the owner key \
            passes. Fees such a tool bills beyond its tokens (web search, file search, \
            containers, image generation) are not priced yet: its row is unpriced, with a \
            price_note naming the calls, and budgets count its token part as a lower bound."
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
            "server_url": {"type": "string", "description": "A server for the upstream to \
                dial (OpenAI's hosted MCP tool); lmgw never dials it. A block whose \
                server_url is not this gateway's own /mcp is not lmgw's to serve: it goes \
                to an upstream that takes /v1/responses natively, and a request served by \
                lmgw's own loop is refused with a 400 naming it. This gateway's own /mcp \
                reads as no server_url."},
            "connector_id": {"type": "string", "description": "One of OpenAI's connectors: \
                never lmgw's to serve, handled as a foreign server_url."},
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
                    "items": {"oneOf": [function_tool(), mcp_tool(), hosted_tool()]},
                    "description": "function tools the loop hands back to the client; mcp \
                        tools run server-side on lmgw's own registered MCP servers, and keep \
                        the request in lmgw's own loop on an upstream that implements the \
                        Responses API itself (an mcp label lmgw does not serve goes there \
                        with the body). Hosted tools (web_search, file_search, \
                        code_interpreter, image_generation, computer_use_preview, …) go \
                        natively to such an upstream, which runs them, and are refused by \
                        lmgw's own loop."
                },
                "tool_choice": {"description": "\"auto\" | \"none\" | \"required\" | \
                    {type:function,name} everywhere; any other form the upstream's API \
                    defines ({type:web_search_preview}, {type:allowed_tools,…}, \
                    {type:mcp,server_label}, …) goes natively to an upstream that implements \
                    the Responses API itself and is refused by lmgw's own loop."},
                "service_tier": {"type": "string", "description": "OpenAI's processing tier \
                    (auto, default, flex, priority, fast, scale, ultrafast), forwarded as sent; on an \
                    OpenAI platform row lmgw sends auto when it is unset, and prices the answer at \
                    the tier the answer reports. A request by a caller with a budget (its key's, \
                    or the gateway's) on a cloud model with no price on file for what it is billed \
                    in is refused before the send: 403 budget_unpriced, naming the model and the \
                    missing price. A flex request needs a flex price: no other tier's price bounds \
                    a flex answer's spend, which is cheaper."},
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
                    or created with storing off). On an upstream that implements the \
                    Responses API itself, an id lmgw holds continues in lmgw's own loop and \
                    any other id goes to the upstream with the body; with mcp tools lmgw \
                    serves, such an id is a 400, since neither side can serve both."},
                "reasoning": {
                    "type": "object",
                    "properties": {"effort": {"type": "string"}},
                    "description": "The one reasoning control this dialect reads: \
                        reasoning.effort."
                },
                "text": {
                    "type": "object",
                    "description": "text.format (json_schema, json_object, text) is the \
                        structured output every upstream gets in its own spelling, as on \
                        /v1/chat/completions; a native Responses upstream gets the body as \
                        sent. Its refusals are 400 response_format_unsupported / \
                        invalid_json_schema with param text.format.",
                    "additionalProperties": true
                },
                "include": {"type": "array", "items": {"type": "string"}},
                "background": {"type": "boolean", "description": "true is refused on every \
                    path: a background response finishes after its answer has left, so lmgw \
                    could never record what it spent or hold it to a budget."},
                "truncation": {"type": "string", "description": "\"auto\" goes natively to \
                    an upstream that implements the Responses API itself; lmgw's own loop \
                    refuses it, since it never silently drops conversation history."}
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
                "output": {"type": "array", "items": {"type": "object", "additionalProperties": true},
                    "description": "Items are loosely typed here. An output_text part of a \
                        message item carries `logprobs: []`: lmgw relays no log probabilities \
                        yet; OpenAI's schema requires the field."},
                "access_programs": {"type": "null", "description": "Always null; present \
                    because OpenAI's schema requires the field."},
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
            "description": "SSE event: response.<name>, data: <this object>. The \
                response.output_text.delta and .done events carry `logprobs: []` (lmgw relays \
                no log probabilities yet; OpenAI's schema requires the field)."
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

/// `POST /v1/responses/input_tokens`: OpenAI's token count of a Responses
/// create body (`TokenCountsResource`).
pub(crate) fn input_tokens(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "ResponseInputTokens",
        schemars::json_schema!({
            "type": "object",
            "required": ["object", "input_tokens"],
            "properties": {
                "object": {"type": "string", "enum": ["response.input_tokens"]},
                "input_tokens": {"type": "integer"}
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
