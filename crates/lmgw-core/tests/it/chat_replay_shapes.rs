//! What a thread's replays look like on the Anthropic and Gemini wire
//! (chat-voice design §7.3, §7.4): the owner's cloud fallbacks answer
//! through these, so the shapes are pinned here as well as on the OpenAI
//! egress (`chat_agent_live`).
//!
//! - Two user messages in a row (a failed send, then another) go out as one
//!   user turn.
//! - A stopped tool turn's closed record, followed by the next user message,
//!   goes out as the call, then one user turn holding the call's result and
//!   the new text. Both egresses fold adjacent same-role turns, and a tool
//!   result rides in a user turn on both.

use lmgw_core::agent::ToolOutcome;
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ir::{ContentPart, Message, Role};
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_actions::{gateway, post};

fn anthropic_sse(text: &str) -> String {
    format!(
        "event: message_start\ndata: {}\n\n\
         event: content_block_start\ndata: {}\n\n\
         event: content_block_delta\ndata: {}\n\n\
         event: message_delta\ndata: {}\n\n\
         event: message_stop\ndata: {}\n\n",
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 7}}}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
               "usage": {"output_tokens": 3}}),
        json!({"type": "message_stop"}),
    )
}

fn gemini_sse(text: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": text}]},
                               "finishReason": "STOP"}],
               "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 3}})
    )
}

/// A thread on `protocol` whose stored history is `seed`, sent `text`: the
/// request body the upstream received.
async fn sent(protocol: Protocol, seed: &[(&str, &str, Option<String>)], text: &str) -> Value {
    let mock = MockServer::start().await;
    let reply = match protocol {
        Protocol::Anthropic => anthropic_sse("ok"),
        _ => gemini_sse("ok"),
    };
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(reply, "text/event-stream"))
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
    for (role, content, record) in seed {
        store::append_chat_message(
            &state.db,
            tid,
            role,
            content,
            "",
            None,
            None,
            record.as_deref(),
        )
        .await
        .unwrap();
    }
    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": text}),
    )
    .await
    .text()
    .await
    .unwrap();
    assert!(body.contains("event: done"), "{body}");
    let reqs = mock.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    serde_json::from_slice(&reqs[0].body).unwrap()
}

/// A tool turn that was stopped mid-tool: its text, its call, and the
/// call's closing result, as the tool loop stores them.
fn stopped_record() -> String {
    let record = vec![
        Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::text("Looking. "),
                ContentPart::ToolUse {
                    id: "c1".into(),
                    name: "stub__lookup".into(),
                    args: json!({"q": "x"}),
                },
            ],
        },
        Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                id: "c1".into(),
                name: Some("stub__lookup".into()),
                content: ToolOutcome::error("abandoned when the run was cancelled").blocks,
                is_error: true,
            }],
        },
    ];
    serde_json::to_string(&record).unwrap()
}

#[tokio::test]
async fn anthropic_gets_two_user_messages_as_one() {
    let body = sent(
        Protocol::Anthropic,
        &[("user", "first try", None)],
        "second try",
    )
    .await;
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "first try\n\nsecond try"}]}]),
        "{body:#}"
    );
}

#[tokio::test]
async fn gemini_gets_two_user_messages_as_one() {
    let body = sent(
        Protocol::Gemini,
        &[("user", "first try", None)],
        "second try",
    )
    .await;
    assert_eq!(
        body["contents"],
        json!([{"role": "user", "parts": [{"text": "first try\n\nsecond try"}]}]),
        "{body:#}"
    );
}

#[tokio::test]
async fn anthropic_gets_a_closed_record_then_the_next_message() {
    let seed = [
        ("user", "look it up", None),
        ("assistant", "Looking. ", Some(stopped_record())),
    ];
    let body = sent(Protocol::Anthropic, &seed, "never mind").await;
    let msgs = body["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "user"], "{body:#}");
    assert_eq!(
        msgs[1]["content"][0],
        json!({"type": "text", "text": "Looking. "})
    );
    assert_eq!(msgs[1]["content"][1]["type"], "tool_use");
    assert_eq!(msgs[1]["content"][1]["id"], "c1");
    // The result and the new text share the one user turn, result first.
    let last = msgs[2]["content"].as_array().unwrap();
    assert_eq!(last.len(), 2, "{body:#}");
    assert_eq!(last[0]["type"], "tool_result");
    assert_eq!(last[0]["tool_use_id"], "c1");
    assert_eq!(last[0]["is_error"], true);
    assert_eq!(last[1], json!({"type": "text", "text": "never mind"}));
}

#[tokio::test]
async fn gemini_gets_a_closed_record_then_the_next_message() {
    let seed = [
        ("user", "look it up", None),
        ("assistant", "Looking. ", Some(stopped_record())),
    ];
    let body = sent(Protocol::Gemini, &seed, "never mind").await;
    let contents = body["contents"].as_array().unwrap();
    let roles: Vec<&str> = contents
        .iter()
        .map(|c| c["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "model", "user"], "{body:#}");
    assert_eq!(contents[1]["parts"][0], json!({"text": "Looking. "}));
    assert_eq!(
        contents[1]["parts"][1]["functionCall"]["name"],
        "stub__lookup"
    );
    let last = contents[2]["parts"].as_array().unwrap();
    assert_eq!(last.len(), 2, "{body:#}");
    assert_eq!(last[0]["functionResponse"]["name"], "stub__lookup");
    assert_eq!(last[1], json!({"text": "never mind"}));
}
