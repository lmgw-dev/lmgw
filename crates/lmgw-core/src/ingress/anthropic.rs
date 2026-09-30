//! Anthropic ingress: `/v1/messages` parser + serializer (§6).

use serde_json::{json, Value};

use crate::error::GatewayError;
use crate::ingress::{rand_id, ClientStreamEncoder};
use crate::ir::{
    ChatRequest, Completion, ContentPart, ImageSource, Message, Params, ReasoningControl, Role,
    StreamDelta, ToolChoice, ToolDef, ToolResultBlock, Usage,
};
use crate::sse::frame;

/// Parse an Anthropic `/v1/messages` request body into the IR.
pub fn parse_messages_request(body: &Value) -> Result<ChatRequest, GatewayError> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::BadRequest("missing 'model'".into()))?
        .to_string();
    let raw_messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| GatewayError::BadRequest("missing 'messages'".into()))?;

    let mut messages = Vec::new();

    // System prompt: top-level string or text blocks → one System message.
    match body.get("system") {
        Some(Value::String(s)) if !s.is_empty() => {
            messages.push(Message::text(Role::System, s.clone()));
        }
        Some(Value::Array(blocks)) => {
            let text = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n");
            if !text.is_empty() {
                messages.push(Message::text(Role::System, text));
            }
        }
        _ => {}
    }

    for m in raw_messages {
        let role = match m.get("role").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            other => {
                return Err(GatewayError::BadRequest(format!(
                    "invalid message role {other:?}"
                )))
            }
        };
        let content = parse_content(m.get("content"))?;
        // Tool results live in user messages on the Anthropic side; the IR
        // models them as Role::Tool so egress adapters can split them out.
        let (tool_results, rest): (Vec<ContentPart>, Vec<ContentPart>) = content
            .into_iter()
            .partition(|p| matches!(p, ContentPart::ToolResult { .. }));
        if !tool_results.is_empty() {
            messages.push(Message {
                role: Role::Tool,
                content: tool_results,
            });
        }
        if !rest.is_empty() {
            messages.push(Message {
                role,
                content: rest,
            });
        }
    }

    let mut tools = Vec::new();
    if let Some(raw) = body.get("tools").and_then(Value::as_array) {
        for t in raw {
            // Only client (custom) tools are translatable; server-side tools
            // (web_search, computer use, …) have no input_schema.
            if t.get("input_schema").is_none() {
                return Err(GatewayError::Unsupported(format!(
                    "server tool type {:?}",
                    t.get("type").and_then(Value::as_str).unwrap_or("unknown")
                )));
            }
            tools.push(ToolDef {
                name: t
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| GatewayError::BadRequest("tool without name".into()))?
                    .to_string(),
                description: t
                    .get("description")
                    .and_then(Value::as_str)
                    .map(String::from),
                parameters: t
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            });
        }
    }

    let tool_choice = match body.get("tool_choice") {
        None | Some(Value::Null) => None,
        Some(v) => match v.get("type").and_then(Value::as_str) {
            Some("auto") => Some(ToolChoice::Auto),
            Some("any") => Some(ToolChoice::Required),
            Some("none") => Some(ToolChoice::None),
            Some("tool") => v
                .get("name")
                .and_then(Value::as_str)
                .map(|name| ToolChoice::Tool { name: name.into() }),
            other => {
                return Err(GatewayError::BadRequest(format!(
                    "unknown tool_choice type {other:?}"
                )))
            }
        },
    };

    let params = Params {
        temperature: body.get("temperature").and_then(Value::as_f64),
        top_p: body.get("top_p").and_then(Value::as_f64),
        top_k: body.get("top_k").and_then(Value::as_u64).map(|v| v as u32),
        max_tokens: body
            .get("max_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        min_p: None,
        repeat_penalty: None,
        presence_penalty: None,
        frequency_penalty: None,
        seed: None,
        stop: body
            .get("stop_sequences")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        reasoning: parse_reasoning(body),
    };

    Ok(ChatRequest {
        model_alias: model,
        messages,
        params,
        tools,
        tool_choice,
        stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        // Anthropic has no `response_format`/`grammar` equivalents; nothing to
        // carry through (these only matter on the OpenAI→OpenAI route).
        passthrough: Default::default(),
        // No template kwargs in this dialect.
        llama_kwargs_enabled: None,
        // A header, not a body field: the handler fills it from the request.
        anthropic_beta: Vec::new(),
    })
}

/// Anthropic's own reasoning controls (§5.2): the `thinking` block and
/// `output_config.effort`. Both used to be dropped on the floor here — an
/// Anthropic client asking for extended thinking through lmgw got a request
/// with no thinking in it and no word about why.
///
/// `pub(crate)` for `POST /v1/messages/count_tokens` (api-docs design §5.2):
/// its Anthropic route passes the body through, so it reads this tier on its
/// own rather than through a full parse that would refuse a server tool.
pub(crate) fn parse_reasoning(body: &Value) -> Option<ReasoningControl> {
    let mut c = ReasoningControl::default();
    if let Some(t) = body.get("thinking") {
        match t.get("type").and_then(Value::as_str) {
            Some("disabled") => c.enabled = Some(false),
            Some("enabled") => {
                c.enabled = Some(true);
                c.budget_tokens = t.get("budget_tokens").and_then(Value::as_i64);
            }
            // `adaptive` is "think as much as you judge necessary" — enabled
            // with no budget, which is exactly the neutral triple.
            Some("adaptive") => c.enabled = Some(true),
            _ => {}
        }
    }
    if let Some(e) = body
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
    {
        c.effort = Some(e.to_string());
    }
    (!c.is_empty()).then_some(c)
}

fn parse_content(v: Option<&Value>) -> Result<Vec<ContentPart>, GatewayError> {
    let mut parts = Vec::new();
    match v {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                parts.push(ContentPart::text(s.clone()));
            }
        }
        Some(Value::Array(blocks)) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => parts.push(ContentPart::text(
                        b.get("text").and_then(Value::as_str).unwrap_or_default(),
                    )),
                    Some("image") => {
                        let src = b.get("source").cloned().unwrap_or_default();
                        match src.get("type").and_then(Value::as_str) {
                            Some("base64") => parts.push(ContentPart::Image {
                                mime: src
                                    .get("media_type")
                                    .and_then(Value::as_str)
                                    .unwrap_or("image/png")
                                    .to_string(),
                                source: ImageSource::Base64 {
                                    data: src
                                        .get("data")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                },
                            }),
                            Some("url") => {
                                let url = src
                                    .get("url")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                parts.push(crate::ingress::openai::parse_image_url(&url));
                            }
                            other => {
                                return Err(GatewayError::Unsupported(format!(
                                    "image source type {other:?}"
                                )))
                            }
                        }
                    }
                    Some("tool_use") => parts.push(ContentPart::ToolUse {
                        id: b
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: b
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        args: b.get("input").cloned().unwrap_or(json!({})),
                    }),
                    Some("tool_result") => {
                        let content = parse_tool_result_content(b.get("content"));
                        parts.push(ContentPart::ToolResult {
                            id: b
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            name: None,
                            content,
                            is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                        });
                    }
                    Some("thinking") => {
                        // The trace of an earlier assistant turn, replayed.
                        // Signed when Anthropic produced it; unsigned when this
                        // gateway did (see `serialize_completion`), in which
                        // case it is bound for an OpenAI-shaped upstream as
                        // `reasoning_content` — llama-server's
                        // `--reasoning-preserve` needs exactly this.
                        let text = b
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !text.is_empty() {
                            parts.push(ContentPart::Reasoning {
                                text: text.to_string(),
                                signature: b
                                    .get("signature")
                                    .and_then(Value::as_str)
                                    .filter(|s| !s.is_empty())
                                    .map(String::from),
                            });
                        }
                    }
                    // Encrypted by Anthropic, readable by Anthropic alone, and
                    // only produced with extended thinking on — which this
                    // gateway's Anthropic egress never requests. Nothing to
                    // carry.
                    Some("redacted_thinking") => {}
                    other => {
                        return Err(GatewayError::Unsupported(format!(
                            "content block type {other:?}"
                        )))
                    }
                }
            }
        }
        _ => {}
    }
    Ok(parts)
}

/// Anthropic `tool_result.content` → IR blocks. Anthropic is the one ingress
/// that can carry a non-text tool result (its block array takes images), so
/// this is where that fidelity enters the IR — previously every block but
/// `text` was dropped on the floor here.
///
/// An unrecognized block is kept as [`ToolResultBlock::Json`] rather than
/// rejected: a new Anthropic block type should reach the model as *something*
/// instead of 400-ing an otherwise valid conversation, and nothing is lost.
fn parse_tool_result_content(v: Option<&Value>) -> Vec<ToolResultBlock> {
    match v {
        Some(Value::String(s)) => ToolResultBlock::one(s.clone()),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => {
                    ToolResultBlock::text(b.get("text").and_then(Value::as_str).unwrap_or_default())
                }
                Some("image") => {
                    let src = b.get("source");
                    ToolResultBlock::Image {
                        mime: src
                            .and_then(|s| s.get("media_type"))
                            .and_then(Value::as_str)
                            .unwrap_or("image/png")
                            .to_string(),
                        data: src
                            .and_then(|s| s.get("data"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    }
                }
                _ => ToolResultBlock::Json { value: b.clone() },
            })
            .collect(),
        // A `tool_result` with structured output and no content blocks.
        Some(other) => vec![ToolResultBlock::Json {
            value: other.clone(),
        }],
        None => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Serialization (IR → Anthropic response shapes)
// ---------------------------------------------------------------------------

fn content_json(content: &[ContentPart]) -> Vec<Value> {
    let mut out = Vec::new();
    for p in content {
        match p {
            ContentPart::Text { text } => {
                if !text.is_empty() {
                    out.push(json!({"type": "text", "text": text}));
                }
            }
            ContentPart::ToolUse { id, name, args } => {
                out.push(json!({"type": "tool_use", "id": id, "name": name, "input": args}));
            }
            _ => {}
        }
    }
    out
}

pub fn serialize_completion(alias: &str, c: &Completion) -> Value {
    let mut blocks = content_json(&c.content);
    // Surface reasoning as a leading `thinking` block. No signature — it came
    // from a non-Anthropic upstream — and when the client sends it back,
    // `parse_content` turns it into the unsigned reasoning part an
    // OpenAI-shaped upstream replays as `reasoning_content`.
    if !c.reasoning.is_empty() {
        blocks.insert(0, json!({"type": "thinking", "thinking": c.reasoning}));
    }
    json!({
        "id": format!("msg_{}", rand_id()),
        "type": "message",
        "role": "assistant",
        "model": alias,
        "content": Value::Array(blocks),
        "stop_reason": c.finish_reason.to_anthropic(),
        "stop_sequence": null,
        "usage": anthropic_usage(&c.usage),
    })
}

/// The IR's usage, back in Anthropic's three-disjoint-counters shape.
///
/// `ir::Usage::prompt_tokens` is the **total** input with cache included (the
/// egress adapter sums Anthropic's three counters into it, so the same
/// conversation reports the same size in either dialect). Emitting that total
/// as `input_tokens` would inflate what an Anthropic client sees by the cache
/// counts and drop the cache fields entirely — so the sum is undone here, and
/// the round trip is exact.
fn anthropic_usage(u: &crate::ir::Usage) -> Value {
    let cached = u.cached_input_tokens;
    let written = u.cache_write_tokens;
    let plain = u
        .prompt_tokens
        .unwrap_or(0)
        .saturating_sub(cached.unwrap_or(0))
        .saturating_sub(written.unwrap_or(0));
    let mut usage = json!({
        "input_tokens": plain,
        "output_tokens": u.completion_tokens.unwrap_or(0),
    });
    // Only when the upstream reported them: an absent counter and a zero one
    // are different statements, and a client that caches reads the difference.
    if let Some(v) = cached {
        usage["cache_read_input_tokens"] = json!(v);
    }
    if let Some(v) = written {
        usage["cache_creation_input_tokens"] = json!(v);
    }
    usage
}

#[derive(PartialEq)]
enum OpenBlock {
    None,
    Text,
    Tool,
    Thinking,
}

/// Streaming encoder implementing the Anthropic event sequence:
/// `message_start → content_block_* → message_delta → message_stop`.
pub struct AnthropicStreamEncoder {
    id: String,
    alias: String,
    next_block_index: usize,
    open: OpenBlock,
    /// IR tool-call ordinal → SSE content block index.
    tool_blocks: std::collections::HashMap<usize, usize>,
    stop_reason: Option<String>,
    usage: Usage,
}

impl AnthropicStreamEncoder {
    pub fn new(alias: &str) -> Self {
        Self {
            id: format!("msg_{}", rand_id()),
            alias: alias.to_string(),
            next_block_index: 0,
            open: OpenBlock::None,
            tool_blocks: std::collections::HashMap::new(),
            stop_reason: None,
            usage: Usage::default(),
        }
    }

    fn close_open_block(&mut self) -> String {
        if self.open == OpenBlock::None {
            return String::new();
        }
        let idx = self.next_block_index - 1;
        self.open = OpenBlock::None;
        frame(
            Some("content_block_stop"),
            &json!({"type": "content_block_stop", "index": idx}).to_string(),
        )
    }

    fn ensure_text_block(&mut self) -> String {
        if self.open == OpenBlock::Text {
            return String::new();
        }
        let mut out = self.close_open_block();
        let idx = self.next_block_index;
        self.next_block_index += 1;
        self.open = OpenBlock::Text;
        out.push_str(&frame(
            Some("content_block_start"),
            &json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {"type": "text", "text": ""},
            })
            .to_string(),
        ));
        out
    }

    fn ensure_thinking_block(&mut self) -> String {
        if self.open == OpenBlock::Thinking {
            return String::new();
        }
        let mut out = self.close_open_block();
        let idx = self.next_block_index;
        self.next_block_index += 1;
        self.open = OpenBlock::Thinking;
        out.push_str(&frame(
            Some("content_block_start"),
            &json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {"type": "thinking", "thinking": ""},
            })
            .to_string(),
        ));
        out
    }
}

impl ClientStreamEncoder for AnthropicStreamEncoder {
    fn start(&mut self) -> String {
        let mut out = frame(
            Some("message_start"),
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.alias,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                },
            })
            .to_string(),
        );
        out.push_str(&frame(Some("ping"), &json!({"type": "ping"}).to_string()));
        out
    }

    fn delta(&mut self, d: &StreamDelta) -> String {
        match d {
            StreamDelta::TextDelta(t) => {
                if t.is_empty() {
                    return String::new();
                }
                let mut out = self.ensure_text_block();
                let idx = self.next_block_index - 1;
                out.push_str(&frame(
                    Some("content_block_delta"),
                    &json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": {"type": "text_delta", "text": t},
                    })
                    .to_string(),
                ));
                out
            }
            // Map reasoning to a native Anthropic `thinking` block so Anthropic
            // clients (e.g. Claude Code pointed at lmgw) render the thoughts.
            StreamDelta::ReasoningDelta(r) => {
                if r.is_empty() {
                    return String::new();
                }
                let mut out = self.ensure_thinking_block();
                let idx = self.next_block_index - 1;
                out.push_str(&frame(
                    Some("content_block_delta"),
                    &json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": {"type": "thinking_delta", "thinking": r},
                    })
                    .to_string(),
                ));
                out
            }
            StreamDelta::ToolCallStart { index, id, name } => {
                let mut out = self.close_open_block();
                let idx = self.next_block_index;
                self.next_block_index += 1;
                self.open = OpenBlock::Tool;
                self.tool_blocks.insert(*index, idx);
                out.push_str(&frame(
                    Some("content_block_start"),
                    &json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}},
                    })
                    .to_string(),
                ));
                out
            }
            StreamDelta::ToolCallArgsDelta { index, fragment } => {
                if fragment.is_empty() {
                    return String::new();
                }
                let idx = self
                    .tool_blocks
                    .get(index)
                    .copied()
                    .unwrap_or(self.next_block_index.saturating_sub(1));
                frame(
                    Some("content_block_delta"),
                    &json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": {"type": "input_json_delta", "partial_json": fragment},
                    })
                    .to_string(),
                )
            }
            StreamDelta::Usage(u) => {
                self.usage.merge(u);
                String::new()
            }
            StreamDelta::Stop(reason) => {
                self.stop_reason = Some(reason.to_anthropic().to_string());
                String::new()
            }
            // Internal-only (Chat tab stats); never part of the public wire.
            StreamDelta::Timings(_) => String::new(),
            StreamDelta::Error(msg) => frame(
                Some("error"),
                &json!({
                    "type": "error",
                    "error": {"type": "api_error", "message": msg},
                })
                .to_string(),
            ),
        }
    }

    fn finish(&mut self) -> String {
        let mut out = self.close_open_block();
        let stop_reason = self
            .stop_reason
            .clone()
            .unwrap_or_else(|| "end_turn".into());
        let mut usage = json!({"output_tokens": self.usage.completion_tokens.unwrap_or(0)});
        if self.usage.prompt_tokens.is_some() {
            let full = anthropic_usage(&self.usage);
            for k in [
                "input_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            ] {
                if let Some(v) = full.get(k) {
                    usage[k] = v.clone();
                }
            }
        }
        out.push_str(&frame(
            Some("message_delta"),
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": usage,
            })
            .to_string(),
        ));
        out.push_str(&frame(
            Some("message_stop"),
            &json!({"type": "message_stop"}).to_string(),
        ));
        out
    }
}
