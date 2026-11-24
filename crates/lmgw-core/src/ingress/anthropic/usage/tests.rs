use serde_json::{json, Value};

use super::*;
use crate::egress::anthropic::usage::parse_usage;

/// Anthropic's `usage` through the IR and back onto the Messages wire.
fn round_trip(wire: Value) -> Value {
    anthropic_usage(&parse_usage(Some(&wire)), None)
}

#[test]
fn the_split_comes_back_as_it_came() {
    let wire = json!({
        "input_tokens": 12,
        "cache_creation_input_tokens": 410,
        "cache_read_input_tokens": 7480,
        "cache_creation": {"ephemeral_5m_input_tokens": 110, "ephemeral_1h_input_tokens": 300},
        "output_tokens": 5,
    });
    assert_eq!(round_trip(wire.clone()), wire);
}

#[test]
fn an_answer_without_a_split_gets_no_object() {
    let wire = json!({
        "input_tokens": 12,
        "cache_creation_input_tokens": 410,
        "cache_read_input_tokens": 7480,
        "output_tokens": 5,
    });
    assert_eq!(round_trip(wire.clone()), wire);
}

#[test]
fn a_zero_split_is_written_as_zeros() {
    let wire = json!({
        "input_tokens": 12,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
        "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0},
        "output_tokens": 5,
    });
    assert_eq!(round_trip(wire.clone()), wire);
}

/// An OpenAI-shaped upstream's usage has one cache-write counter and no
/// split: no `cache_creation` object is made up for it.
#[test]
fn a_usage_without_the_1h_part_writes_no_object() {
    let u = Usage {
        prompt_tokens: Some(100),
        completion_tokens: Some(2),
        cache_write_tokens: Some(40),
        ..Usage::default()
    };
    let wire = anthropic_usage(&u, None);
    assert_eq!(wire["cache_creation_input_tokens"], 40);
    assert!(wire.get("cache_creation").is_none(), "{wire}");
}
