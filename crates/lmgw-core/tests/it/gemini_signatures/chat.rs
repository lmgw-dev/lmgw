//! A Chat thread on Gemini: the tool loop's second model call in the same
//! turn carries the first call's signature, the stored record keeps it in
//! the call's id, and the next send replays it as it came.

use std::time::Duration;

use lmgw_core::ir::split_call_id;
use serde_json::{json, Value};
use wiremock::MockServer;

use super::{model_bodies, mount_replies, signatures, sse_reply, text_sse, SIG};
use crate::chat_actions::{gateway, get_json, post};
use crate::chat_golden::{mcp_stub, register_stub, tool_thread};

/// Gemini calls the stub's `echo`, signed.
fn echo_call() -> String {
    format!(
        "data: {}\n\n",
        json!({"candidates": [{"content": {"role": "model", "parts": [
                   {"functionCall": {"name": "stub__echo", "args": {"text": "hi"}},
                    "thoughtSignature": SIG}]},
                 "finishReason": "STOP"}],
               "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2}})
    )
}

async fn send(gw: &crate::common::Gw, tid: i64, text: &str) {
    let body = post(
        gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": text}),
    )
    .await
    .text()
    .await
    .unwrap();
    assert!(body.contains("event: done"), "{body}");
}

fn sig(s: Option<&str>) -> Vec<(String, Option<String>)> {
    vec![("stub__echo".to_string(), s.map(String::from))]
}

#[tokio::test]
async fn a_thread_keeps_and_replays_the_signature() {
    let mock = MockServer::start().await;
    mount_replies(
        &mock,
        vec![
            sse_reply(echo_call()),
            sse_reply(text_sse("It echoed.")),
            sse_reply(text_sse("Yes.")),
        ],
    )
    .await;
    let (state, gw) = gateway(
        &mock,
        lmgw_core::config::UpstreamKind::Generic,
        lmgw_core::config::Protocol::Gemini,
    )
    .await;
    register_stub(&state, &mcp_stub(Duration::ZERO).await).await;
    let tid = tool_thread(&gw).await;

    send(&gw, tid, "echo hi").await;
    let bodies = model_bodies(&mock).await;
    assert_eq!(bodies.len(), 2);
    // The loop's second call, in the same turn: the model's own signature.
    assert_eq!(
        signatures(&bodies[1]),
        vec![vec![], sig(Some(SIG)), vec![]],
        "{:#}",
        bodies[1]
    );

    // The stored record holds the signature in the call's id.
    let detail = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    let record: Value =
        serde_json::from_str(detail["messages"][1]["ir_messages"].as_str().unwrap()).unwrap();
    let id = record[0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["type"] == "tool_use")
        .unwrap_or_else(|| panic!("{record:#}"))["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (bare, carried) = split_call_id(&id);
    assert!(super::minted(bare), "{id}");
    assert_eq!(carried.as_deref(), Some(SIG));

    // The next send replays it, an earlier turn's call as it came.
    send(&gw, tid, "did it?").await;
    let bodies = model_bodies(&mock).await;
    assert_eq!(bodies.len(), 3);
    let calls: Vec<_> = signatures(&bodies[2])
        .into_iter()
        .filter(|c| !c.is_empty())
        .collect();
    assert_eq!(calls, vec![sig(Some(SIG))], "{:#}", bodies[2]);
}
