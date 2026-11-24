use serde_json::json;

use super::*;

/// A compaction entry's cache counters and its `cache_creation` split are
/// read in the IR's meaning, and entries add up; a `message` entry is not
/// counted.
#[test]
fn a_compaction_entry_s_split_is_summed_in() {
    let list = json!([
        {"type": "compaction", "input_tokens": 100, "output_tokens": 20,
         "cache_read_input_tokens": 50, "cache_creation_input_tokens": 70,
         "cache_creation": {"ephemeral_5m_input_tokens": 20, "ephemeral_1h_input_tokens": 50}},
        {"type": "compaction", "input_tokens": 1, "output_tokens": 2,
         "cache_creation_input_tokens": 5,
         "cache_creation": {"ephemeral_5m_input_tokens": 5, "ephemeral_1h_input_tokens": 0}},
        {"type": "message", "input_tokens": 999, "output_tokens": 999},
    ]);
    let u = compaction_usage(&list).unwrap();
    assert_eq!(u.prompt_tokens, Some(100 + 50 + 70 + 1 + 5));
    assert_eq!(u.completion_tokens, Some(22));
    assert_eq!(u.cached_input_tokens, Some(50));
    assert_eq!(u.cache_write_tokens, Some(75));
    assert_eq!(u.cache_write_1h_tokens, Some(50));
    assert_eq!(compaction_usage(&json!([{"type": "message"}])), None);
}
