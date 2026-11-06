//! Parameters a model refuses (the reasoning off's mechanic extended): a 400
//! in a known wording that names one parameter the send carried is retried
//! once without exactly that one, the refused attempt's row says so, the
//! answer names it in `x-lmgw-params-refused`, and later sends to the same
//! model leave it out. A model that takes the parameter keeps getting it.

use lmgw_core::config::Protocol;
use lmgw_core::state::SharedState;
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const ANTHROPIC_REFUSAL: &str = "`temperature` is deprecated for this model.";
const OPENAI_REFUSAL: &str = "Unsupported value: 'temperature' does not support 0 with this \
                              model. Only the default (1) value is supported.";

/// `mock` refuses a chat send of `protocol` that carries `temperature` with
/// `refusal`, and answers every other one.
async fn refusing_temperature(protocol: Protocol, refusal: Value) -> MockServer {
    let mock = MockServer::start().await;
    for temperature in [json!(0.5), json!(0.0)] {
        Mock::given(method("POST"))
            .and(path(chat_path(protocol)))
            .and(body_partial_json(json!({"temperature": temperature})))
            .respond_with(ResponseTemplate::new(400).set_body_json(refusal.clone()))
            .with_priority(1)
            .mount(&mock)
            .await;
    }
    let (sse, unary) = match protocol {
        Protocol::Anthropic => (
            anthropic_sse(&[
                json!({"type": "message_start", "message": {"id": "msg_1", "type": "message",
                    "role": "assistant", "model": "tgt-model", "content": [],
                    "usage": {"input_tokens": 5, "output_tokens": 0}}}),
                json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "OK"}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn",
                    "stop_sequence": null}, "usage": {"output_tokens": 1}}),
                json!({"type": "message_stop"}),
            ]),
            json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "tgt-model",
                "content": [{"type": "text", "text": "OK"}], "stop_reason": "end_turn",
                "stop_sequence": null, "usage": {"input_tokens": 5, "output_tokens": 1}}),
        ),
        _ => (
            openai_sse(&[
                json!({"id": "c1", "object": "chat.completion.chunk", "created": 1,
                    "model": "tgt-model", "choices": [{"index": 0,
                    "delta": {"role": "assistant", "content": "OK"}, "finish_reason": null}]}),
                json!({"id": "c1", "object": "chat.completion.chunk", "created": 1,
                    "model": "tgt-model", "choices": [{"index": 0, "delta": {},
                    "finish_reason": "stop"}]}),
            ]),
            json!({"id": "c1", "object": "chat.completion", "model": "tgt-model",
                "choices": [{"index": 0, "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "OK"}}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 1}}),
        ),
    };
    answers(&mock, protocol, sse, unary).await;
    mock
}

fn anthropic_refusal() -> Value {
    json!({"type": "error", "error": {"type": "invalid_request_error",
        "message": ANTHROPIC_REFUSAL}})
}

fn openai_refusal() -> Value {
    json!({"error": {"message": OPENAI_REFUSAL, "type": "invalid_request_error",
        "param": "temperature", "code": "unsupported_value"}})
}

/// The chat sends the mock was posted, in order.
async fn posted(mock: &MockServer, protocol: Protocol) -> Vec<Value> {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == chat_path(protocol))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// `(status, error_msg)` of every request row, oldest first.
async fn rows(state: &SharedState) -> Vec<(i64, Option<String>)> {
    sqlx::query_as("SELECT status, error_msg FROM request_logs ORDER BY id")
        .fetch_all(&state.db)
        .await
        .unwrap()
}

fn refused_header(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get("x-lmgw-params-refused")
        .map(|v| v.to_str().unwrap().to_string())
}

#[tokio::test]
async fn an_anthropic_refusal_of_temperature_is_retried_without_it_and_remembered() {
    let mock = refusing_temperature(Protocol::Anthropic, anthropic_refusal()).await;
    let (state, gw) = gateway(&mock, Protocol::Anthropic).await;
    let body = json!({"model": "my-model", "max_tokens": 64, "temperature": 0.5, "top_p": 0.9,
        "messages": [{"role": "user", "content": "Reply with OK."}]});
    let (status, headers, text) = post(&gw, "/v1/messages", &body).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(refused_header(&headers).as_deref(), Some("temperature"));
    let sent = posted(&mock, Protocol::Anthropic).await;
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert_eq!(sent[0]["temperature"], 0.5);
    assert!(sent[1].get("temperature").is_none(), "{}", sent[1]);
    assert_eq!(sent[1]["top_p"], 0.9, "only the named parameter goes");
    let r = rows(&state).await;
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(r[0].0, 400);
    let note = r[0].1.as_deref().unwrap();
    assert!(note.contains(ANTHROPIC_REFUSAL), "{note}");
    assert!(
        note.contains("the model refused temperature; lmgw retried once without it"),
        "{note}"
    );
    assert_eq!(r[1], (200, None));

    // Remembered: the next send leaves it out at once, and says so.
    let (status, headers, text) = post(
        &gw,
        "/v1/chat/completions",
        &json!({
        "model": "my-model", "temperature": 0.5,
        "messages": [{"role": "user", "content": "Reply with OK."}]}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(refused_header(&headers).as_deref(), Some("temperature"));
    let sent = posted(&mock, Protocol::Anthropic).await;
    assert_eq!(sent.len(), 3, "no second refusal: {sent:#?}");
    assert!(sent[2].get("temperature").is_none(), "{}", sent[2]);
}

#[tokio::test]
async fn an_openai_refusal_of_a_value_is_retried_on_a_stream_too() {
    let mock = refusing_temperature(Protocol::OpenaiCompatible, openai_refusal()).await;
    let (_state, gw) = gateway(&mock, Protocol::OpenaiCompatible).await;
    let (status, headers, text) = post(
        &gw,
        "/v1/chat/completions",
        &json!({
        "model": "my-model", "stream": true, "temperature": 0,
        "messages": [{"role": "user", "content": "Reply with OK."}]}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(refused_header(&headers).as_deref(), Some("temperature"));
    let content: String = events(&text)
        .iter()
        .filter_map(|(_, c)| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(content, "OK", "{text}");
    let sent = posted(&mock, Protocol::OpenaiCompatible).await;
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert!(sent[1].get("temperature").is_none(), "{}", sent[1]);
}

#[tokio::test]
async fn responses_left_out_a_refused_parameter_and_say_so() {
    let mock = refusing_temperature(Protocol::Anthropic, anthropic_refusal()).await;
    let (_state, gw) = gateway(&mock, Protocol::Anthropic).await;
    let body = json!({"model": "my-model", "input": "Reply with OK.", "temperature": 0.5});
    let (status, headers, text) = post(&gw, "/v1/responses", &body).await;
    assert_eq!(status, 200, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["status"], "completed", "{text}");
    assert_eq!(refused_header(&headers).as_deref(), Some("temperature"));
    let mut streamed = body.clone();
    streamed["stream"] = json!(true);
    let (status, headers, text) = post(&gw, "/v1/responses", &streamed).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(
        refused_header(&headers).as_deref(),
        Some("temperature"),
        "on record when the stream opened"
    );
    let sent = posted(&mock, Protocol::Anthropic).await;
    assert_eq!(sent.len(), 3, "{sent:#?}");
    assert!(sent[2].get("temperature").is_none(), "{}", sent[2]);
}

/// Nothing is left out without a refusal: a model that takes the parameter
/// gets it, and no header says otherwise; a refusal in wording lmgw does not
/// know stands.
#[tokio::test]
async fn a_model_that_takes_it_keeps_getting_it_and_an_unknown_wording_stands() {
    let mock = MockServer::start().await;
    answers(
        &mock,
        Protocol::OpenaiCompatible,
        openai_sse(&[]),
        json!({"id": "c1", "object": "chat.completion", "model": "tgt-model",
            "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant", "content": "OK"}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1}}),
    )
    .await;
    let (_state, gw) = gateway(&mock, Protocol::OpenaiCompatible).await;
    let body = json!({"model": "my-model", "temperature": 0.5,
        "messages": [{"role": "user", "content": "Reply with OK."}]});
    let (status, headers, _) = post(&gw, "/v1/chat/completions", &body).await;
    assert_eq!(status, 200);
    assert!(refused_header(&headers).is_none());
    assert_eq!(
        posted(&mock, Protocol::OpenaiCompatible).await[0]["temperature"],
        0.5
    );

    let unknown = refusing_temperature(
        Protocol::OpenaiCompatible,
        json!({"error": {"message": "temperature must be at most 0.4",
            "type": "invalid_request_error"}}),
    )
    .await;
    let (_state, gw) = gateway(&unknown, Protocol::OpenaiCompatible).await;
    let (status, headers, text) = post(&gw, "/v1/chat/completions", &body).await;
    assert_eq!(status, 400, "{text}");
    assert!(refused_header(&headers).is_none());
    assert_eq!(posted(&unknown, Protocol::OpenaiCompatible).await.len(), 1);
}
