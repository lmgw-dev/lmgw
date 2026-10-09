//! The routes that forward the client's own body rather than one built from
//! the IR: a signed call id from an earlier Gemini step goes out bare there
//! too — the native `/v1/responses` passthrough (an OpenAI upstream that
//! implements the API) and `/v1/messages/count_tokens` on an Anthropic
//! upstream, where a signature would also be counted as prompt.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ir::{call_id_with_signature, THOUGHT_SIGNATURE_MARKER};
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{Ingress, Turn, SIG};
use crate::chat_actions::{gateway, post};
use crate::common::{serve, Gw};

/// A history whose first step Gemini answered: the signed call and its
/// result, a second call without a signature, and another provider's.
fn history() -> (Vec<Turn>, String) {
    let signed = call_id_with_signature("call_5f0a1b2c3d4e0", SIG);
    let history = vec![
        Turn::User("Weather and time in Paris?"),
        Turn::Calls(vec![
            (signed.clone(), "get_weather"),
            ("call_5f0a1b2c3d4e1".into(), "get_time"),
        ]),
        Turn::Results(vec![
            (signed.clone(), "Sunny"),
            ("call_5f0a1b2c3d4e1".into(), "14:00"),
        ]),
        Turn::Calls(vec![("toolu_01x".into(), "get_weather")]),
        Turn::Results(vec![("toolu_01x".into(), "Still sunny")]),
    ];
    (history, signed)
}

/// A gateway whose alias `m` is an OpenAI upstream on `mock` that
/// implements `/v1/responses` itself.
async fn native_responses_gateway(mock: &MockServer) -> Gw {
    let state = AppState::init_for_tests().await.unwrap();
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "native".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: mock.uri(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "m".into(),
            upstream_id: up,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    serve(state).await
}

async fn received(mock: &MockServer) -> Value {
    let reqs = mock.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1, "one request upstream");
    serde_json::from_slice(&reqs[0].body).unwrap()
}

/// The native passthrough forwards the body as it came, but for the model
/// and the signed `call_id`s, which go out bare.
#[tokio::test]
async fn the_native_responses_passthrough_sends_bare_call_ids() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_up", "object": "response", "status": "completed",
            "output": [{"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "ok"}]}],
        })))
        .mount(&mock)
        .await;
    let gw = native_responses_gateway(&mock).await;
    let (history, signed) = history();
    let body = Ingress::Responses.body(&history, false);
    let r = post(&gw, "/v1/responses", body.clone()).await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());

    let sent = received(&mock).await;
    assert!(
        !sent.to_string().contains(THOUGHT_SIGNATURE_MARKER),
        "{sent:#}"
    );
    let mut want = body;
    want["model"] = json!("tgt-model");
    let bare = signed.split(THOUGHT_SIGNATURE_MARKER).next().unwrap();
    for item in want["input"].as_array_mut().unwrap() {
        if item["call_id"] == json!(signed) {
            item["call_id"] = json!(bare);
        }
    }
    assert_eq!(sent, want);
}

/// Anthropic's count gets the client's body with every `tool_use.id` and
/// `tool_use_id` bare, and nothing else changed but the model.
#[tokio::test]
async fn the_anthropic_count_sends_bare_tool_ids() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 7})))
        .mount(&mock)
        .await;
    let (_, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Anthropic).await;
    let (history, signed) = history();
    let mut body = Ingress::Messages.body(&history, false);
    for k in ["stream", "max_tokens"] {
        body.as_object_mut().unwrap().remove(k);
    }
    let r = gw
        .client()
        .post(format!("{gw}/v1/messages/count_tokens"))
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status();
    let answer: Value = r.json().await.unwrap();
    assert!(status.is_success(), "{answer}");
    assert_eq!(answer["input_tokens"], 7);

    let sent = received(&mock).await;
    assert!(
        !sent.to_string().contains(THOUGHT_SIGNATURE_MARKER),
        "{sent:#}"
    );
    let mut want = body;
    want["model"] = json!("tgt-model");
    let bare = signed.split(THOUGHT_SIGNATURE_MARKER).next().unwrap();
    for m in want["messages"].as_array_mut().unwrap() {
        for b in m["content"].as_array_mut().into_iter().flatten() {
            for k in ["id", "tool_use_id"] {
                if b[k] == json!(signed) {
                    b[k] = json!(bare);
                }
            }
        }
    }
    assert_eq!(sent, want);
}
