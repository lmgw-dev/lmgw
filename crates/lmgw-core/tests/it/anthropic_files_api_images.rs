//! An image by Files API reference (gateway design §6.1, changed
//! 2026-11-06) is an image only where the row is sent it: an
//! Anthropic-protocol row gets it as the client wrote it, a message's and a
//! `tool_result`'s alike; every other row is sent none of it (no file id
//! inlined into a tool result's text either), the response names
//! `content.image`, and `/v1/messages/count_tokens` counts it as the send
//! carries it: as nothing, not as media it left out.

use lmgw_core::config::Protocol;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::messages_context_management::{bodies_at, world};

fn image() -> Value {
    json!({"type": "image", "source": {"type": "file", "file_id": "file_shot"}})
}

/// A tool loop whose result is a screenshot by Files API reference.
fn tool_loop() -> Value {
    json!({"model": "m", "max_tokens": 64, "messages": [
        {"role": "user", "content": "Take a screenshot."},
        {"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_1", "name": "shot", "input": {}},
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                {"type": "text", "text": "the screen:"},
                image(),
            ]},
        ]},
    ]})
}

async fn post(gw: &crate::common::Gw, route: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gw}{route}"))
        .header("anthropic-version", "2023-06-01")
        .json(body)
        .send()
        .await
        .unwrap()
}

fn header(resp: &reqwest::Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_tool_result_s_file_image_goes_to_anthropic_as_written() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 3, "output_tokens": 1},
        })))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;
    let resp = post(&gw, "/v1/messages", &tool_loop()).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lmgw-params-ignored"), "");
    let sent = bodies_at(&mock, "/v1/messages").await;
    assert_eq!(
        sent[0]["messages"][2]["content"][0]["content"],
        json!([{"type": "text", "text": "the screen:"}, image()])
    );
}

#[tokio::test]
async fn another_row_is_sent_none_of_a_tool_result_s_file_image() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "model": "tgt",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::OpenaiCompatible).await;
    let resp = post(&gw, "/v1/messages", &tool_loop()).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lmgw-params-ignored"), "content.image");
    let sent = bodies_at(&mock, "/chat/completions").await;
    let text = sent[0].to_string();
    assert!(!text.contains("file_shot"), "{text}");
    assert_eq!(sent[0]["messages"][2]["role"], "tool");
    assert_eq!(sent[0]["messages"][2]["content"], "the screen:");
}

/// The count on a row that is sent no Files API image counts none and says
/// it left no media out, as the send carries none; an inline image beside
/// it still is media the count leaves out.
#[tokio::test]
async fn the_count_off_anthropic_leaves_no_file_image_out() {
    let mock = MockServer::start().await;
    let gw = world(&mock.uri(), Protocol::OpenaiCompatible).await;
    let ask =
        |content: Value| json!({"model": "m", "messages": [{"role": "user", "content": content}]});
    let files = ask(json!([image(), {"type": "text", "text": "What is it?"}]));
    let resp = post(&gw, "/v1/messages/count_tokens", &files).await;
    assert_eq!(resp.status(), 200);
    let approx = header(&resp, "x-lmgw-count-approximate");
    assert!(!approx.contains("media_omitted"), "{approx}");

    let inline = ask(json!([
        {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                     "data": "iVBORw0KGgo="}},
        {"type": "text", "text": "What is it?"},
    ]));
    let resp = post(&gw, "/v1/messages/count_tokens", &inline).await;
    assert_eq!(resp.status(), 200);
    let approx = header(&resp, "x-lmgw-count-approximate");
    assert!(approx.contains("media_omitted"), "{approx}");
}
