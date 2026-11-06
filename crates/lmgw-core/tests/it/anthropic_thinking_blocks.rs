//! An Anthropic upstream's `thinking` blocks reach a Messages client as
//! Anthropic sent them, whole and streamed: each block where it stood, with
//! its `signature`, an omitted display's block (empty `thinking`, the
//! default on current Claude models) included. Sent back, the signed block
//! reaches Anthropic again with its signature. The in-process assembler
//! keeps a streamed block's signature on its part too.

use lmgw_core::agent::TurnAccumulator;
use lmgw_core::config::Protocol;
use lmgw_core::egress::for_protocol;
use lmgw_core::ir::{ContentPart, StreamDelta};
use lmgw_core::sse::SseEvent;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::Gw;
use crate::messages_context_management::{bodies_at, world};

const SIG_A: &str = "EqQBCkYIBxgCKkDomitted==";
const SIG_B: &str = "EqQBCkYIBxgCKkDsummary==";

fn answer() -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
        "content": [
            {"type": "thinking", "thinking": "", "signature": SIG_A},
            {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "kiwi"}},
            {"type": "thinking", "thinking": "The word was KIWI.", "signature": SIG_B},
            {"type": "text", "text": "KIWI"},
        ],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 100, "output_tokens": 89},
    })
}

async fn post(gw: &Gw, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gw}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(body)
        .send()
        .await
        .unwrap()
}

fn tools() -> Value {
    json!([{"name": "lookup", "input_schema": {"type": "object"}}])
}

#[tokio::test]
async fn every_thinking_block_comes_back_signed_where_it_stood() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer()))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;
    let body = json!({
        "model": "m", "max_tokens": 1024, "tools": tools(),
        "messages": [{"role": "user", "content": "What was the code word?"}],
    });
    let resp = post(&gw, &body).await;
    assert_eq!(resp.status(), 200);
    let got: Value = resp.json().await.unwrap();
    assert_eq!(got["content"], answer()["content"], "{got}");
    assert_eq!(got["usage"]["output_tokens"], 89);
}

/// The tool loop as a client runs it: the assistant turn that thought and
/// called the tool (an omitted display's block, then the call), the user's
/// `tool_result`, and the next request. The turn goes back to Anthropic as
/// it came, its signed block ahead of its call, as Anthropic requires.
#[tokio::test]
async fn a_signed_block_sent_back_reaches_anthropic_with_its_signature() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer()))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;
    let turn = json!([
        {"type": "thinking", "thinking": "", "signature": SIG_A},
        {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "kiwi"}},
    ]);
    let body = json!({
        "model": "m", "max_tokens": 1024, "tools": tools(),
        "messages": [
            {"role": "user", "content": "What was the code word?"},
            {"role": "assistant", "content": turn},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "KIWI"},
            ]},
        ],
    });
    assert_eq!(post(&gw, &body).await.status(), 200);
    let sent = bodies_at(&mock, "/v1/messages").await;
    let messages = &sent[0]["messages"];
    assert_eq!(messages[1]["content"], turn, "{}", sent[0]);
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(messages.as_array().unwrap().len(), 3);
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// The second block's text: it starts with a line break, which its
/// signature covers, so it reaches every reader as it came.
const THOUGHT: &str = "\nThe word was KIWI.";

/// A stream with an omitted display's block (its start and its signature,
/// nothing between), a tool call, a summarized block whose first delta is a
/// line break alone, then the answer.
fn stream_events() -> Vec<Value> {
    let start =
        |i: u64, b: Value| json!({"type": "content_block_start", "index": i, "content_block": b});
    let delta = |i: u64, d: Value| json!({"type": "content_block_delta", "index": i, "delta": d});
    let stop = |i: u64| json!({"type": "content_block_stop", "index": i});
    let thinking = json!({"type": "thinking", "thinking": "", "signature": ""});
    vec![
        json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt", "content": [],
            "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 100, "output_tokens": 1}}}),
        start(0, thinking.clone()),
        delta(0, json!({"type": "signature_delta", "signature": SIG_A})),
        stop(0),
        start(
            1,
            json!({"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}}),
        ),
        delta(
            1,
            json!({"type": "input_json_delta", "partial_json": "{\"q\": \"kiwi\"}"}),
        ),
        stop(1),
        start(2, thinking),
        delta(2, json!({"type": "thinking_delta", "thinking": "\n"})),
        delta(
            2,
            json!({"type": "thinking_delta", "thinking": "The word was KIWI."}),
        ),
        delta(2, json!({"type": "signature_delta", "signature": SIG_B})),
        stop(2),
        start(3, json!({"type": "text", "text": ""})),
        delta(3, json!({"type": "text_delta", "text": "KIWI"})),
        stop(3),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null},
               "usage": {"output_tokens": 89}}),
        json!({"type": "message_stop"}),
    ]
}

#[tokio::test]
async fn a_streamed_thinking_block_keeps_its_signature_on_the_messages_wire() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(&stream_events())),
        )
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;
    let body = json!({
        "model": "m", "max_tokens": 1024, "stream": true,
        "messages": [{"role": "user", "content": "What was the code word?"}],
    });
    let text = post(&gw, &body).await.text().await.unwrap();
    let events: Vec<Value> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect();
    // The blocks as the upstream sent them: start, deltas, stop, in order.
    let blocks: Vec<Value> = events
        .iter()
        .filter(|e| {
            matches!(
                e["type"].as_str(),
                Some("content_block_start" | "content_block_delta" | "content_block_stop")
            )
        })
        .cloned()
        .collect();
    let upstream: Vec<Value> = stream_events()
        .into_iter()
        .filter(|e| e["type"].as_str().unwrap().starts_with("content_block"))
        .collect();
    assert_eq!(blocks, upstream, "{text}");
}

#[test]
fn the_assembler_keeps_each_streamed_block_signed_where_it_stood() {
    let mut dec = for_protocol(Protocol::Anthropic).new_decoder();
    let mut acc = TurnAccumulator::default();
    for e in stream_events() {
        for d in dec.on_event(&SseEvent {
            event: Some(e["type"].as_str().unwrap().into()),
            data: e.to_string(),
        }) {
            acc.on_delta(&d);
            if let StreamDelta::ReasoningSignature { signature: s, .. } = &d {
                assert!(s == SIG_A || s == SIG_B);
            }
        }
    }
    let c = acc.finish("tgt".into());
    assert!(c.reasoning_in_place());
    // A decoder no send site made names no origin.
    let row = Some(lmgw_core::ir::Signer::default());
    // Each block where it stood, the call between them, the second block's
    // text as Anthropic signed it, its leading line break included.
    assert_eq!(
        c.content,
        [
            ContentPart::signed_reasoning("", SIG_A, row.clone()),
            ContentPart::ToolUse {
                id: "toolu_1".into(),
                name: "lookup".into(),
                args: json!({"q": "kiwi"}),
            },
            ContentPart::signed_reasoning(THOUGHT, SIG_B, row),
            ContentPart::text("KIWI"),
        ]
    );
    assert_eq!(c.reasoning, THOUGHT);
}

/// The Messages wire keeps a signed block's text byte for byte, a first
/// delta that is a line break alone included; the blank line the
/// platform's decoder puts between two OpenAI reasoning items is still
/// left off where a sealed item opens a block of its own.
#[test]
fn a_signed_block_s_text_is_not_trimmed_and_a_joined_one_still_is() {
    use lmgw_core::ingress::anthropic::AnthropicStreamEncoder;
    use lmgw_core::ingress::ClientStreamEncoder;
    use lmgw_core::ir::{Seal, SealScope, Signer};
    let thinking_deltas = |deltas: &[StreamDelta]| -> Vec<String> {
        let mut enc = AnthropicStreamEncoder::new("m");
        let out: String = deltas.iter().map(|d| enc.delta(d)).collect();
        out.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .filter(|e| e["delta"]["type"] == "thinking_delta")
            .map(|e| e["delta"]["thinking"].as_str().unwrap().to_string())
            .collect()
    };
    let signed = |s: &str| StreamDelta::ReasoningSignature {
        signature: s.into(),
        scope: Signer::default(),
    };
    // Anthropic's: the answer's first block and one after a signed block.
    let anthropic = [
        StreamDelta::ReasoningDelta("\n".into()),
        StreamDelta::ReasoningDelta("First.".into()),
        signed(SIG_A),
        StreamDelta::ReasoningDelta("\n\nSecond.".into()),
        signed(SIG_B),
    ];
    assert_eq!(thinking_deltas(&anthropic), ["\n", "First.", "\n\nSecond."]);
    // The platform's two sealed items, joined by the decoder's blank line.
    let seal = |id: &str| Seal::openai(&SealScope::new(3, "cfg"), id, "enc");
    let openai = [
        StreamDelta::ReasoningDelta("One.".into()),
        StreamDelta::ReasoningSeal(seal("rs_1")),
        StreamDelta::ReasoningDelta("\n\nTwo.".into()),
        StreamDelta::ReasoningSeal(seal("rs_2")),
    ];
    assert_eq!(thinking_deltas(&openai), ["One.", "Two."]);

    // The assembler alike.
    let mut acc = TurnAccumulator::default();
    anthropic.iter().for_each(|d| acc.on_delta(d));
    let c = acc.finish("m".into());
    assert_eq!(
        c.content[0],
        ContentPart::signed_reasoning("\nFirst.", SIG_A, Some(Signer::default()))
    );
    assert_eq!(
        c.content[1],
        ContentPart::signed_reasoning("\n\nSecond.", SIG_B, Some(Signer::default()))
    );
    let mut acc = TurnAccumulator::default();
    openai.iter().for_each(|d| acc.on_delta(d));
    let c = acc.finish("m".into());
    assert_eq!(
        c.content[1],
        ContentPart::sealed_reasoning("Two.", seal("rs_2"))
    );
}

/// An answer whose thinking came unsigned (an Anthropic-compatible upstream
/// that signs nothing) keeps the shape it always had: the trace apart, as
/// one leading block.
#[test]
fn an_unsigned_answer_keeps_its_trace_apart() {
    let body = json!({
        "content": [
            {"type": "thinking", "thinking": "Hmm."},
            {"type": "text", "text": "KIWI"},
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 2},
    });
    let c = for_protocol(Protocol::Anthropic)
        .parse_completion(body.to_string().as_bytes())
        .unwrap();
    assert!(!c.reasoning_in_place());
    assert_eq!(c.reasoning, "Hmm.");
    assert_eq!(c.content, [ContentPart::text("KIWI")]);
}
