//! `POST /v1/chat/completions` (api-docs design §4.8): request, response and
//! the `chat.completion.chunk` SSE frame, read from what
//! `ingress::openai::parse_chat_request` actually reads and
//! `serialize_completion`/`OpenaiStreamEncoder` actually emit — not from
//! OpenAI's own docs. `additionalProperties: true` throughout: an
//! OpenAI-protocol upstream gets every unmodeled key verbatim
//! (`passthrough_fields`).

use schemars::generate::SchemaGenerator;
use schemars::Schema;
use serde_json::{json, Value};

use super::super::schemas;

/// One `messages[]` entry. Kept intentionally loose on `content` (string or
/// an array of `text`/`image_url`/`input_audio` parts) and on the assistant
/// fields lmgw replays (`reasoning_content`/`reasoning`, `tool_calls`) —
/// exhaustively nesting every part shape here would be re-describing
/// OpenAI's own spec rather than lmgw's reading of it (house rule: accuracy
/// over volume).
fn message_schema() -> Value {
    json!({
        "type": "object",
        "required": ["role"],
        "properties": {
            "role": {
                "type": "string",
                "enum": ["system", "developer", "user", "assistant", "tool", "function"],
                "description": "system and developer both become an IR system message; tool \
                    and function both become an IR tool-result message."
            },
            "content": {
                "description": "A string, or an array of parts: {type:text,text}, \
                    {type:image_url,image_url:{url}} (a plain URL or a data: URI), \
                    {type:input_audio,input_audio:{data,format}} (data: URI, or raw base64 \
                    with format required). A tool message keeps only its text, as OpenAI \
                    defines it; any other part is dropped with a WARN naming it.",
            },
            "tool_call_id": {"type": "string", "description": "role: tool only."},
            "name": {"type": "string", "description": "role: tool only, forwarded as the \
                tool result's name."},
            "reasoning_content": {"type": "string", "description": "role: assistant only. \
                Replayed as the leading reasoning part (also accepted spelled \
                'reasoning' — DeepSeek/llama.cpp vs. OpenRouter)."},
            "tool_calls": {
                "type": "array",
                "description": "role: assistant only: [{id, type:function, \
                    function:{name, arguments: <JSON string>}}].",
                "items": {"type": "object", "additionalProperties": true}
            }
        },
        "additionalProperties": true
    })
}

fn tool_schema() -> Value {
    json!({
        "type": "object",
        "required": ["type", "function"],
        "properties": {
            "type": {"type": "string", "enum": ["function"]},
            "function": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": {"type": "string"},
                    "description": {"type": "string"},
                    "parameters": {"type": "object", "additionalProperties": true}
                }
            }
        }
    })
}

pub(crate) fn request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "ChatCompletionRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["model", "messages"],
            "properties": {
                "model": {"type": "string", "description": "An alias, a local model id, a \
                    candidate alias, or 'prefix/…' passthrough to an exposed upstream."},
                "messages": {"type": "array", "items": message_schema()},
                "tools": {"type": "array", "items": tool_schema()},
                "tool_choice": {
                    "description": "\"auto\" | \"none\" | \"required\" | \
                        {type:function,function:{name}}."
                },
                "stream": {"type": "boolean", "default": false},
                "temperature": {"type": "number"},
                "top_p": {"type": "number"},
                "top_k": {"type": "integer", "description": "Not standard OpenAI, but \
                    accepted — llama.cpp supports it."},
                "max_completion_tokens": {"type": "integer", "description": "Wins over \
                    max_tokens when both are sent."},
                "max_tokens": {"type": "integer"},
                "presence_penalty": {"type": "number"},
                "frequency_penalty": {"type": "number"},
                "seed": {"type": "integer"},
                "stop": {"description": "A string, or an array of strings."},
                "reasoning_effort": {"type": "string", "description": "lmgw extension: an \
                    effort level, or 'none' for off — one spelling of the reasoning control \
                    (also x-lmgw-reasoning-effort, and reasoning.effort below); the header \
                    wins when both are sent."},
                "reasoning": {
                    "type": "object",
                    "description": "lmgw extension, OpenRouter's shape: {effort, \
                        max_tokens, enabled, exclude}. Read for the reasoning tier and \
                        forwarded verbatim.",
                    "additionalProperties": true
                },
                "reasoning_budget_tokens": {"type": "integer", "description": "lmgw \
                    extension: a thinking-token budget (also x-lmgw-reasoning-budget). \
                    Forwarded verbatim alongside thinking_budget_tokens — whichever \
                    spelling a llama-server version reads."},
                "thinking_budget_tokens": {"type": "integer", "description": "Same as \
                    reasoning_budget_tokens; both are read and both stay in the forwarded \
                    body."},
                "chat_template_kwargs": {
                    "type": "object",
                    "description": "Only enable_thinking is read (llama-server's own \
                        template toggle, distinct from the protocol-neutral reasoning \
                        controls above); every key is forwarded verbatim.",
                    "additionalProperties": true
                }
            },
            "additionalProperties": true
        }),
    )
}

fn choice_message() -> Value {
    json!({
        "type": "object",
        "properties": {
            "role": {"type": "string", "enum": ["assistant"]},
            "content": {"type": ["string", "null"]},
            "reasoning_content": {"type": "string"},
            "tool_calls": {"type": "array", "items": {"type": "object", "additionalProperties": true}}
        }
    })
}

fn usage_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "prompt_tokens": {"type": "integer"},
            "completion_tokens": {"type": "integer"},
            "total_tokens": {"type": "integer"},
            "prompt_tokens_details": {
                "type": "object",
                "properties": {"cached_tokens": {"type": "integer"}},
                "description": "Present only when the upstream reported a cache hit."
            },
            "completion_tokens_details": {
                "type": "object",
                "properties": {"reasoning_tokens": {"type": "integer"}},
                "description": "Present only when the upstream reported reasoning tokens."
            }
        }
    })
}

/// The non-streaming shape — the one this operation documents. With
/// `stream: true` lmgw instead answers `text/event-stream`:
/// `chat.completion.chunk` frames shaped like this object's `choices[].delta`
/// in place of `choices[].message`, ending with the literal `data: [DONE]`.
/// `OpenAPI`'s response object cannot express "JSON or SSE depending on a
/// request field", so the streaming shape is prose, not a second schema.
pub(crate) fn response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "ChatCompletion",
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "object": {"type": "string", "enum": ["chat.completion"]},
                "created": {"type": "integer"},
                "model": {"type": "string"},
                "choices": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "index": {"type": "integer"},
                            "message": choice_message(),
                            "finish_reason": {"type": ["string", "null"]}
                        }
                    }
                },
                "usage": usage_schema()
            }
        }),
    )
}

/// The `chat.completion.chunk` SSE frame `stream: true` answers with instead
/// of [`response`] (WP4 "Extra" gap — `response`'s own doc comment explains
/// why this did not exist until now): `choices[].delta` in place of
/// `choices[].message`, the same `id`/`created`/`model` carried on every
/// frame, and a final `usage`-bearing frame when the client asked for one
/// (`stream_options.include_usage`). The stream ends with the literal
/// `data: [DONE]`, which is not JSON and so has no schema of its own.
pub(crate) fn stream_chunk(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "ChatCompletionChunk",
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "object": {"type": "string", "enum": ["chat.completion.chunk"]},
                "created": {"type": "integer"},
                "model": {"type": "string"},
                "choices": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "index": {"type": "integer"},
                            "delta": choice_message(),
                            "finish_reason": {"type": ["string", "null"]}
                        }
                    }
                },
                "usage": usage_schema()
            },
            "description": "One frame: `data: <this object>`, repeated, ending with the \
                literal `data: [DONE]`."
        }),
    )
}

/// `model` is `"<pick a model>"` (§4.10): the tester replaces it with
/// whichever the caller picks.
pub(crate) fn example() -> Value {
    json!({
        "model": "<pick a model>",
        "messages": [{"role": "user", "content": "Say hello in one short sentence."}]
    })
}
