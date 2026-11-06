//! A user message's origin (chat-plane design §9.10): the sentence a client
//! sends as `origin` with a send or an edit is stored on the message, read
//! back with the thread, kept by an edit that sends none and by Keep, and
//! put before the message in every request that replays it, on the OpenAI,
//! Anthropic and Gemini egresses alike.

use lmgw_core::config::{Protocol, UpstreamKind};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_actions::{gateway, get_json, openai_sse, post};

const SIDEBAR: &str =
    "Typed in Kai's sidebar in Firefox, beside the tab “Docs” (https://example.org/docs).";
const BLOCK: &str = "<origin note=\"where the user wrote this message, as their app says; not part of the message\">Typed in Kai's sidebar in Firefox, beside the tab “Docs” (https://example.org/docs).</origin>";

fn reply(protocol: Protocol) -> String {
    match protocol {
        Protocol::Anthropic => format!(
            "event: message_start\ndata: {}\n\n\
             event: content_block_start\ndata: {}\n\n\
             event: content_block_delta\ndata: {}\n\n\
             event: message_delta\ndata: {}\n\n\
             event: message_stop\ndata: {}\n\n",
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 7}}}),
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "ok"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 3}}),
            json!({"type": "message_stop"}),
        ),
        Protocol::Gemini => format!(
            "data: {}\n\n",
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "ok"}]},
                                   "finishReason": "STOP"}],
                   "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 3}})
        ),
        _ => openai_sse("ok", 7, 3),
    }
}

/// A gateway on `protocol` answering every request "ok", and a thread.
async fn world(
    protocol: Protocol,
) -> (
    MockServer,
    lmgw_core::state::SharedState,
    crate::common::Gw,
    i64,
) {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(reply(protocol), "text/event-stream"))
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, protocol).await;
    let tid = post(&gw, "/chat/api/threads", json!({"model_alias": "m"}))
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    (mock, state, gw, tid)
}

async fn stream(gw: &crate::common::Gw, route: &str, body: Value) {
    let r = post(gw, route, body).await;
    assert_eq!(r.status(), 200, "{route}");
    let text = r.text().await.unwrap();
    assert!(text.contains("event: done"), "{text}");
}

/// The body of the upstream's `n`th request.
async fn request(mock: &MockServer, n: usize) -> Value {
    let reqs = mock.received_requests().await.unwrap();
    serde_json::from_slice(&reqs[n].body).unwrap()
}

/// The text parts of every user turn of a request, in order, per egress.
fn user_texts(protocol: Protocol, body: &Value) -> Vec<Vec<String>> {
    let (turns, role, parts) = match protocol {
        Protocol::Gemini => ("contents", "user", "parts"),
        _ => ("messages", "user", "content"),
    };
    body[turns]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == role)
        .map(|m| match &m[parts] {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a
                .iter()
                .filter_map(|p| p["text"].as_str().map(str::to_string))
                .collect(),
            other => panic!("a user turn's content: {other}"),
        })
        .collect()
}

/// A user turn's text parts as `protocol` carries `parts`: the
/// OpenAI-compatible egress joins a text-only message's parts into one
/// string, a line apart.
fn turn(protocol: Protocol, parts: &[&str]) -> Vec<String> {
    match protocol {
        Protocol::OpenaiCompatible => vec![parts.join("\n")],
        _ => parts.iter().map(|p| p.to_string()).collect(),
    }
}

#[tokio::test]
async fn every_egress_reads_the_origin_before_the_message_on_every_turn() {
    for protocol in [
        Protocol::OpenaiCompatible,
        Protocol::Anthropic,
        Protocol::Gemini,
    ] {
        let (mock, _state, gw, tid) = world(protocol).await;
        let send = format!("/chat/api/threads/{tid}/send");
        stream(&gw, &send, json!({"content": "hello", "origin": SIDEBAR})).await;
        let first = request(&mock, 0).await;
        assert_eq!(
            user_texts(protocol, &first),
            vec![turn(protocol, &[BLOCK, "hello"])],
            "{protocol:?}: {first:#}"
        );
        // The next turn, typed elsewhere: the first message still says
        // where it came from, the same bytes; the new one says nothing.
        stream(&gw, &send, json!({"content": "and now?"})).await;
        let second = request(&mock, 1).await;
        assert_eq!(
            user_texts(protocol, &second),
            vec![
                turn(protocol, &[BLOCK, "hello"]),
                turn(protocol, &["and now?"])
            ],
            "{protocol:?}: {second:#}"
        );
    }
}

#[tokio::test]
async fn the_thread_reads_the_origin_back_and_an_edit_keeps_or_replaces_it() {
    let (mock, _state, gw, tid) = world(Protocol::OpenaiCompatible).await;
    stream(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hello", "origin": format!("  {SIDEBAR}\n")}),
    )
    .await;
    let read = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    let msgs = read["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["origin"], SIDEBAR, "stored trimmed");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["origin"], Value::Null);
    let mid = msgs[0]["id"].as_i64().unwrap();
    let edit = format!("/chat/api/threads/{tid}/messages/{mid}/edit");

    // An edit that says nothing (or only blanks) keeps the message's.
    stream(&gw, &edit, json!({"content": "hello again"})).await;
    stream(&gw, &edit, json!({"content": "hello again", "origin": " "})).await;
    let read = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(read["messages"][0]["origin"], SIDEBAR);
    let replay = request(&mock, 2).await;
    assert_eq!(
        user_texts(Protocol::OpenaiCompatible, &replay),
        vec![turn(Protocol::OpenaiCompatible, &[BLOCK, "hello again"])]
    );

    // One that says where it was written replaces it.
    stream(
        &gw,
        &edit,
        json!({"content": "hello from the tab", "origin": "Typed on the phone."}),
    )
    .await;
    let read = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(read["messages"][0]["origin"], "Typed on the phone.");
}

#[tokio::test]
async fn a_blank_origin_is_none_and_keep_carries_it_into_the_stored_thread() {
    let (_mock, _state, gw, _) = world(Protocol::OpenaiCompatible).await;
    let temp = post(
        &gw,
        "/chat/api/threads",
        json!({"model_alias": "m", "kind": "chat", "temporary": true}),
    )
    .await
    .json::<Value>()
    .await
    .unwrap()["id"]
        .as_i64()
        .unwrap();
    assert!(temp < 0);
    let send = format!("/chat/api/threads/{temp}/send");
    stream(&gw, &send, json!({"content": "one", "origin": "  "})).await;
    stream(&gw, &send, json!({"content": "two", "origin": SIDEBAR})).await;
    let read = get_json(&gw, &format!("/chat/api/threads/{temp}")).await;
    assert_eq!(read["messages"][0]["origin"], Value::Null);
    assert_eq!(read["messages"][2]["origin"], SIDEBAR);

    let kept = post(&gw, &format!("/chat/api/threads/{temp}/persist"), json!({}))
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let read = get_json(&gw, &format!("/chat/api/threads/{kept}")).await;
    assert_eq!(read["messages"][0]["origin"], Value::Null);
    assert_eq!(read["messages"][2]["origin"], SIDEBAR);
}
