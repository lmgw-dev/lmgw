//! An Anthropic answer's `usage`, read into the IR's (gateway design §6.1).

use serde_json::Value;

use crate::ir::{cache_writes, Usage};

/// The IR's usage of an Anthropic `usage` object (a whole answer's, a
/// stream's `message_start` or `message_delta`'s). Its compaction entries
/// are [`super::compaction`]'s.
pub(crate) fn parse_usage(v: Option<&Value>) -> Usage {
    let Some(u) = v else {
        return Usage::default();
    };
    let n = |k: &str| u.get(k).and_then(Value::as_u64);
    let cache_read = n("cache_read_input_tokens");
    let cache_write = cache_writes::total(u);
    // Anthropic's three input counters are **disjoint**: `input_tokens`
    // excludes both cache counters. The IR's `prompt_tokens` is the total
    // with cache included (see `ir::Usage`), so they are summed here —
    // otherwise the same conversation reports a different input size
    // depending on which wire shape the client speaks, and a cache-heavy
    // turn looks 10x cheaper than it was.
    let input = n("input_tokens");
    let prompt_tokens = match (input, cache_read, cache_write) {
        (None, None, None) => None,
        _ => Some(input.unwrap_or(0) + cache_read.unwrap_or(0) + cache_write.unwrap_or(0)),
    };
    Usage {
        prompt_tokens,
        completion_tokens: n("output_tokens"),
        cached_input_tokens: cache_read,
        cache_write_tokens: cache_write,
        // The split of the writes by TTL, when Anthropic reported it
        // (`cache_creation`): a 1-hour write costs more than a 5-minute one.
        cache_write_1h_tokens: cache_writes::one_hour(u),
        // Thinking tokens are inside `output_tokens` and Anthropic
        // publishes no separate counter for them.
        reasoning_tokens: None,
    }
}

/// A stream's `message_delta` usage, with `message_start`'s 1-hour part of
/// the cache writes when the delta reports a write total without a split.
/// Anthropic's `message_delta` usage has no `cache_creation` object, and its
/// counts are cumulative and take the place of `message_start`'s
/// ([`Usage::merge`] moves the input group together). A total at least as
/// large as the start's keeps the start's 1-hour part: what was added after
/// `message_start` is Anthropic's own (its server tools write 5-minute
/// entries), so the additional writes count as 5-minute ones. A smaller total
/// contradicts the start's split, so there is none.
pub(super) fn with_start_split(mut delta: Usage, start: &Usage) -> Usage {
    if delta.cache_write_1h_tokens.is_none() {
        if let (Some(total), Some(start_total)) =
            (delta.cache_write_tokens, start.cache_write_tokens)
        {
            if total >= start_total {
                delta.cache_write_1h_tokens = start.cache_write_1h_tokens;
            }
        }
    }
    delta
}

#[cfg(test)]
mod tests;
