//! A local count of a native `/v1/responses` answer whose stream was cut
//! before its terminal event (openai egress design §13.4, decision 22).
//!
//! OpenAI does not store a response cut before its terminal event, so its
//! usage is gone for good; this count is the only source a budget has. It
//! counts the text of the body as the forward sent it and the text of the
//! deltas lmgw relayed, each once, with the platform's tokenizer. Everything
//! it cannot read as text counts as nothing (images, files, encrypted
//! reasoning, hosted tool definitions, a `previous_response_id` chain), and
//! reasoning tokens are never relayed, so the count stays below the real
//! one: a lower bound, which is all a floor may be.

use serde_json::Value;

/// The delta events whose `delta` is output text the client was relayed.
/// Not the reasoning summary's: a summary is not the reasoning tokens it
/// sums up, so its count bounds nothing (the chat path leaves reasoning out
/// too).
pub(super) const DELTA_EVENTS: [&str; 3] = [
    "response.output_text.delta",
    "response.refusal.delta",
    "response.function_call_arguments.delta",
];

/// The text of a Responses body as it was sent, joined by newlines:
/// `instructions`; `input` as a string, or each item's text (a message's
/// text parts, a `function_call`'s name and arguments, a
/// `function_call_output`'s text); each function tool's name, description
/// and `parameters` JSON; the `text.format` schema's JSON. A `reasoning`
/// item counts as nothing, its summary included, as reasoning does on the
/// chat path.
///
/// The JSON is counted as `to_string()` writes it, which may come out a few
/// tokens above OpenAI's own rendering of a tool or a schema.
pub(super) fn prompt_text(body: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        if !s.is_empty() {
            parts.push(s.to_string());
        }
    };
    if let Some(s) = body.get("instructions").and_then(Value::as_str) {
        push(s);
    }
    match body.get("input") {
        Some(Value::String(s)) => push(s),
        Some(Value::Array(items)) => {
            for item in items {
                item_text(item, &mut push);
            }
        }
        _ => {}
    }
    for tool in body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("function"))
    {
        for key in ["name", "description"] {
            if let Some(s) = tool.get(key).and_then(Value::as_str) {
                push(s);
            }
        }
        if let Some(p) = tool.get("parameters") {
            push(&p.to_string());
        }
    }
    if let Some(schema) = body.pointer("/text/format/schema") {
        push(&schema.to_string());
    }
    parts.join("\n")
}

/// The output text of a whole Response object, joined by newlines: each
/// output item's text as [`prompt_text`] reads an input item (a message's
/// text and refusal parts, a `function_call`'s name and arguments), what a
/// whole answer that reported no usage is counted by (§13.4).
pub(super) fn output_text(response: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        if !s.is_empty() {
            parts.push(s.to_string());
        }
    };
    for item in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        item_text(item, &mut push);
    }
    parts.join("\n")
}

/// One input item's text, by its type (an item without one is a message).
fn item_text(item: &Value, push: &mut impl FnMut(&str)) {
    let ty = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    match ty {
        "message" => content_text(item.get("content"), push),
        "function_call" => {
            for key in ["name", "arguments"] {
                if let Some(s) = item.get(key).and_then(Value::as_str) {
                    push(s);
                }
            }
        }
        "function_call_output" => content_text(item.get("output"), push),
        // `reasoning` among them: its summary is not what it costs.
        _ => {}
    }
}

/// A content value's text: a string, or its `input_text` / `output_text`
/// parts (a `function_call_output`'s parts are `input_text` too) and an
/// answer's `refusal` parts.
fn content_text(content: Option<&Value>, push: &mut impl FnMut(&str)) {
    match content {
        Some(Value::String(s)) => push(s),
        Some(Value::Array(parts)) => {
            for s in parts
                .iter()
                .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text") => p.get("text").and_then(Value::as_str),
                    Some("refusal") => p.get("refusal").and_then(Value::as_str),
                    _ => None,
                })
            {
                push(s);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_prompt_is_every_text_the_body_sent_and_nothing_else() {
        let body = json!({
            "model": "gpt-test",
            "instructions": "Be brief.",
            "input": [
                {"role": "user", "content": "hello"},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "look"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}]},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "ok"}]},
                {"type": "reasoning", "id": "rs_1", "encrypted_content": "SEALED",
                 "summary": [{"type": "summary_text", "text": "thought"}]},
                {"type": "function_call", "call_id": "c1", "name": "lookup",
                 "arguments": "{\"q\":1}"},
                {"type": "function_call_output", "call_id": "c1", "output": "found"},
                {"type": "item_reference", "id": "msg_9"}
            ],
            "tools": [
                {"type": "function", "name": "lookup", "description": "Look up.",
                 "parameters": {"type": "object"}},
                {"type": "web_search"}
            ],
            "text": {"format": {"type": "json_schema", "name": "r",
                                "schema": {"type": "object"}}},
            "previous_response_id": "resp_1"
        });
        assert_eq!(
            prompt_text(&body),
            [
                "Be brief.",
                "hello",
                "look",
                "ok",
                "lookup",
                "{\"q\":1}",
                "found",
                "lookup",
                "Look up.",
                "{\"type\":\"object\"}",
                "{\"type\":\"object\"}",
            ]
            .join("\n")
        );
        assert_eq!(prompt_text(&json!({"input": "hi"})), "hi");
    }

    #[test]
    fn a_whole_answer_s_output_is_its_text_refusals_and_calls() {
        let response = json!({
            "output": [
                {"type": "reasoning", "id": "rs_1",
                 "summary": [{"type": "summary_text", "text": "thought"}]},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "Hello"},
                    {"type": "refusal", "refusal": "No."}]},
                {"type": "function_call", "call_id": "c1", "name": "lookup",
                 "arguments": "{}"},
                {"type": "web_search_call", "id": "ws_1"}
            ],
            "usage": null
        });
        assert_eq!(
            output_text(&response),
            ["Hello", "No.", "lookup", "{}"].join("\n")
        );
        assert_eq!(output_text(&json!({})), "");
    }
}
