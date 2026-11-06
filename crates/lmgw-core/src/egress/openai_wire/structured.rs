//! Structured output on the OpenAI-shaped egresses, both directions
//! (structured-output design §6, §9.1): the request's `response_format`, and
//! the refusals of OpenAI, OpenAI-compatible providers and llama.cpp's
//! servers read as the two codes a client branches on.
//!
//! One module for the three OpenAI-shaped egresses because they share the
//! wire, with two classifiers (openai egress design §8): the OpenAI
//! platform's ([`crate::egress::openai`]) reads OpenAI's own shape only
//! ([`openai_refusal`]); the compatible and llama.cpp egresses
//! ([`crate::egress::openai_compatible`], [`crate::egress::llama_cpp`]) read
//! it and every relayed or llama.cpp text ([`refusal`]), because a `generic`
//! row in front of llama-server (llama-swap, a remote box) gets llama.cpp's
//! texts and an aggregator relays OpenAI's in its own shape.

use serde_json::{Map, Value};

use crate::error::GatewayError;
use crate::ir::ChatRequest;

/// Write the IR's structured output into an OpenAI chat body (§6): exactly the
/// fields it has, so an OpenAI client's `response_format` goes out as it came
/// in, and every other dialect's format in OpenAI's spelling. Runs before the
/// passthrough loop, which only fills keys the body does not have yet.
pub(super) fn apply(body: &mut Map<String, Value>, ir: &ChatRequest) {
    if let Some(f) = &ir.response_format {
        body.insert("response_format".into(), f.to_openai());
    }
}

/// llama.cpp's GBNF converter refusing a schema
/// (`common/json-schema-to-grammar.cpp`: `std::invalid_argument("JSON schema
/// conversion failed:\n…")`). Live (image official-master b11714,
/// 2026-10-24) it appears only on legacy `/v1/completions` and `/completion`
/// with a top-level `json_schema`, as a `400 invalid_request_error` whose
/// message is `Field 'json_schema': "json_schema": JSON schema conversion
/// failed:\nJSON schema error at #: …`. Matched anywhere in the message.
const LLAMA_CONVERSION_FAILED: &str = "JSON schema conversion failed";

/// llama.cpp's schema reader on its PEG chat path (`common/json-schema.cpp`,
/// `fail()`: `std::runtime_error("JSON schema error at <path>: <msg>")`),
/// which the server answers as `500 server_error` (design §2.8, §11.11),
/// the path written `#/properties/word` (the root `#`).
const LLAMA_SCHEMA_ERROR: &str = "JSON schema error at ";

/// llama-server refusing a `response_format.type` it does not know
/// (`tools/server/server-common.cpp`).
const LLAMA_UNKNOWN_TYPE: &str = "response_format type must be one of";

/// OpenAI rejecting the schema of a `json_schema` format: `400
/// invalid_request_error`, the message `Invalid schema for response_format
/// '<name>': …` on chat completions (`param: response_format`, `code: null`)
/// and on `/v1/responses` alike (`param: text.format.schema`, `code:
/// invalid_json_schema`); an OpenRouter-shaped aggregator relays the message
/// verbatim under its own `type` and an object `param` (captured 2026-10-22,
/// fixtures `structured_output/openai_*.json`, `aggregator_*.json`).
const OPENAI_INVALID_SCHEMA: &str = "Invalid schema for response_format";

/// The opening of OpenAI's schema refusal whichever field it names: on the
/// Responses wire (`param: text.format` or `text.format.schema`) the
/// message may say `response_format` or `text.format`. Hand-written until
/// the live probe (WP6) captures the platform chat's Responses-wire body.
const OPENAI_INVALID_SCHEMA_ANY: &str = "Invalid schema for ";

/// A model of OpenAI's without that format: `400 invalid_request_error`,
/// `param: null`, the message `Invalid parameter: 'response_format' of type
/// '<type>' is not supported with this model.` (captured 2026-10-22 for
/// `json_schema` on gpt-3.5-turbo and `json_object` on gpt-4). Read only
/// next to the field's name, in `param` or quoted in the message.
const OPENAI_NOT_SUPPORTED: &str = "is not supported with this model";

/// OpenAI refusing a format type it does not define: `400
/// invalid_request_error`, `param: response_format.type`, `code:
/// invalid_value`, the message `Invalid value: '<type>'. Supported values are:
/// …` (captured 2026-10-22 for `{"type": "regex"}` on gpt-4.1-nano, fixture
/// `structured_output/openai_unknown_type.json`). Read by the field it names
/// and the code, never by the sentence; `text.format.type` is the same field
/// on `/v1/responses`.
const OPENAI_TYPE_PARAMS: [&str; 2] = ["response_format.type", "text.format.type"];

/// The upstream's refusal as one of the two structured-output errors, or
/// `None` when it is anything else — the caller then maps it as usual
/// ([`super::map_error`]). The upstream's own message is kept verbatim (§11.10).
///
/// The tolerant classifier of the compatible and llama.cpp egresses (openai
/// egress design §8), exactly as it stood before the platform's egress split
/// off (structured-output design §9.1): OpenAI's texts in any shape —
/// relayed by an aggregator under its own `type` and an object `param` — and
/// llama.cpp's. The OpenAI platform's egress asks [`openai_refusal`] instead.
///
/// [`crate::egress::chat_error`] keeps the answer only where the request's
/// format lets it stand (§11.8): a schema refusal for a JSON type —
/// llama.cpp's converter error also covers tool-parameter schemas, which a
/// request without a JSON format must keep reading as the generic error it
/// is — and a type refusal for any type but `text`.
pub fn refusal(status: u16, body: &[u8]) -> Option<GatewayError> {
    if !(400..600).contains(&status) {
        return None;
    }
    let v: Value = serde_json::from_slice(body).ok()?;
    let error = v.get("error")?;
    let message = error.get("message").and_then(Value::as_str)?;
    let param = error.get("param").and_then(Value::as_str);
    if message.contains(LLAMA_CONVERSION_FAILED)
        || message.starts_with(LLAMA_SCHEMA_ERROR)
        || message.starts_with(OPENAI_INVALID_SCHEMA)
    {
        return Some(GatewayError::InvalidJsonSchema {
            message: message.to_string(),
        });
    }
    let names_the_format = param == Some("response_format")
        || param == Some("text.format")
        || message.contains("'response_format'")
        || message.contains("'text.format'");
    let names_the_type = param.is_some_and(|p| OPENAI_TYPE_PARAMS.contains(&p))
        && error.get("code").and_then(Value::as_str) == Some("invalid_value");
    if message.starts_with(LLAMA_UNKNOWN_TYPE)
        || (names_the_format && message.contains(OPENAI_NOT_SUPPORTED))
        || names_the_type
    {
        return Some(GatewayError::ResponseFormatUnsupported {
            message: message.to_string(),
        });
    }
    None
}

/// The structured-output refusals in OpenAI's own shape (openai egress
/// design §8): a `400 invalid_request_error` whose `param` and `code` name
/// the format exactly, or — for the one refusal OpenAI sends with `param:
/// null` — whose message opens by quoting the field. What the platform's
/// egress reads instead of [`refusal`]; unlike that one it also reads `code:
/// invalid_json_schema` with a format `param` whatever the message.
///
/// - `invalid_json_schema`: `param: response_format` and the message
///   [`OPENAI_INVALID_SCHEMA`] (chat completions, `code: null`), or `code:
///   invalid_json_schema` with `param` the format's schema
///   (`text.format.schema` on `/v1/responses`);
/// - `response_format_unsupported`: `param` [`OPENAI_TYPE_PARAMS`] with
///   `code: invalid_value`, or the message `Invalid parameter:
///   'response_format' of type '<type>' is not supported with this model.`
///   (`param: null`).
///
/// The bodies are a probe's (`fixtures/structured_output/
/// openai_*.json`); WP6's probe re-captures them.
pub fn openai_refusal(status: u16, body: &[u8]) -> Option<GatewayError> {
    if status != 400 {
        return None;
    }
    let v: Value = serde_json::from_slice(body).ok()?;
    let error = v.get("error")?;
    if error.get("type").and_then(Value::as_str) != Some("invalid_request_error") {
        return None;
    }
    let message = error.get("message").and_then(Value::as_str)?;
    let param = error.get("param").and_then(Value::as_str);
    let code = error.get("code").and_then(Value::as_str);
    let schema_param = param.is_some_and(|p| {
        p == "response_format"
            || p.starts_with("response_format.")
            || p == "text.format"
            || p.starts_with("text.format.")
    });
    // The Responses wire's spellings (openai egress design §7.2.3): the
    // schema refusal of a platform chat sent as `text.format`, whose
    // message may name either field.
    let text_format_schema = matches!(param, Some("text.format" | "text.format.schema"))
        && message.starts_with(OPENAI_INVALID_SCHEMA_ANY);
    if (param == Some("response_format") && message.starts_with(OPENAI_INVALID_SCHEMA))
        || (code == Some("invalid_json_schema") && schema_param)
        || text_format_schema
    {
        return Some(GatewayError::InvalidJsonSchema {
            message: message.to_string(),
        });
    }
    let names_the_type =
        param.is_some_and(|p| OPENAI_TYPE_PARAMS.contains(&p)) && code == Some("invalid_value");
    let not_supported = ["'response_format'", "'text.format'"]
        .iter()
        .any(|field| message.starts_with(&format!("Invalid parameter: {field} of type ")))
        && message.contains(OPENAI_NOT_SUPPORTED);
    if names_the_type || not_supported {
        return Some(GatewayError::ResponseFormatUnsupported {
            message: message.to_string(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(status: u16, body: &str) -> Option<&'static str> {
        refusal(status, body.as_bytes()).map(|e| e.kind())
    }

    /// llama.cpp's three texts, as its source writes them.
    #[test]
    fn llama_cpp_refusals_read_by_their_source_texts() {
        assert_eq!(
            code(
                400,
                r#"{"error":{"code":400,"message":"Field 'json_schema': \"json_schema\": JSON schema conversion failed:\nJSON schema error at #: unrecognized type nonsense","type":"invalid_request_error"}}"#
            ),
            Some("invalid_json_schema")
        );
        assert_eq!(
            code(
                500,
                r#"{"error":{"code":500,"message":"JSON schema error at #/properties/a: minLength must be a non-negative integer","type":"server_error"}}"#
            ),
            Some("invalid_json_schema")
        );
        assert_eq!(
            code(
                500,
                r#"{"error":{"code":500,"message":"\"json_schema\": JSON schema conversion failed:\nx","type":"server_error"}}"#
            ),
            Some("invalid_json_schema")
        );
        assert_eq!(
            code(
                400,
                r#"{"error":{"code":400,"message":"response_format type must be one of \"text\" or \"json_object\", but got: regex","type":"invalid_request_error"}}"#
            ),
            Some("response_format_unsupported")
        );
    }

    /// Anything else is not guessed at: a refusal that does not name the
    /// format, a `2xx`, a body that is not OpenAI-shaped.
    #[test]
    fn anything_else_is_left_to_the_generic_mapping() {
        for (status, body) in [
            (
                400,
                r#"{"error":{"message":"Invalid value for 'temperature'.","type":"invalid_request_error","param":"temperature","code":null}}"#,
            ),
            (
                400,
                r#"{"error":{"message":"'tool_choice' is not supported with this model.","type":"invalid_request_error","param":"tool_choice"}}"#,
            ),
            (
                200,
                r#"{"error":{"message":"JSON schema conversion failed"}}"#,
            ),
            (400, "JSON schema conversion failed"),
            (400, ""),
        ] {
            assert_eq!(code(status, body), None, "{body}");
        }
    }

    /// The platform reads OpenAI's `code: invalid_json_schema` with a format
    /// `param` whatever its message; the compatible and llama.cpp egresses
    /// read only the texts, as before the split (openai egress design §8).
    #[test]
    fn only_the_platform_reads_the_schema_code_alone() {
        let body = br#"{"error":{"message":"Some new wording.","type":"invalid_request_error","param":"text.format.schema","code":"invalid_json_schema"}}"#;
        assert_eq!(refusal(400, body).map(|e| e.kind()), None);
        assert_eq!(
            openai_refusal(400, body).map(|e| e.kind()),
            Some("invalid_json_schema")
        );
    }

    /// The Responses wire's spellings of the schema refusal (openai egress
    /// design §7.2.3): `param` `text.format` or `text.format.schema`, with or
    /// without a code, the message naming either field.
    #[test]
    fn the_platform_reads_the_text_format_schema_refusal() {
        for (param, code, message) in [
            (
                "text.format",
                "null",
                "Invalid schema for response_format 'w': x",
            ),
            (
                "text.format.schema",
                "null",
                "Invalid schema for text.format 'w': x",
            ),
            (
                "text.format",
                "\"invalid_json_schema\"",
                "Invalid schema for text.format: x",
            ),
        ] {
            let body = format!(
                r#"{{"error":{{"message":"{message}","type":"invalid_request_error","param":"{param}","code":{code}}}}}"#
            );
            assert_eq!(
                openai_refusal(400, body.as_bytes()).map(|e| e.kind()),
                Some("invalid_json_schema"),
                "{body}"
            );
        }
        // Another refusal under the format's name is not one.
        let other = br#"{"error":{"message":"Unknown parameter: 'text.format.foo'.","type":"invalid_request_error","param":"text.format","code":"unknown_parameter"}}"#;
        assert_eq!(openai_refusal(400, other).map(|e| e.kind()), None);
    }

    #[test]
    fn the_upstream_message_is_kept_verbatim() {
        let e = refusal(
            400,
            br#"{"error":{"code":400,"message":"JSON schema conversion failed:\nbad","type":"invalid_request_error"}}"#,
        )
        .unwrap();
        assert_eq!(e.to_string(), "JSON schema conversion failed:\nbad");
    }
}
