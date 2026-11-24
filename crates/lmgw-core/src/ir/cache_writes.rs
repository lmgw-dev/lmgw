//! The split of an Anthropic answer's cache writes by TTL (gateway design
//! §6.1, "Prompt caching").
//!
//! Beside the flat `cache_creation_input_tokens`, Anthropic's `usage`
//! carries `cache_creation: {ephemeral_5m_input_tokens,
//! ephemeral_1h_input_tokens}`: how many of the written tokens went into
//! the cache with the 5-minute TTL and how many with the 1-hour one, which
//! Anthropic bills at 1.25x and 2x the input rate. The IR keeps the 1-hour
//! part ([`super::Usage::cache_write_1h_tokens`]); the 5-minute part is the
//! total minus it. One reader for the Anthropic egress and the compaction
//! entries of `usage.iterations`, one writer for the Messages wire.

use serde_json::{json, Value};

use super::Usage;

/// The total cache writes of an Anthropic `usage` object (or a
/// `usage.iterations` entry): its flat `cache_creation_input_tokens`, else
/// the sum of its `cache_creation` object's members. `None` when it carries
/// neither.
pub fn total(usage: &Value) -> Option<u64> {
    usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            let split = usage.get("cache_creation")?.as_object()?;
            let n = |k: &str| split.get(k).and_then(Value::as_u64).unwrap_or(0);
            Some(n("ephemeral_5m_input_tokens") + n("ephemeral_1h_input_tokens"))
        })
}

/// The 1-hour part of the same object's cache writes:
/// `cache_creation.ephemeral_1h_input_tokens`. `None` when it carries no
/// `cache_creation` object (the upstream reported no split); a
/// `cache_creation` object without the 1-hour member reports none written
/// with that TTL, `Some(0)`.
pub fn one_hour(usage: &Value) -> Option<u64> {
    let split = usage.get("cache_creation")?.as_object()?;
    Some(
        split
            .get("ephemeral_1h_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    )
}

/// `u`'s split as the Messages wire writes it, `cache_creation` with both
/// members: only when the total and the 1-hour part are both known, the
/// 5-minute member the total minus the 1-hour part. `None` for an answer
/// whose upstream reported no split: an absent object and a zero one are
/// different statements.
pub fn creation_object(u: &Usage) -> Option<Value> {
    let (total, hour) = (u.cache_write_tokens?, u.cache_write_1h_tokens?);
    Some(json!({
        "ephemeral_5m_input_tokens": total.saturating_sub(hour),
        "ephemeral_1h_input_tokens": hour,
    }))
}

#[cfg(test)]
mod tests;
