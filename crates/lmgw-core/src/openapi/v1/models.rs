//! `GET /v1/models` and `GET /v1/models/{id}`, read
//! from `server::openai_model_object`/`anthropic_model_object`/`lmgw_block`
//! (the model-capabilities wire shape). Served OpenAI-shaped unless the
//! caller sends `anthropic-version` (`wants_anthropic`); both shapes publish the same
//! [`crate::capabilities::ModelCapabilities`] object, the Anthropic one with
//! Anthropic's capability blocks merged in (`capabilities::anthropic_blocks`).

use schemars::generate::SchemaGenerator;
use schemars::Schema;
use serde_json::json;

use super::super::schemas;

fn pricing() -> serde_json::Value {
    json!({
        "type": "object",
        "required": ["prompt", "completion"],
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
        "required": ["version", "endpoints", "headers", "notes"],
        "properties": {
            "version": {"type": "string"},
            "endpoints": {
                "type": "object",
                "required": ["openai", "anthropic", "other"],
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

/// The body of the component `reference` names, taken out of `g` when
/// `take` (a schema only ever inlined into another must not stay behind as a
/// component nothing reaches).
fn definition(
    g: &mut SchemaGenerator,
    reference: &Schema,
    take: bool,
) -> serde_json::Map<String, serde_json::Value> {
    let name = reference
        .get("$ref")
        .and_then(serde_json::Value::as_str)
        .and_then(|r| r.rsplit('/').next())
        .unwrap_or_default()
        .to_string();
    let body = if take {
        g.definitions_mut().remove(&name)
    } else {
        g.definitions().get(&name).cloned()
    };
    match body {
        Some(serde_json::Value::Object(o)) => o,
        _ => panic!("openapi: no component behind {reference:?}"),
    }
}

/// The Anthropic dialect's `capabilities` (`capabilities::anthropic_blocks`):
/// lmgw's own object with Anthropic's blocks merged in, as one object
/// schema — an `allOf` cannot say it, because lmgw's object refuses keys it
/// does not define.
fn anthropic_capabilities(g: &mut SchemaGenerator) -> Schema {
    let lmgw = g.subschema_for::<crate::capabilities::ModelCapabilities>();
    let blocks = g.subschema_for::<crate::capabilities::anthropic_blocks::AnthropicBlocks>();
    let mut merged = definition(g, &lmgw, false);
    let blocks = definition(g, &blocks, true);
    merged.remove("additionalProperties");
    if let (Some(serde_json::Value::Object(ours)), Some(serde_json::Value::Object(theirs))) =
        (merged.get_mut("properties"), blocks.get("properties"))
    {
        for (k, v) in theirs {
            assert!(
                ours.insert(k.clone(), v.clone()).is_none(),
                "openapi: Anthropic block {k} collides with an lmgw capability key"
            );
        }
    }
    if let (Some(serde_json::Value::Array(ours)), Some(serde_json::Value::Array(theirs))) =
        (merged.get_mut("required"), blocks.get("required"))
    {
        ours.extend(theirs.iter().cloned());
    }
    if merged.contains_key("title") {
        merged.insert("title".into(), json!("AnthropicModelCapabilities"));
    }
    merged.insert(
        "description".into(),
        json!(
            "lmgw's own capabilities object (as on the OpenAI shape, where an absent field \
            means unknown) plus Anthropic's ten capability blocks. In the blocks, \
            supported: true means a /v1/messages request using the feature works on this \
            model through lmgw; false means lmgw does not serve it yet or does not know — \
            Anthropic's schema has no unknown."
        ),
    );
    schemas::named(
        g,
        "AnthropicModelCapabilities",
        Schema::try_from(serde_json::Value::Object(merged)).expect("an object is a schema"),
    )
}

pub(crate) fn anthropic_model(g: &mut SchemaGenerator) -> Schema {
    let caps = anthropic_capabilities(g).to_value();
    schemas::named(
        g,
        "AnthropicModel",
        schemars::json_schema!({
            "type": "object",
            "required": ["type", "id", "display_name", "created_at", "lifecycle"],
            "properties": {
                "type": {"type": "string", "enum": ["model"]},
                "id": {"type": "string"},
                "display_name": {"type": "string"},
                "created_at": {"type": "string", "format": "date-time"},
                "lifecycle": {"type": "string", "enum": ["active"], "description": "Always \
                    active: a model lmgw lists is one it serves."},
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
