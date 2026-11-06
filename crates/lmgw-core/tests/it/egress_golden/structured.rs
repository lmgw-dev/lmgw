//! Structured output (structured-output design §6, §11.17, §11.18,
//! §11.22): each dialect's format as every upstream gets it — an OpenAI or
//! llama.cpp client's object byte for byte, the keys lmgw does not model
//! included — and llama.cpp's legacy `/v1/completions`, which reads only the
//! top-level `json_schema`.

use lmgw_core::config::Upstream;
use lmgw_core::ir::ChatRequest;
use serde_json::{json, Value};

use super::chat::whole;
use super::{chat_request, params, Case};

/// `hello` with the structured output a client dialect's body asks for, read
/// by that dialect's ingress (structured-output design §5).
fn with_format(chat_body_extra: Value) -> ChatRequest {
    let mut body = json!({"model": "alias", "messages": [{"role": "user", "content": "hello"}]});
    for (k, v) in chat_body_extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    lmgw_core::ingress::openai::parse_chat_request(&body).unwrap()
}

/// Legacy completions bodies as `up`'s server is sent them (§11.22): each
/// body, and what goes or the code that refuses it before anything is sent.
fn legacy(up: &Upstream) -> Value {
    let schema = json!({"type": "object", "properties": {"ok": {"type": "boolean"}}});
    let bodies = [
        json!({"prompt": "Reply with ok.", "response_format": {"type": "json_schema",
               "json_schema": {"name": "answer", "strict": true, "schema": schema}}}),
        json!({"prompt": "Reply with ok.", "response_format": {"type": "json_object"}}),
        json!({"prompt": "Reply with ok.", "response_format": {"type": "json_object",
               "schema": schema}}),
        json!({"prompt": "Reply with ok.", "response_format": {"type": "text"},
               "json_schema": schema}),
        json!({"prompt": "Reply with ok.", "json_schema": schema}),
        json!({"prompt": "Reply with ok.", "response_format": {"type": "regex"}}),
        json!({"prompt": "Reply with ok.", "grammar": "root ::= \"ok\"",
               "response_format": {"type": "json_object"}}),
        // §11.23: an empty grammar goes, a set one beside any top-level schema
        // is refused.
        json!({"prompt": "Reply with ok.", "grammar": "",
               "response_format": {"type": "json_object"}}),
        json!({"prompt": "Reply with ok.", "grammar": null,
               "response_format": {"type": "json_object"}}),
        json!({"prompt": "Reply with ok.", "grammar": "root ::= \"ok\"",
               "json_schema": schema}),
    ];
    let cases: Vec<Value> = bodies
        .into_iter()
        .map(|body| {
            let mut sent = body.clone();
            let out = match lmgw_core::egress::legacy_completions_body(up.protocol, &mut sent) {
                Ok(()) => json!({"sent": sent}),
                Err(e) => json!({"refused": e.kind()}),
            };
            json!({"body": body, "out": out})
        })
        .collect();
    json!({"legacy_completions": cases})
}

pub(super) fn cases() -> Vec<Case> {
    vec![
        // An OpenAI client's object goes out as it came in: the bytes the
        // passthrough sent before the field existed.
        Case {
            name: "structured_output_json_schema",
            run: |up| {
                whole(with_format(
                    json!({"response_format": {"type": "json_schema", "json_schema": {
                        "name": "answer", "strict": true,
                        "schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}},
                    }}}),
                ))(up)
            },
        },
        // llama.cpp's schema on json_object (§11.2).
        Case {
            name: "structured_output_json_object_schema",
            run: |up| {
                whole(with_format(json!({"response_format": {
                    "type": "json_object",
                    "schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}},
                }})))(up)
            },
        },
        // llama.cpp's top-level `json_schema`, written as a response format
        // (§11.1).
        Case {
            name: "structured_output_top_level_json_schema",
            run: |up| {
                whole(with_format(json!({"json_schema": {
                    "type": "object", "properties": {"ok": {"type": "boolean"}},
                }})))(up)
            },
        },
        // An Anthropic client's format: named `response`, strict (§11.4).
        Case {
            name: "structured_output_from_messages",
            run: |up| {
                let ir = lmgw_core::ingress::anthropic::parse_messages_request(&json!({
                    "model": "alias", "max_tokens": 16,
                    "messages": [{"role": "user", "content": "hello"}],
                    "output_config": {"format": {"type": "json_schema", "schema": {
                        "type": "object", "properties": {"ok": {"type": "boolean"}},
                        "required": ["ok"], "additionalProperties": false,
                    }}},
                }))
                .unwrap();
                chat_request(up, &ir, &params(json!({"max_tokens": 16})), false)
            },
        },
        Case {
            name: "structured_output_text",
            run: |up| whole(with_format(json!({"response_format": {"type": "text"}})))(up),
        },
        // Keys lmgw does not model, on the format and its `json_schema`
        // object, and a `null` where a modelled key stands: written back as
        // they came (§11.17).
        Case {
            name: "structured_output_unmodelled_keys",
            run: |up| {
                whole(with_format(json!({"response_format": {
                    "type": "json_schema", "x-vendor-hint": {"level": 2},
                    "json_schema": {
                        "name": "answer", "strict": null, "x-vendor": [1, 2],
                        "schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}},
                    },
                }})))(up)
            },
        },
        // `json_schema` without its object: none is invented (§11.17).
        Case {
            name: "structured_output_bare_json_schema",
            run: |up| {
                whole(with_format(
                    json!({"response_format": {"type": "json_schema"}}),
                ))(up)
            },
        },
        // A `text` format next to llama.cpp's top-level `json_schema` is the
        // schema, as llama-server applies it (§11.18).
        Case {
            name: "structured_output_text_beside_top_level_schema",
            run: |up| {
                whole(with_format(json!({
                    "response_format": {"type": "text"},
                    "json_schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}},
                })))(up)
            },
        },
        Case {
            name: "structured_output_legacy_completions",
            run: legacy,
        },
    ]
}
