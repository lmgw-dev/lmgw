//! Anthropic egress (§7): `/v1/messages`, `x-api-key` + `anthropic-version`
//! headers, content-block SSE events.

use serde_json::{json, Map, Value};

use crate::config::{Protocol, Upstream};
use crate::egress::{finish_from_anthropic, CountPlan, Egress, EgressStreamDecoder};
use crate::error::GatewayError;
use crate::ir::{
    flatten_tool_result, wire_call_id, ChatRequest, Completion, ContentPart, FinishReason,
    ImageSource, Params, ReasoningControl, Role, StreamDelta, ToolChoice, ToolResultBlock, Usage,
};
use crate::sse::SseEvent;

/// Why an audio part cannot go to the Anthropic API: it has no block for
/// one. A heard voice turn's responder reads it as lmgw's own refusal of
/// the audio (`realtime::thread::turn::audio::refusal`).
pub const NO_AUDIO_BLOCK: &str = "audio input has no Anthropic block type";

const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Anthropic requires max_tokens; the last resort when neither the client, the
/// alias, nor the provider's own catalog states one (§5.4 — the handler
/// resolves the catalog's value into `route.param_defaults` first, and reports
/// this substitution when it has to fall through to here).
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 4096;

pub struct AnthropicEgress;

const BETA_HEADER: &str = "anthropic-beta";

/// The client's `anthropic-beta` flags: every line of the header, split on
/// commas (the SDKs send one comma-joined line; HTTP allows several), trimmed,
/// each flag once. A line that is not valid UTF-8 is kept lossily, so the
/// egress refuses it by name ([`with_betas`]) instead of it vanishing here.
pub(crate) fn client_betas(headers: &axum::http::HeaderMap) -> Vec<String> {
    let mut flags: Vec<String> = Vec::new();
    for line in headers.get_all(BETA_HEADER) {
        for flag in String::from_utf8_lossy(line.as_bytes()).split(',') {
            let flag = flag.trim();
            if !flag.is_empty() && !flags.iter().any(|f| f == flag) {
                flags.push(flag.to_string());
            }
        }
    }
    flags
}

/// The upstream row's extra headers plus the client's beta flags, with every
/// `anthropic-beta` folded into **one** header: the row's flags first, then
/// the client's, each once. `reqwest`'s `header` appends, so applying the
/// row's headers and then the client's would send two lines, and whether the
/// provider reads both is not something to leave to chance.
fn with_betas(
    mut rb: reqwest::RequestBuilder,
    up: &Upstream,
    client: &[String],
) -> Result<reqwest::RequestBuilder, GatewayError> {
    let mut flags: Vec<&str> = Vec::new();
    for (name, value) in &up.extra_headers {
        if name.eq_ignore_ascii_case(BETA_HEADER) {
            flags.extend(value.split(',').map(str::trim).filter(|f| !f.is_empty()));
        } else {
            rb = rb.header(name, value);
        }
    }
    for flag in client {
        flags.push(flag);
    }
    let mut seen = std::collections::HashSet::new();
    flags.retain(|f| seen.insert(*f));
    if flags.is_empty() {
        return Ok(rb);
    }
    // A flag is an ASCII token. `HeaderValue` would take any byte above 0x7f,
    // so a mangled one would reach the provider instead of being refused here.
    let joined = flags.join(",");
    let value = Some(joined.as_str())
        .filter(|j| j.bytes().all(|b| b.is_ascii_graphic()))
        .and_then(|j| reqwest::header::HeaderValue::from_str(j).ok())
        .ok_or_else(|| {
            GatewayError::BadRequest(format!(
                "anthropic-beta {joined:?} is not a list of ASCII flags"
            ))
        })?;
    Ok(rb.header(BETA_HEADER, value))
}

/// IR tool-result blocks → Anthropic `tool_result.content` blocks.
///
/// Anthropic is the only upstream protocol whose tool-result slot takes a block
/// array, so images pass through natively here instead of becoming a
/// placeholder. Everything it has no block type for (JSON, audio, resources)
/// falls back to text via [`flatten_tool_result`], which is lossless for those
/// kinds — they are text-representable, unlike a binary image.
fn tool_result_blocks_json(blocks: &[ToolResultBlock]) -> Vec<Value> {
    let mut out = Vec::new();
    // Consecutive text-representable blocks are flattened together so a result
    // of three text blocks stays one text block, as it was before.
    let mut pending: Vec<ToolResultBlock> = Vec::new();
    let flush = |pending: &mut Vec<ToolResultBlock>, out: &mut Vec<Value>| {
        if pending.is_empty() {
            return;
        }
        let (text, _) = flatten_tool_result(pending);
        pending.clear();
        if !text.is_empty() {
            out.push(json!({"type": "text", "text": text}));
        }
    };
    for b in blocks {
        match b {
            ToolResultBlock::Image { mime, data } => {
                flush(&mut pending, &mut out);
                out.push(json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": mime, "data": data},
                }));
            }
            other => pending.push(other.clone()),
        }
    }
    flush(&mut pending, &mut out);
    out
}

fn content_blocks(parts: &[ContentPart]) -> Result<Vec<Value>, GatewayError> {
    let mut out = Vec::new();
    for p in parts {
        match p {
            ContentPart::Text { text } => {
                if !text.is_empty() {
                    out.push(json!({"type": "text", "text": text}));
                }
            }
            ContentPart::Image { mime, source } => out.push(match source {
                ImageSource::Base64 { data } => json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": mime, "data": data},
                }),
                ImageSource::Url { url } => json!({
                    "type": "image",
                    "source": {"type": "url", "url": url},
                }),
            }),
            ContentPart::Audio { .. } => {
                return Err(GatewayError::Unsupported(NO_AUDIO_BLOCK.into()));
            }
            ContentPart::ToolUse { id, name, args } => {
                // A Gemini signature stays with Gemini (gateway design §7.1).
                out.push(json!({
                    "type": "tool_use", "id": wire_call_id(id), "name": name, "input": args,
                }));
            }
            ContentPart::ToolResult {
                id,
                content,
                is_error,
                ..
            } => {
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": wire_call_id(id),
                    "content": Value::Array(tool_result_blocks_json(content)),
                });
                if *is_error {
                    block["is_error"] = json!(true);
                }
                out.push(block);
            }
            ContentPart::Reasoning { text, signature } => match signature {
                // Signed by Anthropic, so it goes back to Anthropic verbatim.
                Some(sig) => out.push(json!({
                    "type": "thinking", "thinking": text, "signature": sig,
                })),
                // Anthropic rejects an unsigned thinking block, and a trace
                // from llama.cpp or Gemini has no signature to give it.
                None => tracing::debug!(
                    chars = text.len(),
                    "dropping unsigned reasoning for anthropic egress"
                ),
            },
        }
    }
    Ok(out)
}

fn messages_json(ir: &ChatRequest) -> Result<Vec<Value>, GatewayError> {
    let mut out: Vec<Value> = Vec::new();
    for m in ir.non_system_messages() {
        // Anthropic only has user/assistant; tool results ride in user turns.
        let role = match m.role {
            Role::Assistant => "assistant",
            _ => "user",
        };
        let blocks = content_blocks(&m.content)?;
        if blocks.is_empty() {
            continue;
        }
        // Merge consecutive same-role turns (Anthropic requires alternation).
        if let Some(last) = out.last_mut() {
            if last.get("role").and_then(Value::as_str) == Some(role) {
                if let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut) {
                    arr.extend(blocks);
                    continue;
                }
            }
        }
        out.push(json!({"role": role, "content": blocks}));
    }
    Ok(out)
}

/// The `max_tokens` floor a thinking budget imposes, or `None` when the
/// control carries no budget.
///
/// The API refuses a request whose `max_tokens` does not exceed
/// `thinking.budget_tokens`, so a budget bigger than the cap raises the cap
/// instead of earning a 400 the client did not ask for (§5.3). The handler
/// calls this too, to report the raise it made (`x-lmgw-max-tokens-raised`).
///
/// A budget that does not fit a `u32` is refused by name rather than wrapped
/// into a small number: `i64::MAX as u32` is 4294967295 as a truncation and
/// something else again after `+ 1024`, and a cap invented that way is exactly
/// the kind of silent, wrong bound the gateway must never produce.
pub(crate) fn budget_floor(c: &ReasoningControl) -> Result<Option<u32>, GatewayError> {
    let Some(b) = c.budget_tokens else {
        return Ok(None);
    };
    let fits = u32::try_from(b).map_err(|_| {
        GatewayError::BadRequest(format!("reasoning budget {b} does not fit this route"))
    })?;
    Ok(Some(fits.saturating_add(1024)))
}

/// Render the normalised reasoning triple (§5.3) into Anthropic's shape.
///
/// Exactly one `thinking` type, and `output_config.effort` only next to
/// `adaptive`: the budget form already says how much to think, and sending a
/// level beside it would be two answers to one question — which the provider,
/// not lmgw, would then have to pick between. When the budget wins, the level
/// is reported to the client as ignored instead.
///
/// `pub(crate)` for `POST /v1/messages/count_tokens` (api-docs design §5.2),
/// which passes the client's body through and renders lmgw's own reasoning
/// tiers (header, alias default) into it exactly as `/v1/messages` would.
pub(crate) fn apply_reasoning(body: &mut Map<String, Value>, c: &ReasoningControl) {
    if c.is_empty() {
        return;
    }
    if c.enabled == Some(false) {
        body.insert("thinking".into(), json!({"type": "disabled"}));
        return;
    }
    if let Some(b) = c.budget_tokens {
        body.insert(
            "thinking".into(),
            json!({"type": "enabled", "budget_tokens": b}),
        );
        return;
    }
    if c.effort.is_some() || c.enabled == Some(true) {
        // `adaptive` is the type that means "think, as much as you judge you
        // need" — the honest rendering of both a bare `enabled` and a level.
        body.insert("thinking".into(), json!({"type": "adaptive"}));
    }
    if let Some(e) = &c.effort {
        body.insert("output_config".into(), json!({"effort": e}));
    }
}

fn request_body(
    ir: &ChatRequest,
    model: &str,
    params: &Params,
    stream: bool,
) -> Result<Value, GatewayError> {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    let control = params.reasoning_control();
    let floor = budget_floor(&control)?.unwrap_or(0);
    body.insert(
        "max_tokens".into(),
        json!(params.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS).max(floor)),
    );
    if let Some(system) = ir.system_text() {
        body.insert("system".into(), json!(system));
    }
    body.insert("messages".into(), Value::Array(messages_json(ir)?));
    if let Some(v) = params.temperature {
        body.insert("temperature".into(), json!(v));
    }
    if let Some(v) = params.top_p {
        body.insert("top_p".into(), json!(v));
    }
    if let Some(v) = params.top_k {
        body.insert("top_k".into(), json!(v));
    }
    if !params.stop.is_empty() {
        body.insert("stop_sequences".into(), json!(params.stop));
    }
    // presence/frequency penalty and seed are unsupported here; dropped with
    // a note (§5).
    if params.presence_penalty.is_some()
        || params.frequency_penalty.is_some()
        || params.seed.is_some()
    {
        tracing::debug!("dropping presence/frequency penalty and seed for anthropic egress");
    }
    if !ir.tools.is_empty() {
        let tools: Vec<Value> = ir
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        if let Some(tc) = &ir.tool_choice {
            let v = match tc {
                ToolChoice::Auto => json!({"type": "auto"}),
                ToolChoice::None => json!({"type": "none"}),
                ToolChoice::Required => json!({"type": "any"}),
                ToolChoice::Tool { name } => json!({"type": "tool", "name": name}),
            };
            body.insert("tool_choice".into(), v);
        }
    }
    if stream {
        body.insert("stream".into(), json!(true));
    }
    apply_reasoning(&mut body, &control);
    Ok(Value::Object(body))
}

impl Egress for AnthropicEgress {
    fn proto(&self) -> Protocol {
        Protocol::Anthropic
    }

    fn build_chat(
        &self,
        http: &reqwest::Client,
        up: &Upstream,
        model: &str,
        ir: &ChatRequest,
        params: &Params,
        stream: bool,
    ) -> Result<reqwest::RequestBuilder, GatewayError> {
        // Tolerate base URLs given with or without the trailing /v1.
        let base = up.base().trim_end_matches("/v1");
        let url = format!("{base}/v1/messages");
        let mut rb = http
            .post(url)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&request_body(ir, model, params, stream)?);
        if let Some(key) = up.api_key.as_deref().filter(|k| !k.is_empty()) {
            rb = rb.header("x-api-key", key);
        }
        with_betas(rb, up, &ir.anthropic_beta)
    }

    fn parse_completion(&self, body: &[u8]) -> Result<Completion, GatewayError> {
        let v: Value = serde_json::from_slice(body)
            .map_err(|e| GatewayError::Transport(format!("invalid upstream JSON: {e}")))?;
        let mut content = Vec::new();
        let mut reasoning = String::new();
        if let Some(blocks) = v.get("content").and_then(Value::as_array) {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                content.push(ContentPart::text(t));
                            }
                        }
                    }
                    Some("thinking") => {
                        if let Some(t) = b.get("thinking").and_then(Value::as_str) {
                            reasoning.push_str(t);
                        }
                    }
                    Some("tool_use") => content.push(ContentPart::ToolUse {
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
                    _ => {}
                }
            }
        }
        let finish_reason = v
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(finish_from_anthropic)
            .unwrap_or(FinishReason::Stop);
        Ok(Completion {
            content,
            reasoning,
            finish_reason,
            usage: parse_usage(v.get("usage")),
            model: v
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            timings: None, // llama.cpp-only block; Anthropic never emits it
        })
    }

    fn new_decoder(&self) -> Box<dyn EgressStreamDecoder> {
        Box::new(AnthropicDecoder::default())
    }

    fn map_error(&self, status: u16, body: &[u8]) -> GatewayError {
        let v: Value = serde_json::from_slice(body).unwrap_or_default();
        let err = v.get("error").cloned().unwrap_or_default();
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(500).collect());
        GatewayError::Upstream {
            status,
            provider_type: err.get("type").and_then(Value::as_str).map(String::from),
            message,
        }
    }

    fn build_count_tokens(
        &self,
        http: &reqwest::Client,
        up: &Upstream,
        model: &str,
        text: &str,
    ) -> Result<CountPlan, GatewayError> {
        // Anthropic counts messages, not raw strings; wrap the text as a single
        // user turn (the count therefore includes minimal turn framing, which
        // the counter reports as `message_framing` — api-docs design §5.1).
        Ok(CountPlan::Request(Box::new(count_messages_request(
            http,
            up,
            model,
            &json!({"messages": [{"role": "user", "content": text}]}),
            &[],
        )?)))
    }

    fn parse_count(&self, body: &[u8]) -> Result<u64, GatewayError> {
        let v: Value = serde_json::from_slice(body)
            .map_err(|e| GatewayError::Transport(format!("invalid upstream JSON: {e}")))?;
        v.get("input_tokens")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                GatewayError::Transport("count_tokens response without 'input_tokens'".into())
            })
    }
}

/// `POST {base}/v1/messages/count_tokens` with `body` as given and `model`
/// set to the upstream's model, plus the egress's own `anthropic-version`,
/// `x-api-key` and extra headers, and the client's beta flags (`betas`) —
/// a beta can change what a request costs, so the count carries them too.
///
/// The body is passed through rather than rendered from the IR (api-docs
/// design §5.2, §10 choice 4): a `/v1/messages/count_tokens` client on an
/// Anthropic route gets Anthropic's own count of exactly what it sent — server
/// tools, `redacted_thinking` and fields newer than the IR included, none of
/// which survive a round trip through it. The universal counter's
/// single-user-turn wrapping goes through here too, so the two cannot disagree
/// about the URL or the auth.
pub(crate) fn count_messages_request(
    http: &reqwest::Client,
    up: &Upstream,
    model: &str,
    body: &Value,
    betas: &[String],
) -> Result<reqwest::RequestBuilder, GatewayError> {
    let mut body = body.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".into(), json!(model));
    }
    let base = up.base().trim_end_matches("/v1");
    let mut rb = http
        .post(format!("{base}/v1/messages/count_tokens"))
        .header("anthropic-version", ANTHROPIC_VERSION)
        .json(&body);
    if let Some(key) = up.api_key.as_deref().filter(|k| !k.is_empty()) {
        rb = rb.header("x-api-key", key);
    }
    with_betas(rb, up, betas)
}

fn parse_usage(v: Option<&Value>) -> Usage {
    match v {
        Some(u) => {
            let n = |k: &str| u.get(k).and_then(Value::as_u64);
            let cache_read = n("cache_read_input_tokens");
            let cache_write = n("cache_creation_input_tokens");
            // Anthropic's three input counters are **disjoint**: `input_tokens`
            // excludes both cache counters. The IR's `prompt_tokens` is the
            // total with cache included (see `ir::Usage`), so they are summed
            // here — otherwise the same conversation reports a different input
            // size depending on which wire shape the client speaks, and a
            // cache-heavy turn looks 10x cheaper than it was.
            let input = n("input_tokens");
            let prompt_tokens = match (input, cache_read, cache_write) {
                (None, None, None) => None,
                _ => Some(input.unwrap_or(0) + cache_read.unwrap_or(0) + cache_write.unwrap_or(0)),
            };
            Usage {
                prompt_tokens,
                completion_tokens: n("output_tokens"),
                cached_input_tokens: cache_read,
                cache_write_tokens: cache_write,
                // Thinking tokens are inside `output_tokens` and Anthropic
                // publishes no separate counter for them.
                reasoning_tokens: None,
            }
        }
        None => Usage::default(),
    }
}

#[derive(Default)]
pub struct AnthropicDecoder {
    /// SSE content block index → IR tool ordinal (for input_json_delta).
    tool_by_block: std::collections::HashMap<u64, usize>,
    next_tool_ordinal: usize,
}

impl EgressStreamDecoder for AnthropicDecoder {
    fn on_event(&mut self, ev: &SseEvent) -> Vec<StreamDelta> {
        let v: Value = match serde_json::from_str(ev.data.trim()) {
            Ok(v) => v,
            Err(_) => return vec![],
        };
        let ty = ev
            .event
            .as_deref()
            .or_else(|| v.get("type").and_then(Value::as_str))
            .unwrap_or_default();
        match ty {
            "message_start" => {
                let usage = parse_usage(v.get("message").and_then(|m| m.get("usage")));
                if usage != Usage::default() {
                    vec![StreamDelta::Usage(usage)]
                } else {
                    vec![]
                }
            }
            "content_block_start" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = v.get("content_block").cloned().unwrap_or_default();
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let ordinal = self.next_tool_ordinal;
                    self.next_tool_ordinal += 1;
                    self.tool_by_block.insert(idx, ordinal);
                    vec![StreamDelta::ToolCallStart {
                        index: ordinal,
                        id: block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    }]
                } else {
                    vec![]
                }
            }
            "content_block_delta" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let delta = v.get("delta").cloned().unwrap_or_default();
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let t = delta
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if t.is_empty() {
                            vec![]
                        } else {
                            vec![StreamDelta::TextDelta(t.to_string())]
                        }
                    }
                    Some("thinking_delta") => {
                        let t = delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if t.is_empty() {
                            vec![]
                        } else {
                            vec![StreamDelta::ReasoningDelta(t.to_string())]
                        }
                    }
                    Some("input_json_delta") => {
                        let frag = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if frag.is_empty() {
                            return vec![];
                        }
                        let ordinal = self.tool_by_block.get(&idx).copied().unwrap_or(0);
                        vec![StreamDelta::ToolCallArgsDelta {
                            index: ordinal,
                            fragment: frag.to_string(),
                        }]
                    }
                    _ => vec![],
                }
            }
            "message_delta" => {
                let mut out = Vec::new();
                let usage = parse_usage(v.get("usage"));
                if usage != Usage::default() {
                    out.push(StreamDelta::Usage(usage));
                }
                if let Some(reason) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    out.push(StreamDelta::Stop(finish_from_anthropic(reason)));
                }
                out
            }
            "error" => {
                let msg = v
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("upstream error");
                vec![StreamDelta::Error(msg.to_string())]
            }
            // ping, content_block_stop, message_stop carry no IR information
            _ => vec![],
        }
    }
}
