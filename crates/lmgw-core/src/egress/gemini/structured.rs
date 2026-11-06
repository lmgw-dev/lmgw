//! Structured output on the Gemini egress, both directions (structured-output
//! design §6, §9.3): the IR's format as `generationConfig.responseMimeType`
//! and `responseJsonSchema`, and Gemini's refusals of it read as the two codes
//! a client branches on.

use serde_json::{json, Map, Value};

use crate::error::GatewayError;
use crate::ir::{ChatRequest, ResponseFormat};

/// Write the IR's format into a `generationConfig` (§6). JSON Schema goes as
/// `responseJsonSchema`, as the client wrote it — not the OpenAPI-subset
/// `responseSchema`, which would need the schema rewritten (§11.5).
/// `name`, `description` and `strict` have no Gemini equivalent; the output
/// is constrained either way (§11.4). A `json_schema` without a schema, or a
/// `json_object` (with llama.cpp's optional schema, §11.2), is JSON mode with
/// whatever schema there is. A type with no Gemini equivalent is refused.
pub(super) fn apply(
    gen_cfg: &mut Map<String, Value>,
    ir: &ChatRequest,
) -> Result<(), GatewayError> {
    let schema = match &ir.response_format {
        None | Some(ResponseFormat::Text { .. }) => return Ok(()),
        Some(ResponseFormat::JsonSchema(f)) => f.schema.clone(),
        Some(ResponseFormat::JsonObject { schema, .. }) => schema.clone(),
        Some(f @ ResponseFormat::Other { .. }) => {
            return Err(GatewayError::ResponseFormatUnsupported {
                message: format!(
                    "response format type '{}' has no Gemini equivalent — Gemini constrains \
                     output to json_schema or json_object",
                    f.type_name()
                ),
            })
        }
    };
    gen_cfg.insert("responseMimeType".into(), json!("application/json"));
    if let Some(s) = schema {
        gen_cfg.insert("responseJsonSchema".into(), s);
    }
    Ok(())
}

/// Gemini's refusal of a request's structured output as one of the two
/// errors, or `None` for anything else (design §9.3). Only asked for a request
/// that carried a format (§11.8). The provider's message is kept verbatim.
///
/// Gemini's `responseJsonSchema` reader answers `400 INVALID_ARGUMENT` with
/// the place in the schema it failed at: `unrecognized type 'nonsense' at
/// top-level`, `value at top-level must be a list`, `unrecognized type
/// 'nonsense' at properties.a` (captured 2026-10-22 — fixtures
/// `structured_output/gemini_*.json`). Those two places are what is read; a
/// failure elsewhere in a schema (under `items`, `anyOf`, …) was not
/// captured and stays generic. A model without JSON mode could not be
/// provoked (Gemma on AI Studio took `json_object`), so no text is read as
/// `response_format_unsupported` yet.
pub fn refusal(status: u16, body: &[u8]) -> Option<GatewayError> {
    if status != 400 {
        return None;
    }
    let v: Value = serde_json::from_slice(body).ok()?;
    let error = v.get("error")?;
    if error.get("status").and_then(Value::as_str) != Some("INVALID_ARGUMENT") {
        return None;
    }
    let message = error.get("message").and_then(Value::as_str)?;
    SCHEMA_PLACES
        .iter()
        .any(|p| message.contains(p))
        .then(|| GatewayError::InvalidJsonSchema {
            message: message.to_string(),
        })
}

/// Where in a schema Gemini's reader says it failed (§9.3).
const SCHEMA_PLACES: [&str; 2] = [" at top-level", " at properties."];

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

    fn cfg(f: Option<ResponseFormat>) -> Result<Value, GatewayError> {
        let mut m = Map::new();
        apply(&mut m, &ir(f))?;
        Ok(Value::Object(m))
    }

    #[test]
    fn each_type_becomes_gemini_json_mode() {
        let schema = json!({"type": "object", "additionalProperties": false});
        assert_eq!(
            cfg(Some(ResponseFormat::JsonSchema(JsonSchemaFormat {
                name: Some("answer".into()),
                schema: Some(schema.clone()),
                strict: Some(true),
                ..Default::default()
            })))
            .unwrap(),
            json!({"responseMimeType": "application/json", "responseJsonSchema": schema})
        );
        assert_eq!(
            cfg(Some(ResponseFormat::JsonObject {
                schema: None,
                extra: Default::default()
            }))
            .unwrap(),
            json!({"responseMimeType": "application/json"})
        );
        assert_eq!(cfg(Some(ResponseFormat::text())).unwrap(), json!({}));
        assert_eq!(cfg(None).unwrap(), json!({}));
        let e = cfg(Some(ResponseFormat::Other {
            raw: json!({"type": "regex"}),
        }))
        .unwrap_err();
        assert_eq!(e.kind(), "response_format_unsupported");
    }
}
