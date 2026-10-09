//! IR messages → the OpenAI `messages` array (llama-egress design §2), with
//! the rendering of a tool result as the one part an egress chooses.

use serde_json::{json, Map, Value};

use crate::ir::{
    flatten_tool_result, wire_call_id, ChatRequest, ContentPart, ImageSource, Role, ToolResultBlock,
};

/// How a tool result becomes the `content` of its `role: "tool"` message.
/// [`messages_json`] calls it once per `ToolResult` part, with the call's id
/// and the result's blocks. [`FlattenToolResults`] is OpenAI's text-only
/// rendering; a server that takes media parts there gets a renderer that
/// returns a `content` array instead of a string (llama-egress design §8.1).
pub trait ToolResultRenderer {
    fn render(&self, id: &str, content: &[ToolResultBlock]) -> Value;
}

/// The text-only rendering: one string, with every block that has no text
/// form replaced by a named placeholder and a WARN.
pub struct FlattenToolResults;

impl ToolResultRenderer for FlattenToolResults {
    fn render(&self, id: &str, content: &[ToolResultBlock]) -> Value {
        // OpenAI's `role: "tool"` message carries text only — this is the one
        // adapter that has to flatten. A lone text block (the common case)
        // comes out byte-identical; anything binary is replaced by a named
        // placeholder and logged, never silently dropped and never base64'd
        // into the prompt (§14).
        let (text, dropped) = flatten_tool_result(content);
        if !dropped.is_empty() {
            tracing::warn!(
                tool_call_id = %id,
                "tool result content not representable on an openai upstream, \
                 replaced with placeholders: {}",
                dropped.join("; ")
            );
        }
        Value::String(text)
    }
}

/// IR messages → OpenAI `messages` array, each tool result rendered by
/// `tool_results`.
pub fn messages_json(ir: &ChatRequest, tool_results: &dyn ToolResultRenderer) -> Vec<Value> {
    let mut out = Vec::new();
    for m in &ir.messages {
        match m.role {
            Role::System => out.push(json!({"role": "system", "content": m.joined_text()})),
            Role::Tool => {
                for p in &m.content {
                    if let ContentPart::ToolResult { id, content, .. } = p {
                        out.push(json!({
                            "role": "tool",
                            // A Gemini signature stays with Gemini (§7.1).
                            "tool_call_id": wire_call_id(id),
                            "content": tool_results.render(wire_call_id(id), content),
                        }));
                    }
                }
            }
            Role::User => out.push(json!({
                "role": "user",
                "content": user_content_json(&m.content),
            })),
            Role::Assistant => {
                let text = m.joined_text();
                let tool_calls: Vec<Value> = m
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::ToolUse { id, name, args } => Some(json!({
                            "id": wire_call_id(id),
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(args)
                                    .unwrap_or_else(|_| "{}".into()),
                            },
                        })),
                        _ => None,
                    })
                    .collect();
                let mut msg = Map::new();
                msg.insert("role".into(), json!("assistant"));
                msg.insert(
                    "content".into(),
                    if text.is_empty() && !tool_calls.is_empty() {
                        Value::Null
                    } else {
                        Value::String(text)
                    },
                );
                if !tool_calls.is_empty() {
                    msg.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                // The turn's reasoning trace, under the name llama-server (and
                // DeepSeek) read it back as. With `--reasoning-preserve` the
                // template renders it into the prompt again; without, the
                // server discards it itself — its call, not the gateway's.
                let reasoning = m.reasoning_text();
                if !reasoning.is_empty() {
                    msg.insert("reasoning_content".into(), Value::String(reasoning));
                }
                out.push(Value::Object(msg));
            }
        }
    }
    out
}

/// User content: plain string when text-only, parts array when multimodal.
fn user_content_json(parts: &[ContentPart]) -> Value {
    let has_media = parts
        .iter()
        .any(|p| matches!(p, ContentPart::Image { .. } | ContentPart::Audio { .. }));
    if !has_media {
        let text = parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Value::String(text);
    }
    let mut arr = Vec::new();
    for p in parts {
        match p {
            ContentPart::Text { text } => arr.push(json!({"type": "text", "text": text})),
            ContentPart::Image { mime, source } => {
                let url = match source {
                    ImageSource::Url { url } => url.clone(),
                    ImageSource::Base64 { data } => format!("data:{mime};base64,{data}"),
                };
                arr.push(json!({"type": "image_url", "image_url": {"url": url}}));
            }
            // Raw base64 (not a data: URI) — llama-server sniffs the bytes
            // and ignores `format`, but cloud OpenAI requires it, so it is
            // always sent, derived from the mime's subtype (`audio/wav` →
            // `wav`).
            ContentPart::Audio { mime, data } => {
                let format = mime.split('/').nth(1).unwrap_or(mime.as_str());
                arr.push(json!({
                    "type": "input_audio",
                    "input_audio": {"data": data, "format": format},
                }));
            }
            _ => {}
        }
    }
    Value::Array(arr)
}
