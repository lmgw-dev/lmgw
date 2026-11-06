//! A compaction's usage on the Messages wire (gateway design §6.1).
//!
//! The IR's usage counts a compaction's tokens in, which Anthropic leaves out
//! of its top-level counts and reports in `usage.iterations`
//! (`egress::anthropic::compaction`). The Messages wire answers as
//! Anthropic did: the top-level counts without them ([`top_level`]), the
//! iterations verbatim beside them.

use crate::ir::compaction::{compaction_usage, UsageIterations};
use crate::ir::Usage;

/// `u` without the compaction entries of `iterations`: Anthropic's own
/// top-level counts again, with its own field set (a cache counter the top
/// level left out stays out, though a compaction entry had it).
pub(super) fn top_level(u: &Usage, iterations: Option<&UsageIterations>) -> Usage {
    let Some((it, spent)) = iterations.and_then(|it| Some((it, compaction_usage(&it.list)?)))
    else {
        return *u;
    };
    let less = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_sub(b)),
        (a, _) => a,
    };
    Usage {
        prompt_tokens: less(u.prompt_tokens, spent.prompt_tokens),
        completion_tokens: less(u.completion_tokens, spent.completion_tokens),
        cached_input_tokens: less(u.cached_input_tokens, spent.cached_input_tokens)
            .filter(|_| it.top_cache_read),
        cache_write_tokens: less(u.cache_write_tokens, spent.cache_write_tokens)
            .filter(|_| it.top_cache_write),
        reasoning_tokens: u.reasoning_tokens,
    }
}
