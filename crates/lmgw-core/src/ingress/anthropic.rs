//! Anthropic ingress: `/v1/messages` parser + serializer (§6).

use serde_json::{json, Value};

use crate::error::GatewayError;
use crate::ingress::{rand_id, ClientStreamEncoder};
use crate::ir::structured::DEFAULT_SCHEMA_NAME;
use crate::ir::{
    AnthropicBlock, BlockEvent, ChatRequest, Compaction, CompactionEvent, Completion, ContentPart,
    ImageSource, JsonSchemaFormat, Message, Params, ReasoningControl, ResponseFormat, Role, Seal,
    StreamDelta, ToolChoice, ToolDef, ToolResultBlock, Usage,
};
use crate::sse::frame;

mod compaction;

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
                // The client's own, else Anthropic's default: not strict
                // (ToolDef::strict).
                strict: Some(t.get("strict").and_then(Value::as_bool).unwrap_or(false)),
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
        reasoning: parse_reasoning(body)?,
        // A send's own, set by what the provider taught it.
        reasoning_summary_refused: false,
        // Anthropic's thinking has no summary choice.
        reasoning_summary: None,
    };

    Ok(ChatRequest {
        model_alias: model,
        messages,
        params,
        tools,
        tool_choice,
        response_format: parse_output_format(body)?,
        stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        // Anthropic has no `grammar` or sampler-extension equivalents; nothing
        // to carry through (these only matter on the OpenAI→OpenAI route).
        passthrough: Default::default(),
        // No template kwargs in this dialect.
        llama_kwargs_enabled: None,
        // A header, not a body field: the handler fills it from the request.
        anthropic_beta: Vec::new(),
        // Anthropic's own field, kept whole for an Anthropic upstream, which
        // answers for it; every other route names it ignored
        // (`ChatRequest::fields_ignored`). A `null` is no request for it.
        context_management: body
            .get("context_management")
            .filter(|v| !v.is_null())
            .cloned(),
        // On-demand compaction: kept whole for an Anthropic upstream, which
        // answers it with a `compaction` block; every other route names it
        // ignored.
        compaction: body.get("compaction").filter(|v| !v.is_null()).cloned(),
        // How Anthropic returns the thinking text: sent only to Anthropic,
        // beside a type that takes it (`ChatRequest::fields_ignored`).
        thinking_display: body
            .pointer("/thinking/display")
            .filter(|v| !v.is_null())
            .cloned(),
    })
}

/// Anthropic's structured output (structured-output design §5):
/// `output_config.format`, else the deprecated top-level `output_format`
/// Anthropic still accepts (§11.12). Its one type, `json_schema`, carries a
/// bare schema: the IR names it `response` (OpenAI requires a name, §11.4) and
/// records `strict: true`, OpenAI's spelling of the constrained decoding
/// Anthropic always applies. A format that is not an object, or has no string
/// `type`, is malformed (`400 bad_request`); any other type has no
/// translation and is refused by name (§11.19).
fn parse_output_format(body: &Value) -> Result<Option<ResponseFormat>, GatewayError> {
    let (at, f) = match (
        body.pointer("/output_config/format"),
        body.get("output_format"),
    ) {
        (Some(f), _) if !f.is_null() => ("output_config.format", f),
        (_, Some(f)) if !f.is_null() => ("output_format", f),
        _ => return Ok(None),
    };
    let o = f
        .as_object()
        .ok_or_else(|| GatewayError::BadRequest(format!("'{at}' must be an object")))?;
    let t = o
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::BadRequest(format!("'{at}' has no string 'type'")))?;
    match t {
        "json_schema" => Ok(Some(ResponseFormat::JsonSchema(JsonSchemaFormat {
            name: Some(DEFAULT_SCHEMA_NAME.to_string()),
            schema: o.get("schema").filter(|s| !s.is_null()).cloned(),
            strict: Some(true),
            ..Default::default()
        }))),
        other => Err(GatewayError::ResponseFormatUnsupported {
            message: format!(
                "'{at}' asks for type '{other}', which lmgw does not translate — json_schema is \
                 the one type Anthropic defines"
            ),
        }),
    }
}

/// Anthropic's own reasoning controls (§5.2): the `thinking` block and
/// `output_config.effort`. Both used to be dropped on the floor here — an
/// Anthropic client asking for extended thinking through lmgw got a request
/// with no thinking in it and no word about why.
///
/// `pub(crate)` for `POST /v1/messages/count_tokens` (api-docs design §5.2):
/// its Anthropic route passes the body through, so it reads this tier on its
/// own rather than through a full parse that would refuse a server tool.
///
/// Every `thinking` type Messages defines is read; one lmgw does not know is
/// refused by name (`400`) rather than dropped, which would send the request
/// without the thinking the client chose.
pub(crate) fn parse_reasoning(body: &Value) -> Result<Option<ReasoningControl>, GatewayError> {
    let mut c = ReasoningControl::default();
    if let Some(t) = body.get("thinking").filter(|t| !t.is_null()) {
        match t.get("type").and_then(Value::as_str) {
            Some("disabled") => c.enabled = Some(false),
            Some("enabled") => {
                c.enabled = Some(true);
                c.budget_tokens = t.get("budget_tokens").and_then(Value::as_i64);
            }
            // `adaptive` is "think as much as you judge necessary" — enabled
            // with no budget, which is exactly the neutral triple.
            Some("adaptive") => c.enabled = Some(true),
            // No up-front thinking, notes between tool calls: an off in
            // Anthropic's own spelling, which its egress sends as written
            // and every other route renders as its own off.
            Some("between_tools") => {
                c.enabled = Some(false);
                c.between_tools = true;
            }
            Some(other) => {
                return Err(GatewayError::BadRequest(format!(
                    "thinking type {other:?} is not one lmgw knows (adaptive, between_tools, \
                     disabled, enabled)"
                )))
            }
            None => {
                return Err(GatewayError::BadRequest(
                    "'thinking' needs a string 'type'".into(),
                ))
            }
        }
    }
    if let Some(e) = body
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
    {
        c.effort = Some(e.to_string());
    }
    Ok((!c.is_empty()).then_some(c))
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
                    // A cited text block of an earlier answer keeps its
                    // citations, for an Anthropic upstream to read again:
                    // the client's own, with no scope.
                    Some("text") => parts.push(ContentPart::cited(
                        b.get("text").and_then(Value::as_str).unwrap_or_default(),
                        b.get("citations")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default(),
                        None,
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
                            // Any other source (a Files API `file`, a type
                            // lmgw does not know) names what only Anthropic
                            // holds: the block goes to an Anthropic-protocol
                            // upstream as the client wrote it, as a document
                            // with that source does, and every other upstream
                            // is sent none of it, named `content.image`
                            // (`ir::AnthropicBlock`).
                            Some(_) => parts.push(ContentPart::AnthropicBlock(
                                AnthropicBlock::from_client(b.clone()),
                            )),
                            None => {
                                return Err(GatewayError::Unsupported(
                                    "an image block without a source type".into(),
                                ))
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
                        // gateway did from a model without a seal (see
                        // `serialize_completion`), in which case it is bound
                        // for an OpenAI-shaped upstream as `reasoning_content`
                        // — llama-server's `--reasoning-preserve` needs exactly
                        // this. A signature this gateway wrote for an OpenAI
                        // reasoning item (`lmgw-openai:…`, openai egress
                        // design §7.3.2) is that item's seal again, kept with
                        // an empty trace too: the seal is what the platform
                        // reads. So is Anthropic's own signature: an omitted
                        // display's block has no text, and its signature is
                        // the thinking Anthropic reads back.
                        let text = b
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let signature = b
                            .get("signature")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty());
                        let sealed = match signature.and_then(Seal::from_carrier) {
                            Some(read) => Some(read.map_err(GatewayError::BadRequest)?),
                            None => None,
                        };
                        if !text.is_empty() || signature.is_some() {
                            // The client's own signature: no signer, it
                            // goes as the client wrote it.
                            parts.push(ContentPart::Reasoning {
                                text: text.to_string(),
                                signature: signature.filter(|_| sealed.is_none()).map(String::from),
                                sealed,
                                signature_scope: None,
                            });
                        }
                    }
                    // Anthropic's summary of the conversation before it,
                    // kept whole for Anthropic and read as its summary
                    // everywhere else (`ir::compaction`).
                    Some(crate::ir::compaction::BLOCK_TYPE) => {
                        parts.push(ContentPart::Compaction(Compaction::from_block(b)));
                    }
                    // Every other block, `redacted_thinking` (encrypted by
                    // Anthropic, readable by Anthropic alone) and a type
                    // this gateway does not know included, goes to an
                    // Anthropic-protocol upstream as the client wrote it;
                    // every other upstream is sent none of it, and
                    // `x-lmgw-params-ignored` names it (`ir::AnthropicBlock`).
                    Some(_) => parts.push(ContentPart::AnthropicBlock(
                        AnthropicBlock::from_client(b.clone()),
                    )),
                    None => {
                        return Err(GatewayError::BadRequest(
                            "a content block without a string 'type' is no Anthropic block".into(),
                        ))
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
/// The content takes `text`, `image`, `document` and `search_result`
/// blocks (<https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls>,
/// a browser use member's result a `browser_state` too). Every block but
/// `text` and an image the IR reads ([`tool_result_image`]) is
/// [`ToolResultBlock::AnthropicBlock`] (gateway design §6.1, changed
/// 2026-11-06; it used to be kept as JSON, which went to Anthropic as a
/// text block and to every other row inlined, a document's file id and
/// all): an Anthropic-protocol row is sent it as the client wrote it, every
/// other row what of it reads as text ([`AnthropicBlock::tool_text`]), and
/// a type this gateway does not know reaches the model there as its JSON,
/// rather than 400-ing an otherwise valid conversation.
fn parse_tool_result_content(v: Option<&Value>) -> Vec<ToolResultBlock> {
    match v {
        Some(Value::String(s)) => ToolResultBlock::one(s.clone()),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => {
                    ToolResultBlock::text(b.get("text").and_then(Value::as_str).unwrap_or_default())
                }
                Some("image") => tool_result_image(b),
                _ => ToolResultBlock::AnthropicBlock(AnthropicBlock::from_client(b.clone())),
            })
            .collect(),
        // A `tool_result` with structured output and no content blocks.
        Some(other) => vec![ToolResultBlock::Json {
            value: other.clone(),
        }],
        None => Vec::new(),
    }
}

/// A `tool_result` image block by its source. Base64 (or no source type) is
/// the image itself. A URL is the resource naming it, as the Responses
/// ingress names its URL images ([`crate::ingress::responses::url_image_resource`]):
/// the IR's tool image carries bytes, and an image block with none used to
/// reach every egress as an empty image. Any other source (a Files API
/// `file`) names what only Anthropic holds: the block goes to an
/// Anthropic-protocol row as the client wrote it, as a message's does, and
/// every other row is sent none of it, named `content.image`
/// ([`ToolResultBlock::AnthropicBlock`], changed 2026-11-06; it used to be
/// kept as JSON, which an OpenAI-shaped egress inlined, file id and all).
fn tool_result_image(b: &Value) -> ToolResultBlock {
    let src = b.get("source");
    let field = |k: &str| src.and_then(|s| s.get(k)).and_then(Value::as_str);
    match field("type") {
        Some("base64") | None => ToolResultBlock::Image {
            mime: field("media_type").unwrap_or("image/png").to_string(),
            data: field("data").unwrap_or_default().to_string(),
        },
        Some("url") => match field("url").filter(|u| !u.is_empty()) {
            Some(url) => crate::ingress::responses::url_image_resource(url),
            None => ToolResultBlock::Json { value: b.clone() },
        },
        Some(_) => ToolResultBlock::AnthropicBlock(AnthropicBlock::from_client(b.clone())),
    }
}

// ---------------------------------------------------------------------------
// Serialization (IR → Anthropic response shapes)
// ---------------------------------------------------------------------------

fn content_json(content: &[ContentPart]) -> Vec<Value> {
    let mut out = Vec::new();
    for p in content {
        match p {
            // Anthropic's wire has no refusal of its own: the model's words
            // are its text.
            ContentPart::Refusal { text } => {
                if !text.is_empty() {
                    out.push(json!({"type": "text", "text": text}));
                }
            }
            // A cited block with its citations, as Anthropic answered it.
            ContentPart::Text {
                text, citations, ..
            } => {
                if !text.is_empty() {
                    out.push(crate::egress::anthropic::text_block(text, citations));
                }
            }
            ContentPart::ToolUse { id, name, args } => {
                out.push(json!({"type": "tool_use", "id": id, "name": name, "input": args}));
            }
            // A reasoning item in place (an answer whose reasoning came
            // sealed or signed): its thinking block, its seal as the
            // `signature` (openai egress design §7.3.2, decision 17's
            // prefix), or Anthropic's own signature as it came, beside the
            // block's text, which an omitted display leaves empty.
            // An unsigned one beside them has the empty `signature` the
            // `ThinkingBlock` shape requires, which reads back as none.
            ContentPart::Reasoning {
                text,
                signature,
                sealed,
                ..
            } => {
                let signature = match (sealed, signature) {
                    (Some(seal), _) => seal.carrier(),
                    (None, Some(sig)) => sig.clone(),
                    (None, None) => String::new(),
                };
                out.push(json!({"type": "thinking", "thinking": text, "signature": signature}));
            }
            // As the Anthropic upstream answered it.
            ContentPart::Compaction(c) => out.push(c.to_block()),
            // A block the IR does not model, where it stood, as it came.
            ContentPart::AnthropicBlock(b) => out.push(b.block.clone()),
            _ => {}
        }
    }
    out
}

pub fn serialize_completion(alias: &str, c: &Completion) -> Value {
    let mut blocks = content_json(&c.content);
    // Surface reasoning as a leading `thinking` block. Its signature is empty
    // — it came from a non-Anthropic upstream — and when the client sends it
    // back, `parse_content` turns it into the unsigned reasoning part an
    // OpenAI-shaped upstream replays as `reasoning_content`. An answer whose
    // reasoning came sealed, or signed by an Anthropic upstream, has its
    // thinking blocks in place instead, each carrying its seal or signature
    // ([`content_json`]).
    if !c.reasoning.is_empty() && !c.reasoning_in_place() {
        blocks.insert(
            0,
            json!({"type": "thinking", "thinking": c.reasoning, "signature": ""}),
        );
    }
    let mut out = json!({
        "id": format!("msg_{}", rand_id()),
        "type": "message",
        "role": "assistant",
        "model": alias,
        "content": Value::Array(blocks),
        "stop_reason": c.finish_reason.to_anthropic(),
        // The sequence the model stopped at, when its upstream named it
        // (`FinishReason::StopSequence`); `null` otherwise, as Anthropic's.
        "stop_sequence": c.finish_reason.stop_sequence(),
        "usage": anthropic_usage(&c.usage, c.usage_iterations.as_ref()),
    });
    // The edits an Anthropic upstream applied, as it reported them.
    if let Some(cm) = &c.context_management {
        out["context_management"] = cm.clone();
    }
    out
}

/// The IR's usage, back in Anthropic's three-disjoint-counters shape.
///
/// `ir::Usage::prompt_tokens` is the **total** input with cache included (the
/// egress adapter sums Anthropic's three counters into it, so the same
/// conversation reports the same size in either dialect). Emitting that total
/// as `input_tokens` would inflate what an Anthropic client sees by the cache
/// counts and drop the cache fields entirely — so the sum is undone here, and
/// the round trip is exact.
///
/// `iterations` is an Anthropic answer's `usage.iterations`
/// ([`Completion::usage_iterations`]): relayed verbatim, and its compaction
/// entries, which the IR's usage counts in, taken out of the top-level
/// counts again ([`compaction::top_level`]).
fn anthropic_usage(u: &crate::ir::Usage, iterations: Option<&crate::ir::UsageIterations>) -> Value {
    let u = &compaction::top_level(u, iterations);
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
    if let Some(it) = iterations {
        usage["iterations"] = it.list.clone();
    }
    usage
}

#[derive(PartialEq)]
enum OpenBlock {
    None,
    Text,
    Tool,
    Thinking,
    Compaction,
    /// A block the IR does not model ([`AnthropicBlock`]).
    Verbatim,
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
    /// The stop sequence the upstream said the model stopped at.
    stop_sequence: Option<String>,
    usage: Usage,
    /// The `context_management` an Anthropic upstream's stream reported,
    /// relayed on the closing `message_delta`, where Anthropic's own sits.
    context_management: Option<Value>,
    /// The `usage.iterations` an Anthropic upstream's stream reported,
    /// relayed on the closing `message_delta`'s usage.
    usage_iterations: Option<crate::ir::UsageIterations>,
    /// The answer ended in an `error` event, as Anthropic's own streams
    /// can: nothing follows it, no clean `message_delta`/`message_stop`.
    errored: bool,
    /// How the last reasoning item closed: `None` before any, `Some(true)`
    /// for Anthropic's own signature. Only a block that follows unsigned or
    /// sealed reasoning loses the blank line a joined trace put ahead of it
    /// ([`StreamDelta::ReasoningDelta`]'s arm); Anthropic's thinking is never
    /// joined, and its text passes as it came, which its signature covers.
    last_signed: Option<bool>,
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
            stop_sequence: None,
            usage: Usage::default(),
            context_management: None,
            usage_iterations: None,
            errored: false,
            last_signed: None,
        }
    }

    /// A verbatim block's event, as the Anthropic upstream sent it: its
    /// `content_block_start` opens a block of its own with the block as it
    /// began, a delta at it goes out as it came, and its stop closes it when
    /// the upstream's came (else where the next block starts or the answer
    /// ends, as every block does). A delta or stop with no verbatim block
    /// open has nowhere to go and is dropped.
    fn verbatim(&mut self, ev: &BlockEvent) -> String {
        match ev {
            BlockEvent::Start(b) => {
                let mut out = self.close_open_block();
                let idx = self.next_block_index;
                self.next_block_index += 1;
                self.open = OpenBlock::Verbatim;
                out.push_str(&frame(
                    Some("content_block_start"),
                    &json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": b.block,
                    })
                    .to_string(),
                ));
                out
            }
            BlockEvent::Delta(d) if self.open == OpenBlock::Verbatim => frame(
                Some("content_block_delta"),
                &json!({
                    "type": "content_block_delta",
                    "index": self.next_block_index - 1,
                    "delta": d,
                })
                .to_string(),
            ),
            BlockEvent::Delta(_) => String::new(),
            BlockEvent::Stop if self.open == OpenBlock::Verbatim => self.close_open_block(),
            BlockEvent::Stop => String::new(),
        }
    }

    /// A compaction block's event, as the Anthropic upstream sent it: its
    /// `content_block_start` opens a block of its own with the block as it
    /// began, a delta at it goes out as it came. A delta with no
    /// compaction block open has nowhere to go and is dropped.
    fn compaction(&mut self, ev: &CompactionEvent) -> String {
        match ev {
            CompactionEvent::Start(c) => {
                let mut out = self.close_open_block();
                let idx = self.next_block_index;
                self.next_block_index += 1;
                self.open = OpenBlock::Compaction;
                out.push_str(&frame(
                    Some("content_block_start"),
                    &json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": c.to_block(),
                    })
                    .to_string(),
                ));
                out
            }
            CompactionEvent::Delta(d) if self.open == OpenBlock::Compaction => frame(
                Some("content_block_delta"),
                &json!({
                    "type": "content_block_delta",
                    "index": self.next_block_index - 1,
                    "delta": d,
                })
                .to_string(),
            ),
            CompactionEvent::Delta(_) => String::new(),
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
                // As Anthropic's own stream starts one: the signature, empty
                // here, comes as a `signature_delta` when the block has one.
                "content_block": {"type": "thinking", "thinking": "", "signature": ""},
            })
            .to_string(),
        ));
        out
    }

    /// The open thinking block's `signature_delta`, which closes it; with
    /// none open, a block of its own opens for it.
    fn sign_thinking(&mut self, signature: &str) -> String {
        let mut out = self.ensure_thinking_block();
        let idx = self.next_block_index - 1;
        out.push_str(&frame(
            Some("content_block_delta"),
            &json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": {"type": "signature_delta", "signature": signature},
            })
            .to_string(),
        ));
        out.push_str(&self.close_open_block());
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
            // Anthropic's wire has no refusal of its own: the model's words
            // are its text.
            StreamDelta::TextDelta(t) | StreamDelta::RefusalDelta(t) => {
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
            // A block that opens here starts without the blank line the
            // platform's decoder puts between two reasoning items' summaries
            // (`egress/openai/responses/decode.rs`): that line joins a whole
            // trace, and this block is one item's, as a whole answer's
            // thinking block is. Only reasoning of that kind is joined so:
            // after Anthropic's own signed block, or as the answer's first,
            // the text is the block's as it came (`last_signed`).
            StreamDelta::ReasoningDelta(r) => {
                let joined = self.last_signed == Some(false);
                let r = match self.open == OpenBlock::Thinking || !joined {
                    true => r.as_str(),
                    false => r.trim_start_matches('\n'),
                };
                self.last_signed = Some(false);
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
                // Only the open tool block takes a delta: a block already
                // closed gets none (an event after its `content_block_stop`
                // is invalid), which is where the `{}` filler of an
                // argument-less call lands, and that call's start already
                // carries `input: {}`.
                if self.open != OpenBlock::Tool || idx + 1 != self.next_block_index {
                    return String::new();
                }
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
                self.stop_sequence = reason.stop_sequence().map(str::to_string);
                String::new()
            }
            // Internal-only (Chat tab stats); never part of the public wire.
            StreamDelta::Timings(_) => String::new(),
            // Anthropic's `service_tier` has its own vocabulary: nothing is
            // mapped onto it (openai egress design §5.7).
            StreamDelta::ServiceTier(_) => String::new(),
            // lmgw's own record of the answer, not the client's.
            StreamDelta::UpstreamItems(_) => String::new(),
            StreamDelta::ContextManagement(cm) => {
                self.context_management = Some(cm.clone());
                String::new()
            }
            StreamDelta::Compaction(ev) => self.compaction(ev),
            StreamDelta::UsageIterations(it) => {
                self.usage_iterations = Some(it.clone());
                String::new()
            }
            // The item's seal is its thinking block's `signature`, as
            // Anthropic's own streams end a thinking block with a
            // `signature_delta` (openai egress design §7.3.2); the block
            // closes with it, the next reasoning is another item's. An item
            // that streamed no summary opens its block for it.
            StreamDelta::ReasoningSeal(seal) => {
                self.last_signed = Some(false);
                self.sign_thinking(&seal.carrier())
            }
            // Anthropic's own signature, as its stream ended the block; a
            // block that streamed no thinking (an omitted display) opens
            // for it, as Anthropic's own stream has it.
            StreamDelta::ReasoningSignature { signature, .. } => {
                self.last_signed = Some(true);
                self.sign_thinking(signature)
            }
            // A block the IR does not model, as Anthropic streamed it.
            StreamDelta::AnthropicBlock(ev) => self.verbatim(ev),
            // A text block's end: its citations, one `citations_delta`
            // each, and its stop, so the text after it is another block's,
            // as the upstream's was.
            StreamDelta::TextBlockEnd { citations: c, .. } => {
                if c.is_empty() && self.open != OpenBlock::Text {
                    return String::new();
                }
                let mut out = self.ensure_text_block();
                let idx = self.next_block_index - 1;
                for citation in c {
                    out.push_str(&frame(
                        Some("content_block_delta"),
                        &json!({
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": {"type": "citations_delta", "citation": citation},
                        })
                        .to_string(),
                    ));
                }
                out.push_str(&self.close_open_block());
                out
            }
            StreamDelta::Error(msg) => {
                self.errored = true;
                frame(
                    Some("error"),
                    &json!({
                        "type": "error",
                        "error": {"type": "api_error", "message": msg},
                    })
                    .to_string(),
                )
            }
        }
    }

    fn finish(&mut self) -> String {
        if self.errored {
            return String::new();
        }
        let mut out = self.close_open_block();
        let stop_reason = self
            .stop_reason
            .clone()
            .unwrap_or_else(|| "end_turn".into());
        let iterations = self.usage_iterations.take();
        let full = anthropic_usage(&self.usage, iterations.as_ref());
        let mut usage = json!({"output_tokens": full["output_tokens"]});
        if self.usage.prompt_tokens.is_some() {
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
        if let Some(it) = iterations {
            usage["iterations"] = it.list;
        }
        let mut delta = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": self.stop_sequence},
            "usage": usage,
        });
        if let Some(cm) = self.context_management.take() {
            delta["context_management"] = cm;
        }
        out.push_str(&frame(Some("message_delta"), &delta.to_string()));
        out.push_str(&frame(
            Some("message_stop"),
            &json!({"type": "message_stop"}).to_string(),
        ));
        out
    }
}
