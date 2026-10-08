//! Gemini egress (§7): `:generateContent` / `:streamGenerateContent?alt=sse`,
//! `contents[]`/`parts` shape, `role: "model"`, key via header.

use serde_json::{json, Map, Value};

use crate::config::{Protocol, Upstream};
use crate::egress::{
    apply_extra_headers, finish_from_gemini, CountPlan, Egress, EgressStreamDecoder,
};
use crate::error::GatewayError;
use crate::ir::{
    flatten_tool_result, ChatRequest, Completion, ContentPart, EmbeddingsRequest,
    EmbeddingsResponse, FinishReason, ImageSource, Params, Role, StreamDelta, ToolChoice,
    ToolResultBlock, Usage,
};
use crate::sse::SseEvent;

pub struct GeminiEgress;

/// IR tool-result blocks → Gemini `functionResponse.response`, which is an
/// arbitrary JSON **object**.
///
/// Gemini is the upstream that most wants structure, so a real
/// [`ToolResultBlock::Json`] goes straight in. The text-parse fallback below
/// exists for the shape the IR could carry before blocks: a tool whose JSON
/// arrives as a string (an OpenAI-ingress client replaying a tool result, say)
/// still reaches Gemini structured, as it did.
///
/// Non-object values are wrapped under `result` rather than dropped — the old
/// code filtered on `is_object` and so turned a perfectly good JSON *array*
/// into a stringified `{"result": "[1,2,3]"}`.
fn function_response(blocks: &[ToolResultBlock]) -> Value {
    let structured = blocks.iter().find_map(|b| match b {
        ToolResultBlock::Json { value } => Some(value.clone()),
        _ => None,
    });
    if let Some(v) = structured {
        return wrap_object(v);
    }
    let (text, _) = flatten_tool_result(blocks);
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => wrap_object(v),
        Err(_) => json!({ "result": text }),
    }
}

fn wrap_object(v: Value) -> Value {
    if v.is_object() {
        v
    } else {
        json!({ "result": v })
    }
}

/// Strip JSON-schema keys Gemini's OpenAPI-subset validator rejects.
fn sanitize_schema(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, val) in map {
                if matches!(
                    k.as_str(),
                    "$schema" | "additionalProperties" | "$id" | "$defs" | "examples"
                ) {
                    continue;
                }
                out.insert(k.clone(), sanitize_schema(val));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(sanitize_schema).collect()),
        other => other.clone(),
    }
}

fn parts_json(
    ir: &ChatRequest,
    m_role: Role,
    parts: &[ContentPart],
) -> Result<Vec<Value>, GatewayError> {
    let mut out = Vec::new();
    for p in parts {
        match p {
            ContentPart::Text { text } => {
                if !text.is_empty() {
                    out.push(json!({"text": text}));
                }
            }
            ContentPart::Image { mime, source } => match source {
                ImageSource::Base64 { data } => {
                    out.push(json!({"inlineData": {"mimeType": mime, "data": data}}));
                }
                ImageSource::Url { .. } => {
                    return Err(GatewayError::Unsupported(
                        "image URLs for gemini upstreams (use base64)".into(),
                    ));
                }
            },
            ContentPart::Audio { mime, data } => {
                out.push(json!({"inlineData": {"mimeType": mime, "data": data}}));
            }
            ContentPart::ToolUse { name, args, .. } => {
                out.push(json!({"functionCall": {"name": name, "args": args}}));
            }
            ContentPart::ToolResult {
                id, name, content, ..
            } => {
                // Gemini correlates function responses by name, not id.
                let fn_name = name
                    .clone()
                    .or_else(|| ir.tool_name_for_id(id).map(String::from))
                    .unwrap_or_else(|| id.clone());
                out.push(json!({
                    "functionResponse": {"name": fn_name, "response": function_response(content)}
                }));
            }
            ContentPart::Reasoning { text, .. } => {
                // Gemini has no slot for replaying a text trace; its own
                // continuity token is a `thoughtSignature`, which this
                // adapter never captures.
                tracing::debug!(chars = text.len(), "dropping reasoning for gemini egress");
            }
        }
    }
    let _ = m_role;
    Ok(out)
}

fn request_body(ir: &ChatRequest, params: &Params) -> Result<Value, GatewayError> {
    let mut body = Map::new();

    if let Some(system) = ir.system_text() {
        body.insert(
            "systemInstruction".into(),
            json!({"parts": [{"text": system}]}),
        );
    }

    let mut contents: Vec<Value> = Vec::new();
    for m in ir.non_system_messages() {
        let role = match m.role {
            Role::Assistant => "model",
            _ => "user", // tool results ride in user turns
        };
        let parts = parts_json(ir, m.role, &m.content)?;
        if parts.is_empty() {
            continue;
        }
        // Merge consecutive same-role turns.
        if let Some(last) = contents.last_mut() {
            if last.get("role").and_then(Value::as_str) == Some(role) {
                if let Some(arr) = last.get_mut("parts").and_then(Value::as_array_mut) {
                    arr.extend(parts);
                    continue;
                }
            }
        }
        contents.push(json!({"role": role, "parts": parts}));
    }
    body.insert("contents".into(), Value::Array(contents));

    let mut gen_cfg = Map::new();
    if let Some(v) = params.temperature {
        gen_cfg.insert("temperature".into(), json!(v));
    }
    if let Some(v) = params.top_p {
        gen_cfg.insert("topP".into(), json!(v));
    }
    if let Some(v) = params.top_k {
        gen_cfg.insert("topK".into(), json!(v));
    }
    if let Some(v) = params.max_tokens {
        gen_cfg.insert("maxOutputTokens".into(), json!(v));
    }
    if let Some(v) = params.seed {
        gen_cfg.insert("seed".into(), json!(v));
    }
    if !params.stop.is_empty() {
        gen_cfg.insert("stopSequences".into(), json!(params.stop));
    }
    if params.presence_penalty.is_some() || params.frequency_penalty.is_some() {
        tracing::debug!("dropping presence/frequency penalty for gemini egress");
    }
    // Reasoning control (§5.3). A bare `enabled: true` has no Gemini spelling
    // — the model decides for itself — so it is reported to the client as
    // ignored (`x-lmgw-reasoning-ignored`) rather than guessed at here.
    //
    // `thinkingLevel` and `thinkingBudget` are two answers to the same
    // question and the API takes one of them: sending both leaves the provider
    // to decide which the caller meant. The budget is the more specific of the
    // two, so it wins and the level is reported ignored.
    let c = params.reasoning_control();
    let mut thinking = Map::new();
    if c.enabled == Some(false) {
        thinking.insert("thinkingBudget".into(), json!(0));
    } else if let Some(b) = c.budget_tokens {
        thinking.insert("thinkingBudget".into(), json!(b));
    } else if let Some(e) = &c.effort {
        thinking.insert("thinkingLevel".into(), json!(e));
    }
    if !thinking.is_empty() {
        gen_cfg.insert("thinkingConfig".into(), Value::Object(thinking));
    }
    if !gen_cfg.is_empty() {
        body.insert("generationConfig".into(), Value::Object(gen_cfg));
    }

    if !ir.tools.is_empty() {
        let decls: Vec<Value> = ir
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": sanitize_schema(&t.parameters),
                })
            })
            .collect();
        body.insert("tools".into(), json!([{"functionDeclarations": decls}]));
        if let Some(tc) = &ir.tool_choice {
            let cfg = match tc {
                ToolChoice::Auto => json!({"mode": "AUTO"}),
                ToolChoice::None => json!({"mode": "NONE"}),
                ToolChoice::Required => json!({"mode": "ANY"}),
                ToolChoice::Tool { name } => {
                    json!({"mode": "ANY", "allowedFunctionNames": [name]})
                }
            };
            body.insert("toolConfig".into(), json!({"functionCallingConfig": cfg}));
        }
    }

    Ok(Value::Object(body))
}

/// Split a Gemini `parts` array into answer content and reasoning text. Parts
/// flagged `thought: true` carry the model's reasoning and would otherwise be
/// merged into the answer, so they're routed to the returned reasoning string.
fn parse_parts(content: Option<&Value>, next_tool: &mut usize) -> (Vec<ContentPart>, String) {
    let mut out = Vec::new();
    let mut reasoning = String::new();
    let parts = content
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array);
    if let Some(parts) = parts {
        for p in parts {
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                if t.is_empty() {
                    continue;
                }
                if p.get("thought").and_then(Value::as_bool).unwrap_or(false) {
                    reasoning.push_str(t);
                } else {
                    out.push(ContentPart::text(t));
                }
            } else if let Some(fc) = p.get("functionCall") {
                let ordinal = *next_tool;
                *next_tool += 1;
                out.push(ContentPart::ToolUse {
                    // Gemini has no call ids; synthesize stable ones.
                    id: format!("call_{ordinal}"),
                    name: fc
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    args: fc.get("args").cloned().unwrap_or(json!({})),
                });
            }
        }
    }
    (out, reasoning)
}

fn parse_usage(v: Option<&Value>) -> Usage {
    match v {
        Some(u) => {
            let n = |k: &str| u.get(k).and_then(Value::as_u64);
            let candidates = n("candidatesTokenCount");
            let thoughts = n("thoughtsTokenCount");
            Usage {
                // `promptTokenCount` already includes the cached content, like
                // OpenAI's and unlike Anthropic's. `toolUsePromptTokenCount`
                // stays out: Google documents it as a count, not as billed
                // input, and `totalTokenCount` (prompt + thoughts + candidates)
                // does not include it either.
                prompt_tokens: n("promptTokenCount"),
                // Thinking is *not* inside `candidatesTokenCount`: Google
                // reports the two side by side (`totalTokenCount` is prompt +
                // thoughts + candidates) and bills output as their sum. The IR's
                // completion includes reasoning, as OpenAI's `completion_tokens`
                // and Anthropic's `output_tokens` do, so the two are added here.
                // Either may be absent; both absent stays unknown.
                completion_tokens: match (candidates, thoughts) {
                    (None, None) => None,
                    (c, t) => Some(c.unwrap_or(0) + t.unwrap_or(0)),
                },
                cached_input_tokens: n("cachedContentTokenCount"),
                cache_write_tokens: None,
                // The thinking share of that completion, for information only,
                // exactly like the IR field.
                reasoning_tokens: thoughts,
            }
        }
        None => Usage::default(),
    }
}

/// `POST {base}/v1beta/models/{model}:countTokens` on the whole chat request
/// (api-docs design §5.2): `generateContentRequest` carries the same
/// `contents`, `systemInstruction`, `tools` and `generationConfig`
/// [`GeminiEgress::build_chat`] would send, so the count is Gemini's own
/// count of that request — not the text-only `contents` the universal
/// counter sends.
pub(crate) fn count_chat_request(
    http: &reqwest::Client,
    up: &Upstream,
    model: &str,
    ir: &ChatRequest,
    params: &Params,
) -> Result<reqwest::RequestBuilder, GatewayError> {
    let base = up.base().trim_end_matches("/v1beta");
    let url = format!("{base}/v1beta/models/{model}:countTokens");
    let mut request = request_body(ir, params)?;
    if let Some(obj) = request.as_object_mut() {
        obj.insert("model".into(), json!(format!("models/{model}")));
    }
    let mut rb = http
        .post(url)
        .json(&json!({"generateContentRequest": request}));
    if let Some(key) = up.api_key.as_deref().filter(|k| !k.is_empty()) {
        rb = rb.header("x-goog-api-key", key);
    }
    Ok(apply_extra_headers(rb, up))
}

impl Egress for GeminiEgress {
    fn proto(&self) -> Protocol {
        Protocol::Gemini
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
        let base = up.base().trim_end_matches("/v1beta");
        let url = if stream {
            format!("{base}/v1beta/models/{model}:streamGenerateContent?alt=sse")
        } else {
            format!("{base}/v1beta/models/{model}:generateContent")
        };
        let mut rb = http.post(url).json(&request_body(ir, params)?);
        if let Some(key) = up.api_key.as_deref().filter(|k| !k.is_empty()) {
            rb = rb.header("x-goog-api-key", key);
        }
        Ok(apply_extra_headers(rb, up))
    }

    fn parse_completion(&self, body: &[u8]) -> Result<Completion, GatewayError> {
        let v: Value = serde_json::from_slice(body)
            .map_err(|e| GatewayError::Transport(format!("invalid upstream JSON: {e}")))?;
        let candidate = v
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .ok_or_else(|| {
                GatewayError::Transport("upstream response without candidates".into())
            })?;
        let mut next_tool = 0usize;
        let (content, reasoning) = parse_parts(candidate.get("content"), &mut next_tool);
        let saw_tool = content
            .iter()
            .any(|p| matches!(p, ContentPart::ToolUse { .. }));
        let finish_reason = candidate
            .get("finishReason")
            .and_then(Value::as_str)
            .map(|s| finish_from_gemini(s, saw_tool))
            .unwrap_or(FinishReason::Stop);
        Ok(Completion {
            content,
            reasoning,
            finish_reason,
            usage: parse_usage(v.get("usageMetadata")),
            timings: None, // llama.cpp-only block; Gemini never emits it
            model: v
                .get("modelVersion")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    fn new_decoder(&self) -> Box<dyn EgressStreamDecoder> {
        Box::new(GeminiDecoder::default())
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
            provider_type: err.get("status").and_then(Value::as_str).map(String::from),
            message,
        }
    }

    fn build_embeddings(
        &self,
        http: &reqwest::Client,
        up: &Upstream,
        model: &str,
        req: &EmbeddingsRequest,
    ) -> Result<reqwest::RequestBuilder, GatewayError> {
        let base = up.base().trim_end_matches("/v1beta");
        let url = format!("{base}/v1beta/models/{model}:batchEmbedContents");
        let requests: Vec<Value> = req
            .inputs
            .iter()
            .map(|text| {
                let mut r = json!({
                    "model": format!("models/{model}"),
                    "content": {"parts": [{"text": text}]},
                });
                if let Some(d) = req.dimensions {
                    r["outputDimensionality"] = json!(d);
                }
                r
            })
            .collect();
        let mut rb = http.post(url).json(&json!({"requests": requests}));
        if let Some(key) = up.api_key.as_deref().filter(|k| !k.is_empty()) {
            rb = rb.header("x-goog-api-key", key);
        }
        Ok(apply_extra_headers(rb, up))
    }

    fn build_count_tokens(
        &self,
        http: &reqwest::Client,
        up: &Upstream,
        model: &str,
        text: &str,
    ) -> Result<CountPlan, GatewayError> {
        let base = up.base().trim_end_matches("/v1beta");
        let url = format!("{base}/v1beta/models/{model}:countTokens");
        let mut rb = http.post(url).json(&json!({
            "contents": [{"role": "user", "parts": [{"text": text}]}],
        }));
        if let Some(key) = up.api_key.as_deref().filter(|k| !k.is_empty()) {
            rb = rb.header("x-goog-api-key", key);
        }
        Ok(CountPlan::Request(Box::new(apply_extra_headers(rb, up))))
    }

    fn parse_count(&self, body: &[u8]) -> Result<u64, GatewayError> {
        let v: Value = serde_json::from_slice(body)
            .map_err(|e| GatewayError::Transport(format!("invalid upstream JSON: {e}")))?;
        v.get("totalTokens").and_then(Value::as_u64).ok_or_else(|| {
            GatewayError::Transport("countTokens response without 'totalTokens'".into())
        })
    }

    fn parse_embeddings(&self, body: &[u8]) -> Result<EmbeddingsResponse, GatewayError> {
        let v: Value = serde_json::from_slice(body)
            .map_err(|e| GatewayError::Transport(format!("invalid upstream JSON: {e}")))?;
        let mut embeddings = Vec::new();
        if let Some(arr) = v.get("embeddings").and_then(Value::as_array) {
            for e in arr {
                embeddings.push(
                    e.get("values")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_f64)
                                .map(|f| f as f32)
                                .collect()
                        })
                        .unwrap_or_default(),
                );
            }
        }
        Ok(EmbeddingsResponse {
            embeddings,
            usage: Usage::default(), // Gemini embeddings report no usage
            model: String::new(),
        })
    }
}

#[derive(Default)]
pub struct GeminiDecoder {
    next_tool: usize,
    saw_tool: bool,
    finish: Option<String>,
    usage: Option<Usage>,
}

impl EgressStreamDecoder for GeminiDecoder {
    fn on_event(&mut self, ev: &SseEvent) -> Vec<StreamDelta> {
        let v: Value = match serde_json::from_str(ev.data.trim()) {
            Ok(v) => v,
            Err(_) => return vec![],
        };
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("upstream error");
            return vec![StreamDelta::Error(msg.to_string())];
        }
        let mut out = Vec::new();
        if let Some(candidate) = v
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        {
            let mut ordinal = self.next_tool;
            let (parts, reasoning) = parse_parts(candidate.get("content"), &mut self.next_tool);
            if !reasoning.is_empty() {
                out.push(StreamDelta::ReasoningDelta(reasoning));
            }
            for p in parts {
                match p {
                    ContentPart::Text { text } => out.push(StreamDelta::TextDelta(text)),
                    ContentPart::ToolUse { id, name, args } => {
                        self.saw_tool = true;
                        // Gemini delivers complete functionCall parts: emit
                        // start + full args in one go.
                        out.push(StreamDelta::ToolCallStart {
                            index: ordinal,
                            id,
                            name,
                        });
                        out.push(StreamDelta::ToolCallArgsDelta {
                            index: ordinal,
                            fragment: serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
                        });
                        ordinal += 1;
                    }
                    _ => {}
                }
            }
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                self.finish = Some(reason.to_string());
            }
        }
        if let Some(u) = v.get("usageMetadata") {
            let usage = parse_usage(Some(u));
            if usage != Usage::default() {
                self.usage = Some(usage);
            }
        }
        // Defer Stop/Usage until the chunk that carries finishReason so
        // saw_tool is final; Gemini repeats usageMetadata per chunk.
        if let Some(reason) = self.finish.take() {
            if let Some(usage) = self.usage.take() {
                out.push(StreamDelta::Usage(usage));
            }
            out.push(StreamDelta::Stop(finish_from_gemini(
                &reason,
                self.saw_tool,
            )));
        }
        out
    }
}
