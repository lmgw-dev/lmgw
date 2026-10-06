use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::{Protocol, UpstreamKind};

/// An official llama-server's answer at 0c6a6a7, in the shape
/// `get_res_props` builds (`server-context.cpp:4515-4560`), the template
/// shortened.
fn official() -> Value {
    json!({
        "default_generation_settings": {"params": {"temperature": 0.8}, "n_ctx": 32768},
        "total_slots": 2,
        "model_alias": "gemma4-26b",
        "model_ftype": "Q4_K - Medium",
        "model_path": "/models/gemma-4-26b.gguf",
        "modalities": {"vision": true, "video": false, "audio": false},
        "media_marker": "<__media__>",
        "endpoint_slots": true,
        "endpoint_props": false,
        "endpoint_metrics": false,
        "ui": true,
        "ui_settings": {},
        "chat_template": "{{ bos_token }}{% for m in messages %}…{% endfor %}",
        "chat_template_caps": {
            "supports_string_content": true,
            "supports_typed_content": true,
            "supports_tools": true,
            "supports_tool_calls": true,
            "supports_parallel_tool_calls": true,
            "supports_system_role": true,
            "supports_preserve_reasoning": false,
            "supports_reasoning_effort": false,
            "supports_object_arguments": true
        },
        "bos_token": "<bos>",
        "eos_token": "<eos>",
        "build_info": "b11226-0c6a6a7",
        "is_sleeping": false,
        "cors_proxy_enabled": false
    })
}

/// ik_llama.cpp's answer at 7ff619c, as the test fake serves it
/// (`tests/it/support/llama_fake.rs`, `Shape::Ik`): no `video`, no
/// `build_info`, empty caps, and the whole context at the top level.
fn ik() -> Value {
    json!({
        "default_generation_settings": {"n_ctx": 4096},
        "total_slots": 2,
        "chat_template_caps": {},
        "modalities": {"vision": false, "audio": false},
        "n_ctx": 8192
    })
}

/// A router-mode llama-server asked without `?model=`
/// (`server-models.cpp:1875-1893`).
fn router() -> Value {
    json!({
        "role": "router",
        "max_instances": 4,
        "models_autoload": true,
        "model_alias": "llama-server",
        "model_path": "none",
        "default_generation_settings": {"params": {}, "n_ctx": 0},
        "ui_settings": {},
        "build_info": "b11226-0c6a6a7",
        "cors_proxy_enabled": false
    })
}

fn facts(body: Value) -> LlamaFacts {
    match Props::read(body).unwrap() {
        Props::Model(f) => f,
        other => panic!("not a model's facts: {other:?}"),
    }
}

#[test]
fn an_official_body_reads_every_fact() {
    let f = facts(official());
    assert_eq!(f.vision, Some(true));
    assert_eq!(f.audio, Some(false));
    assert_eq!(f.video, Some(false));
    assert_eq!(f.n_ctx_slot, Some(32768));
    assert_eq!(f.build_info.as_deref(), Some("b11226-0c6a6a7"));
    let caps = f.caps.as_ref().expect("official builds send their caps");
    assert_eq!(caps.len(), 9);
    assert_eq!(f.cap("supports_tools"), Some(true));
    assert_eq!(f.cap("supports_reasoning_effort"), Some(false));
    assert_eq!(f.cap("supports_something_later"), None);
    // The body is kept whole, template included, and never serialized.
    assert_eq!(f.raw, official());
    let shown = serde_json::to_value(&f).unwrap();
    assert!(shown.get("raw").is_none(), "{shown}");
    assert_eq!(shown["build_info"], "b11226-0c6a6a7");
}

#[test]
fn an_ik_body_leaves_what_it_does_not_say_unknown() {
    let f = facts(ik());
    assert_eq!((f.vision, f.audio), (Some(false), Some(false)));
    // ik sends no video and no build: unknown, not false.
    assert_eq!(f.video, None);
    assert_eq!(f.build_info, None);
    // `{}` is a map with nothing in it: every cap unknown.
    assert_eq!(f.caps, Some(Default::default()));
    assert_eq!(f.cap("supports_tools"), None);
    // The slot's context, never the top-level whole.
    assert_eq!(f.n_ctx_slot, Some(4096));
}

#[test]
fn a_router_answer_is_no_model_facts() {
    assert_eq!(
        Props::read(router()).unwrap(),
        Props::Router {
            build_info: Some("b11226-0c6a6a7".into())
        }
    );
}

#[test]
fn any_object_reads_and_anything_else_does_not() {
    // An old or stripped build: nothing known, still an answer.
    let f = facts(json!({"default_generation_settings": {"n_ctx": 0}}));
    assert_eq!(
        f,
        LlamaFacts {
            raw: json!({"default_generation_settings": {"n_ctx": 0}}),
            ..Default::default()
        }
    );
    // Wrong types are unknown too.
    let f = facts(json!({"modalities": {"vision": "yes"}, "build_info": ""}));
    assert_eq!((f.vision, f.build_info), (None, None));
    let e = Props::read(json!(["not", "props"])).unwrap_err();
    assert!(e.contains("an array"), "{e}");
}

fn upstream(base_url: String, api_key: Option<&str>) -> Upstream {
    Upstream {
        id: 7,
        name: "llama".into(),
        // The request is the same on every row that reaches a llama-server.
        protocol: Protocol::Openai,
        kind: UpstreamKind::Generic,
        base_url,
        api_key: api_key.map(str::to_string),
        extra_headers: vec![("x-team".into(), "lab".into())],
        timeout_ms: 0,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

/// The row's request goes to the root, not under `/v1`, with its bearer and
/// headers; a router is asked about one model without loading it.
#[tokio::test]
async fn the_request_carries_the_rows_auth_and_asks_a_router_without_loading() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .and(query_param("model", "qwen"))
        .and(query_param("autoload", "false"))
        .and(header("authorization", "Bearer sk-local"))
        .and(header("x-team", "lab"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ik()))
        .mount(&server)
        .await;
    let http = reqwest::Client::new();
    let up = upstream(format!("{}/v1", server.uri()), Some("sk-local"));
    let got = probe(
        request(&http, &up, Some("qwen")),
        Some(Duration::from_secs(5)),
    )
    .await
    .unwrap();
    assert_eq!(got, Props::Model(facts(ik())));
}

#[tokio::test]
async fn failures_say_whether_the_server_answered() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .and(query_param("model", "cold"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "code": 400, "message": "model is not loaded", "type": "invalid_request_error"
        }})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .and(query_param("model", "garbled"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
        .mount(&server)
        .await;
    let http = reqwest::Client::new();
    let up = upstream(server.uri(), None);
    let t = Some(Duration::from_secs(5));

    match probe(request(&http, &up, Some("cold")), t).await {
        Err(PropsFailure::Status { status: 400, body }) => {
            assert!(body.contains("model is not loaded"), "{body}")
        }
        other => panic!("{other:?}"),
    }
    let garbled = probe(request(&http, &up, Some("garbled")), t).await;
    assert!(
        matches!(garbled, Err(PropsFailure::Unreadable(_))),
        "{garbled:?}"
    );
    // Nothing mounted for the bare path: an old build's 404 is an answer.
    let old = probe(request(&http, &up, None), t).await.unwrap_err();
    assert!(
        matches!(old, PropsFailure::Status { status: 404, .. }),
        "{old:?}"
    );
    assert!(old.to_string().starts_with("HTTP 404"), "{old}");

    // A port nothing listens on reaches nothing.
    let gone = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let none = probe(container_request(&http, gone), t).await.unwrap_err();
    assert!(matches!(none, PropsFailure::Unreachable(_)), "{none:?}");
}
