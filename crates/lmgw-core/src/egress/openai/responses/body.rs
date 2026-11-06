//! The platform's chat request as a Responses body (openai egress design
//! §7.2.2), `POST {base}/responses`. The create reference it follows
//! (<https://developers.openai.com/api/reference/resources/responses/methods/create.md>,
//! checked 2026-10-24) and the IR's meanings:
//!
//! - `store: false`: lmgw keeps its own record, and a reasoning item's
//!   `encrypted_content` comes by default then (a probe), so no
//!   `include` is sent. A client's own `store` goes as it sent it;
//! - the system text before the first other message is `instructions`; a
//!   system message after it is a `developer` input message where it stood,
//!   so the order holds (decided, §7.2.5);
//! - user text and images are `input_text` and `input_image` parts (an image
//!   with `detail: "auto"`, OpenAI's default: the IR carries no detail);
//! - an assistant turn is its messages and its `function_call` items in the
//!   order of its parts, each run of text one `{"role": "assistant",
//!   "content": "<text>"}` (the input's `EasyInputMessage`: an output
//!   `message` item would need the `id` and `status` it never had here);
//!   each reasoning part sealed by this upstream row is its `reasoning`
//!   item where it stood (`id`, its summary, `encrypted_content`, §7.3.1),
//!   which is how the model sees its earlier reasoning again, when an
//!   answer of its turn follows it ([`followed`]: OpenAI refuses one
//!   without); every other reasoning part is not sent: unsealed text has no
//!   place in the Responses input, another row's seal is encrypted for
//!   another project, and so may be one this row made under another key,
//!   address or header ([`SealScope`], WP8 review W-R2): that turn replays
//!   as on a row that never sealed it. The Chat wire sends reasoning text as
//!   `reasoning_content`, a field OpenAI's Chat Completions does not
//!   document (llama-server's and DeepSeek's name);
//! - a tool result is a `function_call_output`: a string when it is text
//!   only, the array of content outputs when it carries an image, so the
//!   platform's tool images stop being flattened to text;
//! - tools are `function` tools with `strict` as the IR has it, which each
//!   ingress filled with its own dialect's default (`ToolDef::strict`): a
//!   chat or Anthropic client's tool without one goes `strict: false`, a
//!   Responses client's goes with no key, OpenAI's "strict when the schema
//!   is compatible", as does an in-process caller's;
//! - the format is `text.format`, the inverse of the Responses ingress's
//!   reading (`ingress/responses.rs`, `parse_text_format`), and a client's
//!   chat `verbosity` is `text.verbosity`;
//! - reasoning is `reasoning: {effort, summary: "auto"}`, an off as `effort:
//!   "none"` as on the Chat wire (`proxy::reasoning_fit` fits it), the
//!   summary always asked for (decided, §7.2.5) unless the route refused it
//!   (`Params::reasoning_summary_refused`);
//! - the cap is `max_output_tokens`; `temperature` and `top_p` as they are;
//!   the llama samplers are not written (P2, as on the Chat wire).
//!
//! Every other passthrough key goes verbatim, as on the Chat wire: the
//! client sent it. OpenRouter's `reasoning` object is not one: its contents
//! went into the control (P3).

use serde_json::{json, Map, Value};

use crate::egress::openai_wire::responses_items::{item, part};
use crate::egress::openai_wire::{FlattenToolResults, ToolResultRenderer};
use crate::ir::{
    tool_image_note, tool_image_placeholder, wire_call_id, ChatRequest, ContentPart, ImageSource,
    Message, Params, ReasoningControl, ResponseFormat, Role, Seal, SealScope, ToolChoice,
    ToolResultBlock,
};

/// The Responses body of the platform's send of `ir` to `model` on the
/// upstream row `scope` names, as configured now, with its final `params`:
/// the row's own seals go back to it (module doc).
pub fn responses_body(
    ir: &ChatRequest,
    model: &str,
    params: &Params,
    stream: bool,
    scope: &SealScope,
) -> Value {
    // An Anthropic compaction block, as Anthropic makes of it.
    let ir = &*crate::ir::compaction::folded(ir);
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    let (instructions, input) = input_items(ir, scope);
    if let Some(text) = instructions {
        body.insert("instructions".into(), json!(text));
    }
    body.insert("input".into(), Value::Array(input));
    if let Some(v) = params.temperature {
        body.insert("temperature".into(), json!(v));
    }
    if let Some(v) = params.top_p {
        body.insert("top_p".into(), json!(v));
    }
    if let Some(v) = params.max_tokens {
        body.insert("max_output_tokens".into(), json!(v));
    }
    if !ir.tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools(ir)));
        if let Some(tc) = &ir.tool_choice {
            body.insert("tool_choice".into(), tool_choice(tc));
        }
    }
    let mut text = Map::new();
    if let Some(f) = &ir.response_format {
        text.insert("format".into(), text_format(f));
    }
    if let Some(v) = ir.passthrough.get("verbosity") {
        text.insert("verbosity".into(), v.clone());
    }
    if !text.is_empty() {
        body.insert("text".into(), Value::Object(text));
    }
    if let Some(r) = reasoning(&params.reasoning_control(), params) {
        body.insert("reasoning".into(), r);
    }
    if stream {
        body.insert("stream".into(), json!(true));
    }
    for (k, v) in &ir.passthrough {
        // OpenRouter's object went into the control (P3), and `verbosity`
        // into `text` above.
        let reasoning_object = k == "reasoning" && v.is_object();
        if reasoning_object || k == "verbosity" {
            continue;
        }
        body.entry(k.clone()).or_insert_with(|| v.clone());
    }
    body.entry("store").or_insert(json!(false));
    let mut body = Value::Object(body);
    super::super::ask_for_the_tier(&mut body);
    body
}

/// `instructions` (the system text before the first other message) and the
/// `input` items, with the seals of the row `scope` names.
fn input_items(ir: &ChatRequest, scope: &SealScope) -> (Option<String>, Vec<Value>) {
    let lead = ir
        .messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    let instructions: Vec<String> = ir.messages[..lead]
        .iter()
        .map(Message::joined_text)
        .filter(|s| !s.is_empty())
        .collect();
    let instructions = (!instructions.is_empty()).then(|| instructions.join("\n\n"));
    let mut out = Vec::new();
    for m in &ir.messages[lead..] {
        match m.role {
            Role::System => {
                let text = m.joined_text();
                if !text.is_empty() {
                    out.push(json!({
                        "type": item::MESSAGE,
                        "role": "developer",
                        "content": [{"type": part::INPUT_TEXT, "text": text}],
                    }));
                }
            }
            Role::User => out.push(json!({
                "type": item::MESSAGE,
                "role": "user",
                "content": user_content(&m.content),
            })),
            Role::Assistant => assistant_items(&m.content, scope, &mut out),
            Role::Tool => {
                for p in &m.content {
                    if let ContentPart::ToolResult { id, content, .. } = p {
                        // A Gemini signature stays with Gemini (§7.1).
                        let id = wire_call_id(id);
                        out.push(json!({
                            "type": item::FUNCTION_CALL_OUTPUT,
                            "call_id": id,
                            "output": tool_output(id, content),
                        }));
                    }
                }
            }
        }
    }
    followed::keep_followed(&mut out);
    (instructions, out)
}

/// A user message's text and images. An audio part never reaches here: it
/// takes the Chat wire (`wire::chat_only`).
fn user_content(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text, .. } => Some(json!({"type": part::INPUT_TEXT, "text": text})),
            ContentPart::Image { mime, source } => Some(input_image(mime, source)),
            _ => None,
        })
        .collect()
}

/// An `input_image` part: the URL as sent, or the data URL of an inline
/// image.
fn input_image(mime: &str, source: &ImageSource) -> Value {
    let url = match source {
        ImageSource::Url { url } => url.clone(),
        ImageSource::Base64 { data } => format!("data:{mime};base64,{data}"),
    };
    json!({"type": part::INPUT_IMAGE, "image_url": url, "detail": "auto"})
}

/// An assistant turn's items in the order of its parts: each run of text an
/// assistant message (module doc), each tool call a `function_call`, each
/// reasoning part the row `scope` names sealed, as it is configured now,
/// its `reasoning` item.
fn assistant_items(parts: &[ContentPart], scope: &SealScope, out: &mut Vec<Value>) {
    let mut texts: Vec<&str> = Vec::new();
    // A run's texts joined as `Message::joined_text` joins a message's.
    let flush = |texts: &mut Vec<&str>, out: &mut Vec<Value>| {
        if !texts.is_empty() {
            out.push(json!({
                "role": "assistant",
                "content": std::mem::take(texts).join("\n"),
            }));
        }
    };
    for p in parts {
        match p {
            // A refusal replays as its text (`ContentPart::Refusal`).
            ContentPart::Text { text, .. } | ContentPart::Refusal { text } if !text.is_empty() => {
                texts.push(text)
            }
            ContentPart::ToolUse { id, name, args } => {
                flush(&mut texts, out);
                out.push(json!({
                    "type": item::FUNCTION_CALL,
                    "call_id": wire_call_id(id),
                    "name": name,
                    "arguments": serde_json::to_string(args).unwrap_or_else(|_| "{}".into()),
                }));
            }
            ContentPart::Reasoning {
                text,
                sealed: Some(seal),
                ..
            } if seal.is_for(scope) => {
                flush(&mut texts, out);
                out.push(reasoning_item(text, seal));
            }
            // Unsealed, or sealed by another row or by this one under
            // another configuration (module doc).
            _ => {}
        }
    }
    flush(&mut texts, out);
}

/// A sealed reasoning part as the `reasoning` input item it was answered
/// as: its id, its summary (none when it came without one) and its
/// `encrypted_content` (the create reference's `Reasoning` input item, whose
/// `id`, `summary` and `type` are required).
fn reasoning_item(text: &str, seal: &Seal) -> Value {
    let summary: Vec<Value> = match text.is_empty() {
        true => Vec::new(),
        false => vec![json!({"type": part::SUMMARY_TEXT, "text": text})],
    };
    json!({
        "type": item::REASONING,
        "id": seal.item_id,
        "summary": summary,
        "encrypted_content": seal.data,
    })
}

/// A tool result's `output`: one string when it has no image OpenAI takes,
/// as the Chat wire's, else the array of content outputs ("An array of
/// content outputs (text, image, file) for the function tool call", the
/// create reference), each image an `input_image`. An image in a format
/// OpenAI does not take is its placeholder ([`tool_images`]), and what has
/// no content output (audio, a binary resource) the text the Chat wire
/// gives it, each with a WARN.
fn tool_output(id: &str, content: &[ToolResultBlock]) -> Value {
    if !has_image(content) {
        return FlattenToolResults.render(id, content);
    }
    let mut out = Vec::new();
    let mut dropped = Vec::new();
    for b in content {
        match b {
            ToolResultBlock::Image { mime, data } => match tool_images::openai_image(mime, data) {
                Ok(format) => out.push(input_image(
                    format.mime(),
                    &ImageSource::Base64 { data: data.clone() },
                )),
                Err(why) => {
                    let text = tool_image_placeholder(mime, data, &why);
                    out.push(json!({"type": part::INPUT_TEXT, "text": text}));
                    dropped.push(format!("{} ({why})", tool_image_note(mime, data)));
                }
            },
            other => {
                let text = FlattenToolResults.render(id, std::slice::from_ref(other));
                if text.as_str().is_some_and(|t| !t.is_empty()) {
                    out.push(json!({"type": part::INPUT_TEXT, "text": text}));
                }
            }
        }
    }
    if !dropped.is_empty() {
        tracing::warn!(
            tool_call_id = %id,
            "tool result image not in a format OpenAI takes, replaced with placeholders: {}",
            dropped.join("; ")
        );
    }
    // No image left to send: the one string the Chat wire's would be.
    if !out.iter().any(|p| p["type"] == part::INPUT_IMAGE) {
        let texts: Vec<&str> = out.iter().filter_map(|p| p["text"].as_str()).collect();
        return Value::String(texts.join("\n"));
    }
    Value::Array(out)
}

fn has_image(content: &[ToolResultBlock]) -> bool {
    content
        .iter()
        .any(|b| matches!(b, ToolResultBlock::Image { .. }))
}

/// The `function` tools, with `strict` where the IR has one, and no key
/// where it has none (module doc).
fn tools(ir: &ChatRequest) -> Vec<Value> {
    ir.tools
        .iter()
        .map(|t| {
            let mut tool = json!({
                "type": "function",
                "name": t.name,
                "parameters": t.parameters,
            });
            if let Some(s) = t.strict {
                tool["strict"] = json!(s);
            }
            // No key when there is none, as on the Chat wire (135fb92c).
            if let Some(d) = &t.description {
                tool["description"] = json!(d);
            }
            tool
        })
        .collect()
}

fn tool_choice(tc: &ToolChoice) -> Value {
    match tc {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({"type": "function", "name": name}),
    }
}

/// The IR's structured output as `text.format`: the chat `response_format`
/// flattened — a `json_schema`'s `name`, `description`, `schema` and
/// `strict` beside its `type`, every key kept verbatim beside them — and a
/// type lmgw does not model verbatim, which OpenAI then answers for.
fn text_format(f: &ResponseFormat) -> Value {
    match f {
        ResponseFormat::JsonSchema(s) => {
            let mut o = s.format_extra.clone();
            if !s.bare {
                o.extend(s.extra.clone());
                if let Some(v) = &s.name {
                    o.insert("name".into(), json!(v));
                }
                if let Some(v) = &s.description {
                    o.insert("description".into(), json!(v));
                }
                if let Some(v) = &s.schema {
                    o.insert("schema".into(), v.clone());
                }
                if let Some(v) = s.strict {
                    o.insert("strict".into(), json!(v));
                }
            }
            o.insert("type".into(), json!("json_schema"));
            Value::Object(o)
        }
        // `text` and `json_object` have the same object on both wires.
        other => other.to_openai(),
    }
}

/// `reasoning: {effort, summary}` for the resolved control, `None` when
/// nothing is to be said. The summary is the client's own when it said one
/// (`Params::reasoning_summary`: a kind, or `null` for none); when it said
/// nothing, `auto` is asked for on every send (decided, §7.2.5) but one
/// whose route refused it (`reasoning_summary_refused`, the lesson in
/// `proxy::reasoning_fit`).
fn reasoning(c: &ReasoningControl, params: &Params) -> Option<Value> {
    let mut o = Map::new();
    if c.enabled == Some(false) {
        o.insert("effort".into(), json!("none"));
    } else if let Some(e) = &c.effort {
        o.insert("effort".into(), json!(e));
    }
    let summary = match &params.reasoning_summary {
        Some(asked) => asked.as_deref(),
        None if params.reasoning_summary_refused => None,
        None => Some("auto"),
    };
    if let Some(s) = summary {
        o.insert("summary".into(), json!(s));
    }
    (!o.is_empty()).then_some(Value::Object(o))
}

mod followed;
#[cfg(test)]
mod tests;
mod tool_images;
