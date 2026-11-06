//! Every ingress's format into the IR's field (design §5).

use lmgw_core::ingress::{anthropic, openai, responses};
use lmgw_core::ir::{JsonSchemaFormat, ResponseFormat};
use serde_json::{json, Value};

fn chat(extra: Value) -> lmgw_core::ir::ChatRequest {
    let mut body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    openai::parse_chat_request(&body).unwrap()
}

fn answer_schema() -> Value {
    json!({"type": "object", "properties": {"ok": {"type": "boolean"}}})
}

#[test]
fn chat_completions_reads_every_type_and_keeps_it_out_of_passthrough() {
    let ir = chat(
        json!({"response_format": {"type": "json_schema", "json_schema": {
            "name": "answer", "description": "d", "strict": true, "schema": answer_schema(),
        }}}),
    );
    assert_eq!(
        ir.response_format,
        Some(ResponseFormat::JsonSchema(JsonSchemaFormat {
            name: Some("answer".into()),
            description: Some("d".into()),
            schema: Some(answer_schema()),
            strict: Some(true),
            ..Default::default()
        }))
    );
    assert!(!ir.passthrough.contains_key("response_format"));
    assert!(ir.asks_json());

    let ir = chat(json!({"response_format": {"type": "json_object"}}));
    assert_eq!(
        ir.response_format,
        Some(ResponseFormat::JsonObject {
            schema: None,
            extra: Default::default()
        })
    );
    // llama.cpp's schema on json_object (§11.2).
    let ir = chat(json!({"response_format": {"type": "json_object", "schema": answer_schema()}}));
    assert_eq!(
        ir.response_format,
        Some(ResponseFormat::JsonObject {
            schema: Some(answer_schema()),
            extra: Default::default()
        })
    );
    let ir = chat(json!({"response_format": {"type": "text"}}));
    assert_eq!(ir.response_format, Some(ResponseFormat::text()));
    assert!(!ir.asks_json());
    // A type lmgw does not model is kept verbatim (§11.3).
    let tag = json!({"type": "structural_tag", "structures": [], "triggers": ["<a>"]});
    let ir = chat(json!({ "response_format": tag.clone() }));
    assert_eq!(ir.response_format, Some(ResponseFormat::Other { raw: tag }));
    assert!(!ir.asks_json());
    assert_eq!(chat(json!({})).response_format, None);
}

/// llama.cpp's own top-level `json_schema` becomes the field only when
/// `response_format` is absent (§11.1); with both, `response_format` is the
/// field and the top-level key stays for llama-server's own precedence.
#[test]
fn llama_cpps_top_level_schema_is_the_field_when_alone() {
    let ir = chat(json!({"json_schema": answer_schema()}));
    assert_eq!(
        ir.response_format,
        Some(ResponseFormat::JsonSchema(JsonSchemaFormat {
            name: Some("response".into()),
            schema: Some(answer_schema()),
            ..Default::default()
        }))
    );
    assert!(!ir.passthrough.contains_key("json_schema"));

    let ir =
        chat(json!({"json_schema": answer_schema(), "response_format": {"type": "json_object"}}));
    assert_eq!(
        ir.response_format,
        Some(ResponseFormat::JsonObject {
            schema: None,
            extra: Default::default()
        })
    );
    assert_eq!(ir.passthrough["json_schema"], answer_schema());
}

#[test]
fn a_response_format_with_nothing_to_translate_is_a_bad_request() {
    for rf in [
        json!("json_object"),
        json!({"json_schema": {}}),
        json!({"type": "json_schema", "json_schema": [1]}),
        json!({"type": "json_schema", "json_schema": {"name": 7}}),
    ] {
        let body = json!({"model": "m", "messages": [], "response_format": rf});
        let e = openai::parse_chat_request(&body).unwrap_err();
        assert_eq!(e.kind(), "bad_request", "{rf}");
    }
}

/// `text.format`, flat in this dialect, with the defaults the route always
/// used — `description` now kept (§5).
#[test]
fn responses_reads_text_format() {
    let parse = |format: Value| {
        responses::parse_request(&json!({"model": "m", "input": "hi", "text": {"format": format}}))
            .unwrap()
            .ir
    };
    let ir = parse(
        json!({"type": "json_schema", "name": "answer", "description": "d",
                          "schema": answer_schema(), "strict": true}),
    );
    assert_eq!(
        ir.response_format,
        Some(ResponseFormat::JsonSchema(JsonSchemaFormat {
            name: Some("answer".into()),
            description: Some("d".into()),
            schema: Some(answer_schema()),
            strict: Some(true),
            ..Default::default()
        }))
    );
    assert!(!ir.passthrough.contains_key("response_format"));
    let ir = parse(json!({"type": "json_schema"}));
    assert_eq!(
        ir.response_format,
        Some(ResponseFormat::JsonSchema(JsonSchemaFormat {
            name: Some("response".into()),
            description: None,
            schema: Some(json!({})),
            strict: Some(false),
            ..Default::default()
        }))
    );
    assert_eq!(
        parse(json!({"type": "json_object"})).response_format,
        Some(ResponseFormat::JsonObject {
            schema: None,
            extra: Default::default()
        })
    );
    assert_eq!(
        parse(json!({"type": "text"})).response_format,
        Some(ResponseFormat::text())
    );
    let odd = json!({"type": "grammar", "syntax": "lark"});
    assert_eq!(
        parse(odd.clone()).response_format,
        Some(ResponseFormat::Other { raw: odd })
    );
}

/// Anthropic's `output_config.format` (and the deprecated `output_format`):
/// a bare schema, named `response`, strict as Anthropic always is (§11.4,
/// §11.12). The effort beside it is still the reasoning tier.
#[test]
fn messages_read_output_config_format() {
    let parse = |extra: Value| {
        let mut body = json!({"model": "m", "max_tokens": 16,
                              "messages": [{"role": "user", "content": "hi"}]});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        anthropic::parse_messages_request(&body)
    };
    let want = Some(ResponseFormat::JsonSchema(JsonSchemaFormat {
        name: Some("response".into()),
        description: None,
        schema: Some(answer_schema()),
        strict: Some(true),
        ..Default::default()
    }));
    let ir = parse(json!({"output_config": {"effort": "low",
                          "format": {"type": "json_schema", "schema": answer_schema()}}}))
    .unwrap();
    assert_eq!(ir.response_format, want);
    assert_eq!(ir.params.reasoning.unwrap().effort.as_deref(), Some("low"));
    let ir = parse(json!({"output_format": {"type": "json_schema", "schema": answer_schema()}}))
        .unwrap();
    assert_eq!(ir.response_format, want);
    // The current spelling wins over the deprecated one.
    let ir = parse(json!({
        "output_config": {"format": {"type": "json_schema", "schema": answer_schema()}},
        "output_format": {"type": "json_schema", "schema": {"type": "string"}},
    }))
    .unwrap();
    assert_eq!(ir.response_format, want);
    assert_eq!(parse(json!({})).unwrap().response_format, None);

    let e = parse(json!({"output_config": {"format": {"type": "regex"}}})).unwrap_err();
    assert_eq!(e.kind(), "response_format_unsupported");
    assert_eq!(
        e.to_string(),
        "'output_config.format' asks for type 'regex', which lmgw does not translate — \
         json_schema is the one type Anthropic defines"
    );
    // Malformed is not unsupported (§11.19).
    for format in [
        json!("json_schema"),
        json!(["json_schema"]),
        json!({"schema": {}}),
    ] {
        let e = parse(json!({"output_config": {"format": format}})).unwrap_err();
        assert_eq!(e.kind(), "bad_request", "{format}");
        assert_eq!(
            e.to_anthropic_json()["error"]["type"],
            "invalid_request_error"
        );
    }
}

/// A `text` (or `null`) `response_format` asks for nothing more than text,
/// so llama.cpp's top-level `json_schema` beside it is the field — what
/// llama-server applies (§11.18); next to a `grammar` it is refused, as
/// llama-server refuses the pair (§11.1).
#[test]
fn a_text_format_beside_a_top_level_schema_is_the_schema() {
    let want = Some(ResponseFormat::JsonSchema(JsonSchemaFormat {
        name: Some("response".into()),
        schema: Some(answer_schema()),
        ..Default::default()
    }));
    for rf in [json!({"type": "text"}), Value::Null] {
        let ir = chat(json!({"response_format": rf, "json_schema": answer_schema()}));
        assert_eq!(ir.response_format, want, "{rf}");
        assert!(ir.asks_json());
        assert!(!ir.passthrough.contains_key("json_schema"));
        assert!(!ir.passthrough.contains_key("response_format"));
    }

    for extra in [
        json!({"json_schema": answer_schema(), "grammar": "root ::= \"a\""}),
        json!({"json_schema": answer_schema(), "grammar": "root ::= \"a\"",
               "response_format": {"type": "text"}}),
        // llama-server throws on the pair whatever the format says (§11.23).
        json!({"json_schema": answer_schema(), "grammar": "root ::= \"a\"",
               "response_format": {"type": "json_object"}}),
    ] {
        let mut body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let e = openai::parse_chat_request(&body).unwrap_err();
        assert_eq!(e.kind(), "bad_request", "{extra}");
        assert!(
            e.to_string()
                .contains("Cannot use both json_schema and grammar"),
            "{e}"
        );
    }
    // An empty grammar is no grammar: the schema is applied.
    for g in [json!(""), Value::Null] {
        let ir = chat(json!({"json_schema": answer_schema(), "grammar": g}));
        assert!(ir.asks_json(), "{g}");
    }
    // A grammar beside a JSON `response_format` alone is llama-server's own
    // business, as before: both go as sent.
    let ir = chat(json!({"grammar": "root ::= \"a\"", "response_format": {"type": "json_object"}}));
    assert_eq!(ir.passthrough["grammar"], "root ::= \"a\"");
}
