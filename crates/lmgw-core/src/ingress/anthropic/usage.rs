//! The IR's usage on the Messages wire (gateway design §6.1).

use serde_json::{json, Value};

use super::compaction;
use crate::ir::{cache_writes, Usage, UsageIterations};

/// The input counters of Anthropic's `usage`: what a stream's closing
/// `message_delta` carries beside `output_tokens` when the upstream
/// reported its input. Anthropic's `MessageDeltaUsage` has the flat counters
/// only: the `cache_creation` object belongs to `message_start` and a whole
/// answer's `usage`.
pub(super) const INPUT_KEYS: [&str; 3] = [
    "input_tokens",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
];

/// The IR's usage, back in Anthropic's three-disjoint-counters shape.
///
/// `ir::Usage::prompt_tokens` is the **total** input with cache included (the
/// egress adapter sums Anthropic's three counters into it, so the same
/// conversation reports the same size in either dialect). Emitting that total
/// as `input_tokens` would inflate what an Anthropic client sees by the cache
/// counts and drop the cache fields entirely — so the sum is undone here, and
/// the round trip is exact.
///
/// `iterations` is an Anthropic answer's `usage.iterations`
/// ([`crate::ir::Completion::usage_iterations`]): relayed verbatim, and its
/// compaction entries, which the IR's usage counts in, taken out of the
/// top-level counts again ([`compaction::top_level`]).
pub(super) fn anthropic_usage(u: &Usage, iterations: Option<&UsageIterations>) -> Value {
    let u = &compaction::top_level(u, iterations);
    let cached = u.cached_input_tokens;
    let written = u.cache_write_tokens;
    let plain = u
        .prompt_tokens
        .unwrap_or(0)
        .saturating_sub(cached.unwrap_or(0))
        .saturating_sub(written.unwrap_or(0));
    let mut usage = json!({
        "input_tokens": plain,
        "output_tokens": u.completion_tokens.unwrap_or(0),
    });
    // Only when the upstream reported them: an absent counter and a zero one
    // are different statements, and a client that caches reads the difference.
    if let Some(v) = cached {
        usage["cache_read_input_tokens"] = json!(v);
    }
    if let Some(v) = written {
        usage["cache_creation_input_tokens"] = json!(v);
    }
    // The writes' split by TTL, beside their total, when the upstream
    // reported it (`ir::cache_writes`).
    if let Some(split) = cache_writes::creation_object(u) {
        usage["cache_creation"] = split;
    }
    if let Some(it) = iterations {
        usage["iterations"] = it.list.clone();
    }
    usage
}

#[cfg(test)]
mod tests;
