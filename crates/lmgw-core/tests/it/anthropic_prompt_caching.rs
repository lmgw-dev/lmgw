//! A `/v1/messages` request's prompt-caching marks (gateway design §6.1,
//! "Prompt caching", 2026-11-12): an Anthropic upstream gets every
//! `cache_control` where the client wrote it (a `system` block's, a
//! tool's, a message block's, one nested in a tool result, the top-level
//! one) and the answer's cache counters come back as Anthropic reported
//! them, the `cache_creation` split by TTL included, whole and streamed.
//! Every other upstream gets none of them, and
//! `x-lmgw-params-ignored` names `cache_control` and a tool's other
//! Anthropic fields. Each body passes the level-1 schema check.

use lmgw_core::config::Protocol;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::messages_context_management::{bodies_at, world};

/// A coding client's turn: a marked system manual, a marked tool, a tool
/// loop whose result is marked inside and out, automatic caching on top.
fn marked() -> Value {
    let five = json!({"type": "ephemeral"});
    let hour = json!({"type": "ephemeral", "ttl": "1h"});
    json!({
        "model": "m",
        "max_tokens": 64,
        "cache_control": five,
        "system": [
            {"type": "text", "text": "You are a careful assistant."},
            {"type": "text", "text": "The manual, at length.", "cache_control": hour},
        ],
        "tools": [{
            "name": "lookup",
            "description": "Looks a word up.",
            "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}},
            "cache_control": five,
            "eager_input_streaming": true,
        }],
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "Look up cat.", "cache_control": five},
            ]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "cat"},
                 "cache_control": five},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "cache_control": five, "content": [
                    {"type": "text", "text": "a small animal", "cache_control": hour},
                ]},
            ]},
        ],
    })
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

fn ignored(resp: &reqwest::Response) -> String {
    resp.headers()
        .get("x-lmgw-params-ignored")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

#[tokio::test]
async fn an_anthropic_upstream_gets_every_mark_and_its_counters_come_back() {
    let mock = MockServer::start().await;
    let usage = json!({
        "input_tokens": 12,
        "cache_creation_input_tokens": 410,
        "cache_read_input_tokens": 7480,
        "cache_creation": {"ephemeral_5m_input_tokens": 110, "ephemeral_1h_input_tokens": 300},
        "output_tokens": 5,
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
            "content": [{"type": "text", "text": "A cat is a small animal."}],
            "stop_reason": "end_turn", "usage": usage,
        })))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;
    let client = marked();
    let resp = post(&gw, "/v1/messages", &client).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp), "");
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(answer["usage"], usage);

    let sent = bodies_at(&mock, "/v1/messages").await;
    let sent = &sent[0];
    assert_eq!(sent["cache_control"], client["cache_control"]);
    assert_eq!(sent["system"], client["system"]);
    assert_eq!(sent["tools"], client["tools"]);
    assert_eq!(sent["messages"], client["messages"]);
}

/// The SSE body of an Anthropic stream: `message_start` with `start`'s
/// usage, one text block, `message_delta` with `end`'s.
pub(crate) fn anthropic_stream(start: &Value, end: &Value) -> String {
    [
        json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
            "content": [], "stop_reason": null, "stop_sequence": null, "usage": start}}),
        json!({"type": "ping"}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": "A cat."}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta",
               "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": end}),
        json!({"type": "message_stop"}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect()
}

/// The `data` objects of an SSE body, in order.
pub(crate) fn events(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .collect()
}

/// Streamed, the counters come back on the closing `message_delta` as the
/// upstream's `message_start` reported them, also when the upstream's
/// `message_delta` repeats the input counts. The `cache_creation` object
/// stays on `message_start`: Anthropic's `message_delta` usage has none.
#[tokio::test]
async fn a_streamed_answer_s_cache_split_comes_back() {
    let start = json!({
        "input_tokens": 12,
        "cache_creation_input_tokens": 410,
        "cache_read_input_tokens": 7480,
        "cache_creation": {"ephemeral_5m_input_tokens": 110, "ephemeral_1h_input_tokens": 300},
        "output_tokens": 1,
    });
    let repeated = json!({
        "input_tokens": 12,
        "cache_creation_input_tokens": 410,
        "cache_read_input_tokens": 7480,
        "output_tokens": 5,
    });
    for end in [json!({"output_tokens": 5}), repeated] {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(anthropic_stream(&start, &end), "text/event-stream"),
            )
            .mount(&mock)
            .await;
        let gw = world(&mock.uri(), Protocol::Anthropic).await;
        let mut client = marked();
        client["stream"] = json!(true);
        let resp = post(&gw, "/v1/messages", &client).await;
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.unwrap();
        let delta = events(&text)
            .into_iter()
            .find(|e| e["type"] == "message_delta")
            .unwrap_or_else(|| panic!("{text}"));
        let mut expected = start.clone();
        expected["output_tokens"] = json!(5);
        expected.as_object_mut().unwrap().remove("cache_creation");
        assert_eq!(delta["usage"], expected, "{end}: {text}");
    }
}

#[tokio::test]
async fn another_upstream_gets_no_mark_and_the_response_names_them() {
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
    let resp = post(&gw, "/v1/messages", &marked()).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp), "cache_control,tools.eager_input_streaming");
    let sent = bodies_at(&mock, "/chat/completions").await;
    let text = sent[0].to_string();
    assert!(!text.contains("cache_control"), "{text}");
    assert!(!text.contains("eager_input_streaming"), "{text}");
    // The system blocks as one system text, a blank line between them.
    assert_eq!(
        sent[0]["messages"][0],
        json!({"role": "system",
               "content": "You are a careful assistant.\n\nThe manual, at length."})
    );
}

/// A mark is no prompt text: the count of a marked request off an
/// Anthropic route is the count of the same request unmarked.
#[tokio::test]
async fn a_mark_counts_as_no_tokens() {
    let mock = MockServer::start().await;
    let gw = world(&mock.uri(), Protocol::OpenaiCompatible).await;
    let count = |body: Value| {
        let gw = &gw;
        async move {
            let resp = post(gw, "/v1/messages/count_tokens", &body).await;
            assert_eq!(resp.status(), 200);
            let v: Value = resp.json().await.unwrap();
            v["input_tokens"].as_u64().unwrap()
        }
    };
    let mut unmarked = marked();
    fn strip(v: &mut Value) {
        match v {
            Value::Array(a) => a.iter_mut().for_each(strip),
            Value::Object(o) => {
                o.remove("cache_control");
                o.values_mut().for_each(strip);
            }
            _ => {}
        }
    }
    strip(&mut unmarked);
    unmarked["tools"][0]
        .as_object_mut()
        .unwrap()
        .remove("eager_input_streaming");
    let mut marked_body = marked();
    marked_body.as_object_mut().unwrap().remove("max_tokens");
    unmarked.as_object_mut().unwrap().remove("max_tokens");
    assert_eq!(count(marked_body).await, count(unmarked).await);
}

/// The request-log row's cost keeps the 1-hour write dearer: the split the
/// upstream reported reaches `price_call`, whole and streamed (a stream's
/// `message_delta` without the object keeps `message_start`'s split). At
/// 1.00 in, 0.10 read, 1.25 write and 10.00 out per 1M tokens (micro-units
/// are tokens x rate): 12 plain + 7480 read + 100 five-minute writes + 300
/// one-hour writes at twice the input rate.
#[tokio::test]
async fn a_one_hour_write_costs_twice_the_input_rate_in_the_row() {
    use lmgw_core::config::{PriceScope, PriceTier, PriceUnit, UpstreamKind};
    use lmgw_core::pricing::{PriceSource, Prices};

    let start = json!({
        "input_tokens": 12,
        "cache_creation_input_tokens": 400,
        "cache_read_input_tokens": 7480,
        "cache_creation": {"ephemeral_5m_input_tokens": 100, "ephemeral_1h_input_tokens": 300},
        "output_tokens": 1,
    });
    let mut whole = start.clone();
    whole["output_tokens"] = json!(5);
    let mut repeated = whole.clone();
    repeated.as_object_mut().unwrap().remove("cache_creation");

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            if body["stream"] == true {
                ResponseTemplate::new(200)
                    .set_body_raw(anthropic_stream(&start, &repeated), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn", "usage": whole,
                }))
            }
        })
        .mount(&mock)
        .await;
    let (state, gw) =
        crate::e2e_proxy::setup_kind(&mock.uri(), Protocol::Anthropic, UpstreamKind::Generic).await;
    lmgw_core::store::upsert_price(
        &state.db,
        PriceScope::Alias,
        "my-model",
        PriceUnit::PerMtok,
        PriceTier::Standard,
        &Prices {
            price_in: Some(1.0),
            price_out: Some(10.0),
            price_cache_read: Some(0.1),
            price_cache_write: Some(1.25),
            source: PriceSource::Manual,
        },
        None,
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    for (n, stream) in [false, true].into_iter().enumerate() {
        let mut body = json!({
            "model": "my-model", "max_tokens": 16, "stream": stream,
            "messages": [{"role": "user", "content": "hi"}],
        });
        body["stream"] = json!(stream);
        let resp = post(&gw, "/v1/messages", &body).await;
        assert_eq!(resp.status(), 200);
        let _ = resp.text().await.unwrap();
        let row = crate::stream_usage::row_after(&state, n).await;
        assert_eq!(row.cache_write_tokens, Some(400), "stream {stream}");
        // 12 + 7480 x 0.1 + 100 x 1.25 + 300 x 1.00 x 2
        assert_eq!(
            row.cost_in_micro,
            Some(12 + 748 + 125 + 600),
            "stream {stream}"
        );
        assert_eq!(row.cost_out_micro, Some(50), "stream {stream}");
    }
}
