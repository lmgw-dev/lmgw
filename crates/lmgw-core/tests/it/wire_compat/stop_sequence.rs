//! A stop at a stop sequence (§9 finding 5): the native Anthropic leg names
//! the sequence, and `/v1/messages` says `stop_reason: "stop_sequence"`
//! with it, streamed or not; Chat Completions says `stop`.

use lmgw_core::config::Protocol;
use serde_json::json;
use wiremock::MockServer;

use super::*;

async fn upstream() -> MockServer {
    let mock = MockServer::start().await;
    let sse = anthropic_sse(&[
        json!({"type": "message_start", "message": {"id": "msg_1", "type": "message",
            "role": "assistant", "model": "tgt-model", "content": [],
            "usage": {"input_tokens": 5, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "1, 2, 3, 4, "}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "stop_sequence",
            "stop_sequence": "5"}, "usage": {"output_tokens": 9}}),
        json!({"type": "message_stop"}),
    ]);
    answers(
        &mock,
        Protocol::Anthropic,
        sse,
        json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "tgt-model",
            "content": [{"type": "text", "text": "1, 2, 3, 4, "}],
            "stop_reason": "stop_sequence", "stop_sequence": "5",
            "usage": {"input_tokens": 5, "output_tokens": 9}}),
    )
    .await;
    mock
}

fn ask(stream: bool) -> serde_json::Value {
    json!({"model": "my-model", "max_tokens": 64, "stream": stream,
        "stop_sequences": ["5"],
        "messages": [{"role": "user", "content": "count to ten"}]})
}

#[tokio::test]
async fn messages_names_the_stop_sequence_the_upstream_named() {
    let mock = upstream().await;
    let (_state, gw) = gateway(&mock, Protocol::Anthropic).await;
    let (status, _, text) = post(&gw, "/v1/messages", &ask(false)).await;
    assert_eq!(status, 200, "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["stop_reason"], "stop_sequence", "{text}");
    assert_eq!(v["stop_sequence"], "5");

    let (status, _, text) = post(&gw, "/v1/messages", &ask(true)).await;
    assert_eq!(status, 200, "{text}");
    let delta = events(&text)
        .into_iter()
        .find(|(_, e)| e["type"] == "message_delta")
        .expect("a message_delta")
        .1;
    assert_eq!(delta["delta"]["stop_reason"], "stop_sequence", "{text}");
    assert_eq!(delta["delta"]["stop_sequence"], "5");
}

#[tokio::test]
async fn chat_completions_reads_a_stop_sequence_end_as_stop() {
    let mock = upstream().await;
    let (_state, gw) = gateway(&mock, Protocol::Anthropic).await;
    let body = json!({"model": "my-model", "max_tokens": 64, "stop": ["5"],
        "messages": [{"role": "user", "content": "count to ten"}]});
    let (status, _, text) = post(&gw, "/v1/chat/completions", &body).await;
    assert_eq!(status, 200, "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "stop", "{text}");

    let mut streamed = body.clone();
    streamed["stream"] = json!(true);
    let (_, _, text) = post(&gw, "/v1/chat/completions", &streamed).await;
    let finish: Vec<String> = events(&text)
        .iter()
        .filter_map(|(_, c)| c["choices"][0]["finish_reason"].as_str().map(String::from))
        .collect();
    assert_eq!(finish, ["stop"], "{text}");
}

/// An upstream that does not say which sequence matched stays `end_turn`:
/// nothing is guessed.
#[tokio::test]
async fn an_upstream_that_names_no_sequence_stays_end_turn() {
    let mock = MockServer::start().await;
    answers(
        &mock,
        Protocol::OpenaiCompatible,
        openai_sse(&[]),
        json!({"id": "c1", "object": "chat.completion", "model": "tgt-model",
            "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant", "content": "1, 2, 3, 4, "}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 9}}),
    )
    .await;
    let (_state, gw) = gateway(&mock, Protocol::OpenaiCompatible).await;
    let (status, _, text) = post(&gw, "/v1/messages", &ask(false)).await;
    assert_eq!(status, 200, "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["stop_reason"], "end_turn", "{text}");
    assert_eq!(v["stop_sequence"], serde_json::Value::Null);
}
