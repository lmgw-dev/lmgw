//! An ended task's result as its row stores it (`mcp_tasks.result`) until
//! it enters its thread (MCP Tasks design §2.2): what the model is given —
//! IR tool-result blocks — and, beside them, the result's
//! `structuredContent` for the thread's readers, which the blocks carry only
//! when the result's `content` was empty (client-apps design §7.5).
//!
//! A result without structured content is stored as the bare block array,
//! as every row was before; one with it as `{"blocks": […],
//! "structured_content": …}`. Both read.

use serde_json::{json, Value};

use crate::ir::ToolResultBlock;

/// `blocks` and `structured` as the row stores them.
pub(crate) fn encode(blocks: &[ToolResultBlock], structured: Option<&Value>) -> String {
    match structured {
        None => serde_json::to_string(blocks),
        Some(s) => serde_json::to_string(&json!({"blocks": blocks, "structured_content": s})),
    }
    .unwrap_or_default()
}

/// A stored result: its blocks and its structured content. `None` when it
/// does not read (a hand-edited row).
pub(crate) fn decode(raw: &str) -> Option<(Vec<ToolResultBlock>, Option<Value>)> {
    match serde_json::from_str::<Value>(raw).ok()? {
        Value::Array(a) => serde_json::from_value(Value::Array(a))
            .ok()
            .map(|b| (b, None)),
        Value::Object(mut o) => {
            let blocks = serde_json::from_value(o.remove("blocks")?).ok()?;
            Some((
                blocks,
                o.remove("structured_content").filter(|v| !v.is_null()),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_shapes_read() {
        let blocks = ToolResultBlock::one("job 7f3a (wx__run) completed\nsunny");
        let bare = encode(&blocks, None);
        assert!(bare.starts_with('['), "{bare}");
        assert_eq!(decode(&bare), Some((blocks.clone(), None)));
        let with = encode(&blocks, Some(&json!({"temp": 21})));
        assert_eq!(decode(&with), Some((blocks, Some(json!({"temp": 21})))));
        assert_eq!(decode("not json"), None);
    }
}
