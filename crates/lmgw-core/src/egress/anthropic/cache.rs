//! The prompt-caching marks and Anthropic tool fields of a Messages body
//! (gateway design §7, "Prompt caching", 2026-11-12;
//! [`crate::ir::cache`]): the `system` prompt as the client wrote it, a
//! string or its blocks with their marks, and each tool with its fields.
//! The content blocks' marks are written where the blocks are
//! (`super::content_blocks`, `super::tool_result_blocks_json`), the
//! top-level one with the client's other top-level keys
//! (`super::verbatim_keys`).

use serde_json::{json, Value};

use crate::ir::{CacheControl, ChatRequest, ContentPart, Role, ToolDef};

/// The `system` value of `ir`. With no mark on any part and exactly one
/// text part in every System message, the string it always was: the
/// messages' texts joined by a blank line ([`ChatRequest::system_text`]),
/// so a request from another dialect with several System messages reaches
/// Anthropic as before. Otherwise (a mark, or a System message of several
/// text parts, which only the Messages ingress makes) an array of `text`
/// blocks, one per part, each with its mark. An empty text is left out
/// (Anthropic refuses an empty text block). `None` when there is no system
/// text.
pub(super) fn system(ir: &ChatRequest) -> Option<Value> {
    let mut parts: Vec<(&str, Option<&CacheControl>)> = Vec::new();
    let mut simple = true;
    for m in ir.messages.iter().filter(|m| m.role == Role::System) {
        let mut texts = 0;
        for p in &m.content {
            if let ContentPart::Text {
                text,
                cache_control,
                ..
            } = p
            {
                texts += 1;
                simple &= cache_control.is_none();
                if !text.is_empty() {
                    parts.push((text.as_str(), cache_control.as_ref()));
                }
            }
        }
        simple &= texts <= 1;
    }
    if parts.is_empty() {
        return None;
    }
    if simple {
        return ir.system_text().map(Value::String);
    }
    Some(Value::Array(
        parts
            .into_iter()
            .map(|(text, mark)| {
                let mut block = json!({"type": "text", "text": text});
                CacheControl::mark(&mut block, mark);
                block
            })
            .collect(),
    ))
}

/// One custom tool: its name, schema and description, and its other
/// Anthropic fields as the client wrote them, a mark included.
pub(super) fn tool(t: &ToolDef) -> Value {
    let mut tool = json!({
        "name": t.name,
        "input_schema": t.parameters,
    });
    // The key is left out when there is no description: Anthropic expects
    // a string there, never null.
    if let Some(d) = &t.description {
        tool["description"] = json!(d);
    }
    if let Some(o) = tool.as_object_mut() {
        for (k, v) in &t.anthropic_fields {
            o.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    tool
}

#[cfg(test)]
mod tests;
