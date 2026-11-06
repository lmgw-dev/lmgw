//! `POST /v1/messages` and `POST /v1/messages/count_tokens`, read from
//! `ingress::anthropic::parse_messages_request` and `parse_reasoning` and
//! `serialize_completion`/`AnthropicStreamEncoder`. Unlike the chat ingress,
//! nothing outside the modeled keys is forwarded (`passthrough: Default`) —
//! an Anthropic-protocol upstream gets exactly the client body lmgw
//! understood, never a raw pass of unknown keys.

use schemars::generate::SchemaGenerator;
use schemars::Schema;
use serde_json::{json, Value};

use super::super::schemas;

fn content_block() -> Value {
    json!({
        "description": "A string, or an array of blocks: {type:text,text}, \
            {type:image,source:{type:base64,media_type,data} | {type:url,url}}, \
            {type:tool_use,id,name,input} (the id as lmgw gave it: a Gemini model's carries \
            its thought signature and runs to hundreds of characters; echo it exactly, and \
            do not send it to another provider directly), {type:tool_result,tool_use_id,content,is_error}, \
            {type:thinking,thinking,signature} (replayed; unsigned when lmgw itself \
            produced it). Every other block type (redacted_thinking, the provider's own \
            tool blocks, a type newer than lmgw) goes to an Anthropic upstream as written; \
            any other upstream is sent none of it, and x-lmgw-params-ignored names it as \
            content.<type>. A block without a type is refused with 400. A custom tool \
            without input_schema (a server tool) is refused with 400.",
    })
}

fn message_schema() -> Value {
    json!({
        "type": "object",
        "required": ["role", "content"],
        "properties": {
            "role": {"type": "string", "enum": ["user", "assistant"]},
            "content": content_block()
        }
    })
}

fn tool_schema() -> Value {
    json!({
        "type": "object",
        "required": ["name", "input_schema"],
        "properties": {
            "name": {"type": "string"},
            "description": {"type": "string"},
            "input_schema": {"type": "object", "additionalProperties": true}
        }
    })
}

/// `POST /v1/messages`'s request. `max_tokens` is optional here even though
/// Anthropic's own API requires it — a missing one is filled by lmgw
/// (`x-lmgw-max-tokens-defaulted`) rather than refused.
pub(crate) fn messages_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AnthropicMessagesRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["model", "messages"],
            "properties": {
                "model": {"type": "string"},
                "messages": {"type": "array", "items": message_schema()},
                "system": {"description": "A string, or an array of {type:text,text} blocks, \
                    joined with blank lines."},
                "tools": {"type": "array", "items": tool_schema(), "description": "Custom \
                    tools only (input_schema required); a server tool (web_search, computer \
                    use, …) is refused with 400."},
                "tool_choice": {"description": "{type:auto|any|none|tool} \
                    (tool also carries name)."},
                "temperature": {"type": "number"},
                "top_p": {"type": "number"},
                "top_k": {"type": "integer"},
                "max_tokens": {"type": "integer"},
                "stop_sequences": {"type": "array", "items": {"type": "string"}},
                "stream": {"type": "boolean", "default": false},
                "thinking": {
                    "type": "object",
                    "description": "{type:enabled,budget_tokens} | {type:disabled} | \
                        {type:adaptive} (enabled, no budget — \"think as much as judged \
                        necessary\") | {type:between_tools} (no up-front thinking: an off \
                        that goes to an Anthropic model as written; every other model gets its \
                        own off, and x-lmgw-reasoning-ignored names between_tools). Any other \
                        type is refused with 400. display goes to an Anthropic model inside \
                        adaptive and enabled; beside an off, and to every other model, it is \
                        left out and x-lmgw-params-ignored names thinking.display.",
                    "additionalProperties": true
                },
                "output_config": {
                    "type": "object",
                    "properties": {
                        "effort": {"type": "string"},
                        "format": {"type": "object", "additionalProperties": true}
                    },
                    "description": "lmgw reads output_config.effort as the same reasoning \
                        tier x-lmgw-reasoning-effort sets, and output_config.format \
                        ({type:json_schema, schema}; the deprecated top-level output_format \
                        too) as the structured output every upstream gets in its own \
                        spelling. Its refusals are 400 invalid_request_error with the \
                        upstream's message. To an OpenAI model the schema goes with \
                        strict: true (this format guarantees the schema), so OpenAI's \
                        strict-mode rules apply — every property in required, \
                        additionalProperties false — and a schema that breaks them comes \
                        back as OpenAI's invalid_json_schema refusal, its message kept."
                },
                "context_management": {
                    "type": "object",
                    "additionalProperties": true,
                    "description": "Anthropic's context management ({edits: [...]}; send its \
                        beta flag in anthropic-beta). To an Anthropic model it goes as written, \
                        and Anthropic answers for it; a compaction edit (compact_…) included, \
                        whose compaction block comes back as Anthropic wrote it. Every other \
                        model's provider has no equivalent: it is left out and named in \
                        x-lmgw-params-ignored. A model's capabilities.context_editing on GET \
                        /v1/models says what it takes."
                },
                "compaction": {
                    "type": "object",
                    "additionalProperties": true,
                    "description": "Anthropic's on-demand compaction ({type: summarize}; send \
                        its beta flag in anthropic-beta). To an Anthropic model it goes as \
                        written, and the compaction block Anthropic answers with comes back as \
                        it wrote it (stop_reason compaction). Every other model's provider has \
                        no equivalent: it is left out and named in x-lmgw-params-ignored. A \
                        model's capabilities.context_editing.compaction says which types it \
                        takes."
                }
            },
            "additionalProperties": true
        }),
    )
}

fn content_json() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "object",
            "description": "{type:text,text} | {type:tool_use,id,name,input}; a leading \
                {type:thinking,thinking} block when the completion carried reasoning \
                (unsigned: produced by lmgw, not by Anthropic); a leading \
                {type:compaction,content,signature,…} block exactly as an Anthropic model \
                answered a compaction (send it back unchanged).",
            "additionalProperties": true
        }
    })
}

fn usage_schema() -> Value {
    json!({
        "type": "object",
        "required": ["input_tokens", "output_tokens"],
        "properties": {
            "input_tokens": {"type": "integer"},
            "output_tokens": {"type": "integer"},
            "cache_read_input_tokens": {"type": "integer", "description": "Present only when \
                the upstream reported a cache read."},
            "cache_creation_input_tokens": {"type": "integer", "description": "Present only \
                when the upstream reported cache writes."},
            "iterations": {"type": "array", "items": {"type": "object", "additionalProperties": true},
                "description": "An Anthropic model's per-iteration usage, as it reported it \
                (a compaction entry for a compaction it ran, which the top-level counts \
                leave out). Present only when the upstream reported it."}
        }
    })
}

/// The non-streaming shape — the one this operation documents. With
/// `stream: true` lmgw instead answers `text/event-stream`: the
/// `message_start → content_block_start/delta/stop* → message_delta →
/// message_stop` sequence `AnthropicStreamEncoder` sends (an `error` event
/// mid-stream on an upstream failure) — prose, not a second schema, for the
/// same reason as the chat route (`v1::chat::response`'s doc comment).
pub(crate) fn messages_response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AnthropicMessage",
        schemars::json_schema!({
            "type": "object",
            "required": [
                "id", "type", "role", "model", "content", "stop_reason", "stop_sequence", "usage"
            ],
            "properties": {
                "id": {"type": "string"},
                "type": {"type": "string", "enum": ["message"]},
                "role": {"type": "string", "enum": ["assistant"]},
                "model": {"type": "string"},
                "content": content_json(),
                "stop_reason": {"type": ["string", "null"]},
                "stop_sequence": {"type": "null"},
                "usage": usage_schema(),
                "context_management": {
                    "type": "object",
                    "additionalProperties": true,
                    "description": "The context management an Anthropic model applied \
                        ({applied_edits: [...]}), as Anthropic reported it; a stream carries \
                        it on its closing message_delta. Absent from every other model's \
                        answer."
                }
            }
        }),
    )
}

pub(crate) fn example() -> Value {
    json!({
        "model": "<pick a model>",
        "max_tokens": 256,
        "messages": [{"role": "user", "content": "Say hello in one short sentence."}]
    })
}

/// One SSE event of the `message_start → content_block_start/delta/stop* →
/// message_delta → message_stop` sequence `AnthropicStreamEncoder` sends
/// (`messages_response`'s own doc comment explains why it is a separate
/// schema), or a mid-stream `error` event on an
/// upstream failure. One shape for all of them: the `type` enum is the only
/// thing that reliably differs at this level of detail, and `additional
/// Properties: true` carries the event-specific fields (`index`, `delta`,
/// `content_block`, `usage`, …) without re-deriving Anthropic's own event
/// catalog here.
pub(crate) fn stream_event(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AnthropicStreamEvent",
        schemars::json_schema!({
            "type": "object",
            "required": ["type"],
            "properties": {
                "type": {
                    "type": "string",
                    "enum": [
                        "message_start", "content_block_start", "content_block_delta",
                        "content_block_stop", "message_delta", "message_stop", "error"
                    ]
                }
            },
            "additionalProperties": true,
            "description": "SSE event: <type>, data: <this object>, one per line of the \
                sequence above."
        }),
    )
}

// ---------------------------------------------------------------------------
// POST /v1/messages/count_tokens
// ---------------------------------------------------------------------------

/// Exactly the Anthropic SDKs' `messages.count_tokens()` body — the same
/// shape as [`messages_request`] minus `stream`/`stop_sequences` (the SDK
/// does not send them), plus `output_config`. lmgw reads `model`; everything
/// else rides to the Anthropic-protocol path verbatim (the client body
/// verbatim, only `model` replaced — the one route where the request schema
/// is also, almost, the wire body an Anthropic upstream receives).
pub(crate) fn count_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AnthropicCountTokensRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["model", "messages"],
            "properties": {
                "model": {"type": "string"},
                "messages": {"type": "array", "items": message_schema()},
                "system": {"description": "A string, or an array of {type:text,text} blocks."},
                "tools": {"type": "array", "items": tool_schema()},
                "tool_choice": {"description": "{type:auto|any|none|tool}."},
                "thinking": {"type": "object", "additionalProperties": true},
                "output_config": {"type": "object", "additionalProperties": true}
            },
            "additionalProperties": true
        }),
    )
}

pub(crate) fn count_response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AnthropicCountTokensResponse",
        schemars::json_schema!({
            "type": "object",
            "required": ["input_tokens"],
            "properties": {
                "input_tokens": {"type": "integer"},
                "context_management": {
                    "type": "object",
                    "additionalProperties": true,
                    "description": "Anthropic's own count of the input before the request's \
                        context edits ({original_input_tokens}), as it reported it. Only on \
                        an Anthropic model's count of a request with context_management."
                }
            }
        }),
    )
}

pub(crate) fn count_example() -> Value {
    json!({
        "model": "<pick a model>",
        "messages": [{"role": "user", "content": "Say hello in one short sentence."}]
    })
}
