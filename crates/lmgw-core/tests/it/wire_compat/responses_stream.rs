//! Streamed `/v1/responses` as OpenAI streams it (§9 findings 2 and 4): a
//! function call's deltas join to exactly its `.done` arguments, which the
//! item and the final response carry too; a run the output cap cut off is
//! `incomplete` with the reason `max_output_tokens`.

use lmgw_core::config::Protocol;
use serde_json::{json, Value};
use wiremock::MockServer;

use super::*;

fn chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "tgt-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
}

fn tools() -> Value {
    json!([{"type": "function", "name": "weather",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}])
}

/// The upstream's own argument text, spacing and all, in two fragments.
const ARGS: [&str; 2] = ["{\"city\": ", "\"Paris\"}"];

async fn calling_upstream() -> MockServer {
    let mock = MockServer::start().await;
    let sse = openai_sse(&[
        chunk(
            json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1",
            "type": "function", "function": {"name": "weather", "arguments": ""}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": ARGS[0]}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": ARGS[1]}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ]);
    answers(
        &mock,
        Protocol::OpenaiCompatible,
        sse,
        json!({"id": "c1", "object": "chat.completion", "model": "tgt-model",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null, "tool_calls": [{"id": "call_1",
                "type": "function", "function": {"name": "weather",
                "arguments": ARGS.concat()}}]}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 7}}),
    )
    .await;
    mock
}

fn find<'a>(evs: &'a [(Option<String>, Value)], ty: &str) -> &'a Value {
    &evs.iter()
        .find(|(_, e)| e["type"] == ty)
        .unwrap_or_else(|| panic!("no {ty}"))
        .1
}

#[tokio::test]
async fn the_deltas_join_to_the_done_arguments_which_item_and_response_carry() {
    let mock = calling_upstream().await;
    let (_state, gw) = gateway(&mock, Protocol::OpenaiCompatible).await;
    let (status, _, text) = post(
        &gw,
        "/v1/responses",
        &json!({"model": "my-model", "stream": true, "input": "weather in Paris?",
            "tools": tools()}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let evs = events(&text);
    let added = find(&evs, "response.output_item.added");
    assert_eq!(added["item"]["arguments"], "", "{text}");
    let deltas: String = evs
        .iter()
        .filter(|(_, e)| e["type"] == "response.function_call_arguments.delta")
        .filter_map(|(_, e)| e["delta"].as_str())
        .collect();
    let whole = ARGS.concat();
    assert_eq!(deltas, whole, "the upstream's text, verbatim");
    assert_eq!(
        find(&evs, "response.function_call_arguments.done")["arguments"],
        whole
    );
    assert_eq!(
        find(&evs, "response.output_item.done")["item"]["arguments"],
        whole
    );
    let done = find(&evs, "response.completed");
    assert_eq!(done["response"]["output"][0]["arguments"], whole);
}

/// A unary turn streams no fragments: its arguments go out as one delta, so
/// a client joining deltas still has them.
#[tokio::test]
async fn an_unstreamed_call_is_one_delta_and_the_same_done() {
    let mock = calling_upstream().await;
    let (_state, gw) = gateway(&mock, Protocol::OpenaiCompatible).await;
    let (status, _, text) = post(
        &gw,
        "/v1/responses",
        &json!({"model": "my-model", "input": "weather in Paris?", "tools": tools()}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    let args = v["output"][0]["arguments"].as_str().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(args).unwrap(),
        json!({"city": "Paris"})
    );
}

async fn capped_upstream() -> MockServer {
    let mock = MockServer::start().await;
    let sse = openai_sse(&[
        chunk(json!({"role": "assistant", "content": "Once upon"}), None),
        chunk(json!({}), Some("length")),
    ]);
    answers(
        &mock,
        Protocol::OpenaiCompatible,
        sse,
        json!({"id": "c1", "object": "chat.completion", "model": "tgt-model",
            "choices": [{"index": 0, "finish_reason": "length",
                "message": {"role": "assistant", "content": "Once upon"}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 16}}),
    )
    .await;
    mock
}

#[tokio::test]
async fn a_run_the_output_cap_cut_off_is_incomplete() {
    let mock = capped_upstream().await;
    let (_state, gw) = gateway(&mock, Protocol::OpenaiCompatible).await;
    let body = json!({"model": "my-model", "input": "a story", "max_output_tokens": 16});
    let (status, _, text) = post(&gw, "/v1/responses", &body).await;
    assert_eq!(status, 200, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["status"], "incomplete", "{text}");
    assert_eq!(v["incomplete_details"]["reason"], "max_output_tokens");
    assert_eq!(v["output"][0]["content"][0]["text"], "Once upon");

    let mut streamed = body.clone();
    streamed["stream"] = json!(true);
    let (status, _, text) = post(&gw, "/v1/responses", &streamed).await;
    assert_eq!(status, 200, "{text}");
    let evs = events(&text);
    let last = &evs.last().expect("events").1;
    assert_eq!(last["type"], "response.incomplete", "{text}");
    assert_eq!(last["response"]["status"], "incomplete");
    assert_eq!(
        last["response"]["incomplete_details"]["reason"],
        "max_output_tokens"
    );
    assert!(
        !evs.iter().any(|(_, e)| e["type"] == "response.completed"),
        "{text}"
    );
    // The message it cut off is not a completed one, in either shape.
    assert_eq!(v["output"][0]["status"], "incomplete", "{text}");
    let done = evs
        .iter()
        .find(|(_, e)| e["type"] == "response.output_item.done" && e["item"]["type"] == "message")
        .expect("the message item is done");
    assert_eq!(done.1["item"]["status"], "incomplete", "{text}");
    assert_eq!(last["response"]["output"][0]["status"], "incomplete");
}

/// A call whose arguments are not JSON keeps the model's text: one unstreamed
/// turn's arguments are that text as it came, not a JSON string around it.
#[tokio::test]
async fn an_unparseable_call_keeps_the_models_text_verbatim() {
    let mock = MockServer::start().await;
    answers(
        &mock,
        Protocol::OpenaiCompatible,
        String::new(),
        json!({"id": "c1", "object": "chat.completion", "model": "tgt-model",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null, "tool_calls": [{"id": "call_1",
                "type": "function", "function": {"name": "weather",
                "arguments": "city = Paris"}}]}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 7}}),
    )
    .await;
    let (_state, gw) = gateway(&mock, Protocol::OpenaiCompatible).await;
    let (status, _, text) = post(
        &gw,
        "/v1/responses",
        &json!({"model": "my-model", "input": "weather?", "tools": tools()}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["output"][0]["arguments"], "city = Paris", "{text}");
}

/// A call held for approval is sent whole, as OpenAI sends an
/// `mcp_approval_request`: its arguments in `added` and in `done`, and no
/// argument events between.
#[tokio::test]
async fn a_gated_call_is_added_and_done_whole_without_argument_events() {
    let mock = MockServer::start().await;
    let sse = openai_sse(&[
        chunk(
            json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1",
            "type": "function", "function": {"name": "tools__echo", "arguments": ""}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"text\":"}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"hi\"}"}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ]);
    answers(&mock, Protocol::OpenaiCompatible, sse, json!({})).await;
    let (state, base) = crate::responses_api::setup(&mock.uri()).await;
    let (mcp_url, _calls) = crate::responses_api::mcp_stub().await;
    crate::responses_api::register_mcp(&state, "stub", "tools", &mcp_url).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .json(
            &json!({"model": "my-model", "stream": true, "input": "echo hi",
            "tools": [{"type": "mcp", "server_label": "tools", "require_approval": "always"}]}),
        )
        .send()
        .await
        .unwrap();
    let text = resp.text().await.unwrap();
    let evs = events(&text);
    let kinds: Vec<&str> = evs.iter().filter_map(|(_, e)| e["type"].as_str()).collect();
    assert!(
        !kinds
            .iter()
            .any(|t| t.contains("arguments") || t.contains("approval")),
        "no argument or approval-state events for an approval request: {text}"
    );
    let is_request = |e: &Value| e["item"]["type"] == "mcp_approval_request";
    let added = evs
        .iter()
        .find(|(_, e)| e["type"] == "response.output_item.added" && is_request(e))
        .unwrap_or_else(|| panic!("no added: {text}"));
    assert_eq!(added.1["item"]["arguments"], "{\"text\":\"hi\"}", "{text}");
    let done = evs
        .iter()
        .find(|(_, e)| e["type"] == "response.output_item.done" && is_request(e))
        .unwrap_or_else(|| panic!("no done: {text}"));
    assert_eq!(done.1["item"]["arguments"], "{\"text\":\"hi\"}", "{text}");
}
