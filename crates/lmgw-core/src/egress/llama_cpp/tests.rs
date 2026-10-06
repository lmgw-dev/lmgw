//! The llama.cpp egress's own seams. The bytes it sends are the egress and
//! route corpora's business (`tests/it/egress_golden.rs`,
//! `tests/it/route_golden.rs`); these pin what the gate and the `/tokenize`
//! route build on.

use serde_json::{json, Value};

use super::count::{
    apply_template_request, prompt_tokenize_request, rendered_prompt, token_count, tokenize_request,
};
use super::*;
use crate::config::UpstreamKind;
use crate::egress::for_protocol;
use crate::ir::{Message, Role};

fn upstream() -> Upstream {
    Upstream {
        id: 7,
        name: "llama".into(),
        protocol: Protocol::LlamaCpp,
        kind: UpstreamKind::LlamaServer,
        base_url: "http://llama.test:8080/v1".into(),
        api_key: Some("sk-llama".into()),
        extra_headers: vec![("x-extra".into(), "one".into())],
        timeout_ms: 1000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

fn body_of(req: &reqwest::Request) -> Value {
    serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap()
}

#[test]
fn the_llama_cpp_protocol_dispatches_here() {
    assert_eq!(for_protocol(Protocol::LlamaCpp).proto(), Protocol::LlamaCpp);
}

/// The gate counts [`chat_body`]; `build_chat` must post exactly it.
#[test]
fn build_chat_posts_the_counted_body() {
    let up = upstream();
    let ir = ChatRequest {
        model_alias: "a".into(),
        messages: vec![Message::text(Role::User, "hi")],
        params: Params::default(),
        tools: vec![],
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    };
    let params: Params = serde_json::from_value(json!({"reasoning": {"enabled": false}})).unwrap();
    let req = LlamaCppEgress
        .build_chat(&reqwest::Client::new(), &up, "m", &ir, &params, true)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(
        req.url().as_str(),
        "http://llama.test:8080/v1/chat/completions"
    );
    assert_eq!(req.headers()["authorization"], "Bearer sk-llama");
    assert_eq!(req.headers()["x-extra"], "one");
    let counted = chat_body(&ir, "m", &params, true, &up);
    assert_eq!(
        req.body().unwrap().as_bytes().unwrap(),
        serde_json::to_vec(&counted).unwrap()
    );
    // Off is the template kwarg alone, never a level (§7).
    assert_eq!(
        counted["chat_template_kwargs"],
        json!({"enable_thinking": false})
    );
    assert!(counted.get("reasoning_effort").is_none());
}

#[test]
fn the_count_plan_is_tokenize_at_the_server_root() {
    let up = upstream();
    let plan = LlamaCppEgress
        .build_count_tokens(&reqwest::Client::new(), &up, "m", "hello world")
        .unwrap();
    let CountPlan::Request(rb) = plan else {
        panic!("a llama.cpp count is a request to the server");
    };
    let req = rb.build().unwrap();
    assert_eq!(req.url().as_str(), "http://llama.test:8080/tokenize");
    assert_eq!(req.headers()["authorization"], "Bearer sk-llama");
    assert_eq!(
        body_of(&req),
        json!({"model": "m", "content": "hello world"})
    );
    assert_eq!(
        LlamaCppEgress
            .parse_count(br#"{"tokens":[1,2,3]}"#)
            .unwrap(),
        3
    );
}

/// The `/tokenize` route forwards the client's own object to the same root,
/// with the row's auth.
#[test]
fn tokenize_request_forwards_the_body_as_given() {
    let body = json!({"model": "m", "content": "x", "with_pieces": true});
    let req = tokenize_request(&reqwest::Client::new(), &upstream(), &body)
        .build()
        .unwrap();
    assert_eq!(req.url().as_str(), "http://llama.test:8080/tokenize");
    assert_eq!(req.headers()["x-extra"], "one");
    assert_eq!(body_of(&req), body);
}

/// The gate's two steps go to a managed container's root, with no auth, and
/// the second one carries the completion path's `add_special: true`.
#[test]
fn the_gates_count_requests_and_readers() {
    let http = reqwest::Client::new();
    let chat = json!({"model": "m", "messages": []});
    let req = apply_template_request(&http, "http://127.0.0.1:9000", &chat)
        .build()
        .unwrap();
    assert_eq!(req.url().as_str(), "http://127.0.0.1:9000/apply-template");
    assert!(req.headers().get("authorization").is_none());
    assert_eq!(body_of(&req), chat);

    let req = prompt_tokenize_request(&http, "http://127.0.0.1:9000", &json!("p"))
        .build()
        .unwrap();
    assert_eq!(req.url().as_str(), "http://127.0.0.1:9000/tokenize");
    assert_eq!(body_of(&req), json!({"content": "p", "add_special": true}));

    assert_eq!(rendered_prompt(&json!({"prompt": "p"})), Some("p"));
    assert_eq!(rendered_prompt(&json!({"text": "p"})), None);
    assert_eq!(token_count(&json!({"tokens": [1, 2]})), Some(2));
    assert_eq!(token_count(&json!({"count": 2})), None);
}

#[test]
fn the_context_refusal_maps_to_context_exceeded() {
    let body = br#"{"error":{"code":400,"message":"too long","type":"exceed_context_size_error","n_prompt_tokens":8010,"n_ctx":4096}}"#;
    assert!(matches!(
        LlamaCppEgress.map_error(400, body),
        GatewayError::ContextExceeded {
            prompt_tokens: 8010,
            limit: 4096,
            ..
        }
    ));
    assert_eq!(
        parse_exceed_context(body),
        Some(ExceedContext {
            n_prompt_tokens: 8010,
            n_ctx: 4096
        })
    );
}

/// I3 and the passthrough rule for a client's own `thinking_budget_tokens`
/// (`reasoning.rs`): a resolved budget goes under both names and wins over
/// the client's copy; a control without one removes the client's copy; no
/// control leaves it verbatim.
#[test]
fn the_budget_goes_under_both_names() {
    let up = upstream();
    let mut ir = ChatRequest {
        model_alias: "a".into(),
        messages: vec![Message::text(Role::User, "hi")],
        params: Params::default(),
        tools: vec![],
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    };
    let control = |c: Value| -> Params { serde_json::from_value(json!({"reasoning": c})).unwrap() };

    let b = chat_body(
        &ir,
        "m",
        &control(json!({"budget_tokens": 512})),
        false,
        &up,
    );
    assert_eq!(b["reasoning_budget_tokens"], 512);
    assert_eq!(b["thinking_budget_tokens"], 512);

    ir.passthrough = json!({"thinking_budget_tokens": 100})
        .as_object()
        .unwrap()
        .clone();
    let b = chat_body(
        &ir,
        "m",
        &control(json!({"budget_tokens": 512})),
        false,
        &up,
    );
    assert_eq!(b["thinking_budget_tokens"], 512);
    let b = chat_body(&ir, "m", &control(json!({"effort": "high"})), false, &up);
    assert!(b.get("thinking_budget_tokens").is_none(), "{b}");
    assert!(b.get("reasoning_budget_tokens").is_none(), "{b}");
    let b = chat_body(&ir, "m", &Params::default(), false, &up);
    assert_eq!(b["thinking_budget_tokens"], 100);
}
