//! `GET /v1/models` and `GET /v1/models/{id}`, read
//! from `server::openai_model_object`/`anthropic_model_object`/`lmgw_block`
//! (the model-capabilities wire shape). Served OpenAI-shaped unless the
//! caller sends `anthropic-version` (`wants_anthropic`); both shapes publish the same
//! [`crate::capabilities::ModelCapabilities`] object.

use schemars::generate::SchemaGenerator;
use schemars::Schema;
use serde_json::json;

use super::super::schemas;

fn pricing() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "prompt": {"type": "string", "description": "USD per prompt token, as a decimal \
                string; \"0\" for a local model."},
            "completion": {"type": "string"}
        }
    })
}

fn lmgw_block(g: &mut SchemaGenerator) -> serde_json::Value {
    let headers = g.subschema_for::<std::collections::BTreeMap<String, String>>();
    json!({
        "type": "object",
        "properties": {
            "version": {"type": "string"},
            "endpoints": {
                "type": "object",
                "properties": {
                    "openai": {"type": "array", "items": {"type": "string"}},
                    "anthropic": {"type": "array", "items": {"type": "string"}},
                    "other": {"type": "array", "items": {"type": "string"}}
                },
                "description": "Every path listed here exists in this API and carries the capability \
                    its own operation documents."
            },
            "headers": headers.to_value(),
            "notes": {"type": "array", "items": {"type": "string"}}
        }
    })
}

pub(crate) fn openai_model(g: &mut SchemaGenerator) -> Schema {
    let caps = g
        .subschema_for::<crate::capabilities::ModelCapabilities>()
        .to_value();
    schemas::named(
        g,
        "OpenAiModel",
        schemars::json_schema!({
            "type": "object",
            "required": ["id", "object", "created", "owned_by"],
            "properties": {
                "id": {"type": "string"},
                "object": {"type": "string", "enum": ["model"]},
                "created": {"type": "integer", "description": "Stable for the life of the gateway process \
                    rather than the time of each request; the catalog's own timestamp when \
                    it has one."},
                "owned_by": {"type": "string"},
                "context_length": {"type": "integer"},
                "max_output_tokens": {"type": "integer"},
                "pricing": pricing(),
                "capabilities": caps,
                "notes": {"type": "array", "items": {"type": "string"}}
            }
        }),
    )
}

pub(crate) fn anthropic_model(g: &mut SchemaGenerator) -> Schema {
    let caps = g
        .subschema_for::<crate::capabilities::ModelCapabilities>()
        .to_value();
    schemas::named(
        g,
        "AnthropicModel",
        schemars::json_schema!({
            "type": "object",
            "required": ["type", "id", "display_name", "created_at"],
            "properties": {
                "type": {"type": "string", "enum": ["model"]},
                "id": {"type": "string"},
                "display_name": {"type": "string"},
                "created_at": {"type": "string", "format": "date-time"},
                "context_window": {"type": "integer"},
                "max_input_tokens": {"type": "integer", "description": "Alias of \
                    context_window, under the name the Anthropic SDK types it by."},
                "max_output_tokens": {"type": "integer"},
                "max_tokens": {"type": "integer", "description": "Alias of \
                    max_output_tokens."},
                "pricing": pricing(),
                "capabilities": caps,
                "notes": {"type": "array", "items": {"type": "string"}}
            }
        }),
    )
}

pub(crate) fn model_list(g: &mut SchemaGenerator) -> Schema {
    let openai_item = openai_model(g).to_value();
    let anthropic_item = anthropic_model(g).to_value();
    let block = lmgw_block(g);
    schemas::named(
        g,
        "ModelList",
        schemars::json_schema!({
            "description": "OpenAiModelList unless the caller sent anthropic-version, in \
                which case AnthropicModelList.",
            "oneOf": [
                {
                    "type": "object",
                    "required": ["object", "data", "lmgw"],
                    "properties": {
                        "object": {"type": "string", "enum": ["list"]},
                        "data": {"type": "array", "items": openai_item},
                        "lmgw": block
                    }
                },
                {
                    "type": "object",
                    "required": ["data", "has_more", "lmgw"],
                    "properties": {
                        "data": {"type": "array", "items": anthropic_item},
                        "has_more": {"type": "boolean", "enum": [false]},
                        "first_id": {"type": "null"},
                        "last_id": {"type": "null"},
                        "lmgw": block
                    }
                }
            ]
        }),
    )
}

pub(crate) fn model_by_id(g: &mut SchemaGenerator) -> Schema {
    let openai_item = openai_model(g).to_value();
    let anthropic_item = anthropic_model(g).to_value();
    schemas::named(
        g,
        "ModelById",
        schemars::json_schema!({
            "description": "The OpenAiModel shape unless the caller sent anthropic-version, \
                in which case AnthropicModel. No lmgw block on the single-model route.",
            "oneOf": [openai_item, anthropic_item]
        }),
    )
}
