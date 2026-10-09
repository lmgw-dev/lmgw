//! Capture: a Gemini answer's `thoughtSignature` ends up in the id of the
//! call it signed — in the egress's completion and stream deltas, and in
//! what each client shape receives, streamed or not.

use lmgw_core::config::Protocol;
use lmgw_core::egress::for_protocol;
use lmgw_core::ir::{split_call_id, ContentPart, StreamDelta};
use lmgw_core::sse::SseEvent;
use wiremock::MockServer;

use super::{
    call_ids, calls_body, calls_sse, gemini_gateway, json_reply, minted, mount_replies, sse_reply,
    Ingress, Turn, SIG,
};

/// The two calls' ids: minted, apart, the first carrying [`SIG`] and the
/// second none.
fn assert_signed_pair(ids: &[String], what: &str) {
    assert_eq!(ids.len(), 2, "{what}: {ids:?}");
    let (bare, sig) = split_call_id(&ids[0]);
    assert!(minted(bare), "{what}: {ids:?}");
    assert_eq!(sig.as_deref(), Some(SIG), "{what}");
    assert!(minted(&ids[1]), "{what}: {ids:?}");
    assert_ne!(bare, ids[1], "{what}");
}

#[test]
fn a_completion_carries_the_signature_in_its_calls_id() {
    let c = for_protocol(Protocol::Gemini)
        .parse_completion(calls_body().to_string().as_bytes())
        .unwrap();
    let ids: Vec<String> = c
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_signed_pair(&ids, "parse_completion");
}

#[test]
fn a_stream_carries_the_signature_in_its_calls_id() {
    let mut dec = for_protocol(Protocol::Gemini).new_decoder();
    let mut ids = Vec::new();
    let mut indices = Vec::new();
    for block in calls_sse().split("\n\n").filter(|b| !b.is_empty()) {
        let data = block.strip_prefix("data: ").unwrap();
        for d in dec.on_event(&SseEvent {
            event: None,
            data: data.to_string(),
        }) {
            if let StreamDelta::ToolCallStart { index, id, .. } = d {
                ids.push(id);
                indices.push(index);
            }
        }
    }
    assert_signed_pair(&ids, "decoder");
    assert_eq!(indices, [0, 1]);
}

/// A second call in a later chunk gets an id of its own, and only a signed
/// part's id carries a signature.
#[test]
fn a_signature_in_a_later_chunk_stays_with_its_own_call() {
    let mut dec = for_protocol(Protocol::Gemini).new_decoder();
    let chunk = |v: serde_json::Value| SseEvent {
        event: None,
        data: v.to_string(),
    };
    let mut ids = Vec::new();
    for ev in [
        chunk(serde_json::json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "a", "args": {}}}]}}]})),
        chunk(serde_json::json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "b", "args": {}}, "thoughtSignature": "LATE"}]},
            "finishReason": "STOP"}]})),
    ] {
        for d in dec.on_event(&ev) {
            if let StreamDelta::ToolCallStart { id, .. } = d {
                ids.push(id);
            }
        }
    }
    assert!(minted(&ids[0]), "{ids:?}");
    let (bare, sig) = split_call_id(&ids[1]);
    assert!(minted(bare) && bare != ids[0], "{ids:?}");
    assert_eq!(sig.as_deref(), Some("LATE"));
}

#[tokio::test]
async fn each_client_shape_receives_the_signed_id() {
    for ingress in Ingress::ALL {
        for stream in [false, true] {
            let mock = MockServer::start().await;
            let reply = if stream {
                sse_reply(calls_sse())
            } else {
                json_reply(calls_body())
            };
            mount_replies(&mock, vec![reply]).await;
            let gw = gemini_gateway(&mock).await;
            let answer = ingress
                .send(&gw, &[Turn::User("Weather and time in Paris?")], stream)
                .await;
            assert_signed_pair(
                &call_ids(&answer),
                &format!("{} stream={stream}: {answer}", ingress.name()),
            );
        }
    }
}
