//! `POST /v1/chat/completions`: request, response and
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
                "description": "system and developer are both treated as a system message; \
                    tool and function are both treated as a tool-result message."
            },
            "content": {
                "description": "A string, or an array of parts: {type:text,text}, \
                    {type:image_url,image_url:{url}} (a plain URL or a data: URI), \
                    {type:input_audio,input_audio:{data,format}} (data: URI, or raw base64 \
                    with format required). A tool message keeps only its text, as OpenAI \
                    defines it; any other part is dropped with a WARN naming it.",
            },
            "tool_call_id": {"type": "string", "description": "role: tool only."},
            "name": {"type": "string", "description": "role: tool only. Forwarded as the \
                tool result's name."},
            "reasoning_content": {"type": "string", "description": "role: assistant only. \
                Replayed as the leading reasoning part (also accepted spelled \
                'reasoning', as OpenRouter spells it; DeepSeek and llama.cpp use \
                reasoning_content)."},
            "tool_calls": {
                "type": "array",
                "description": "role: assistant only: [{id, type:function, \
                    function:{name, arguments: <JSON string>}}]. Send each id back as lmgw \
                    gave it: a Gemini model's call id carries the model's thought \
                    signature, which Gemini 3 refuses a replayed call without. Such ids \
                    run to hundreds of characters; echo them exactly, and do not send \
                    them to another provider directly (lmgw strips the signature there).",
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
                    candidate alias, or 'prefix/…' to pass through to an exposed upstream."},
                "messages": {"type": "array", "items": message_schema()},
                "tools": {"type": "array", "items": tool_schema()},
                "tool_choice": {
                    "description": "\"auto\" | \"none\" | \"required\" | \
                        {type:function,function:{name}}."
                },
                "stream": {"type": "boolean", "default": false},
                "stream_options": {
                    "type": "object",
                    "properties": {"include_usage": {"type": "boolean", "default": false}},
                    "description": "With stream: true, include_usage ends the stream with a \
                        chunk whose choices is [] and that carries usage; every other chunk \
                        then has usage: null. Without it no chunk carries usage. Not \
                        forwarded: lmgw asks the upstream for usage itself, for its request \
                        log."
                },
                "temperature": {"type": "number"},
                "top_p": {"type": "number"},
                "top_k": {"type": "integer", "description": "Not part of the OpenAI API, but \
                    accepted; llama.cpp supports it."},
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

/// A choice's `message` (`complete`: role and content always there) or a
/// stream frame's `delta` (every key optional: the finish frame's is `{}`).
fn choice_message(complete: bool) -> Value {
    let required: &[&str] = if complete { &["role", "content"] } else { &[] };
    json!({
        "type": "object",
        "required": required,
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
        "required": ["prompt_tokens", "completion_tokens", "total_tokens"],
        "properties": {
            "prompt_tokens": {"type": "integer"},
            "completion_tokens": {"type": "integer"},
            "total_tokens": {"type": "integer"},
            "prompt_tokens_details": {
                "type": "object",
                "properties": {
                    "cached_tokens": {"type": "integer"},
                    "cache_write_tokens": {"type": "integer"}
                },
                "description": "Present only when the upstream reported prompt-cache \
                    counts, and each field only when reported: cached_tokens read from \
                    the cache, cache_write_tokens written to it. Both are part of \
                    prompt_tokens."
            },
            "completion_tokens_details": {
                "type": "object",
                "required": ["reasoning_tokens"],
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
            "required": ["id", "object", "created", "model", "choices", "usage"],
            "properties": {
                "id": {"type": "string"},
                "object": {"type": "string", "enum": ["chat.completion"]},
                "created": {"type": "integer"},
                "model": {"type": "string"},
                "choices": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["index", "message", "finish_reason"],
                        "properties": {
                            "index": {"type": "integer"},
                            "message": choice_message(true),
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
/// of [`response`] (`response`'s own doc comment explains why it is a
/// separate schema): `choices[].delta` in place of
/// `choices[].message`, the same `id`/`created`/`model` carried on every
/// frame, and a final `usage`-bearing frame when the client asked for one
/// (`stream_options.include_usage`) — every other frame then has `usage:
/// null`, as OpenAI's do; unasked, no frame has the key. The stream ends with
/// the literal `data: [DONE]`, which is not JSON and so has no schema of its
/// own.
pub(crate) fn stream_chunk(g: &mut SchemaGenerator) -> Schema {
    let mut usage = usage_schema();
    usage["type"] = json!(["object", "null"]);
    usage["description"] = json!(
        "Only with stream_options.include_usage: null on every frame but the last, \
         whose choices is [] and which carries the request's usage."
    );
    schemas::named(
        g,
        "ChatCompletionChunk",
        schemars::json_schema!({
            "type": "object",
            "required": ["id", "object", "created", "model", "choices"],
            "properties": {
                "id": {"type": "string"},
                "object": {"type": "string", "enum": ["chat.completion.chunk"]},
                "created": {"type": "integer"},
                "model": {"type": "string"},
                "choices": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["index", "delta", "finish_reason"],
                        "properties": {
                            "index": {"type": "integer"},
                            "delta": choice_message(false),
                            "finish_reason": {"type": ["string", "null"]}
                        }
                    }
                },
                "usage": usage
            },
            "description": "One frame: `data: <this object>`, repeated, ending with the \
                literal `data: [DONE]`."
        }),
    )
}

/// The error frame an upstream that fails after the stream began is reported
/// with: `data: {"error": {...}}` on the unnamed event, in place of a chunk
/// (the HTTP status is already 200 by then). The stream ends after it.
fn stream_error(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "ChatCompletionStreamError",
        schemars::json_schema!({
            "type": "object",
            "required": ["error"],
            "properties": {
                "error": {
                    "type": "object",
                    "required": ["message", "type", "code"],
                    "properties": {
                        "message": {"type": "string"},
                        "type": {"type": "string", "enum": ["api_error"]},
                        "code": {"type": "string", "enum": ["upstream"]}
                    }
                }
            },
            "description": "The frame a stream carries instead of a chunk when the upstream \
                fails after the first byte was sent."
        }),
    )
}

/// What one `data:` line of a `stream: true` answer holds: a
/// [`stream_chunk`], or the [`stream_error`] frame.
pub(crate) fn stream_frame(g: &mut SchemaGenerator) -> Schema {
    let chunk = stream_chunk(g);
    let error = stream_error(g);
    schemars::json_schema!({"anyOf": [chunk, error]})
}

/// `model` is `"<pick a model>"`: the tester replaces it with
/// whichever the caller picks.
pub(crate) fn example() -> Value {
    json!({
        "model": "<pick a model>",
        "messages": [{"role": "user", "content": "Say hello in one short sentence."}]
    })
}
