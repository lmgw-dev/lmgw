//! A compaction's usage on an Anthropic answer (gateway design §6.1).
//!
//! An answer that ran a compaction reports it in `usage.iterations`, as a
//! `compaction` entry beside the `message` ones, and leaves it out of the
//! top-level counts; Anthropic bills the sum over every iteration. The IR's
//! usage is what the request cost, so the egress adds the compaction
//! entries in ([`billed`]); the iterations go to the Messages wire verbatim
//! ([`iterations`]), which takes them out again for its top-level counts.

use serde_json::Value;

use crate::ir::compaction::{compaction_usage, UsageIterations};
use crate::ir::Usage;

/// The `iterations` of an answer's `usage`, when it carries them.
pub(super) fn iterations(usage: Option<&Value>) -> Option<&Value> {
    usage?.get("iterations").filter(|it| it.is_array())
}

/// The iterations of an answer's `usage` for the IR, with the cache
/// counters the top level carried: `top`'s, or on a stream `start`'s
/// (`message_start`'s, which a `message_delta` stands on).
pub(super) fn carried(
    usage: Option<&Value>,
    top: &Usage,
    start: Option<&Usage>,
) -> Option<UsageIterations> {
    let list = iterations(usage)?.clone();
    let had =
        |f: fn(&Usage) -> Option<u64>| f(top).is_some() || start.is_some_and(|s| f(s).is_some());
    Some(UsageIterations {
        list,
        top_cache_read: had(|u| u.cached_input_tokens),
        top_cache_write: had(|u| u.cache_write_tokens),
    })
}

/// `top` (the answer's top-level counts) with the compaction entries of
/// its `iterations` added. A stream's `message_delta` that reports no input
/// stands on `start`'s (`message_start`'s), so the compaction's input is
/// added to that and not put in its place.
pub(super) fn billed(mut top: Usage, usage: Option<&Value>, start: Option<&Usage>) -> Usage {
    let Some(spent) = iterations(usage).and_then(compaction_usage) else {
        return top;
    };
    if top.prompt_tokens.is_none() {
        if let Some(start) = start {
            top.prompt_tokens = start.prompt_tokens;
            top.cached_input_tokens = start.cached_input_tokens;
            top.cache_write_tokens = start.cache_write_tokens;
        }
    }
    top.add(&spent);
    top
}
