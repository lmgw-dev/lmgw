//! Each egress's spelling of the IR's format (design §6). The OpenAI-shaped
//! egresses' bodies are in the egress corpus (`egress_golden`,
//! `structured_output_*` cases); these are Anthropic's and Gemini's, from
//! each ingress dialect.

use lmgw_core::config::{Protocol, Upstream, UpstreamKind};
use lmgw_core::egress::for_protocol;
use lmgw_core::error::GatewayError;
use lmgw_core::ingress::{anthropic, openai};
use lmgw_core::ir::{ChatRequest, Params, ReasoningControl};
use serde_json::{json, Value};

fn upstream(protocol: Protocol) -> Upstream {
    Upstream {
        id: 1,
        name: "cloud".into(),
        protocol,
        kind: UpstreamKind::Generic,
        base_url: "http://cloud.test".into(),
        api_key: Some("sk-test".into()),
        extra_headers: vec![],
        timeout_ms: 30_000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

/// The body `protocol`'s egress posts for `ir`, or its refusal.
fn body(protocol: Protocol, ir: &ChatRequest, params: &Params) -> Result<Value, GatewayError> {
    let http = reqwest::Client::new();
    let rb =
        for_protocol(protocol).build_chat(&http, &upstream(protocol), "tgt", ir, params, false)?;
    let req = rb.build().unwrap();
    Ok(serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap())
}

fn from_chat(rf: Value) -> ChatRequest {
    openai::parse_chat_request(&json!({
        "model": "m", "messages": [{"role": "user", "content": "hi"}], "response_format": rf,
    }))
    .unwrap()
}

fn schema() -> Value {
    json!({"type": "object", "properties": {"ok": {"type": "boolean"}},
           "required": ["ok"], "additionalProperties": false})
}

fn json_schema() -> Value {
    json!({"type": "json_schema", "json_schema": {
        "name": "answer", "description": "d", "strict": false, "schema": schema(),
    }})
}

#[test]
fn anthropic_gets_output_config_format_beside_the_effort() {
    let ir = from_chat(json_schema());
    let params = Params {
        reasoning: Some(ReasoningControl {
            effort: Some("low".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let b = body(Protocol::Anthropic, &ir, &params).unwrap();
    assert_eq!(
        b["output_config"],
        json!({"effort": "low", "format": {"type": "json_schema", "schema": schema()}}),
        "{b}"
    );
    // Without an effort the object carries the format alone.
    let b = body(Protocol::Anthropic, &ir, &Params::default()).unwrap();
    assert_eq!(
        b["output_config"],
        json!({"format": {"type": "json_schema", "schema": schema()}})
    );
    // `{"type": "text"}` is the default there: nothing to write.
    let b = body(
        Protocol::Anthropic,
        &from_chat(json!({"type": "text"})),
        &Params::default(),
    )
    .unwrap();
    assert!(b.get("output_config").is_none(), "{b}");
}

/// No JSON-object mode, no unknown type, no schema-less schema: refused
/// before anything is sent, never dropped (§6).
#[test]
fn anthropic_refuses_what_it_has_no_equivalent_for() {
    let refuse =
        |rf: Value| body(Protocol::Anthropic, &from_chat(rf), &Params::default()).unwrap_err();
    let e = refuse(json!({"type": "json_object"}));
    assert_eq!(e.kind(), "response_format_unsupported");
    assert!(e.to_string().contains("no JSON-object mode"), "{e}");
    assert_eq!(
        refuse(json!({"type": "structural_tag"})).kind(),
        "response_format_unsupported"
    );
    assert_eq!(
        refuse(json!({"type": "json_schema", "json_schema": {"name": "x"}})).kind(),
        "invalid_json_schema"
    );
}

#[test]
fn gemini_gets_json_mode_and_the_schema_as_json_schema() {
    let b = body(
        Protocol::Gemini,
        &from_chat(json_schema()),
        &Params::default(),
    )
    .unwrap();
    assert_eq!(
        b["generationConfig"],
        json!({"responseMimeType": "application/json", "responseJsonSchema": schema()}),
        "{b}"
    );
    let b = body(
        Protocol::Gemini,
        &from_chat(json!({"type": "json_object"})),
        &Params::default(),
    )
    .unwrap();
    assert_eq!(
        b["generationConfig"],
        json!({"responseMimeType": "application/json"})
    );
    // llama.cpp's schema on json_object is a schema to Gemini too (§11.2).
    let b = body(
        Protocol::Gemini,
        &from_chat(json!({"type": "json_object", "schema": schema()})),
        &Params::default(),
    )
    .unwrap();
    assert_eq!(b["generationConfig"]["responseJsonSchema"], schema());
    let e = body(
        Protocol::Gemini,
        &from_chat(json!({"type": "structural_tag"})),
        &Params::default(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), "response_format_unsupported");
}

/// A `/v1/messages` client's format reaches Gemini and OpenAI-shaped
/// upstreams too: every ingress reaches every egress.
#[test]
fn an_anthropic_clients_format_reaches_every_egress() {
    let ir = anthropic::parse_messages_request(&json!({
        "model": "m", "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}],
        "output_config": {"format": {"type": "json_schema", "schema": schema()}},
    }))
    .unwrap();
    let b = body(Protocol::Gemini, &ir, &Params::default()).unwrap();
    assert_eq!(b["generationConfig"]["responseJsonSchema"], schema());
    let b = body(Protocol::OpenaiCompatible, &ir, &Params::default()).unwrap();
    assert_eq!(
        b["response_format"],
        json!({"type": "json_schema", "json_schema": {
            "name": "response", "strict": true, "schema": schema(),
        }})
    );
    let b = body(Protocol::Anthropic, &ir, &Params::default()).unwrap();
    assert_eq!(
        b["output_config"]["format"],
        json!({"type": "json_schema", "schema": schema()})
    );
}

/// Function tools beside a JSON schema: both go, on every egress — Gemini and
/// Gemma took the pair in the fix round's probes (§9.3, §9.5), and were it
/// refused, the refusal would be the answer, never a format dropped to let
/// the tools through.
#[test]
fn tools_beside_a_schema_both_go() {
    let ir = openai::parse_chat_request(&json!({
        "model": "m", "messages": [{"role": "user", "content": "Reply with ok."}],
        "response_format": json_schema(),
        "tools": [{"type": "function", "function": {
            "name": "get_weather", "description": "The weather in a city.",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}},
                           "required": ["city"]}}}],
    }))
    .unwrap();
    let g = body(Protocol::Gemini, &ir, &Params::default()).unwrap();
    assert_eq!(
        g["tools"][0]["functionDeclarations"][0]["name"], "get_weather",
        "{g}"
    );
    assert_eq!(
        g["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(g["generationConfig"]["responseJsonSchema"], schema());
    let a = body(Protocol::Anthropic, &ir, &Params::default()).unwrap();
    assert_eq!(a["tools"][0]["name"], "get_weather", "{a}");
    assert_eq!(a["output_config"]["format"]["schema"], schema());
    let o = body(Protocol::OpenaiCompatible, &ir, &Params::default()).unwrap();
    assert_eq!(o["tools"][0]["function"]["name"], "get_weather", "{o}");
    assert_eq!(o["response_format"], json_schema());
}
