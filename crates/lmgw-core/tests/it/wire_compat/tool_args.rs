//! A streamed tool call without arguments (§9 finding 1): Anthropic streams
//! no `input_json_delta` for one, and every client wire must still hear
//! `{}` — OpenAI's own stream sends `"arguments": "{}"`.

use lmgw_core::config::Protocol;
use serde_json::{json, Value};
use wiremock::MockServer;

use super::*;

/// An Anthropic stream with one call to `now` and nothing for its input.
fn no_arg_call() -> String {
    anthropic_sse(&[
        json!({"type": "message_start", "message": {"id": "msg_1", "type": "message",
            "role": "assistant", "model": "tgt-model", "content": [],
            "usage": {"input_tokens": 5, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block":
            {"type": "tool_use", "id": "toolu_1", "name": "now", "input": {}}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use",
            "stop_sequence": null}, "usage": {"output_tokens": 3}}),
        json!({"type": "message_stop"}),
    ])
}

async fn upstream() -> MockServer {
    let mock = MockServer::start().await;
    answers(
        &mock,
        Protocol::Anthropic,
        no_arg_call(),
        json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "tgt-model",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "now", "input": {}}],
            "stop_reason": "tool_use", "stop_sequence": null,
            "usage": {"input_tokens": 5, "output_tokens": 3}}),
    )
    .await;
    mock
}

fn tool() -> Value {
    json!({"type": "function", "function": {"name": "now", "description": "The time.",
        "parameters": {"type": "object", "properties": {}}}})
}

#[tokio::test]
async fn chat_streams_a_no_argument_call_as_empty_object_arguments() {
    let mock = upstream().await;
    let (_state, gw) = gateway(&mock, Protocol::Anthropic).await;
    let (status, _, text) = post(
        &gw,
        "/v1/chat/completions",
        &json!({"model": "my-model", "stream": true, "tools": [tool()],
            "messages": [{"role": "user", "content": "what time is it?"}]}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let args: String = events(&text)
        .iter()
        .filter_map(|(_, c)| {
            c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
        })
        .collect();
    assert_eq!(args, "{}", "{text}");
    let finish: Vec<String> = events(&text)
        .iter()
        .filter_map(|(_, c)| c["choices"][0]["finish_reason"].as_str().map(String::from))
        .collect();
    assert_eq!(finish, ["tool_calls"], "{text}");
}

#[tokio::test]
async fn messages_streams_a_no_argument_call_as_an_empty_object_input() {
    let mock = upstream().await;
    let (_state, gw) = gateway(&mock, Protocol::Anthropic).await;
    let (status, _, text) = post(
        &gw,
        "/v1/messages",
        &json!({"model": "my-model", "stream": true, "max_tokens": 64,
            "tools": [{"name": "now", "input_schema": {"type": "object", "properties": {}}}],
            "messages": [{"role": "user", "content": "what time is it?"}]}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let input: String = events(&text)
        .iter()
        .filter(|(_, e)| e["delta"]["type"] == "input_json_delta")
        .filter_map(|(_, e)| e["delta"]["partial_json"].as_str())
        .collect();
    let parsed: Value = serde_json::from_str(&input).expect("the input is JSON");
    assert_eq!(parsed, json!({}), "{text}");
}

#[tokio::test]
async fn responses_stream_a_no_argument_call_as_empty_object_arguments() {
    let mock = upstream().await;
    let (_state, gw) = gateway(&mock, Protocol::Anthropic).await;
    let (status, _, text) = post(
        &gw,
        "/v1/responses",
        &json!({"model": "my-model", "stream": true, "input": "what time is it?",
            "tools": [{"type": "function", "name": "now",
                "parameters": {"type": "object", "properties": {}}}]}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let evs = events(&text);
    let added = evs
        .iter()
        .find(|(_, e)| {
            e["type"] == "response.output_item.added" && e["item"]["type"] == "function_call"
        })
        .expect("the call's item was added");
    assert_eq!(added.1["item"]["arguments"], "", "{text}");
    let deltas: String = evs
        .iter()
        .filter(|(_, e)| e["type"] == "response.function_call_arguments.delta")
        .filter_map(|(_, e)| e["delta"].as_str())
        .collect();
    let done = evs
        .iter()
        .find(|(_, e)| e["type"] == "response.function_call_arguments.done")
        .expect("arguments done");
    assert_eq!(deltas, "{}", "{text}");
    assert_eq!(done.1["arguments"], "{}");
    let item_done = evs
        .iter()
        .find(|(_, e)| {
            e["type"] == "response.output_item.done" && e["item"]["type"] == "function_call"
        })
        .expect("the call's item is done");
    assert_eq!(item_done.1["item"]["arguments"], "{}");
}

/// An Anthropic stream with call 0 (`now`, no input) ahead of `rest`.
fn after_no_arg_call(rest: Vec<Value>) -> String {
    let mut evs = vec![
        json!({"type": "message_start", "message": {"id": "msg_1", "type": "message",
            "role": "assistant", "model": "tgt-model", "content": [],
            "usage": {"input_tokens": 5, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block":
            {"type": "tool_use", "id": "toolu_1", "name": "now", "input": {}}}),
        json!({"type": "content_block_stop", "index": 0}),
    ];
    evs.extend(rest);
    evs.push(
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use",
        "stop_sequence": null}, "usage": {"output_tokens": 3}}),
    );
    evs.push(json!({"type": "message_stop"}));
    anthropic_sse(&evs)
}

fn second_call_with_args() -> Vec<Value> {
    vec![
        json!({"type": "content_block_start", "index": 1, "content_block":
            {"type": "tool_use", "id": "toolu_2", "name": "weather", "input": {}}}),
        json!({"type": "content_block_delta", "index": 1, "delta":
            {"type": "input_json_delta", "partial_json": "{\"city\":"}}),
        json!({"type": "content_block_delta", "index": 1, "delta":
            {"type": "input_json_delta", "partial_json": "\"Paris\"}"}}),
        json!({"type": "content_block_stop", "index": 1}),
    ]
}

fn text_after_call() -> Vec<Value> {
    vec![
        json!({"type": "content_block_start", "index": 1, "content_block":
            {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 1, "delta":
            {"type": "text_delta", "text": "Checking."}}),
        json!({"type": "content_block_stop", "index": 1}),
    ]
}

async fn upstream_streaming(sse: String) -> MockServer {
    let mock = MockServer::start().await;
    answers(
        &mock,
        Protocol::Anthropic,
        sse,
        json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "tgt-model",
            "content": [], "stop_reason": "tool_use", "stop_sequence": null,
            "usage": {"input_tokens": 5, "output_tokens": 3}}),
    )
    .await;
    mock
}

/// Every delta of a Messages stream belongs to the block that is open at
/// that point; returns each block's assembled `input_json_delta` text.
fn assert_valid_block_order(text: &str) -> Vec<String> {
    let mut open: Option<u64> = None;
    let mut next = 0u64;
    let mut inputs: Vec<String> = Vec::new();
    for (_, e) in events(text) {
        match e["type"].as_str() {
            Some("content_block_start") => {
                assert_eq!(open, None, "a block opened inside another: {text}");
                assert_eq!(e["index"], next, "{text}");
                open = Some(next);
                next += 1;
                inputs.push(String::new());
            }
            Some("content_block_delta") => {
                assert_eq!(open, e["index"].as_u64(), "delta outside its block: {text}");
                if let Some(p) = e["delta"]["partial_json"].as_str() {
                    inputs.last_mut().unwrap().push_str(p);
                }
            }
            Some("content_block_stop") => {
                assert_eq!(open, e["index"].as_u64(), "{text}");
                open = None;
            }
            _ => {}
        }
    }
    assert_eq!(open, None, "{text}");
    inputs
}

fn messages_request() -> Value {
    json!({"model": "my-model", "stream": true, "max_tokens": 64,
        "tools": [{"name": "now", "input_schema": {"type": "object", "properties": {}}},
            {"name": "weather", "input_schema": {"type": "object", "properties": {}}}],
        "messages": [{"role": "user", "content": "go"}]})
}

#[tokio::test]
async fn messages_keeps_event_order_for_parallel_calls_with_the_first_without_arguments() {
    let mock = upstream_streaming(after_no_arg_call(second_call_with_args())).await;
    let (_state, gw) = gateway(&mock, Protocol::Anthropic).await;
    let (status, _, text) = post(&gw, "/v1/messages", &messages_request()).await;
    assert_eq!(status, 200, "{text}");
    let inputs = assert_valid_block_order(&text);
    assert_eq!(inputs.len(), 2, "{text}");
    assert!(inputs[0].is_empty() || inputs[0] == "{}", "{text}");
    let second: Value = serde_json::from_str(&inputs[1]).expect("JSON");
    assert_eq!(second, json!({"city": "Paris"}), "{text}");
}

#[tokio::test]
async fn messages_keeps_event_order_for_a_no_argument_call_followed_by_text() {
    let mock = upstream_streaming(after_no_arg_call(text_after_call())).await;
    let (_state, gw) = gateway(&mock, Protocol::Anthropic).await;
    let (status, _, text) = post(&gw, "/v1/messages", &messages_request()).await;
    assert_eq!(status, 200, "{text}");
    let inputs = assert_valid_block_order(&text);
    assert_eq!(inputs.len(), 2, "{text}");
    assert!(inputs[0].is_empty() || inputs[0] == "{}", "{text}");
}

#[tokio::test]
async fn chat_streams_parallel_calls_with_the_first_without_arguments_unmerged() {
    let mock = upstream_streaming(after_no_arg_call(second_call_with_args())).await;
    let (_state, gw) = gateway(&mock, Protocol::Anthropic).await;
    let mut req = messages_request();
    req["tools"] = json!([
        tool(),
        json!({"type": "function", "function": {"name": "weather",
        "parameters": {"type": "object", "properties": {}}}})
    ]);
    req.as_object_mut().unwrap().remove("max_tokens");
    let (status, _, text) = post(&gw, "/v1/chat/completions", &req).await;
    assert_eq!(status, 200, "{text}");
    let mut args = [String::new(), String::new()];
    for (_, c) in events(&text) {
        for call in c["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let i = call["index"].as_u64().unwrap() as usize;
            if let Some(a) = call["function"]["arguments"].as_str() {
                args[i].push_str(a);
            }
        }
    }
    assert_eq!(args[0], "{}", "{text}");
    assert_eq!(args[1], "{\"city\":\"Paris\"}", "{text}");
}
