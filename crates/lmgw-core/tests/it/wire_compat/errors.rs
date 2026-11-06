//! An upstream's error answer in each client's error dialect (§9 findings 7
//! and 8): a refused request is Anthropic's `invalid_request_error` and
//! OpenAI's, not `api_error`; a streamed Responses run that fails names a
//! code of OpenAI's enum.

use lmgw_core::config::Protocol;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

async fn refusing(status: u16, error_type: &str) -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({
            "type": "error",
            "error": {"type": error_type,
                "message": "thinking.type.enabled is not supported for this model."},
        })))
        .mount(&mock)
        .await;
    mock
}

fn messages_body() -> Value {
    json!({"model": "my-model", "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}]})
}

#[tokio::test]
async fn an_upstream_refusal_is_an_invalid_request_on_both_wires() {
    let mock = refusing(400, "invalid_request_error").await;
    let (_state, gw) = gateway(&mock, Protocol::Anthropic).await;
    let (status, _, text) = post(&gw, "/v1/messages", &messages_body()).await;
    assert_eq!(status, 400, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "invalid_request_error", "{text}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("thinking.type.enabled"),
        "{text}"
    );
    let (status, _, text) = post(
        &gw,
        "/v1/chat/completions",
        &json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 400, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["error"]["type"], "invalid_request_error", "{text}");
}

#[tokio::test]
async fn an_overloaded_upstream_is_overloaded_on_the_anthropic_wire() {
    let mock = refusing(529, "overloaded_error").await;
    let (_state, gw) = gateway(&mock, Protocol::Anthropic).await;
    let (status, _, text) = post(&gw, "/v1/messages", &messages_body()).await;
    assert_eq!(status, 503, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["error"]["type"], "overloaded_error", "{text}");
}

#[tokio::test]
async fn a_failed_streamed_run_names_a_code_of_openai_s_enum() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": {"message": "the model broke", "type": "server_error"},
        })))
        .mount(&mock)
        .await;
    let (_state, gw) = gateway(&mock, Protocol::OpenaiCompatible).await;
    let (status, _, text) = post(
        &gw,
        "/v1/responses",
        &json!({"model": "my-model", "stream": true, "input": "hi"}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let failed = events(&text)
        .into_iter()
        .find(|(_, e)| e["type"] == "response.failed")
        .unwrap_or_else(|| panic!("no response.failed in {text}"))
        .1;
    assert_eq!(
        failed["response"]["error"]["code"], "server_error",
        "{failed}"
    );
    assert!(
        failed["response"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("the model broke"),
        "lmgw's text kept: {failed}"
    );
}
