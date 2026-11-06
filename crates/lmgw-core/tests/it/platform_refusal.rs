//! The model's refusal on the OpenAI platform (openai egress design §7.2.3,
//! WP7 review F-R1), read off either wire and written by each client wire
//! as its own refusal where it has one: Chat Completions' `message.refusal`
//! and `delta.refusal`, the Responses API's `refusal` part and
//! `response.refusal.delta`, and on Anthropic's wire, which has none, text.

use lmgw_core::config::{Protocol, UpstreamKind};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::chat_actions::post;
use crate::common::Gw;
use crate::e2e_proxy::setup_kind;
use crate::support::responses_wire as rw;

const NO: &str = "I can't help with that.";

/// A platform row whose `/responses` refuses, whole or streamed as asked,
/// and whose `/chat/completions` refuses on the Chat wire.
async fn refusing() -> (MockServer, Gw) {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let items = [rw::refusal("msg_1", NO)];
            match body["stream"] == true {
                true => ResponseTemplate::new(200)
                    .set_body_raw(rw::stream("resp_1", &items), "text/event-stream"),
                false => ResponseTemplate::new(200).set_body_json(rw::response(
                    "resp_1",
                    "completed",
                    json!(items),
                )),
            }
        })
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
            match body["stream"] == true {
                true => ResponseTemplate::new(200).set_body_raw(
                    format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        json!({"id": "c", "object": "chat.completion.chunk", "choices": [
                            {"index": 0, "delta": {"role": "assistant", "content": null,
                                                   "refusal": NO}}]}),
                        json!({"id": "c", "object": "chat.completion.chunk", "choices": [
                            {"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                    ),
                    "text/event-stream",
                ),
                false => ResponseTemplate::new(200).set_body_json(json!({
                    "id": "c", "object": "chat.completion", "model": "tgt-model",
                    "choices": [{"index": 0, "finish_reason": "stop", "message": {
                        "role": "assistant", "content": null, "refusal": NO}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1},
                })),
            }
        })
        .mount(&mock)
        .await;
    let (_state, gw) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    (mock, gw)
}

/// The `data:` payloads of an SSE body, parsed.
fn data(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect()
}

/// The chat chunks' `delta.refusal`s, joined.
fn chunk_refusal(text: &str) -> String {
    data(text)
        .iter()
        .filter_map(|c| {
            c["choices"][0]["delta"]["refusal"]
                .as_str()
                .map(String::from)
        })
        .collect()
}

#[tokio::test]
async fn chat_completions_gets_message_refusal_on_either_wire() {
    let (_mock, gw) = refusing().await;
    // The Responses wire, then the Chat wire (`stop` takes it).
    for extra in [json!({}), json!({"stop": ["END"]})] {
        let mut body = json!({"model": "my-model",
                              "messages": [{"role": "user", "content": "hi"}]});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let v: Value = post(&gw, "/v1/chat/completions", body.clone())
            .await
            .json()
            .await
            .unwrap();
        let message = &v["choices"][0]["message"];
        assert_eq!(message["refusal"], NO, "{extra}: {v}");
        assert_eq!(message["content"], Value::Null, "{extra}: {v}");

        body["stream"] = json!(true);
        let text = post(&gw, "/v1/chat/completions", body)
            .await
            .text()
            .await
            .unwrap();
        assert_eq!(chunk_refusal(&text), NO, "{extra}: {text}");
        assert!(!text.contains(&format!("\"content\":\"{NO}\"")), "{text}");
    }
}

#[tokio::test]
async fn messages_gets_the_refusal_as_text() {
    let (_mock, gw) = refusing().await;
    let v: Value = post(
        &gw,
        "/v1/messages",
        json!({"model": "my-model", "max_tokens": 100,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(v["content"], json!([{"type": "text", "text": NO}]), "{v}");
}

/// The `/v1/responses` loop (kept in it by a label lmgw serves) writes the
/// refusal as the Responses API's own part, whole and streamed.
#[tokio::test]
async fn the_responses_loop_writes_a_refusal_part() {
    let (_mock, gw) = refusing().await;
    let ask = json!({"model": "my-model", "input": "hi",
                     "tools": [{"type": "mcp", "server_label": "docs"}]});
    let v: Value = post(&gw, "/v1/responses", ask.clone())
        .await
        .json()
        .await
        .unwrap();
    let parts: Vec<&Value> = v["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "message")
        .flat_map(|i| i["content"].as_array().unwrap())
        .collect();
    assert_eq!(parts, [&json!({"type": "refusal", "refusal": NO})], "{v}");

    let mut ask = ask;
    ask["stream"] = json!(true);
    let text = post(&gw, "/v1/responses", ask).await.text().await.unwrap();
    let events = data(&text);
    let delta: String = events
        .iter()
        .filter(|e| e["type"] == "response.refusal.delta")
        .filter_map(|e| e["delta"].as_str().map(String::from))
        .collect();
    assert_eq!(delta, NO, "{text}");
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "response.refusal.done" && e["refusal"] == NO),
        "{text}"
    );
    assert!(
        !events
            .iter()
            .any(|e| e["type"] == "response.output_text.delta"),
        "{text}"
    );
}

/// A `refusal` part is the model's, an assistant part only: on any other
/// role both ingresses refuse the request with a 400, as OpenAI does,
/// rather than drop the part; on the assistant's it is read.
#[test]
fn a_refusal_part_on_another_role_is_refused() {
    use lmgw_core::error::GatewayError;
    use lmgw_core::ingress::{openai::parse_chat_request, responses::parse_request};
    let part = json!([{"type": "refusal", "refusal": NO}]);
    for role in ["user", "system", "developer"] {
        let chat = parse_chat_request(&json!({"model": "m", "messages": [
            {"role": role, "content": part},
        ]}));
        assert!(
            matches!(&chat, Err(GatewayError::BadRequest(m)) if m.contains("refusal")),
            "chat {role}: {chat:?}"
        );
        let responses = parse_request(&json!({"model": "m", "input": [
            {"type": "message", "role": role, "content": part},
        ]}));
        assert!(
            matches!(&responses, Err(GatewayError::BadRequest(m)) if m.contains("refusal")),
            "responses {role}"
        );
    }
    let chat = parse_chat_request(&json!({"model": "m", "messages": [
        {"role": "user", "content": "x"},
        {"role": "assistant", "content": part},
    ]}))
    .unwrap();
    assert_eq!(
        chat.messages[1].content,
        [lmgw_core::ir::ContentPart::Refusal { text: NO.into() }]
    );
    let responses = parse_request(&json!({"model": "m", "input": [
        {"type": "message", "role": "user", "content": "x"},
        {"type": "message", "role": "assistant", "content": part},
    ]}))
    .unwrap();
    assert_eq!(
        responses.ir.messages[1].content,
        [lmgw_core::ir::ContentPart::Refusal { text: NO.into() }]
    );
}
