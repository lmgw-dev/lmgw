//! The Responses-wire readers on hand-written answers (openai egress design
//! §7.2.3, §7.2.4 step 3): each event, a cut stream, a terminal
//! `incomplete`, an `error` event, and the tier echo of `response.created`
//! ignored.

use serde_json::{json, Value};

use super::*;
use crate::egress::EgressStreamDecoder;
use crate::ir::{FinishReason, StreamDelta, UpstreamItem, UpstreamItems, Usage};
use crate::sse::SseDecoder;

/// The upstream row the answers come from, which their seals name.
/// The row the tests' seals come from, as configured.
fn up() -> crate::ir::SealScope {
    crate::ir::SealScope::new(7, "cfg7")
}

/// A whole Response from [`up`].
fn parse(v: &Value) -> Result<Completion, GatewayError> {
    parse_response(v, &up())
}

/// `events` as the SSE body the platform streams: each with its `event:`
/// line and its `type` in the data.
fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// The deltas a platform decoder reads off `body`, fed in small chunks; a
/// fragment of a stream, whose body has not ended ([`decode_to_end`]).
fn decode(body: &str) -> Vec<StreamDelta> {
    decode_with(body, false)
}

/// [`decode`] of a whole body, its end included.
fn decode_to_end(body: &str) -> Vec<StreamDelta> {
    decode_with(body, true)
}

fn decode_with(body: &str, to_end: bool) -> Vec<StreamDelta> {
    let mut sse = SseDecoder::new();
    let mut dec = PlatformDecoder::for_scope(up());
    let mut out = Vec::new();
    for chunk in body.as_bytes().chunks(7) {
        for ev in sse.feed(chunk) {
            out.extend(dec.on_event(&ev));
        }
    }
    for ev in sse.finish() {
        out.extend(dec.on_event(&ev));
    }
    if to_end {
        out.extend(dec.on_end());
    }
    out
}

fn created(tier: &str) -> Value {
    json!({"type": "response.created", "sequence_number": 0, "response": {
        "id": "resp_1", "object": "response", "status": "in_progress",
        "service_tier": tier, "output": [], "usage": null}})
}

fn usage() -> Value {
    json!({"input_tokens": 20, "input_tokens_details": {"cached_tokens": 4},
           "output_tokens": 12, "output_tokens_details": {"reasoning_tokens": 5},
           "total_tokens": 32})
}

fn ir_usage() -> Usage {
    Usage {
        prompt_tokens: Some(20),
        completion_tokens: Some(12),
        cached_input_tokens: Some(4),
        cache_write_tokens: None,
        reasoning_tokens: Some(5),
    }
}

/// A reasoning item with two summary parts, a message, a function call:
/// every event in its order, the tier read from the terminal event only.
#[test]
fn a_whole_stream_reads_reasoning_text_a_call_the_tier_and_the_usage() {
    let body = sse(&[
        created("auto"),
        json!({"type": "response.in_progress", "response": {"service_tier": "auto"}}),
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
        json!({"type": "response.reasoning_summary_part.added", "output_index": 0,
               "summary_index": 0, "item_id": "rs_1",
               "part": {"type": "summary_text", "text": ""}}),
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
               "summary_index": 0, "item_id": "rs_1", "delta": "Weighing "}),
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
               "summary_index": 0, "item_id": "rs_1", "delta": "it."}),
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
               "summary_index": 1, "item_id": "rs_1", "delta": "Then a call."}),
        json!({"type": "response.output_item.done", "output_index": 0,
               "item": {"type": "reasoning", "id": "rs_1",
                        "summary": [{"type": "summary_text", "text": "Weighing it."},
                                    {"type": "summary_text", "text": "Then a call."}],
                        "encrypted_content": "gAAAA"}}),
        json!({"type": "response.output_item.added", "output_index": 1,
               "item": {"type": "message", "id": "msg_1", "role": "assistant",
                        "status": "in_progress", "content": []}}),
        json!({"type": "response.output_text.delta", "output_index": 1, "content_index": 0,
               "item_id": "msg_1", "delta": "Checking"}),
        json!({"type": "response.output_text.delta", "output_index": 1, "content_index": 0,
               "item_id": "msg_1", "delta": " now."}),
        json!({"type": "response.output_item.added", "output_index": 2,
               "item": {"type": "function_call", "id": "fc_1", "call_id": "call_9",
                        "name": "get_weather", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 2,
               "item_id": "fc_1", "delta": "{\"city\":"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 2,
               "item_id": "fc_1", "delta": "\"Oslo\"}"}),
        json!({"type": "response.output_item.done", "output_index": 2,
               "item": {"type": "function_call", "id": "fc_1", "call_id": "call_9",
                        "name": "get_weather", "arguments": "{\"city\":\"Oslo\"}"}}),
        json!({"type": "response.completed", "response": {
            "id": "resp_1", "object": "response", "status": "completed",
            "service_tier": "default", "usage": usage(), "output": [
                {"type": "reasoning", "id": "rs_1"},
                {"type": "message", "id": "msg_1"},
                {"type": "function_call", "id": "fc_1", "call_id": "call_9"}]}}),
    ]);
    assert_eq!(
        decode(&body),
        vec![
            StreamDelta::ReasoningDelta("Weighing ".into()),
            StreamDelta::ReasoningDelta("it.".into()),
            StreamDelta::ReasoningDelta("\n\nThen a call.".into()),
            // The item's seal, from its `done` (§7.3.1).
            StreamDelta::ReasoningSeal(crate::ir::Seal::openai(&up(), "rs_1", "gAAAA")),
            StreamDelta::TextDelta("Checking".into()),
            StreamDelta::TextDelta(" now.".into()),
            StreamDelta::ToolCallStart {
                index: 0,
                id: "call_9".into(),
                name: "get_weather".into(),
            },
            StreamDelta::ToolCallArgsDelta {
                index: 0,
                fragment: "{\"city\":".into(),
            },
            StreamDelta::ToolCallArgsDelta {
                index: 0,
                fragment: "\"Oslo\"}".into(),
            },
            StreamDelta::ServiceTier("default".into()),
            StreamDelta::Usage(ir_usage()),
            // The sealed item recorded by its `done`'s encrypted content,
            // which the terminal event's copy left out (WP8 verify V-F4).
            StreamDelta::UpstreamItems(UpstreamItems {
                response_id: "resp_1".into(),
                items: vec![
                    UpstreamItem {
                        id: "rs_1".into(),
                        encrypted_sha256: Some(crate::ir::encrypted_sha256("gAAAA")),
                    },
                    UpstreamItem::plain("msg_1"),
                    UpstreamItem::plain("fc_1"),
                ],
            }),
            StreamDelta::Stop(FinishReason::ToolUse),
        ]
    );
}

/// Two reasoning items: a blank line between their summaries too.
#[test]
fn a_second_reasoning_item_s_summary_is_set_apart() {
    let body = sse(&[
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
               "summary_index": 0, "delta": "One."}),
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 2,
               "summary_index": 0, "delta": "Two."}),
    ]);
    assert_eq!(
        decode(&body),
        vec![
            StreamDelta::ReasoningDelta("One.".into()),
            StreamDelta::ReasoningDelta("\n\nTwo.".into()),
        ]
    );
}

/// A stream cut before its terminal event: what came is read, and there is
/// no tier, no usage and no stop, but the error that it was cut, never a
/// clean end. `response.created`'s tier is the requested one, not what
/// served the request.
#[test]
fn a_cut_stream_has_no_tier_no_usage_and_ends_with_an_error() {
    let body = sse(&[
        created("flex"),
        json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0,
               "delta": "Partial"}),
    ]);
    assert_eq!(
        decode_to_end(&body),
        vec![
            StreamDelta::TextDelta("Partial".into()),
            StreamDelta::Error(CUT_BEFORE_TERMINAL.into()),
        ]
    );
    // A stream that reached its terminal event ends as it said; a chat
    // chunk stream has no such event to miss.
    let done = sse(&[json!({"type": "response.completed", "response": {
        "status": "completed", "output": []}})]);
    assert_eq!(
        decode_to_end(&done),
        vec![StreamDelta::Stop(FinishReason::Stop)]
    );
    let chat = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"}}]}\n\n";
    assert_eq!(
        decode_to_end(chat),
        vec![StreamDelta::TextDelta("a".into())]
    );
}

#[test]
fn an_incomplete_response_stops_for_its_reason() {
    for (reason, want) in [
        ("max_output_tokens", FinishReason::Length),
        ("content_filter", FinishReason::ContentFilter),
    ] {
        let body = sse(&[
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"type": "function_call", "call_id": "c1", "name": "f"}}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                   "delta": "{\"a\":"}),
            json!({"type": "response.incomplete", "response": {
                "status": "incomplete", "incomplete_details": {"reason": reason},
                "usage": usage(), "output": []}}),
        ]);
        let got = decode(&body);
        // A call cut short is not a call to run.
        assert_eq!(got.last(), Some(&StreamDelta::Stop(want)), "{reason}");
        assert!(got.contains(&StreamDelta::Usage(ir_usage())));
    }
}

/// A reason lmgw has no stop for (`max_messages`, `steered`, one added
/// later) or none at all is `length`: cut short, in a word every client's
/// wire has, whole and streamed.
#[test]
fn an_unknown_or_missing_incomplete_reason_is_length() {
    for details in [
        json!({"reason": "max_messages"}),
        json!({"reason": "steered"}),
        json!({"reason": "a_reason_from_next_year"}),
        json!(null),
    ] {
        let response = json!({"status": "incomplete", "incomplete_details": details,
                              "output": []});
        let body = sse(&[json!({"type": "response.incomplete", "response": response})]);
        assert_eq!(
            decode(&body).last(),
            Some(&StreamDelta::Stop(FinishReason::Length)),
            "{details}"
        );
        let whole = parse(&whole(json!([]), response.clone())).unwrap();
        assert_eq!(whole.finish_reason, FinishReason::Length, "{details}");
    }
}

#[test]
fn an_error_event_and_a_failed_response_end_the_stream_with_an_error() {
    let body = sse(&[json!({"type": "error", "code": "server_error",
                             "message": "The server had an error.", "param": null})]);
    assert_eq!(
        decode(&body),
        vec![StreamDelta::Error(
            "The server had an error. (server_error)".into()
        )]
    );
    let body = sse(&[json!({"type": "response.failed", "response": {
        "status": "failed", "usage": usage(),
        "error": {"code": "server_error", "message": "The model crashed."}}})]);
    assert_eq!(
        decode(&body),
        vec![
            StreamDelta::Usage(ir_usage()),
            StreamDelta::Error("The model crashed. (server_error)".into()),
        ]
    );
}

/// A refusal is the model's words, read as the refusal it is: each client
/// wire writes it as its own refusal where it has one.
#[test]
fn a_refusal_is_read_as_a_refusal() {
    let body = sse(&[
        json!({"type": "response.refusal.delta", "output_index": 0, "content_index": 0,
               "delta": "I can't help with that."}),
        json!({"type": "response.completed", "response": {"status": "completed",
               "output": []}}),
    ]);
    assert_eq!(
        decode(&body),
        vec![
            StreamDelta::RefusalDelta("I can't help with that.".into()),
            StreamDelta::Stop(FinishReason::Stop),
        ]
    );
}

/// A call seen only in its `done` (no `added`, arguments whole) is read from
/// it; a call without a `call_id` gets a minted one; the call ordinals count
/// calls, not items.
#[test]
fn a_call_read_from_its_done_and_one_without_an_id() {
    let body = sse(&[
        json!({"type": "response.output_item.done", "output_index": 1,
               "item": {"type": "function_call", "call_id": "c1", "name": "f",
                        "arguments": "{}"}}),
        json!({"type": "response.output_item.added", "output_index": 3,
               "item": {"type": "function_call", "name": "g", "arguments": ""}}),
        json!({"type": "response.output_item.done", "output_index": 3,
               "item": {"type": "function_call", "name": "g", "arguments": "{\"x\":1}"}}),
    ]);
    let got = decode(&body);
    assert_eq!(
        got[..2],
        [
            StreamDelta::ToolCallStart {
                index: 0,
                id: "c1".into(),
                name: "f".into()
            },
            StreamDelta::ToolCallArgsDelta {
                index: 0,
                fragment: "{}".into()
            },
        ]
    );
    match &got[2] {
        StreamDelta::ToolCallStart { index, id, name } => {
            assert_eq!((*index, name.as_str()), (1, "g"));
            assert!(id.starts_with("call_"), "{id}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        got[3],
        StreamDelta::ToolCallArgsDelta {
            index: 1,
            fragment: "{\"x\":1}".into()
        }
    );
    assert_eq!(got.len(), 4);
}

/// The platform's decoder still reads a chat stream: a send on the Chat
/// wire (`stop` set) answers in chunks.
#[test]
fn the_platform_decoder_reads_chat_chunks_too() {
    let body = "data: {\"service_tier\":\"default\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n\
                data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                data: [DONE]\n\n";
    assert_eq!(
        decode(body),
        vec![
            StreamDelta::ServiceTier("default".into()),
            StreamDelta::TextDelta("Hi".into()),
            StreamDelta::Stop(FinishReason::Stop),
        ]
    );
}

fn whole(output: Value, extra: Value) -> Value {
    let mut v = json!({"id": "resp_2", "object": "response", "status": "completed",
                       "model": "gpt-x-2026", "service_tier": "flex", "usage": usage(),
                       "output": output});
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    v
}

#[test]
fn a_whole_response_reads_text_calls_reasoning_usage_and_tier() {
    let c = parse(&whole(
        json!([
            {"type": "reasoning", "id": "rs_1", "summary": [
                {"type": "summary_text", "text": "First."},
                {"type": "summary_text", "text": "Second."}]},
            {"type": "message", "id": "msg_1", "role": "assistant", "content": [
                {"type": "output_text", "text": "Calling.", "annotations": []}]},
            {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "f",
             "arguments": "{\"a\":1}"},
        ]),
        json!({}),
    ))
    .unwrap();
    assert_eq!(
        c.content,
        vec![
            ContentPart::text("Calling."),
            ContentPart::ToolUse {
                id: "call_1".into(),
                name: "f".into(),
                args: json!({"a": 1}),
            },
        ]
    );
    assert_eq!(c.reasoning, "First.\n\nSecond.");
    assert_eq!(c.finish_reason, FinishReason::ToolUse);
    assert_eq!(c.usage, ir_usage());
    assert_eq!(c.service_tier.as_deref(), Some("flex"));
    assert_eq!(c.model, "gpt-x-2026");
    assert_eq!(
        c.upstream_items,
        Some(UpstreamItems {
            response_id: "resp_2".into(),
            items: ["rs_1", "msg_1", "fc_1"].map(UpstreamItem::plain).to_vec(),
        })
    );
}

#[test]
fn a_whole_response_s_refusal_incompleteness_and_failure() {
    let refusal = parse(&whole(
        json!([{"type": "message", "content": [{"type": "refusal", "refusal": "No."}]}]),
        json!({}),
    ))
    .unwrap();
    assert_eq!(
        refusal.content,
        vec![ContentPart::Refusal { text: "No.".into() }]
    );
    assert_eq!(refusal.finish_reason, FinishReason::Stop);
    let cut = parse(&whole(
        json!([]),
        json!({"status": "incomplete",
               "incomplete_details": {"reason": "max_output_tokens"}}),
    ))
    .unwrap();
    assert_eq!(cut.finish_reason, FinishReason::Length);
    let failed = parse(&whole(
        json!([]),
        json!({"status": "failed",
               "error": {"code": "server_error", "message": "It broke."}}),
    ))
    .unwrap_err();
    assert_eq!(
        failed.to_string(),
        "the upstream's response failed: It broke. (server_error)"
    );
    // An upstream error, not a transport one: OpenAI's own code reaches the
    // client, and the usage and tier it reported stay for the row.
    assert_eq!(
        (failed.kind(), failed.http_status().as_u16()),
        ("upstream", 502)
    );
    assert_eq!(failed.to_openai_json()["error"]["code"], "server_error");
    let f = failed
        .failed_answer()
        .expect("the report stays with the error");
    assert_eq!(f.usage, ir_usage());
    assert_eq!(f.service_tier.as_deref(), Some("flex"));
    // A failed Response without an `error` says so in lmgw's words, and
    // without usage leaves the row to count locally.
    let bare = parse(&json!({"object": "response", "status": "failed", "output": []})).unwrap_err();
    let f = bare.failed_answer().unwrap();
    assert_eq!((f.code.as_deref(), f.usage.reported()), (None, false));
}

#[test]
fn a_response_object_is_told_from_a_chat_completion() {
    assert!(is_response_object(
        &json!({"object": "response", "output": []})
    ));
    assert!(is_response_object(&json!({"output": []})));
    assert!(!is_response_object(
        &json!({"object": "chat.completion", "choices": []})
    ));
    assert!(!is_response_object(&json!({"error": {"message": "x"}})));
}

/// A real stream (gpt-6-luna, 96 summary deltas,
/// then a call; `tests/fixtures/openai_platform/`, its `encrypted_content`
/// scrubbed): the summary in order, the call by its own id with its
/// arguments, the tier and usage of the terminal event only, the answer's
/// items, then the stop for the call, and no error at its end.
#[test]
fn a_real_stream_reads_whole() {
    let body =
        include_str!("../../../../../tests/fixtures/openai_platform/luna_reasoning_then_call.sse");
    let got = decode_to_end(body);
    let reasoning: String = got
        .iter()
        .filter_map(|d| match d {
            StreamDelta::ReasoningDelta(r) => Some(r.as_str()),
            _ => None,
        })
        .collect();
    // The summary's own `done` text, which the deltas add up to.
    let done = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .find(|e| e["type"] == "response.reasoning_summary_text.done")
        .unwrap();
    assert_eq!(reasoning, done["text"].as_str().unwrap());
    let args: String = got
        .iter()
        .filter_map(|d| match d {
            StreamDelta::ToolCallArgsDelta { index: 0, fragment } => Some(fragment.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(args, "{\"city\":\"Graz\"}");
    assert!(got.contains(&StreamDelta::ToolCallStart {
        index: 0,
        id: "call_pL5mA035sWqoalT0IvVGwzrb".into(),
        name: "get_weather".into(),
    }));
    let tail = &got[got.len() - 4..];
    assert_eq!(tail[0], StreamDelta::ServiceTier("default".into()));
    assert_eq!(
        tail[1],
        StreamDelta::Usage(Usage {
            prompt_tokens: Some(90),
            completion_tokens: Some(94),
            cached_input_tokens: Some(0),
            cache_write_tokens: Some(0),
            reasoning_tokens: Some(73),
        })
    );
    let StreamDelta::UpstreamItems(items) = &tail[2] else {
        panic!("{:?}", tail[2]);
    };
    assert_eq!(
        items.response_id,
        "resp_0635d08921a88cfc016addf1eec28087d296f158a202bfaeb4"
    );
    assert_eq!(items.items.len(), 2, "{items:?}");
    // The reasoning item with its encrypted content's hash (S-R3).
    assert_eq!(
        items.items[0].encrypted_sha256,
        Some(crate::ir::encrypted_sha256("gAAAAAB-scrubbed"))
    );
    assert_eq!(tail[3], StreamDelta::Stop(FinishReason::ToolUse));
    assert!(!got.iter().any(|d| matches!(d, StreamDelta::Error(_))));
    // `response.created` echoed the requested tier: it is not read.
    assert_eq!(
        got.iter()
            .filter(|d| matches!(d, StreamDelta::ServiceTier(_)))
            .count(),
        1
    );
}
