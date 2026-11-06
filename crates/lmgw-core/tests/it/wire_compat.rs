//! The first live compatibility runs' findings (api-compat design §9), each
//! pinned against a wiremock upstream answering what the provider answered:
//! what a client of each wire reads where the upstream said less, or said
//! it another way.

mod errors;
mod input_tokens;
mod native_param_refusal;
mod native_reasoning_off;
mod param_refusal;
mod responses_stream;
mod stop_sequence;
mod tool_args;

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::SharedState;
use serde_json::Value;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::Gw;

/// A gateway with `my-model` on an upstream of `protocol` at `mock`.
async fn gateway(mock: &MockServer, protocol: Protocol) -> (SharedState, Gw) {
    crate::e2e_proxy::setup_kind(&mock.uri(), protocol, UpstreamKind::Generic).await
}

/// The path a chat send of `protocol` posts to.
fn chat_path(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Anthropic => "/v1/messages",
        _ => "/chat/completions",
    }
}

/// `mock` answers a streamed chat send of `protocol` with `sse` and an
/// unstreamed one with `unary`.
async fn answers(mock: &MockServer, protocol: Protocol, sse: String, unary: Value) {
    Mock::given(method("POST"))
        .and(path(chat_path(protocol)))
        .and(body_partial_json(serde_json::json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse, "text/event-stream"),
        )
        .with_priority(1)
        .mount(mock)
        .await;
    Mock::given(method("POST"))
        .and(path(chat_path(protocol)))
        .respond_with(ResponseTemplate::new(200).set_body_json(unary))
        .with_priority(2)
        .mount(mock)
        .await;
}

/// Anthropic SSE from `(event, data)` pairs.
fn anthropic_sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// OpenAI chat SSE from chunk bodies, `[DONE]` after them.
fn openai_sse(chunks: &[Value]) -> String {
    let mut out: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    out.push_str("data: [DONE]\n\n");
    out
}

/// A client's stream read back: each event's name (when it has one) and
/// JSON data; `[DONE]` and non-JSON data are left out.
fn events(text: &str) -> Vec<(Option<String>, Value)> {
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut name = None;
        let mut data = String::new();
        for line in block.lines() {
            if let Some(n) = line.strip_prefix("event: ") {
                name = Some(n.to_string());
            } else if let Some(d) = line.strip_prefix("data: ") {
                data.push_str(d);
            }
        }
        if let Ok(v) = serde_json::from_str::<Value>(&data) {
            out.push((name, v));
        }
    }
    out
}

/// POST `body` to `route` and read the answer's text.
async fn post(gw: &Gw, route: &str, body: &Value) -> (u16, reqwest::header::HeaderMap, String) {
    let resp = reqwest::Client::new()
        .post(format!("{gw}{route}"))
        .json(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    (status, headers, resp.text().await.unwrap())
}
