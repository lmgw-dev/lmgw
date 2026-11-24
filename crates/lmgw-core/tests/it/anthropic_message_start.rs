//! A `/v1/messages` stream's `message_start` (gateway design §6.2): on an
//! Anthropic upstream it carries the upstream's own input counts (the
//! cache counters and their split included), held back until the upstream's
//! `message_start` came; on every other upstream it goes out first, with
//! zeros, as before. The frame order is Anthropic's either way.

use lmgw_core::config::Protocol;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::anthropic_prompt_caching::{anthropic_stream, events};
use crate::messages_context_management::world;

async fn stream(gw: &crate::common::Gw) -> String {
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "m", "max_tokens": 16, "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.text().await.unwrap()
}

fn types(evs: &[Value]) -> Vec<&str> {
    evs.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

const ORDER: [&str; 7] = [
    "message_start",
    "ping",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "message_delta",
    "message_stop",
];

#[tokio::test]
async fn an_anthropic_upstream_s_input_counts_open_the_stream() {
    let start = json!({
        "input_tokens": 12,
        "cache_creation_input_tokens": 410,
        "cache_read_input_tokens": 7480,
        "cache_creation": {"ephemeral_5m_input_tokens": 110, "ephemeral_1h_input_tokens": 300},
        "output_tokens": 1,
    });
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            anthropic_stream(&start, &json!({"output_tokens": 5})),
            "text/event-stream",
        ))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;
    let text = stream(&gw).await;
    let evs = events(&text);
    assert_eq!(types(&evs), ORDER, "{text}");
    assert_eq!(evs[0]["message"]["usage"], start, "{text}");
    // The closing usage has the totals, as before.
    assert_eq!(evs[5]["usage"]["output_tokens"], 5, "{text}");
    assert_eq!(evs[5]["usage"]["cache_read_input_tokens"], 7480, "{text}");
}

#[tokio::test]
async fn another_upstream_s_stream_opens_at_once_with_zeros() {
    let chunks = [
        json!({"id": "c", "object": "chat.completion.chunk", "model": "tgt",
               "choices": [{"index": 0, "delta": {"role": "assistant", "content": "A cat."}}]}),
        json!({"id": "c", "object": "chat.completion.chunk", "model": "tgt",
               "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        json!({"id": "c", "object": "chat.completion.chunk", "model": "tgt", "choices": [],
               "usage": {"prompt_tokens": 9, "completion_tokens": 2}}),
    ]
    .iter()
    .map(|c| format!("data: {c}\n\n"))
    .collect::<String>()
        + "data: [DONE]\n\n";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(chunks, "text/event-stream"))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::OpenaiCompatible).await;
    let text = stream(&gw).await;
    let evs = events(&text);
    assert_eq!(types(&evs), ORDER, "{text}");
    assert_eq!(
        evs[0]["message"]["usage"],
        json!({"input_tokens": 0, "output_tokens": 0}),
        "{text}"
    );
    assert_eq!(evs[5]["usage"]["input_tokens"], 9, "{text}");
}
