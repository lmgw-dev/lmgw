//! The two modes of a Messages stream's `message_start`, through the
//! encoder.

use serde_json::{json, Value};

use crate::config::Protocol;
use crate::ingress::anthropic::AnthropicStreamEncoder;
use crate::ingress::ClientStreamEncoder;
use crate::ir::{FinishReason, StreamDelta, Usage};

/// The events of an SSE text, as `(event, data)`.
fn events(text: &str) -> Vec<(String, Value)> {
    text.split("\n\n")
        .filter(|f| !f.trim().is_empty())
        .map(|f| {
            let name = f
                .lines()
                .find_map(|l| l.strip_prefix("event: "))
                .unwrap_or_default()
                .to_string();
            let data = f
                .lines()
                .find_map(|l| l.strip_prefix("data: "))
                .map(|d| serde_json::from_str(d).unwrap())
                .unwrap_or_default();
            (name, data)
        })
        .collect()
}

fn names(text: &str) -> Vec<String> {
    events(text).into_iter().map(|(n, _)| n).collect()
}

/// An Anthropic `message_start`'s usage, as the decoder hands it on.
fn input() -> Usage {
    Usage {
        prompt_tokens: Some(12 + 410 + 7480),
        completion_tokens: Some(1),
        cached_input_tokens: Some(7480),
        cache_write_tokens: Some(410),
        cache_write_1h_tokens: Some(300),
        reasoning_tokens: None,
    }
}

/// The whole stream an encoder writes for `deltas`, `start()` first.
fn run(mut enc: AnthropicStreamEncoder, deltas: &[StreamDelta]) -> (String, String) {
    let opened = enc.start();
    let mut out = opened.clone();
    for d in deltas {
        out.push_str(&enc.delta(d));
    }
    out.push_str(&enc.finish());
    (opened, out)
}

fn held() -> AnthropicStreamEncoder {
    AnthropicStreamEncoder::for_upstream("m", Protocol::Anthropic)
}

#[test]
fn held_the_first_usage_lands_in_message_start() {
    let (opened, out) = run(
        held(),
        &[
            StreamDelta::Usage(input()),
            StreamDelta::TextDelta("hi".into()),
            StreamDelta::Usage(Usage {
                completion_tokens: Some(5),
                ..Usage::default()
            }),
            StreamDelta::Stop(FinishReason::Stop),
        ],
    );
    assert_eq!(opened, "", "nothing before the upstream said anything");
    assert_eq!(
        names(&out),
        [
            "message_start",
            "ping",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    let ev = events(&out);
    assert_eq!(
        ev[0].1["message"]["usage"],
        json!({
            "input_tokens": 12,
            "cache_creation_input_tokens": 410,
            "cache_read_input_tokens": 7480,
            "cache_creation": {"ephemeral_5m_input_tokens": 110, "ephemeral_1h_input_tokens": 300},
            "output_tokens": 1,
        })
    );
    // The closing usage is the totals, as before.
    assert_eq!(ev[5].1["usage"]["output_tokens"], 5);
    assert_eq!(ev[5].1["usage"]["cache_read_input_tokens"], 7480);
}

#[test]
fn held_a_first_delta_that_is_no_usage_opens_with_zeros() {
    let (_, out) = run(
        held(),
        &[
            StreamDelta::TextDelta("hi".into()),
            StreamDelta::Stop(FinishReason::Stop),
        ],
    );
    let ev = events(&out);
    assert_eq!(ev[0].0, "message_start");
    assert_eq!(
        ev[0].1["message"]["usage"],
        json!({"input_tokens": 0, "output_tokens": 0})
    );
    assert_eq!(ev[1].0, "ping");
    assert_eq!(ev[2].0, "content_block_start");
}

#[test]
fn held_a_stream_with_no_delta_still_opens_the_message() {
    let (_, out) = run(held(), &[]);
    assert_eq!(
        names(&out),
        ["message_start", "ping", "message_delta", "message_stop"]
    );
}

#[test]
fn held_an_error_first_goes_out_alone() {
    let (_, out) = run(
        held(),
        &[
            StreamDelta::Error("overloaded".into()),
            StreamDelta::TextDelta("late".into()),
        ],
    );
    assert_eq!(names(&out), ["error"], "{out}");
}

/// Every other upstream: `message_start` at `start()`, with zeros, and a
/// first usage goes to the closing `message_delta` only.
#[test]
fn now_message_start_goes_out_at_once_with_zeros() {
    let enc = AnthropicStreamEncoder::for_upstream("m", Protocol::OpenaiCompatible);
    let (opened, out) = run(
        enc,
        &[
            StreamDelta::Usage(input()),
            StreamDelta::TextDelta("hi".into()),
            StreamDelta::Stop(FinishReason::Stop),
        ],
    );
    assert_eq!(names(&opened), ["message_start", "ping"]);
    let ev = events(&out);
    assert_eq!(
        ev[0].1["message"]["usage"],
        json!({"input_tokens": 0, "output_tokens": 0})
    );
    let delta = ev.iter().find(|(n, _)| n == "message_delta").unwrap();
    assert_eq!(delta.1["usage"]["cache_read_input_tokens"], 7480);
}
