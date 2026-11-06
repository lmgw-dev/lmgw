//! The reasoning seals of the platform's answers (openai egress design
//! §7.3.1, WP8 step 2): read off a reasoning item's `done` and a whole
//! Response, each item's part where it stood, folded by the in-process
//! assembler, and sent back to the row that made them only.

use serde_json::{json, Value};

use super::*;
use crate::agent::TurnAccumulator;
use crate::egress::EgressStreamDecoder;
use crate::ir::{ChatRequest, Message, Params, Role};
use crate::sse::SseDecoder;

/// The row the tests' seals come from, as configured.
fn up() -> crate::ir::SealScope {
    crate::ir::SealScope::new(7, "cfg7")
}
const LUNA: &str =
    include_str!("../../../../../tests/fixtures/openai_platform/luna_reasoning_then_call.sse");
const LUNA_RS: &str = "rs_0635d08921a88cfc016addf1ef6ac087d2bf6402f23d14b137";

fn decode(body: &str, scope: crate::ir::SealScope) -> Vec<StreamDelta> {
    let mut sse = SseDecoder::new();
    let mut dec = PlatformDecoder::for_scope(scope);
    let mut out = Vec::new();
    for ev in sse.feed(body.as_bytes()) {
        out.extend(dec.on_event(&ev));
    }
    for ev in sse.finish() {
        out.extend(dec.on_event(&ev));
    }
    out.extend(dec.on_end());
    out
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// The real luna stream's reasoning item is sealed for the row that
/// streamed it, after its summary and before the call that followed it.
#[test]
fn a_real_stream_s_reasoning_item_is_sealed_where_it_stood() {
    let got = decode(LUNA, up());
    let seal = Seal::openai(&up(), LUNA_RS, "gAAAAAB-scrubbed");
    let at = got
        .iter()
        .position(|d| *d == StreamDelta::ReasoningSeal(seal.clone()))
        .expect("the item's seal");
    let last_summary = got
        .iter()
        .rposition(|d| matches!(d, StreamDelta::ReasoningDelta(_)))
        .unwrap();
    let call = got
        .iter()
        .position(|d| matches!(d, StreamDelta::ToolCallStart { .. }))
        .unwrap();
    assert!(
        last_summary < at && at < call,
        "{last_summary} < {at} < {call}"
    );
    assert_eq!(
        got.iter()
            .filter(|d| matches!(d, StreamDelta::ReasoningSeal(_)))
            .count(),
        1
    );

    // Folded in-process: the item's part, its summary and its seal, ahead
    // of the call; the trace stays whole beside it.
    let mut acc = TurnAccumulator::default();
    for d in &got {
        acc.on_delta(d);
    }
    let c = acc.finish("gpt-6-luna".into());
    assert!(c.reasoning_in_place());
    let [ContentPart::Reasoning {
        text,
        sealed: Some(s),
        ..
    }, ContentPart::ToolUse { name, .. }] = c.content.as_slice()
    else {
        panic!("{:?}", c.content);
    };
    assert_eq!((s, name.as_str()), (&seal, "get_weather"));
    assert_eq!(text, &c.reasoning);
    assert!(text.starts_with("**Determining Austrian Capitals**"));
    assert_eq!(c.assistant_parts(), c.content);
}

/// Two reasoning items around a message: each its own part, in order, the
/// second's text without the blank line that joins the whole trace; an item
/// that came with no summary is a part of its own.
#[test]
fn each_item_is_its_own_part_and_an_unsummarized_one_too() {
    let body = sse(&[
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
               "summary_index": 0, "delta": "First."}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {
            "id": "rs_1", "type": "reasoning", "encrypted_content": "e1",
            "summary": [{"type": "summary_text", "text": "First."}]}}),
        json!({"type": "response.output_text.delta", "output_index": 1, "delta": "Hm."}),
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 2,
               "summary_index": 0, "delta": "Second."}),
        json!({"type": "response.output_item.done", "output_index": 2, "item": {
            "id": "rs_2", "type": "reasoning", "encrypted_content": "e2", "summary": []}}),
        json!({"type": "response.output_item.done", "output_index": 3, "item": {
            "id": "rs_3", "type": "reasoning", "encrypted_content": "e3", "summary": []}}),
        json!({"type": "response.output_text.delta", "output_index": 4, "delta": "Done."}),
        json!({"type": "response.completed", "response": {
            "id": "resp_1", "object": "response", "status": "completed", "output": []}}),
    ]);
    let mut acc = TurnAccumulator::default();
    for d in decode(&body, up()) {
        acc.on_delta(&d);
    }
    let c = acc.finish("m".into());
    assert_eq!(
        c.content,
        vec![
            ContentPart::sealed_reasoning("First.", Seal::openai(&up(), "rs_1", "e1")),
            ContentPart::text("Hm."),
            ContentPart::sealed_reasoning("Second.", Seal::openai(&up(), "rs_2", "e2")),
            ContentPart::sealed_reasoning("", Seal::openai(&up(), "rs_3", "e3")),
            ContentPart::text("Done."),
        ]
    );
    assert_eq!(c.reasoning, "First.\n\nSecond.");
}

/// A stream's items are recorded by the data their seals were taken from,
/// the item's `done`, whatever the terminal event's `output` holds, and a
/// sealed item that output left out is recorded too (WP8 verify V-F4).
#[test]
fn a_stream_s_item_is_recorded_by_its_seal_s_data() {
    let body = sse(&[
        json!({"type": "response.output_item.done", "output_index": 0, "item": {
            "id": "rs_1", "type": "reasoning", "summary": [],
            "encrypted_content": "from-done"}}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": {
            "id": "rs_2", "type": "reasoning", "summary": [],
            "encrypted_content": "only-done"}}),
        json!({"type": "response.output_text.delta", "output_index": 2, "delta": "Hi."}),
        json!({"type": "response.completed", "response": {
            "id": "resp_1", "object": "response", "status": "completed", "output": [
                {"id": "rs_1", "type": "reasoning", "summary": []},
                {"id": "msg_1", "type": "message", "role": "assistant", "content": []}]}}),
    ]);
    let got = decode(&body, up());
    let items = got
        .iter()
        .find_map(|d| match d {
            StreamDelta::UpstreamItems(i) => Some(i.clone()),
            _ => None,
        })
        .expect("the answer's items");
    assert_eq!(
        items.items,
        [
            UpstreamItem {
                id: "rs_1".into(),
                encrypted_sha256: Some(encrypted_sha256("from-done")),
            },
            UpstreamItem::plain("msg_1"),
            UpstreamItem {
                id: "rs_2".into(),
                encrypted_sha256: Some(encrypted_sha256("only-done")),
            },
        ]
    );
}

/// A turn without a seal keeps the shape it always had: its reasoning
/// apart, led by it in the conversation.
#[test]
fn an_unsealed_turn_keeps_its_old_shape() {
    let body = sse(&[
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
               "summary_index": 0, "delta": "Hm."}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {
            "id": "rs_1", "type": "reasoning", "summary": []}}),
        json!({"type": "response.output_text.delta", "output_index": 1, "delta": "Hi."}),
        json!({"type": "response.completed", "response": {
            "id": "resp_1", "object": "response", "status": "completed", "output": []}}),
    ]);
    let got = decode(&body, up());
    assert!(!got
        .iter()
        .any(|d| matches!(d, StreamDelta::ReasoningSeal(_))));
    let mut acc = TurnAccumulator::default();
    for d in &got {
        acc.on_delta(d);
    }
    let c = acc.finish("m".into());
    assert_eq!(c.content, vec![ContentPart::text("Hi.")]);
    assert_eq!(
        c.assistant_parts(),
        vec![ContentPart::reasoning("Hm."), ContentPart::text("Hi.")]
    );
}

/// A whole Response: its reasoning items in place with their seals, when
/// any came sealed; the trace joined beside them.
#[test]
fn a_whole_response_s_items_are_sealed_in_place() {
    let v = json!({"id": "resp_2", "object": "response", "status": "completed", "output": [
        {"type": "reasoning", "id": "rs_1", "encrypted_content": "e1", "summary": [
            {"type": "summary_text", "text": "First."},
            {"type": "summary_text", "text": "Second."}]},
        {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "f",
         "arguments": "{}"},
    ]});
    let c = parse_response(&v, &up()).unwrap();
    assert_eq!(
        c.content,
        vec![
            ContentPart::sealed_reasoning("First.\n\nSecond.", Seal::openai(&up(), "rs_1", "e1")),
            ContentPart::ToolUse {
                id: "call_1".into(),
                name: "f".into(),
                args: json!({}),
            },
        ]
    );
    assert_eq!(c.reasoning, "First.\n\nSecond.");
}

/// The platform's next send writes the row's own seals back as `reasoning`
/// items before what followed them, and leaves out another row's seals and
/// unsealed reasoning: the Responses input has no place for them.
#[test]
fn the_platform_sends_back_its_own_row_s_seals_only() {
    let mut ir: ChatRequest = crate::ingress::openai::parse_chat_request(&json!({
        "model": "a", "messages": [{"role": "user", "content": "Weather?"}]}))
    .unwrap();
    ir.messages.push(Message {
        role: Role::Assistant,
        content: vec![
            ContentPart::sealed_reasoning("Thinking.", Seal::openai(&up(), "rs_1", "e1")),
            ContentPart::sealed_reasoning("", Seal::openai(&up(), "rs_2", "e2")),
            ContentPart::sealed_reasoning(
                "Other.",
                Seal::openai(&crate::ir::SealScope::new(8, "cfg8"), "rs_9", "e9"),
            ),
            ContentPart::reasoning("Plain."),
            ContentPart::ToolUse {
                id: "call_1".into(),
                name: "f".into(),
                args: json!({}),
            },
        ],
    });
    let b = crate::egress::openai::responses::responses_body(
        &ir,
        "gpt-x",
        &Params::default(),
        false,
        &up(),
    );
    assert_eq!(
        b["input"],
        json!([
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "Weather?"}]},
            {"type": "reasoning", "id": "rs_1", "encrypted_content": "e1",
             "summary": [{"type": "summary_text", "text": "Thinking."}]},
            {"type": "reasoning", "id": "rs_2", "encrypted_content": "e2", "summary": []},
            {"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}"},
        ])
    );
    // To another row of the platform: none of them.
    let other = crate::egress::openai::responses::responses_body(
        &ir,
        "gpt-x",
        &Params::default(),
        false,
        &crate::ir::SealScope::new(9, "cfg9"),
    );
    assert!(
        !other["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["type"] == "reasoning"),
        "{other}"
    );
}
