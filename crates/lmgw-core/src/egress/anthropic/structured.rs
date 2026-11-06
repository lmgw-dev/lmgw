//! Structured output on the Anthropic egress, both directions
//! (structured-output design §6, §9.2): the IR's format as
//! `output_config.format`, and Anthropic's refusals of it read as the two
//! codes a client branches on.

use serde_json::{json, Map, Value};

use crate::error::GatewayError;
use crate::ir::{ChatRequest, ResponseFormat};

/// Write the IR's format into a Messages body (§6), merged into the
/// `output_config` the effort may already have written. Anthropic's one type
/// is `json_schema` with a bare schema; `name`, `description` and `strict`
/// have no equivalent there and the output is constrained in any case
/// (§11.4). A type with no Anthropic equivalent is refused before anything
/// is sent — never dropped.
pub(super) fn apply(body: &mut Map<String, Value>, ir: &ChatRequest) -> Result<(), GatewayError> {
    let format = match &ir.response_format {
        None | Some(ResponseFormat::Text { .. }) => return Ok(()),
        Some(ResponseFormat::JsonSchema(f)) => {
            let schema = f
                .schema
                .clone()
                .ok_or_else(|| GatewayError::InvalidJsonSchema {
                    message: "a json_schema format without a schema has nothing for Anthropic's \
                          output_config.format to constrain the answer to — send the schema"
                        .into(),
                })?;
            json!({"type": "json_schema", "schema": schema})
        }
        Some(ResponseFormat::JsonObject { .. }) => {
            return Err(GatewayError::ResponseFormatUnsupported {
                message: "Anthropic has no JSON-object mode (its structured output is \
                          json_schema only) — ask for json_schema with a schema"
                    .into(),
            })
        }
        Some(f @ ResponseFormat::Other { .. }) => {
            return Err(GatewayError::ResponseFormatUnsupported {
                message: format!(
                    "response format type '{}' has no Anthropic equivalent — json_schema is the \
                     one type Anthropic's output_config.format takes",
                    f.type_name()
                ),
            })
        }
    };
    let config = body
        .entry("output_config")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(o) = config.as_object_mut() {
        o.insert("format".into(), format);
    }
    Ok(())
}

/// Anthropic's refusal of a request's structured output as one of the two
/// errors, or `None` for anything else (design §9.2). Only asked for a request
/// that carried a format (§11.8). The provider's message is kept verbatim.
///
/// Anthropic names the field it rejects at the start of the message: a schema
/// it refuses reads `output_config.format.schema: …` (captured 2026-10-22:
/// an unknown type, `additionalProperties: true` — fixtures
/// `structured_output/anthropic_*.json`); the deprecated top-level spelling
/// would read `output_format.schema: …`. A model without structured output
/// could not be provoked (every model the catalog lists publishes
/// `structured_outputs.supported: true`), so no text is read as
/// `response_format_unsupported` yet; the early refusal answers that from the
/// catalog's own flag (§7).
pub fn refusal(status: u16, body: &[u8]) -> Option<GatewayError> {
    if status != 400 {
        return None;
    }
    let v: Value = serde_json::from_slice(body).ok()?;
    let error = v.get("error")?;
    if error.get("type").and_then(Value::as_str) != Some("invalid_request_error") {
        return None;
    }
    let message = error.get("message").and_then(Value::as_str)?;
    SCHEMA_FIELDS
        .iter()
        .any(|f| message.starts_with(f))
        .then(|| GatewayError::InvalidJsonSchema {
            message: message.to_string(),
        })
}

/// The fields a schema refusal names first (§9.2).
const SCHEMA_FIELDS: [&str; 2] = ["output_config.format.schema", "output_format.schema"];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::JsonSchemaFormat;

    fn ir(f: Option<ResponseFormat>) -> ChatRequest {
        let mut ir: ChatRequest = serde_json::from_value(json!({
            "model_alias": "a", "messages": [],
        }))
        .unwrap();
        ir.response_format = f;
        ir
    }

    #[test]
    fn a_schema_merges_into_the_effort_config() {
        let mut body = Map::new();
        body.insert("output_config".into(), json!({"effort": "low"}));
        let f = ResponseFormat::JsonSchema(JsonSchemaFormat {
            name: Some("answer".into()),
            description: Some("d".into()),
            schema: Some(json!({"type": "object"})),
            strict: Some(false),
            ..Default::default()
        });
        apply(&mut body, &ir(Some(f))).unwrap();
        assert_eq!(
            Value::Object(body),
            json!({"output_config": {
                "effort": "low",
                "format": {"type": "json_schema", "schema": {"type": "object"}},
            }})
        );
    }

    #[test]
    fn json_object_and_unknown_types_are_refused_text_is_nothing() {
        let mut body = Map::new();
        let e = apply(
            &mut body,
            &ir(Some(ResponseFormat::JsonObject {
                schema: None,
                extra: Default::default(),
            })),
        )
        .unwrap_err();
        assert_eq!(e.kind(), "response_format_unsupported");
        let e = apply(
            &mut body,
            &ir(Some(ResponseFormat::Other {
                raw: json!({"type": "regex"}),
            })),
        )
        .unwrap_err();
        assert_eq!(e.kind(), "response_format_unsupported");
        assert!(e.to_string().contains("'regex'"), "{e}");
        apply(&mut body, &ir(Some(ResponseFormat::text()))).unwrap();
        apply(&mut body, &ir(None)).unwrap();
        assert!(body.is_empty());
    }
}
