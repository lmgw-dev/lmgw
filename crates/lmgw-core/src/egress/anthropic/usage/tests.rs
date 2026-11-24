use serde_json::json;

use super::*;

#[test]
fn the_cache_creation_split_is_read() {
    let u = parse_usage(Some(&json!({
        "input_tokens": 12,
        "cache_creation_input_tokens": 410,
        "cache_read_input_tokens": 7480,
        "cache_creation": {"ephemeral_5m_input_tokens": 110, "ephemeral_1h_input_tokens": 300},
        "output_tokens": 5,
    })));
    assert_eq!(
        u,
        Usage {
            prompt_tokens: Some(12 + 410 + 7480),
            completion_tokens: Some(5),
            cached_input_tokens: Some(7480),
            cache_write_tokens: Some(410),
            cache_write_1h_tokens: Some(300),
            reasoning_tokens: None,
        }
    );
}

/// No object, no split: the 1-hour part is unknown, never a zero. An
/// object of zeros is a real zero.
#[test]
fn no_object_is_no_split_and_a_zero_object_is_a_zero() {
    let flat = parse_usage(Some(&json!({
        "input_tokens": 3, "cache_creation_input_tokens": 20, "output_tokens": 1,
    })));
    assert_eq!(
        (flat.cache_write_tokens, flat.cache_write_1h_tokens),
        (Some(20), None)
    );
    let zero = parse_usage(Some(&json!({
        "input_tokens": 3, "cache_creation_input_tokens": 0, "output_tokens": 1,
        "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0},
    })));
    assert_eq!(
        (zero.cache_write_tokens, zero.cache_write_1h_tokens),
        (Some(0), Some(0))
    );
    assert_eq!(parse_usage(None), Usage::default());
}

/// A `message_delta` without the object keeps `message_start`'s 1-hour
/// part when its write total is the start's or larger (the extra writes are
/// 5-minute ones), and has none when the total is smaller.
#[test]
fn a_message_delta_keeps_the_start_s_split() {
    let start = parse_usage(Some(&json!({
        "input_tokens": 3, "cache_creation_input_tokens": 20, "output_tokens": 1,
        "cache_creation": {"ephemeral_5m_input_tokens": 5, "ephemeral_1h_input_tokens": 15},
    })));
    let same = parse_usage(Some(&json!({
        "input_tokens": 3, "cache_creation_input_tokens": 20, "output_tokens": 9,
    })));
    assert_eq!(
        with_start_split(same, &start).cache_write_1h_tokens,
        Some(15)
    );
    let larger = parse_usage(Some(&json!({
        "input_tokens": 3, "cache_creation_input_tokens": 30, "output_tokens": 9,
    })));
    assert_eq!(
        with_start_split(larger, &start).cache_write_1h_tokens,
        Some(15)
    );
    let smaller = parse_usage(Some(&json!({
        "input_tokens": 3, "cache_creation_input_tokens": 10, "output_tokens": 9,
    })));
    assert_eq!(
        with_start_split(smaller, &start).cache_write_1h_tokens,
        None
    );
    let output_only = parse_usage(Some(&json!({"output_tokens": 9})));
    assert_eq!(
        with_start_split(output_only, &start).cache_write_1h_tokens,
        None
    );
}
