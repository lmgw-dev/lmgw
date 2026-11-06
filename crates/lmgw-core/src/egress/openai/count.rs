//! The platform's local count of an answer whose usage never came (openai
//! egress design §13.4, decision 22, and §13.6 for the chat streams): a
//! stream cut before its terminal event or its usage chunk. OpenAI reports
//! a chat stream's usage only in its last chunk and keeps nothing of a cut
//! Responses stream, so this count is the only source a budget has.
//!
//! Text only, counted once per side with tiktoken: what cannot be read as
//! text (images, audio, files, encrypted reasoning) counts as nothing, and
//! reasoning tokens are never relayed, so the count stays below the real
//! one. A lower bound is all a floor may be.

use super::tiktoken_count;
use crate::config::{Protocol, Route};
use crate::ir::{flatten_tool_result, ChatRequest, Completion, ContentPart, ResponseFormat};
use crate::pricing::LocalCount;

/// The platform's token count of `text` for `model`: tiktoken with the
/// model's own encoding, `o200k_base` for a name tiktoken does not know.
pub fn local_count(model: &str, text: &str) -> u64 {
    tiktoken_count(model, text).0
}

/// [`local_count`] of `prompt` and `relayed` for `model`, on a blocking
/// thread: a long prompt is a long encode. `None` only when that thread
/// failed, which is a log line.
pub async fn count_locally(model: String, prompt: String, relayed: String) -> Option<LocalCount> {
    let counted = tokio::task::spawn_blocking(move || LocalCount {
        prompt: local_count(&model, &prompt),
        output: local_count(&model, &relayed),
        estimated: false,
        uncounted: 0,
    })
    .await;
    match counted {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!("counting a cut stream locally: {e}");
            None
        }
    }
}

/// [`count_locally`] of a chat request sent on `route` and the output text
/// it `relayed`, on an `openai` route only: `None` elsewhere, where lmgw
/// knows no tokenizer for what the upstream serves (§13.4). The prompt's
/// text is read from `ir` here, when a count is needed, never up front.
pub async fn count_chat(route: &Route, ir: &ChatRequest, relayed: String) -> Option<LocalCount> {
    if route.upstream.protocol != Protocol::Openai {
        return None;
    }
    let uncounted = crate::proxy::estimate::uncounted(ir, None).len();
    count_locally(route.upstream_model.clone(), chat_prompt_text(ir), relayed)
        .await
        .map(|c| LocalCount { uncounted, ..c })
}

/// The output text of a whole chat answer as [`count_locally`] reads it:
/// its text, and each tool call's name and arguments. Reasoning is left
/// out, as on a stream.
pub fn completion_text(c: &Completion) -> String {
    let mut parts: Vec<String> = Vec::new();
    for p in &c.content {
        match p {
            ContentPart::Text { text, .. } | ContentPart::Refusal { text } => {
                parts.push(text.clone())
            }
            ContentPart::ToolUse { name, args, .. } => {
                parts.push(name.clone());
                parts.push(args.to_string());
            }
            _ => {}
        }
    }
    parts.retain(|s| !s.is_empty());
    parts.join("\n")
}

/// The text of a chat request as the platform's chat body sends it, joined
/// by newlines: the system text; each tool's name, description and
/// parameters JSON; the response format's schema JSON; each message's text,
/// each tool call's name and arguments, each tool result's text (as the
/// Chat wire takes it, text only; on the Responses wire an image goes as an
/// image, which costs more than its placeholder text here, so the count
/// stays below the real one). Reasoning parts are left out: neither wire
/// carries unsealed reasoning back, and a seal's content cannot be counted.
/// A compaction block counts as the body sends it (`ir::compaction::folded`).
pub fn chat_prompt_text(ir: &ChatRequest) -> String {
    let ir = &*crate::ir::compaction::folded(ir);
    let mut parts: Vec<String> = Vec::new();
    if let Some(system) = ir.system_text() {
        parts.push(system);
    }
    for t in &ir.tools {
        parts.push(t.name.clone());
        if let Some(d) = t.description.as_deref() {
            parts.push(d.to_string());
        }
        parts.push(t.parameters.to_string());
    }
    let schema = match &ir.response_format {
        Some(ResponseFormat::JsonSchema(f)) => f.schema.as_ref(),
        Some(ResponseFormat::JsonObject { schema, .. }) => schema.as_ref(),
        _ => None,
    };
    if let Some(s) = schema {
        parts.push(s.to_string());
    }
    for m in ir.non_system_messages() {
        for p in &m.content {
            match p {
                ContentPart::Text { text, .. } | ContentPart::Refusal { text } => {
                    parts.push(text.clone())
                }
                ContentPart::ToolUse { name, args, .. } => {
                    parts.push(name.clone());
                    parts.push(args.to_string());
                }
                ContentPart::ToolResult { content, .. } => {
                    parts.push(flatten_tool_result(content).0)
                }
                // Folded to text above.
                ContentPart::Compaction(c) => parts.extend(c.summary.clone()),
                ContentPart::Image { .. }
                | ContentPart::Audio { .. }
                | ContentPart::Reasoning { .. }
                | ContentPart::AnthropicBlock(_) => {}
            }
        }
    }
    parts.retain(|s| !s.is_empty());
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_chat_prompt_is_the_text_the_platform_is_sent() {
        let ir = crate::ingress::openai::parse_chat_request(&json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": [
                    {"type": "text", "text": "Weather in Oslo?"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}]},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "t1", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"Oslo\"}"}}]},
                {"role": "tool", "tool_call_id": "t1", "content": "4 °C"}
            ],
            "tools": [{"type": "function", "function": {"name": "get_weather",
                       "description": "Weather by city", "parameters": {"type": "object"}}}]
        }))
        .unwrap();
        assert_eq!(
            chat_prompt_text(&ir),
            [
                "Be brief.",
                "get_weather",
                "Weather by city",
                "{\"type\":\"object\"}",
                "Weather in Oslo?",
                "get_weather",
                "{\"city\":\"Oslo\"}",
                "4 °C",
            ]
            .join("\n")
        );
    }
}
