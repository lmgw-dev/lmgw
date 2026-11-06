//! The chat wire's decoder: the reasoning trace's two names (module doc of
//! [`super::reasoning_text`]), the service tier, timings, call ids, an empty
//! finish reason.

use serde_json::{json, Value};

use crate::egress::openai::OpenaiEgress;
use crate::egress::Egress;
use crate::ir::StreamDelta;
use crate::sse::SseEvent;

/// OpenRouter's `reasoning` string is read as well as llama-server's
/// `reasoning_content`, which wins when both come — streamed and whole.
#[test]
fn reasoning_is_read_under_either_name() {
    let mut dec = OpenaiEgress.new_decoder();
    let mut deltas = Vec::new();
    for c in [
        json!({"choices":[{"delta":{"role":"assistant","reasoning":"Let me "}}]}),
        json!({"choices":[{"delta":{"reasoning":"x","reasoning_content":"think."}}]}),
        // An object under the name is no text.
        json!({"choices":[{"delta":{"reasoning":{"effort":"low"},"content":"OK"}}]}),
    ] {
        deltas.extend(dec.on_event(&SseEvent {
            event: None,
            data: c.to_string(),
        }));
    }
    assert_eq!(
        deltas,
        vec![
            StreamDelta::ReasoningDelta("Let me ".into()),
            StreamDelta::ReasoningDelta("think.".into()),
            StreamDelta::TextDelta("OK".into()),
        ]
    );
    let whole = |m: Value| {
        let body = json!({"model": "m", "choices": [{"message": m}]}).to_string();
        OpenaiEgress
            .parse_completion(body.as_bytes())
            .unwrap()
            .reasoning
    };
    assert_eq!(whole(json!({"content": "OK", "reasoning": "Hm."})), "Hm.");
    assert_eq!(
        whole(json!({"content": "OK", "reasoning": "a", "reasoning_content": "b"})),
        "b"
    );
    assert_eq!(whole(json!({"content": "OK"})), "");
}

/// The platform reads `service_tier` (openai egress design §5.2): streamed,
/// once when first seen and again when it changes, ahead of the chunk's
/// other deltas; whole, onto the completion. A chunk without it changes
/// nothing; the compatible egress reads none of it.
#[test]
fn the_platform_reads_the_service_tier_and_the_compatible_egress_does_not() {
    use crate::egress::openai_compatible::OpenaiCompatibleEgress;
    let chunks = [
        json!({"service_tier": "default", "choices": [{"delta": {"content": "a"}}]}),
        json!({"service_tier": "default", "choices": [{"delta": {"content": "b"}}]}),
        json!({"choices": [{"delta": {"content": "c"}}]}),
        json!({"service_tier": "priority", "choices": [], "usage": {"prompt_tokens": 1}}),
    ];
    let run = |dec: &mut Box<dyn crate::egress::EgressStreamDecoder>| {
        chunks
            .iter()
            .flat_map(|c| {
                dec.on_event(&SseEvent {
                    event: None,
                    data: c.to_string(),
                })
            })
            .filter(|d| !matches!(d, StreamDelta::Usage(_)))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        run(&mut OpenaiEgress.new_decoder()),
        vec![
            StreamDelta::ServiceTier("default".into()),
            StreamDelta::TextDelta("a".into()),
            StreamDelta::TextDelta("b".into()),
            StreamDelta::TextDelta("c".into()),
            StreamDelta::ServiceTier("priority".into()),
        ]
    );
    assert_eq!(
        run(&mut OpenaiCompatibleEgress.new_decoder()),
        vec![
            StreamDelta::TextDelta("a".into()),
            StreamDelta::TextDelta("b".into()),
            StreamDelta::TextDelta("c".into()),
        ]
    );

    let body = |tier: Option<&str>| {
        let mut v = json!({"model": "m", "choices": [{"message": {"content": "ok"}}]});
        if let Some(t) = tier {
            v["service_tier"] = json!(t);
        }
        v.to_string()
    };
    let whole = |e: &dyn Egress, b: String| e.parse_completion(b.as_bytes()).unwrap().service_tier;
    assert_eq!(
        whole(&OpenaiEgress, body(Some("flex"))).as_deref(),
        Some("flex")
    );
    assert_eq!(
        whole(&OpenaiEgress, body(None)),
        None,
        "absent stays absent"
    );
    assert_eq!(whole(&OpenaiCompatibleEgress, body(Some("flex"))), None);
}

/// llama.cpp writes a non-finite rate as `null`; a missing key is the same
/// "no number". Neither becomes a measured zero.
#[test]
fn a_null_or_missing_timing_is_unknown_not_zero() {
    let t = super::parse_timings(Some(&json!({
        "prompt_n": 10,
        "prompt_ms": null,
        "predicted_n": 4,
        "predicted_ms": 50.0,
        "predicted_per_second": 80.0
    })))
    .expect("prompt_n marks a timings block");
    assert_eq!(
        (
            t.prompt_ms,
            t.prompt_per_second,
            t.predicted_ms,
            t.predicted_per_second
        ),
        (None, None, Some(50.0), Some(80.0))
    );
}

/// A call the upstream sent without an id gets a minted one, unique across
/// the model calls of a tool loop (each its own decoder) and within one
/// answer; a call that names its id keeps it. Each step's `call_{index}`
/// once gave a Chat reply two calls named `call_0` (chat-media-tools design
/// §14), and their cards each other's pictures.
#[test]
fn calls_without_an_id_get_one_unique_across_the_loop() {
    let started = |chunks: Vec<Value>| {
        let mut dec = OpenaiEgress.new_decoder();
        chunks
            .into_iter()
            .flat_map(|c| {
                dec.on_event(&SseEvent {
                    event: None,
                    data: c.to_string(),
                })
            })
            .filter_map(|d| match d {
                StreamDelta::ToolCallStart { id, .. } => Some(id),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let call = |index: u64, id: Option<&str>| {
        let mut c = json!({"index": index, "type": "function",
                           "function": {"name": "image__generate", "arguments": ""}});
        if let Some(id) = id {
            c["id"] = json!(id);
        }
        json!({"choices": [{"delta": {"tool_calls": [c]}}]})
    };
    let first = started(vec![call(0, None), call(1, Some(""))]);
    let second = started(vec![call(0, None)]);
    let named = started(vec![call(0, Some("call_abc"))]);
    let all: Vec<&String> = first.iter().chain(&second).collect();
    assert_eq!(all.len(), 3);
    for (i, a) in all.iter().enumerate() {
        assert!(a.starts_with("call_") && a.len() > "call_0".len(), "{a}");
        assert!(all[i + 1..].iter().all(|b| b != a), "{all:?}");
    }
    assert_eq!(named, ["call_abc"]);

    // Whole answers too.
    let whole = |calls: Value| {
        let body = json!({"model": "m", "choices": [{"message":
            {"role": "assistant", "tool_calls": calls}}]})
        .to_string();
        OpenaiEgress
            .parse_completion(body.as_bytes())
            .unwrap()
            .content
            .into_iter()
            .filter_map(|p| match p {
                crate::ir::ContentPart::ToolUse { id, .. } => Some(id),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let f = json!({"name": "a", "arguments": "{}"});
    let ids = whole(json!([{"type": "function", "function": f},
                           {"type": "function", "function": f},
                           {"id": "call_x", "type": "function", "function": f}]));
    assert_eq!(ids.len(), 3);
    assert_ne!(ids[0], ids[1]);
    assert!(!ids[0].is_empty() && ids[0] != "call_0", "{ids:?}");
    assert_eq!(ids[2], "call_x");
}

/// An empty `finish_reason` is none: a provider that sends `""` on every
/// chunk ends its answer with its real reason only.
#[test]
fn an_empty_finish_reason_is_no_stop() {
    let mut dec = OpenaiEgress.new_decoder();
    let mut deltas = Vec::new();
    for c in [
        json!({"choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":""}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ] {
        deltas.extend(dec.on_event(&SseEvent {
            event: None,
            data: c.to_string(),
        }));
    }
    let stops = deltas
        .iter()
        .filter(|d| matches!(d, StreamDelta::Stop(_)))
        .count();
    assert_eq!(stops, 1, "{deltas:?}");
    assert_eq!(deltas[0], StreamDelta::TextDelta("Hi".into()));
}
