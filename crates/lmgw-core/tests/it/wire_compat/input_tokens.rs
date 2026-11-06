//! `POST /v1/responses/input_tokens` (§9 finding 6): OpenAI's Responses
//! input token count, a thin adapter onto lmgw's counter. Before it, the
//! route fell through to `/v1/responses/{id}` and answered 405 with an empty
//! body.

use lmgw_core::config::Protocol;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

/// An Anthropic route counts on Anthropic's own counter, exactly: the
/// Messages body the send would render, with the counter's keys only.
#[tokio::test]
async fn an_anthropic_route_counts_the_rendered_messages_body_exactly() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 42})))
        .mount(&mock)
        .await;
    let (_state, gw) = gateway(&mock, Protocol::Anthropic).await;
    let (status, headers, text) = post(
        &gw,
        "/v1/responses/input_tokens",
        &json!({"model": "my-model", "instructions": "Be brief.", "input": "Tell me a joke.",
            "max_output_tokens": 100, "temperature": 0.5,
            "tools": [{"type": "function", "name": "now",
                "parameters": {"type": "object", "properties": {}}}]}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v,
        json!({"object": "response.input_tokens", "input_tokens": 42})
    );
    assert!(headers.get("x-lmgw-count-approximate").is_none());
    let sent: Value = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .find(|r| r.url.path() == "/v1/messages/count_tokens")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .expect("a count was sent");
    assert_eq!(sent["model"], "tgt-model");
    assert_eq!(sent["system"], "Be brief.");
    assert_eq!(sent["messages"][0]["role"], "user");
    assert_eq!(sent["tools"][0]["name"], "now");
    for key in ["max_tokens", "temperature", "stream"] {
        assert!(sent.get(key).is_none(), "{key} is no counter key: {sent}");
    }
}

/// Every other route counts the flattened request, as
/// `/v1/messages/count_tokens` does, and says so.
#[tokio::test]
async fn a_compatible_route_counts_flattened_and_says_so() {
    let mock = MockServer::start().await;
    let (_state, gw) = gateway(&mock, Protocol::OpenaiCompatible).await;
    let (status, headers, text) = post(
        &gw,
        "/v1/responses/input_tokens",
        &json!({"model": "my-model", "input": "Tell me a joke."}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["object"], "response.input_tokens");
    assert!(v["input_tokens"].as_u64().unwrap() > 0, "{text}");
    let approx = headers["x-lmgw-count-approximate"].to_str().unwrap();
    assert!(approx.contains("flattened"), "{approx}");
}

#[tokio::test]
async fn a_body_without_input_is_a_400_in_openai_s_shape() {
    let mock = MockServer::start().await;
    let (_state, gw) = gateway(&mock, Protocol::OpenaiCompatible).await;
    let (status, _, text) = post(
        &gw,
        "/v1/responses/input_tokens",
        &json!({"model": "my-model"}),
    )
    .await;
    assert_eq!(status, 400, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["error"]["type"], "invalid_request_error", "{text}");
}

/// The count renders the off as the send does: a model that refused
/// `thinking: disabled` on `/v1/messages` (and was retried without it) is
/// not asked to count it either, on `/v1/responses/input_tokens` or
/// `/v1/messages/count_tokens`.
#[tokio::test]
async fn the_count_carries_the_off_form_a_refusal_taught_the_send() {
    let mock = MockServer::start().await;
    let refusal = ResponseTemplate::new(400).set_body_json(json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message":
            "thinking.type: Input tag 'disabled' found using 'type' does not match any of \
             the expected tags"}}));
    for p in ["/v1/messages", "/v1/messages/count_tokens"] {
        Mock::given(method("POST"))
            .and(path(p))
            .and(wiremock::matchers::body_partial_json(
                json!({"thinking": {"type": "disabled"}}),
            ))
            .respond_with(refusal.clone())
            .with_priority(1)
            .mount(&mock)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 7})))
        .with_priority(2)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt-model",
            "content": [{"type": "text", "text": "hi"}], "stop_reason": "end_turn",
            "stop_sequence": null, "usage": {"input_tokens": 5, "output_tokens": 1}})))
        .with_priority(2)
        .mount(&mock)
        .await;
    let (_state, gw) = gateway(&mock, Protocol::Anthropic).await;
    let client = reqwest::Client::new();
    let off = |route: &str, body: Value| {
        client
            .post(format!("{gw}{route}"))
            .header("x-lmgw-reasoning", "off")
            .json(&body)
            .send()
    };
    // The send learns the lesson.
    let sent = off(
        "/v1/messages",
        json!({"model": "my-model", "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await
    .unwrap();
    assert_eq!(sent.status(), 200);
    let counted = off(
        "/v1/responses/input_tokens",
        json!({"model": "my-model", "input": "hi"}),
    )
    .await
    .unwrap();
    assert_eq!(counted.status(), 200, "{}", counted.text().await.unwrap());
    let counted = off(
        "/v1/messages/count_tokens",
        json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await
    .unwrap();
    assert_eq!(counted.status(), 200, "{}", counted.text().await.unwrap());
    let reqs = mock.received_requests().await.unwrap();
    let counts: Vec<_> = reqs
        .iter()
        .filter(|r| r.url.path() == "/v1/messages/count_tokens")
        .collect();
    assert_eq!(counts.len(), 2, "no refused count attempts");
    for c in counts {
        let b: Value = serde_json::from_slice(&c.body).unwrap();
        assert!(b.get("thinking").is_none(), "{b}");
    }
}

/// A stored response waiting on an approval is counted all the same: the
/// count decides nothing, and says the call's result is not in it.
#[tokio::test]
async fn a_response_waiting_on_approval_is_counted_and_flagged() {
    use crate::responses_api::{call_reply, mcp_stub, mount_sequence, register_mcp, setup};
    let mock = MockServer::start().await;
    mount_sequence(
        &mock,
        vec![call_reply("c1", "tools__echo", json!({"text": "hi"}))],
    )
    .await;
    let (state, base) = setup(&mock.uri()).await;
    let (mcp_url, calls) = mcp_stub().await;
    register_mcp(&state, "stub", "tools", &mcp_url).await;
    let (status, asked) = crate::responses_api::post(
        &base,
        json!({"model": "my-model", "input": "echo hi", "tools": [
            {"type": "mcp", "server_label": "tools", "require_approval": "always"}]}),
    )
    .await;
    assert_eq!(status, 200);
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses/input_tokens"))
        .json(
            &json!({"model": "my-model", "previous_response_id": asked["id"],
            "input": "and then?"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let approx = resp.headers()["x-lmgw-count-approximate"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(approx.contains("pending_calls_omitted"), "{approx}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}
