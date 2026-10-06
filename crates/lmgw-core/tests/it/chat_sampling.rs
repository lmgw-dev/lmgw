//! Chat sampling parameters (chat-complete design §2): kept with the thread,
//! sent only where the route takes them, and the refused ones reported.

use lmgw_core::config::{Protocol, UpstreamKind};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_actions::{gateway, get_json, mount_openai_reply, post, sse_events};
use crate::common::Gw;

fn all() -> Value {
    json!({
        "top_p": 0.9,
        "top_k": 40,
        "min_p": 0.05,
        "repeat_penalty": 1.1,
        "presence_penalty": 0.3,
        "frequency_penalty": -0.4,
        "seed": 7,
        "stop": ["END", "STOP"],
    })
}

async fn thread(gw: &Gw, extra: Value) -> i64 {
    let mut body = json!({"model_alias": "m"});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    post(gw, "/chat/api/threads", body)
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap()
}

/// Send one message and return the `done` event.
async fn send(gw: &Gw, tid: i64) -> Value {
    let body = post(
        gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi"}),
    )
    .await
    .text()
    .await
    .unwrap();
    sse_events(&body)
        .into_iter()
        .find(|(e, _)| e == "done")
        .unwrap_or_else(|| panic!("no done event: {body}"))
        .1
}

async fn settings(gw: &Gw, tid: i64, body: Value) -> reqwest::Response {
    post(gw, &format!("/chat/api/threads/{tid}/settings"), body).await
}

async fn upstream_body(mock: &MockServer, route: &str) -> Value {
    let seen = mock.received_requests().await.unwrap();
    let r = seen
        .iter()
        .rfind(|r| r.url.path() == route)
        .unwrap_or_else(|| panic!("no call to {route}"));
    serde_json::from_slice(&r.body).unwrap()
}

#[tokio::test]
async fn a_llama_server_route_receives_every_sampling_param() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, "ok", 3, 1).await;
    let (_state, gw) = gateway(&mock, UpstreamKind::LlamaServer, Protocol::LlamaCpp).await;
    let tid = thread(&gw, json!({})).await;
    assert_eq!(settings(&gw, tid, all()).await.status(), 200);

    let done = send(&gw, tid).await;
    let sent = upstream_body(&mock, "/chat/completions").await;
    assert_eq!(sent["top_p"], 0.9, "{sent}");
    assert_eq!(sent["top_k"], 40);
    assert_eq!(sent["min_p"], 0.05);
    assert_eq!(sent["repeat_penalty"], 1.1);
    assert_eq!(sent["presence_penalty"], 0.3);
    assert_eq!(sent["frequency_penalty"], -0.4);
    assert_eq!(sent["seed"], 7);
    assert_eq!(sent["stop"], json!(["END", "STOP"]));
    assert_eq!(done["reasoning_ignored"], json!([]), "{done}");
}

#[tokio::test]
async fn a_generic_openai_route_is_not_sent_what_it_cannot_take_and_the_turn_says_so() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, "ok", 3, 1).await;
    let (_state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw, json!({})).await;
    settings(&gw, tid, all()).await;

    let done = send(&gw, tid).await;
    let sent = upstream_body(&mock, "/chat/completions").await;
    for gone in ["top_k", "min_p", "repeat_penalty"] {
        assert!(sent.get(gone).is_none(), "{gone} was sent: {sent}");
    }
    assert_eq!(sent["top_p"], 0.9);
    assert_eq!(sent["seed"], 7);
    assert_eq!(sent["presence_penalty"], 0.3);
    assert_eq!(sent["stop"], json!(["END", "STOP"]));
    assert_eq!(
        done["reasoning_ignored"],
        json!(["top_k", "min_p", "repeat_penalty"]),
        "{done}"
    );
}

#[tokio::test]
async fn an_anthropic_route_gets_top_k_and_stop_sequences() {
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse, "text/event-stream"),
        )
        .mount(&mock)
        .await;
    let (_state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Anthropic).await;
    let tid = thread(&gw, json!({})).await;
    settings(&gw, tid, all()).await;

    let done = send(&gw, tid).await;
    let sent = upstream_body(&mock, "/v1/messages").await;
    assert_eq!(sent["top_p"], 0.9, "{sent}");
    assert_eq!(sent["top_k"], 40);
    assert_eq!(sent["stop_sequences"], json!(["END", "STOP"]));
    assert!(sent.get("seed").is_none() && sent.get("min_p").is_none());
    assert_eq!(
        done["reasoning_ignored"],
        json!([
            "min_p",
            "repeat_penalty",
            "presence_penalty",
            "frequency_penalty",
            "seed"
        ]),
        "{done}"
    );
}

#[tokio::test]
async fn the_values_persist_clear_and_are_validated_server_side() {
    let mock = MockServer::start().await;
    let (_state, gw) = gateway(&mock, UpstreamKind::LlamaServer, Protocol::LlamaCpp).await;
    let tid = thread(&gw, json!({})).await;
    let fresh = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(fresh["thread"]["top_k"], Value::Null, "{fresh}");
    assert_eq!(fresh["thread"]["stop"], json!([]));

    settings(&gw, tid, all()).await;
    let t = get_json(&gw, &format!("/chat/api/threads/{tid}")).await["thread"].clone();
    assert_eq!(t["min_p"], 0.05);
    assert_eq!(t["seed"], 7);
    assert_eq!(t["stop"], json!(["END", "STOP"]));

    // Absent = unchanged; null / [] = cleared.
    settings(
        &gw,
        tid,
        json!({"top_k": null, "stop": [], "temperature": 0.2}),
    )
    .await;
    let t = get_json(&gw, &format!("/chat/api/threads/{tid}")).await["thread"].clone();
    assert_eq!(t["top_k"], Value::Null);
    assert_eq!(t["stop"], json!([]));
    assert_eq!(t["min_p"], 0.05, "untouched: {t}");

    // Each out-of-range value is a 400 naming it, and nothing is written.
    for (bad, name) in [
        (json!({"top_p": 1.5}), "top_p"),
        (json!({"min_p": -1}), "min_p"),
        (json!({"top_k": -3}), "top_k"),
        (json!({"repeat_penalty": 0}), "repeat_penalty"),
        (json!({"presence_penalty": 3}), "presence_penalty"),
        (json!({"frequency_penalty": -2.5}), "frequency_penalty"),
    ] {
        let mut body = bad.clone();
        body["seed"] = json!(99);
        let r = settings(&gw, tid, body).await;
        assert_eq!(r.status(), 400, "{bad}");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["code"], "bad_request");
        assert!(v["message"].as_str().unwrap().contains(name), "{v}");
    }
    let t = get_json(&gw, &format!("/chat/api/threads/{tid}")).await["thread"].clone();
    assert_eq!(t["seed"], 7, "a refused save wrote nothing: {t}");
}

#[tokio::test]
async fn temporary_threads_carry_the_fields_and_keeping_stores_them() {
    let mock = MockServer::start().await;
    let (_state, gw) = gateway(&mock, UpstreamKind::LlamaServer, Protocol::LlamaCpp).await;
    let tid = thread(&gw, json!({"temporary": true})).await;
    assert!(tid < 0);
    assert_eq!(settings(&gw, tid, all()).await.status(), 200);
    let t = get_json(&gw, &format!("/chat/api/threads/{tid}")).await["thread"].clone();
    assert_eq!(t["min_p"], 0.05, "{t}");
    assert_eq!(t["stop"], json!(["END", "STOP"]));

    let kept: Value = post(&gw, &format!("/chat/api/threads/{tid}/persist"), json!({}))
        .await
        .json()
        .await
        .unwrap();
    let new_id = kept["id"].as_i64().unwrap();
    let t = get_json(&gw, &format!("/chat/api/threads/{new_id}")).await["thread"].clone();
    assert_eq!(t["temporary"], false);
    assert_eq!(t["repeat_penalty"], 1.1, "{t}");
    assert_eq!(t["top_k"], 40);
    assert_eq!(t["seed"], 7);
    assert_eq!(t["stop"], json!(["END", "STOP"]));
}
