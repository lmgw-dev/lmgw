//! OpenAI ingress: `/v1/chat/completions` parser + serializer (§6).

use serde_json::{json, Value};

use crate::error::GatewayError;
use crate::ingress::{now_unix, rand_id, ClientStreamEncoder};
use crate::ir::{
    ChatRequest, Completion, ContentPart, ImageSource, Message, Params, ReasoningControl, Role,
    StreamDelta, ToolChoice, ToolDef, ToolResultBlock, Usage,
};
use crate::sse::frame;

/// Parse an OpenAI chat-completions request body into the IR.
pub fn parse_chat_request(body: &Value) -> Result<ChatRequest, GatewayError> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::BadRequest("missing 'model'".into()))?
        .to_string();
    let raw_messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| GatewayError::BadRequest("missing 'messages'".into()))?;

    let mut messages = Vec::with_capacity(raw_messages.len());
    for m in raw_messages {
        messages.push(parse_message(m)?);
    }

    let mut tools = Vec::new();
    if let Some(raw_tools) = body.get("tools").and_then(Value::as_array) {
        for t in raw_tools {
            let f = t
                .get("function")
                .ok_or_else(|| GatewayError::BadRequest("tool without 'function'".into()))?;
            tools.push(ToolDef {
                name: f
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| GatewayError::BadRequest("tool function without name".into()))?
                    .to_string(),
                description: f
                    .get("description")
                    .and_then(Value::as_str)
                    .map(String::from),
                parameters: f
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            });
        }
    }

    let tool_choice = match body.get("tool_choice") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => match s.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            other => {
                return Err(GatewayError::BadRequest(format!(
                    "unknown tool_choice '{other}'"
                )))
            }
        },
        Some(v) => v
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .map(|name| ToolChoice::Tool { name: name.into() }),
    };

    let stop = match body.get("stop") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    };

    let params = Params {
        temperature: body.get("temperature").and_then(Value::as_f64),
        top_p: body.get("top_p").and_then(Value::as_f64),
        // Non-standard for OpenAI but accepted (llama.cpp supports it).
        top_k: body.get("top_k").and_then(Value::as_u64).map(|v| v as u32),
        // llama.cpp extensions; modelled, so not in passthrough.
        min_p: body.get("min_p").and_then(Value::as_f64),
        repeat_penalty: body.get("repeat_penalty").and_then(Value::as_f64),
        max_tokens: body
            .get("max_completion_tokens")
            .or_else(|| body.get("max_tokens"))
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        presence_penalty: body.get("presence_penalty").and_then(Value::as_f64),
        frequency_penalty: body.get("frequency_penalty").and_then(Value::as_f64),
        seed: body.get("seed").and_then(Value::as_i64),
        stop,
        reasoning: parse_reasoning(body),
    };

    Ok(ChatRequest {
        model_alias: model,
        messages,
        params,
        tools,
        tool_choice,
        stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        passthrough: passthrough_fields(body),
        llama_kwargs_enabled: body
            .pointer("/chat_template_kwargs/enable_thinking")
            .and_then(Value::as_bool),
        // A header, not a body field: the handler fills it from the request.
        anthropic_beta: Vec::new(),
    })
}

/// OpenAI's `stream_options.include_usage`: whether a streamed answer shows
/// the client its usage — a final chunk with `choices: []`, and `usage: null`
/// on every other chunk. Absent and `false` both mean no, as on OpenAI. Only
/// what the client is shown: the egress asks every upstream for usage either
/// way, because the request row, pricing and budgets need it.
pub fn stream_include_usage(body: &Value) -> bool {
    body.pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Top-level request keys consumed by the parser above and re-emitted from the
/// modeled IR. Everything *not* in this set is carried through verbatim (see
/// [`passthrough_fields`]) so OpenAI-compatible upstreams still receive
/// `response_format`, `grammar`, extra sampler params, etc. `stream_options`
/// is listed here because the egress builder manages it itself; the client's
/// own `include_usage` is read by [`stream_include_usage`].
///
/// Of the reasoning controls (§5.2) only `reasoning_effort` is modeled — it is
/// the one key whose spelling changes per route, so the egress has to own it.
/// Everything else that carries a control (`reasoning_budget_tokens`,
/// `thinking_budget_tokens`, the OpenRouter-shaped `reasoning` object,
/// `chat_template_kwargs`) is *read* as a source and **left in passthrough**:
/// each carries something lmgw does not model (`exclude`, arbitrary template
/// kwargs, the client's own choice of budget spelling), and an upstream that
/// understands it should still receive exactly what was sent. The egress
/// overrides the ones that would otherwise contradict the resolved control.
const MODELED_KEYS: &[&str] = &[
    "model",
    "messages",
    "tools",
    "tool_choice",
    "stream",
    "stream_options",
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "repeat_penalty",
    "max_completion_tokens",
    "max_tokens",
    "presence_penalty",
    "frequency_penalty",
    "seed",
    "stop",
    "reasoning_effort",
];

/// Read every OpenAI-dialect spelling of the reasoning control (§5.2). All of
/// them sit at the same tier, so the scalar keys — the ones an OpenAI client
/// actually sends — win over the OpenRouter object when a request carries
/// both.
///
/// `chat_template_kwargs.enable_thinking` is deliberately **not** read here:
/// it is a llama-server template variable, not a protocol-neutral control, and
/// it reaches [`ChatRequest::llama_kwargs_enabled`] instead.
fn parse_reasoning(body: &Value) -> Option<ReasoningControl> {
    let mut c = ReasoningControl::default();

    // OpenRouter shape: {effort, max_tokens, enabled, exclude}. Read here,
    // *not* removed from passthrough — see MODELED_KEYS.
    if let Some(o) = body.get("reasoning").and_then(Value::as_object) {
        c.effort = o.get("effort").and_then(Value::as_str).map(String::from);
        c.budget_tokens = o.get("max_tokens").and_then(Value::as_i64);
        c.enabled = o.get("enabled").and_then(Value::as_bool);
    }

    if let Some(e) = body.get("reasoning_effort").and_then(Value::as_str) {
        c.effort = Some(e.to_string());
    }
    // llama-server spells the budget both ways depending on version; both stay
    // in passthrough, so the client's own spelling still reaches the upstream.
    if let Some(b) = ["reasoning_budget_tokens", "thinking_budget_tokens"]
        .iter()
        .find_map(|k| body.get(*k).and_then(Value::as_i64))
    {
        c.budget_tokens = Some(b);
    }

    (!c.is_empty()).then_some(c)
}

/// Collect every top-level field the IR does not otherwise model, to be
/// re-emitted verbatim on OpenAI egress.
fn passthrough_fields(body: &Value) -> serde_json::Map<String, Value> {
    let mut out: serde_json::Map<String, Value> = match body.as_object() {
        Some(obj) => obj
            .iter()
            .filter(|(k, _)| !MODELED_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        None => serde_json::Map::new(),
    };
    // A key is only *modeled* when it carries the modeled type. A
    // `reasoning_effort` that is null, a number or an object is not a level
    // lmgw can resolve — and swallowing it here would delete, in silence, a
    // field the upstream might well have understood. It rides along verbatim
    // instead, exactly like any other unmodeled key.
    if let Some(v) = body.get("reasoning_effort").filter(|v| !v.is_string()) {
        out.insert("reasoning_effort".into(), v.clone());
    }
    out
}

fn parse_message(m: &Value) -> Result<Message, GatewayError> {
    let role_str = m
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::BadRequest("message without role".into()))?;
    let role = match role_str {
        "system" | "developer" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" | "function" => Role::Tool,
        other => {
            return Err(GatewayError::BadRequest(format!(
                "unknown message role '{other}'"
            )))
        }
    };

    let mut content = Vec::new();

    if role == Role::Tool {
        let id = m
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // OpenAI's tool-message content is text (string or text parts), so
        // one Text block is the faithful reading — no JSON sniffing here,
        // a client that sent a string gets a string back out (§7).
        let text = tool_message_text(m.get("content"), &id);
        content.push(ContentPart::ToolResult {
            id,
            name: m.get("name").and_then(Value::as_str).map(String::from),
            content: ToolResultBlock::one(text),
            is_error: false,
        });
        return Ok(Message { role, content });
    }

    match m.get("content") {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                content.push(ContentPart::text(s.clone()));
            }
        }
        Some(Value::Array(parts)) => {
            for p in parts {
                match p.get("type").and_then(Value::as_str) {
                    Some("text") => content.push(ContentPart::text(
                        p.get("text").and_then(Value::as_str).unwrap_or_default(),
                    )),
                    Some("image_url") => {
                        let url = p
                            .get("image_url")
                            .and_then(|i| i.get("url"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        content.push(parse_image_url(url));
                    }
                    Some("input_audio") => {
                        content.push(parse_input_audio(p.get("input_audio"))?);
                    }
                    other => {
                        return Err(GatewayError::Unsupported(format!(
                            "content part type {other:?}"
                        )))
                    }
                }
            }
        }
        _ => {}
    }

    if role == Role::Assistant {
        // The replayed trace: DeepSeek and llama.cpp spell it
        // `reasoning_content`, OpenRouter and some OpenAI-compatible servers
        // `reasoning`. Same thing, and both used to fall on the floor here —
        // which made llama-server's `--reasoning-preserve` a no-op behind the
        // gateway. It leads the turn, as it did when the model produced it.
        let reasoning = ["reasoning_content", "reasoning"]
            .iter()
            .find_map(|k| m.get(*k).and_then(Value::as_str))
            .filter(|s| !s.is_empty());
        if let Some(r) = reasoning {
            content.insert(0, ContentPart::reasoning(r));
        }
        if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
            for c in calls {
                let f = c.get("function").cloned().unwrap_or_default();
                let args_raw = f.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                let args = serde_json::from_str(args_raw)
                    .unwrap_or_else(|_| Value::String(args_raw.to_string()));
                content.push(ContentPart::ToolUse {
                    id: c
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: f
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    args,
                });
            }
        }
    }

    Ok(Message { role, content })
}

/// A tool message's text: the string, or its text parts joined by newlines.
///
/// OpenAI's tool message carries text only, so that is all lmgw reads from
/// it (§7) — but every part it drops is named in a WARN (llama-egress design
/// §8.3), so a client that put an image there can see why the model never
/// saw it. A tool image belongs in the Anthropic or Responses shape, which
/// carry one.
fn tool_message_text(v: Option<&Value>, tool_call_id: &str) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut texts: Vec<&str> = Vec::with_capacity(parts.len());
            let mut dropped: Vec<String> = Vec::new();
            for (i, p) in parts.iter().enumerate() {
                match p.get("text").and_then(Value::as_str) {
                    Some(t) => texts.push(t),
                    None => dropped.push(format!("#{i} {}", part_type(p))),
                }
            }
            if !dropped.is_empty() {
                tracing::warn!(
                    tool_call_id,
                    "tool message parts dropped (an OpenAI tool message carries text only): {}",
                    dropped.join(", ")
                );
            }
            texts.join("\n")
        }
        None | Some(Value::Null) => String::new(),
        Some(other) => {
            tracing::warn!(
                tool_call_id,
                "tool message content dropped: a {} is neither a string nor an array of parts",
                crate::mcp::spec::kind_of(other)
            );
            String::new()
        }
    }
}

/// A content part's `type` for a log line, or the JSON kind of a part that
/// has none.
fn part_type(p: &Value) -> &str {
    match p.get("type").and_then(Value::as_str) {
        Some(t) => t,
        None if p.is_object() => "untyped object",
        None => crate::mcp::spec::kind_of(p),
    }
}

/// `data:` URIs become Base64 parts; everything else stays a URL.
pub(crate) fn parse_image_url(url: &str) -> ContentPart {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(",") {
            let mime = meta.split(';').next().unwrap_or("image/png").to_string();
            return ContentPart::Image {
                mime,
                source: ImageSource::Base64 {
                    data: data.to_string(),
                },
            };
        }
    }
    let mime = mime_guess::from_path(url.split('?').next().unwrap_or(url))
        .first_raw()
        .unwrap_or("image/jpeg")
        .to_string();
    ContentPart::Image {
        mime,
        source: ImageSource::Url { url: url.into() },
    }
}

/// `input_audio.data` is either a `data:` URI (which keeps its own mime) or
/// raw base64, in which case `input_audio.format` supplies the mime as
/// `audio/<format>` (lowercased) and is required — there is nowhere else to
/// get it from.
pub(crate) fn parse_input_audio(input_audio: Option<&Value>) -> Result<ContentPart, GatewayError> {
    let data = input_audio
        .and_then(|v| v.get("data"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if let Some(rest) = data.strip_prefix("data:") {
        if let Some((meta, payload)) = rest.split_once(',') {
            let mime = meta.split(';').next().unwrap_or("audio/wav").to_string();
            return Ok(ContentPart::Audio {
                mime,
                data: payload.to_string(),
            });
        }
    }
    let format = input_audio
        .and_then(|v| v.get("format"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            GatewayError::BadRequest(
                "input_audio.format is required when data is not a data: URI".into(),
            )
        })?;
    Ok(ContentPart::Audio {
        mime: format!("audio/{}", format.to_lowercase()),
        data: data.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Serialization (IR → OpenAI response shapes)
// ---------------------------------------------------------------------------

fn tool_calls_json(content: &[ContentPart]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolUse { id, name, args } => Some(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(args).unwrap_or_else(|_| "{}".into()),
                }
            })),
            _ => None,
        })
        .collect()
}

fn joined_text(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

pub fn serialize_completion(alias: &str, c: &Completion) -> Value {
    let text = joined_text(&c.content);
    let tool_calls = tool_calls_json(&c.content);
    let mut message = json!({
        "role": "assistant",
        "content": if text.is_empty() && !tool_calls.is_empty() { Value::Null } else { Value::String(text) },
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    if !c.reasoning.is_empty() {
        message["reasoning_content"] = Value::String(c.reasoning.clone());
    }
    json!({
        "id": format!("chatcmpl-{}", rand_id()),
        "object": "chat.completion",
        "created": now_unix(),
        "model": alias,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": c.finish_reason.to_openai(),
        }],
        "usage": usage_json(&c.usage),
    })
}

fn usage_json(u: &Usage) -> Value {
    let p = u.prompt_tokens.unwrap_or(0);
    let c = u.completion_tokens.unwrap_or(0);
    let mut out = json!({"prompt_tokens": p, "completion_tokens": c, "total_tokens": p + c});
    // The detail objects, in OpenAI's own shape, when the upstream reported
    // them — including an Anthropic upstream, whose cache counters the egress
    // adapter folded into the total. A client that caches can otherwise see
    // only the total and has no way to tell a cache hit from a cold prompt.
    // Absent stays absent: an unreported counter and a zero one are different
    // statements. A cache write is OpenAI's `cache_write_tokens` (GPT-5.6 on)
    // or Anthropic's `cache_creation_input_tokens`, both a subset of the total.
    let mut prompt_details = serde_json::Map::new();
    if let Some(v) = u.cached_input_tokens {
        prompt_details.insert("cached_tokens".into(), json!(v));
    }
    if let Some(v) = u.cache_write_tokens {
        prompt_details.insert("cache_write_tokens".into(), json!(v));
    }
    if !prompt_details.is_empty() {
        out["prompt_tokens_details"] = Value::Object(prompt_details);
    }
    if let Some(v) = u.reasoning_tokens {
        out["completion_tokens_details"] = json!({"reasoning_tokens": v});
    }
    out
}

/// Streaming encoder: IR deltas → `chat.completion.chunk` SSE frames.
pub struct OpenaiStreamEncoder {
    id: String,
    alias: String,
    created: i64,
    sent_role: bool,
    sent_finish: bool,
    /// The client's `stream_options.include_usage` ([`stream_include_usage`]).
    /// Without it no chunk carries `usage` and none has empty `choices`, so
    /// the SDK loop that reads `chunk.choices[0]` holds to the end.
    include_usage: bool,
    usage: Option<Usage>,
}

impl OpenaiStreamEncoder {
    pub fn new(alias: &str, include_usage: bool) -> Self {
        Self {
            id: format!("chatcmpl-{}", rand_id()),
            alias: alias.to_string(),
            created: now_unix(),
            sent_role: false,
            sent_finish: false,
            include_usage,
            usage: None,
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> String {
        let mut body = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.alias,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }],
        });
        // OpenAI's own shape when usage was asked for: every chunk but the
        // last carries the key, null.
        if self.include_usage {
            body["usage"] = Value::Null;
        }
        frame(None, &body.to_string())
    }

    fn role_chunk(&mut self) -> String {
        if self.sent_role {
            return String::new();
        }
        self.sent_role = true;
        self.chunk(json!({"role": "assistant", "content": ""}), None)
    }
}

impl ClientStreamEncoder for OpenaiStreamEncoder {
    fn start(&mut self) -> String {
        self.role_chunk()
    }

    fn delta(&mut self, d: &StreamDelta) -> String {
        let mut out = String::new();
        match d {
            StreamDelta::TextDelta(t) => {
                if t.is_empty() {
                    return out;
                }
                out.push_str(&self.role_chunk());
                out.push_str(&self.chunk(json!({"content": t}), None));
            }
            // Forward reasoning as the OpenAI/DeepSeek `reasoning_content` delta
            // field. Previously dropped, so reasoning-model output was lost on
            // /v1 too; non-reasoning models never produce this, so their stream
            // is unchanged.
            StreamDelta::ReasoningDelta(r) => {
                if r.is_empty() {
                    return out;
                }
                out.push_str(&self.role_chunk());
                out.push_str(&self.chunk(json!({"reasoning_content": r}), None));
            }
            StreamDelta::ToolCallStart { index, id, name } => {
                out.push_str(&self.role_chunk());
                out.push_str(&self.chunk(
                    json!({"tool_calls": [{
                        "index": index,
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": ""},
                    }]}),
                    None,
                ));
            }
            StreamDelta::ToolCallArgsDelta { index, fragment } => {
                out.push_str(&self.chunk(
                    json!({"tool_calls": [{
                        "index": index,
                        "function": {"arguments": fragment},
                    }]}),
                    None,
                ));
            }
            // Kept whether or not the client sees it: only `finish` decides.
            StreamDelta::Usage(u) => {
                let mut merged = self.usage.unwrap_or_default();
                merged.merge(u);
                self.usage = Some(merged);
            }
            StreamDelta::Stop(reason) => {
                if !self.sent_finish {
                    self.sent_finish = true;
                    out.push_str(&self.chunk(json!({}), Some(reason.to_openai())));
                }
            }
            // Internal-only (Chat tab stats); never part of the public wire.
            StreamDelta::Timings(_) => {}
            StreamDelta::Error(msg) => {
                let body =
                    json!({"error": {"message": msg, "type": "api_error", "code": "upstream"}});
                out.push_str(&frame(None, &body.to_string()));
            }
        }
        out
    }

    fn finish(&mut self) -> String {
        let mut out = String::new();
        if !self.sent_finish {
            self.sent_finish = true;
            out.push_str(&self.chunk(json!({}), Some("stop")));
        }
        // The usage-only chunk is OpenAI's answer to `include_usage`, and
        // only to it. An upstream that reported no usage gets none invented
        // here.
        if let Some(u) = self.usage.as_ref().filter(|_| self.include_usage) {
            let body = json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.alias,
                "choices": [],
                "usage": usage_json(u),
            });
            out.push_str(&frame(None, &body.to_string()));
        }
        out.push_str("data: [DONE]\n\n");
        out
    }
}
