//! The small OpenAI-family routes: `POST /v1/completions` (legacy),
//! `POST /v1/embeddings`, `POST /v1/rerank` and lmgw's own
//! `POST /v1/count_tokens`. Read from
//! `proxy::legacy::handle_legacy_completions`, `proxy::embeddings` and
//! `proxy::count::handle_count_tokens`.

use schemars::generate::SchemaGenerator;
use schemars::Schema;
use serde_json::{json, Value};

use super::super::schemas;

// ---------------------------------------------------------------------------
// POST /v1/completions (legacy)
// ---------------------------------------------------------------------------

pub(crate) fn completions_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "CompletionRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["model", "prompt"],
            "properties": {
                "model": {"type": "string"},
                "prompt": {"description": "A string, an array of strings, an array of token \
                    ids, or a mix of both."},
                "stream": {"type": "boolean", "default": false},
                "max_tokens": {"type": "integer"},
                "n_predict": {"type": "integer", "description": "llama.cpp spelling of \
                    max_tokens; folded into it for local models with a guard or a ladder."},
                "max_completion_tokens": {"type": "integer"}
            },
            "additionalProperties": true,
            "description": "Forwarded to an OpenAI-protocol upstream verbatim: this route \
                refuses anything else (an Anthropic or Gemini alias gets 400 rather than a \
                request its upstream cannot answer)."
        }),
    )
}

pub(crate) fn completions_response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "TextCompletion",
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "object": {"type": "string", "enum": ["text_completion"]},
                "created": {"type": "integer"},
                "model": {"type": "string"},
                "choices": {"type": "array", "items": {"type": "object", "additionalProperties": true}},
                "usage": {"type": "object", "additionalProperties": true}
            }
        }),
    )
}

pub(crate) fn completions_example() -> Value {
    json!({"model": "<pick a model>", "prompt": "Once upon a time"})
}

/// The SSE frame `stream: true` answers with (see `completions_response`): loosely typed
/// like [`completions_response`] itself — the legacy route's own upstreams
/// vary more than the chat completions shape does, and this is `additional
/// Properties: true` for the same reason. Ends with the literal `data:
/// [DONE]`.
pub(crate) fn completions_stream_chunk(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "TextCompletionChunk",
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "object": {"type": "string", "enum": ["text_completion"]},
                "created": {"type": "integer"},
                "model": {"type": "string"},
                "choices": {"type": "array", "items": {"type": "object", "additionalProperties": true}}
            },
            "additionalProperties": true,
            "description": "One frame: `data: <this object>`, repeated, ending with the \
                literal `data: [DONE]`."
        }),
    )
}

// ---------------------------------------------------------------------------
// POST /v1/embeddings
// ---------------------------------------------------------------------------

pub(crate) fn embeddings_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "EmbeddingsRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["model", "input"],
            "properties": {
                "model": {"type": "string"},
                "input": {"description": "A string, or an array of strings. Token-id arrays \
                    are refused with 400: an alias can route to a model with another \
                    tokenizer."},
                "dimensions": {"type": "integer", "minimum": 1,
                    "description": "Forwarded (Gemini: outputDimensionality). A route that \
                        answers with another length — llama-server has no such parameter — \
                        is refused with 400 rather than served full-length vectors."},
                "encoding_format": {"type": "string", "enum": ["float", "base64"],
                    "description": "Encoded by the gateway, whatever the upstream: base64 is \
                        the little-endian float32 bytes, as OpenAI serves it."}
            }
        }),
    )
}

pub(crate) fn embeddings_response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "EmbeddingsResponse",
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "object": {"type": "string", "enum": ["list"]},
                "model": {"type": "string"},
                "data": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "object": {"type": "string", "enum": ["embedding"]},
                            "index": {"type": "integer"},
                            "embedding": {
                                "description": "Floats, or a base64 string when \
                                    encoding_format was base64.",
                                "oneOf": [
                                    {"type": "array", "items": {"type": "number"}},
                                    {"type": "string"}
                                ]
                            }
                        }
                    }
                },
                "usage": {"type": "object", "additionalProperties": true}
            }
        }),
    )
}

pub(crate) fn embeddings_example() -> Value {
    json!({"model": "<pick a model>", "input": "lmgw is a gateway."})
}

// ---------------------------------------------------------------------------
// POST /v1/rerank
// ---------------------------------------------------------------------------

pub(crate) fn rerank_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "RerankRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["model", "query"],
            "properties": {
                "model": {"type": "string"},
                "query": {"type": "string"},
                "documents": {"type": "array", "items": {"type": "string"},
                    "description": "Jina's spelling."},
                "texts": {"type": "array", "items": {"type": "string"},
                    "description": "TEI's spelling — either works; an embedding-only row is \
                        refused."},
                "top_n": {"type": "integer"}
            }
        }),
    )
}

pub(crate) fn rerank_response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "RerankResponse",
        schemars::json_schema!({
            "type": "object",
            "description": "Jina's results shape, regardless of which request spelling was \
                sent.",
            "properties": {
                "results": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "index": {"type": "integer"},
                            "relevance_score": {"type": "number"},
                            "document": {"type": ["object", "null"], "additionalProperties": true}
                        }
                    }
                }
            }
        }),
    )
}

pub(crate) fn rerank_example() -> Value {
    json!({
        "model": "<pick a model>",
        "query": "What is lmgw?",
        "documents": ["An LLM gateway.", "A city in France."]
    })
}

// ---------------------------------------------------------------------------
// POST /v1/count_tokens (lmgw's own, not an upstream protocol route)
// ---------------------------------------------------------------------------

pub(crate) fn count_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "CountTokensRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["model", "input"],
            "properties": {
                "model": {"type": "string"},
                "input": {"type": "string"}
            },
            "description": "Routed by model exactly like the chat endpoints, so counting a \
                cold local model's tokens starts its container. The key's alias scope \
                applies: a refused count answers 403 key_scope and is logged; a successful \
                count is not logged. No budget applies, neither the key's nor the \
                gateway's: a count costs nothing."
        }),
    )
}

pub(crate) fn count_response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "CountTokensResponse",
        schemars::json_schema!({
            "type": "object",
            "required": ["model", "tokens"],
            "properties": {
                "model": {"type": "string"},
                "tokens": {"type": "integer"}
            }
        }),
    )
}

pub(crate) fn count_example() -> Value {
    json!({"model": "<pick a model>", "input": "How many tokens is this?"})
}
