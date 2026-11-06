//! An Anthropic upstream: the requests that ask for compaction go out as
//! written, the `compaction` block comes back as Anthropic wrote it, whole
//! and streamed, and goes back to Anthropic as the client sent it.

use lmgw_core::config::Protocol;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{block, events, post_with, sse, ON_DEMAND, THRESHOLD};
use crate::messages_context_management::{bodies_at, world};

/// On-demand compaction's answer: the block alone, `stop_reason`
/// `compaction`, zero top-level usage and the compaction's iteration.
fn on_demand_answer() -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
        "content": [block()],
        "stop_reason": "compaction", "stop_sequence": null,
        "usage": {
            "input_tokens": 0, "output_tokens": 0,
            "iterations": [{"type": "compaction", "input_tokens": 144, "output_tokens": 276}],
        },
    })
}

fn history() -> Value {
    json!([
        {"role": "user", "content": "Name the entities of a recipe app."},
        {"role": "assistant", "content": "Recipe, Ingredient, Step."},
        {"role": "user", "content": "Now the fields of Recipe."},
    ])
}

#[tokio::test]
async fn on_demand_compaction_goes_out_and_its_block_comes_back_whole() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(on_demand_answer()))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;

    let body = json!({
        "model": "m", "max_tokens": 4096, "messages": history(),
        "compaction": {"type": "summarize", "instructions": "Keep every entity name."},
    });
    let resp = post_with(&gw, "/v1/messages", ON_DEMAND, &body).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-lmgw-params-ignored").is_none());
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(answer["content"], json!([block()]), "{answer}");
    assert_eq!(answer["stop_reason"], "compaction");
    // Anthropic's own top-level counts, and its iterations as it sent them.
    assert_eq!(answer["usage"], on_demand_answer()["usage"], "{answer}");

    let sent = bodies_at(&mock, "/v1/messages").await;
    assert_eq!(sent[0]["compaction"], body["compaction"]);
    let received = mock.received_requests().await.unwrap();
    assert_eq!(received[0].headers["anthropic-beta"], ON_DEMAND);
}

/// The next turn sends the block first, in place of what it summarizes; it
/// reaches Anthropic exactly as the client sent it (a `cache_control` the
/// client put on it included), the turns after it unchanged.
#[tokio::test]
async fn a_block_sent_back_reaches_anthropic_as_the_client_sent_it() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_2", "type": "message", "role": "assistant", "model": "tgt",
            "content": [{"type": "text", "text": "title, servings"}],
            "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 80, "output_tokens": 4, "iterations": [
                {"type": "message", "input_tokens": 80, "output_tokens": 4}]},
        })))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;

    let mut sent_block = block();
    sent_block["cache_control"] = json!({"type": "ephemeral"});
    let body = json!({
        "model": "m", "max_tokens": 256,
        "messages": [
            {"role": "assistant", "content": [sent_block]},
            {"role": "user", "content": "Now the fields of Ingredient."},
        ],
    });
    let resp = post_with(&gw, "/v1/messages", ON_DEMAND, &body).await;
    assert_eq!(resp.status(), 200);
    let answer: Value = resp.json().await.unwrap();
    // No compaction in this answer: the iterations still come back.
    assert_eq!(answer["usage"]["iterations"][0]["type"], "message");
    assert_eq!(answer["usage"]["input_tokens"], 80);

    let sent = bodies_at(&mock, "/v1/messages").await;
    assert_eq!(sent[0]["messages"][0], body["messages"][0], "{}", sent[0]);
    assert_eq!(
        sent[0]["messages"][1]["content"],
        json!([{"type": "text", "text": "Now the fields of Ingredient."}])
    );
}

/// Streamed, an on-demand block arrives whole in its `content_block_start`
/// and has no delta; the Messages stream relays the same events, and the
/// closing `message_delta` carries `stop_reason` `compaction` and the
/// iterations.
#[tokio::test]
async fn a_streamed_on_demand_block_is_relayed_as_it_came() {
    let usage = on_demand_answer()["usage"].clone();
    let upstream = sse(&[
        json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 0, "output_tokens": 0}}}),
        json!({"type": "ping"}),
        json!({"type": "content_block_start", "index": 0, "content_block": block()}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta",
               "delta": {"stop_reason": "compaction", "stop_sequence": null},
               "usage": usage}),
        json!({"type": "message_stop"}),
    ]);
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(upstream, "text/event-stream"))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;

    let body = json!({
        "model": "m", "max_tokens": 4096, "stream": true, "messages": history(),
        "compaction": {"type": "summarize"},
    });
    let resp = post_with(&gw, "/v1/messages", ON_DEMAND, &body).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let evs = events(&text);
    let blocks: Vec<&Value> = evs
        .iter()
        .filter(|e| {
            e["type"]
                .as_str()
                .is_some_and(|t| t.starts_with("content_block"))
        })
        .collect();
    assert_eq!(
        blocks,
        [
            &json!({"type": "content_block_start", "index": 0, "content_block": block()}),
            &json!({"type": "content_block_stop", "index": 0}),
        ],
        "{text}"
    );
    let delta = evs.iter().find(|e| e["type"] == "message_delta").unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "compaction", "{text}");
    assert_eq!(delta["usage"]["iterations"], usage["iterations"], "{text}");
    assert_eq!(delta["usage"]["input_tokens"], 0, "{text}");
    assert_eq!(delta["usage"]["output_tokens"], 0, "{text}");
}

/// Threshold compaction: the edit goes out as written; the block's
/// `content_block_start` is followed by one `compaction_delta` with the
/// whole summary (and its `encrypted_content`), relayed as they came, and
/// the answer goes on with its text.
#[tokio::test]
async fn a_compact_edit_reaches_anthropic_and_its_streamed_block_comes_back() {
    let start = json!({"type": "compaction", "content": null});
    let delta = json!({"type": "compaction_delta", "content": "Summary so far.",
                       "encrypted_content": "opaque=="});
    let iterations = json!([
        {"type": "compaction", "input_tokens": 180000, "output_tokens": 3500},
        {"type": "message", "input_tokens": 23000, "output_tokens": 1000},
    ]);
    let upstream = sse(&[
        json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 23000, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": start}),
        json!({"type": "content_block_delta", "index": 0, "delta": delta}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 1,
               "delta": {"type": "text_delta", "text": "Go on."}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta",
               "delta": {"stop_reason": "end_turn", "stop_sequence": null},
               "usage": {"input_tokens": 23000, "output_tokens": 1000,
                         "iterations": iterations}}),
        json!({"type": "message_stop"}),
    ]);
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(upstream, "text/event-stream"))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;

    let cm = json!({"edits": [{"type": "compact_20260112",
                               "trigger": {"type": "input_tokens", "value": 150000}}]});
    let body = json!({"model": "m", "max_tokens": 4096, "stream": true,
                      "messages": history(), "context_management": cm});
    let resp = post_with(&gw, "/v1/messages", THRESHOLD, &body).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let evs = events(&text);
    let at0: Vec<&Value> = evs.iter().filter(|e| e["index"] == 0).collect();
    assert_eq!(
        at0,
        [
            &json!({"type": "content_block_start", "index": 0, "content_block": start}),
            &json!({"type": "content_block_delta", "index": 0, "delta": delta}),
            &json!({"type": "content_block_stop", "index": 0}),
        ],
        "{text}"
    );
    assert!(text.contains("Go on."), "{text}");
    let md = evs.iter().find(|e| e["type"] == "message_delta").unwrap();
    // Anthropic's top-level counts (the compaction left out), its
    // iterations beside them.
    assert_eq!(md["usage"]["input_tokens"], 23000, "{text}");
    assert_eq!(md["usage"]["output_tokens"], 1000, "{text}");
    assert_eq!(md["usage"]["iterations"], iterations, "{text}");

    let sent = bodies_at(&mock, "/v1/messages").await;
    assert_eq!(sent[0]["context_management"], cm);
    let received = mock.received_requests().await.unwrap();
    assert_eq!(received[0].headers["anthropic-beta"], THRESHOLD);
}

/// A threshold compaction paused after its summary
/// (`pause_after_compaction`): `stop_reason` `compaction` and the block,
/// whole, before any text.
#[tokio::test]
async fn a_paused_compaction_answers_its_stop_reason() {
    let answer = json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
        "content": [{"type": "compaction", "content": "Summary so far.",
                     "encrypted_content": "opaque=="}],
        "stop_reason": "compaction", "stop_sequence": null,
        "usage": {"input_tokens": 0, "output_tokens": 0,
                  "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0,
                  "iterations": [
            {"type": "compaction", "input_tokens": 51000, "output_tokens": 900,
             "cache_read_input_tokens": 40000, "cache_creation_input_tokens": 0}]},
    });
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer.clone()))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;

    let cm = json!({"edits": [{"type": "compact_20260112", "pause_after_compaction": true,
                               "trigger": {"type": "input_tokens", "value": 50000}}]});
    let body = json!({"model": "m", "max_tokens": 4096, "messages": history(),
                      "context_management": cm});
    let resp = post_with(&gw, "/v1/messages", THRESHOLD, &body).await;
    assert_eq!(resp.status(), 200);
    let got: Value = resp.json().await.unwrap();
    assert_eq!(got["stop_reason"], "compaction");
    assert_eq!(got["content"], answer["content"]);
    // Anthropic's own usage, the compaction's cache read taken out of the
    // top level again.
    assert_eq!(got["usage"], answer["usage"], "{got}");
    assert_eq!(
        bodies_at(&mock, "/v1/messages").await[0]["context_management"],
        cm
    );
}

/// A top level without cache counters keeps its field set on the Messages
/// wire, though the compaction's iteration has them (and the IR's usage
/// counts them in), whole and streamed.
#[tokio::test]
async fn a_top_level_without_cache_counters_stays_without_them() {
    let iterations = json!([
        {"type": "compaction", "input_tokens": 51000, "output_tokens": 900,
         "cache_read_input_tokens": 40000, "cache_creation_input_tokens": 0},
        {"type": "message", "input_tokens": 700, "output_tokens": 20},
    ]);
    let answer = json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
        "content": [{"type": "compaction", "content": "Summary so far."},
                    {"type": "text", "text": "Go on."}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 700, "output_tokens": 20, "iterations": iterations},
    });
    let streamed = sse(&[
        json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 700, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": "Go on."}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta",
               "delta": {"stop_reason": "end_turn", "stop_sequence": null},
               "usage": {"output_tokens": 20, "iterations": iterations}}),
        json!({"type": "message_stop"}),
    ]);
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(wiremock::matchers::body_partial_json(
            json!({"stream": true}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(streamed, "text/event-stream"))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer.clone()))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;

    let cm = json!({"edits": [{"type": "compact_20260112"}]});
    let body = json!({"model": "m", "max_tokens": 4096, "messages": history(),
                      "context_management": cm});
    let got: Value = post_with(&gw, "/v1/messages", THRESHOLD, &body)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(got["usage"], answer["usage"], "{got}");

    let mut body = body;
    body["stream"] = json!(true);
    let text = post_with(&gw, "/v1/messages", THRESHOLD, &body)
        .await
        .text()
        .await
        .unwrap();
    for e in events(&text) {
        let usage = match e["type"].as_str() {
            Some("message_start") => &e["message"]["usage"],
            Some("message_delta") => &e["usage"],
            _ => continue,
        };
        for k in ["cache_read_input_tokens", "cache_creation_input_tokens"] {
            assert!(usage.get(k).is_none(), "{k} in {text}");
        }
    }
    let md = events(&text)
        .into_iter()
        .find(|e| e["type"] == "message_delta")
        .unwrap();
    assert_eq!(md["usage"]["iterations"], iterations, "{text}");
}

/// `/v1/messages/count_tokens` on an Anthropic route passes the client's
/// body through, the block in it untouched.
#[tokio::test]
async fn a_count_passes_the_block_through() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 31})))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;
    let body = json!({"model": "m", "messages": [
        {"role": "assistant", "content": [block()]},
        {"role": "user", "content": "Go on."},
    ]});
    let resp = post_with(&gw, "/v1/messages/count_tokens", ON_DEMAND, &body).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({"input_tokens": 31})
    );
    let sent = bodies_at(&mock, "/v1/messages/count_tokens").await;
    assert_eq!(sent[0]["messages"], body["messages"]);
}
