//! A Messages body's prompt-caching marks and Anthropic tool fields, read
//! into the IR (gateway design §6.1, "Prompt caching", 2026-11-12;
//! [`crate::ir::cache`]): each `system` text block a part of its own with
//! its mark, a content block's mark on its part, a tool's keys the IR does
//! not model kept verbatim.

use serde_json::{Map, Value};

use crate::ir::{CacheControl, ContentPart, Message, Role};

/// The `system` prompt as one System message: a string is one text part, a
/// block array one text part per non-empty text block, each with its mark.
/// `None` when there is no text.
pub(super) fn system(v: Option<&Value>) -> Option<Message> {
    let content: Vec<ContentPart> = match v {
        Some(Value::String(s)) if !s.is_empty() => vec![ContentPart::text(s.clone())],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| {
                let text = b.get("text").and_then(Value::as_str)?;
                (!text.is_empty()).then(|| marked(ContentPart::text(text), b))
            })
            .collect(),
        _ => Vec::new(),
    };
    (!content.is_empty()).then_some(Message {
        role: Role::System,
        content,
    })
}

/// The keys the IR's tool models, read into its own fields.
const MODELLED: &[&str] = &["name", "description", "input_schema", "strict"];

/// A client tool's other keys, verbatim ([`crate::ir::ToolDef::anthropic_fields`]):
/// its `cache_control`, `eager_input_streaming`, `input_examples`, a field
/// Anthropic adds next. `"type": "custom"` is the default the egress
/// implies, and a `null` is no value.
pub(super) fn tool_fields(t: &Value) -> Map<String, Value> {
    let Some(o) = t.as_object() else {
        return Map::new();
    };
    o.iter()
        .filter(|(k, v)| {
            !MODELLED.contains(&k.as_str())
                && !v.is_null()
                && !(k.as_str() == "type" && v.as_str() == Some("custom"))
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// `part` with the mark the block `b` carries, where the part takes one
/// (a text, an image).
pub(super) fn marked(mut part: ContentPart, b: &Value) -> ContentPart {
    if let ContentPart::Text { cache_control, .. } | ContentPart::Image { cache_control, .. } =
        &mut part
    {
        *cache_control = CacheControl::of(b);
    }
    part
}
