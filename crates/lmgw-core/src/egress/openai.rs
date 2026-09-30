//! OpenAI-compatible egress (§7) — near pass-through. Also serves llama-server
//! (an `openai`-protocol upstream of kind `llama_server`).

use serde_json::{json, Map, Value};

use crate::config::{Protocol, Upstream, UpstreamKind};
use crate::egress::{
    apply_bearer_auth, finish_from_openai, CountPlan, Egress, EgressStreamDecoder,
};
use crate::error::GatewayError;
use crate::ir::{
    flatten_tool_result, ChatRequest, Completion, ContentPart, EmbeddingsRequest,
    EmbeddingsResponse, FinishReason, ImageSource, Params, RerankRequest, RerankResponse,
    RerankScore, Role, StreamDelta, Timings, ToolChoice, Usage,
};
use crate::sse::SseEvent;

pub struct OpenaiEgress;

/// IR messages → OpenAI `messages` array.
pub fn messages_json(ir: &ChatRequest) -> Vec<Value> {
    let mut out = Vec::new();
    for m in &ir.messages {
        match m.role {
            Role::System => out.push(json!({"role": "system", "content": m.joined_text()})),
            Role::Tool => {
                for p in &m.content {
                    if let ContentPart::ToolResult { id, content, .. } = p {
                        // OpenAI's `role: "tool"` message carries text only —
                        // this is the one adapter that has to flatten. A lone
                        // text block (the common case) comes out byte-identical;
                        // anything binary is replaced by a named placeholder and
                        // logged, never silently dropped and never base64'd into
                        // the prompt (§14).
                        let (text, dropped) = flatten_tool_result(content);
                        if !dropped.is_empty() {
                            tracing::warn!(
                                tool_call_id = %id,
                                "tool result content not representable on an openai upstream, \
                                 replaced with placeholders: {}",
                                dropped.join("; ")
                            );
                        }
                        out.push(json!({"role": "tool", "tool_call_id": id, "content": text}));
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
                            "id": id,
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

/// Did the client send an OpenRouter-shaped `reasoning` object? That object is
/// the vocabulary lmgw reconciles rather than replaces, and its presence
/// changes what the scalar keys are allowed to do (§5.3).
pub(crate) fn has_reasoning_object(ir: &ChatRequest) -> bool {
    ir.passthrough
        .get("reasoning")
        .is_some_and(Value::is_object)
}

/// Render the normalised reasoning triple (§5.3) into the OpenAI-shaped body.
///
/// Two different routes share this wire shape and do *not* share its
/// vocabulary: a llama-server takes `reasoning_effort`,
/// `reasoning_budget_tokens` and the template's own
/// `chat_template_kwargs.enable_thinking`, while a generic OpenAI-protocol
/// provider knows `reasoning_effort` alone — the other two are reported back
/// to the client as ignored (`x-lmgw-reasoning-ignored`) by the handler rather
/// than sent somewhere they would be meaningless.
///
/// Called **before** the passthrough loop so that these keys, which the
/// gateway resolved from every tier, win over a stale copy riding along in
/// `passthrough` (that loop only fills keys the body does not already have).
fn apply_reasoning(
    body: &mut Map<String, Value>,
    ir: &ChatRequest,
    params: &Params,
    kind: UpstreamKind,
) {
    let c = params.reasoning_control();
    if c.is_empty() {
        return;
    }
    let llama = kind == UpstreamKind::LlamaServer;
    // A client that spoke OpenRouter's dialect gets OpenRouter's dialect back:
    // its `reasoning` object is reconciled to the resolved control below, and
    // adding a second, scalar spelling of the same thing would be lmgw
    // inventing a field the client did not send — which providers that read
    // both have every right to resolve differently than lmgw would. The
    // llama-server route is the exception: it does not understand that object
    // at all, so the scalar is the only thing that can carry the control.
    let scalars_ok = llama || !has_reasoning_object(ir);

    // `enable_thinking` is a *template* kwarg, so the client's own kwargs
    // object has to survive: this is an object-level deep merge, not the
    // whole-key passthrough rule that would discard it (§5.3).
    let mut kwargs: Map<String, Value> = ir
        .passthrough
        .get("chat_template_kwargs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut thinking: Option<bool> = None;

    if c.enabled == Some(false) {
        // The two routes switch thinking off by genuinely different means.
        //
        // llama-server: `reasoning_effort: "none"` is a *newer master* special
        // case — on the build lmgw runs it is not one, and Qwen3.8's template
        // raises on a level it does not know. Verified against that build:
        // `chat_template_kwargs.enable_thinking` is what works, in both
        // directions, including on a row started with `--reasoning off`. So the
        // kwarg is the control here and no level is sent at all — a stray
        // `reasoning_effort` from passthrough is stripped below, since the
        // template must not see a level while thinking is off.
        //
        // A generic OpenAI-protocol provider has no template kwargs; `"none"`
        // is that vocabulary's own way of saying it.
        if llama {
            thinking = Some(false);
        } else if scalars_ok {
            body.insert("reasoning_effort".into(), json!("none"));
        }
    } else {
        if let Some(e) = &c.effort {
            if scalars_ok {
                body.insert("reasoning_effort".into(), json!(e));
            }
            // A row started with `--reasoning off` needs the template switched
            // on as well, or the level it was given has nothing to act on.
            thinking = Some(true);
        } else if c.enabled == Some(true) {
            thinking = Some(true);
        }
        if let Some(b) = c.budget_tokens {
            if llama {
                body.insert("reasoning_budget_tokens".into(), json!(b));
            }
        }
    }

    if llama {
        if let Some(t) = thinking {
            kwargs.insert("enable_thinking".into(), json!(t));
        }
        if !kwargs.is_empty() {
            body.insert("chat_template_kwargs".into(), Value::Object(kwargs));
        }
    }
}

/// Rewrite the client's own reasoning keys that rode through passthrough so
/// they cannot contradict the control lmgw resolved (§5.3) — the OpenRouter
/// `reasoning` object's `effort`/`enabled`, and llama-server's alternative
/// budget spelling. Everything else in them (`exclude`, the object's
/// `max_tokens`, other template kwargs) is the client's and stays untouched.
fn reconcile_reasoning_passthrough(
    body: &mut Map<String, Value>,
    params: &Params,
    kind: UpstreamKind,
) {
    let c = params.reasoning_control();
    if c.is_empty() {
        return;
    }
    // Thinking is off on a llama-server route by template kwarg alone; a level
    // that rode in through passthrough would be handed to a template that must
    // not see one (and, on Qwen3.8, would raise).
    if kind == UpstreamKind::LlamaServer && c.enabled == Some(false) {
        body.remove("reasoning_effort");
    }
    // `thinking_budget_tokens` is the same control under llama-server's older
    // name. lmgw emits the resolved budget as `reasoning_budget_tokens`, so a
    // client's copy under the other name has to agree with it or the server
    // would be told two different numbers.
    if let Some(slot) = body.get_mut("thinking_budget_tokens") {
        match c.budget_tokens {
            Some(b) => *slot = json!(b),
            None => {
                body.remove("thinking_budget_tokens");
            }
        }
    }
    let Some(Value::Object(o)) = body.get_mut("reasoning") else {
        return;
    };
    match &c.effort {
        Some(e) => {
            o.insert("effort".into(), json!(e));
        }
        None => {
            o.remove("effort");
        }
    }
    match c.enabled {
        Some(b) => {
            o.insert("enabled".into(), json!(b));
        }
        None => {
            o.remove("enabled");
        }
    }
}

/// Build the OpenAI-shaped chat body [`OpenaiEgress::build_chat`] posts.
///
/// `pub` (renamed from the former private `request_body`) so the gate's
/// per-send half ([`crate::gate::fit_chat`]) can build the exact body it is
/// about to send and run it
/// through [`crate::gate::count::count_chat_prompt`]'s `/apply-template` step
/// — ladder design §3.3 step 1 is explicit that the fit check counts "the
/// exact chat body egress is about to send", not a re-derived approximation
/// of it. `build_chat` calls this directly, so the posted body stays
/// byte-identical to before this existed.
pub fn chat_body(
    ir: &ChatRequest,
    model: &str,
    params: &Params,
    stream: bool,
    kind: UpstreamKind,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("messages".into(), Value::Array(messages_json(ir)));
    if let Some(v) = params.temperature {
        body.insert("temperature".into(), json!(v));
    }
    if let Some(v) = params.top_p {
        body.insert("top_p".into(), json!(v));
    }
    if let Some(v) = params.top_k {
        // Non-standard; llama.cpp & most OAI-compatible local servers accept it.
        body.insert("top_k".into(), json!(v));
    }
    // llama.cpp extensions, modelled; what passthrough used to re-emit.
    if let Some(v) = params.min_p {
        body.insert("min_p".into(), json!(v));
    }
    if let Some(v) = params.repeat_penalty {
        body.insert("repeat_penalty".into(), json!(v));
    }
    if let Some(v) = params.max_tokens {
        body.insert("max_tokens".into(), json!(v));
    }
    if let Some(v) = params.presence_penalty {
        body.insert("presence_penalty".into(), json!(v));
    }
    if let Some(v) = params.frequency_penalty {
        body.insert("frequency_penalty".into(), json!(v));
    }
    if let Some(v) = params.seed {
        body.insert("seed".into(), json!(v));
    }
    if !params.stop.is_empty() {
        body.insert("stop".into(), json!(params.stop));
    }
    if !ir.tools.is_empty() {
        let tools: Vec<Value> = ir
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    },
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        if let Some(tc) = &ir.tool_choice {
            let v = match tc {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::None => json!("none"),
                ToolChoice::Required => json!("required"),
                ToolChoice::Tool { name } => {
                    json!({"type": "function", "function": {"name": name}})
                }
            };
            body.insert("tool_choice".into(), v);
        }
    }
    if stream {
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    apply_reasoning(&mut body, ir, params, kind);
    // Re-emit verbatim the top-level fields the IR doesn't model
    // (`response_format`, `grammar`, extra llama.cpp sampler params, …). The
    // modeled fields above always win, so this only *adds* keys the gateway
    // would otherwise silently strip on the OpenAI→OpenAI "near pass-through".
    for (k, v) in &ir.passthrough {
        body.entry(k.clone()).or_insert_with(|| v.clone());
    }
    // After the loop, because the object it rewrites arrives *in* that loop.
    reconcile_reasoning_passthrough(&mut body, params, kind);
    Value::Object(body)
}

/// llama-server's `exceed_context_size_error` body (ladder design §2.1 fact 3):
/// `{"error":{"code":400,"message":"…","type":"exceed_context_size_error",
/// "n_prompt_tokens":8010,"n_ctx":4096}}`. Measured verbatim on this machine's
/// image (`server-task.cpp:1503`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExceedContext {
    pub n_prompt_tokens: u64,
    pub n_ctx: u64,
}

/// Recognise llama-server's own over-long-prompt refusal in a raw error body,
/// so [`OpenaiEgress::map_error`] can turn it into a
/// [`GatewayError::ContextExceeded`] instead of a generic `Upstream` 400.
/// `None` for every other shape — including the shared-pool overflow (fact 3
/// again: `type: "server_error"`, no `n_prompt_tokens`/`n_ctx`), which must
/// keep falling through to the generic path since it is not this refusal.
pub fn parse_exceed_context(body: &[u8]) -> Option<ExceedContext> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let error = v.get("error")?;
    if error.get("type").and_then(Value::as_str) != Some("exceed_context_size_error") {
        return None;
    }
    Some(ExceedContext {
        n_prompt_tokens: error.get("n_prompt_tokens").and_then(Value::as_u64)?,
        n_ctx: error.get("n_ctx").and_then(Value::as_u64)?,
    })
}

impl Egress for OpenaiEgress {
    fn proto(&self) -> Protocol {
        Protocol::Openai
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
        let url = format!("{}/chat/completions", up.base());
        let rb = http
            .post(url)
            .json(&chat_body(ir, model, params, stream, up.kind));
        Ok(apply_bearer_auth(rb, up))
    }

    fn parse_completion(&self, body: &[u8]) -> Result<Completion, GatewayError> {
        let v: Value = serde_json::from_slice(body)
            .map_err(|e| GatewayError::Transport(format!("invalid upstream JSON: {e}")))?;
        let choice = v
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .ok_or_else(|| GatewayError::Transport("upstream response without choices".into()))?;
        let msg = choice.get("message").cloned().unwrap_or_default();

        let mut content = Vec::new();
        if let Some(text) = msg.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                content.push(ContentPart::text(text));
            }
        }
        if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
            for c in calls {
                let f = c.get("function").cloned().unwrap_or_default();
                let args_raw = f.get("arguments").and_then(Value::as_str).unwrap_or("{}");
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
                    args: serde_json::from_str(args_raw)
                        .unwrap_or_else(|_| Value::String(args_raw.into())),
                });
            }
        }

        let finish_reason = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(finish_from_openai)
            .unwrap_or(FinishReason::Stop);

        Ok(Completion {
            content,
            reasoning: msg
                .get("reasoning_content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            finish_reason,
            usage: parse_usage(v.get("usage")),
            model: v
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            // llama-server puts its timings block on the non-streamed response
            // too; before usage analytics nothing read it here.
            timings: parse_timings(v.get("timings")),
        })
    }

    fn new_decoder(&self) -> Box<dyn EgressStreamDecoder> {
        Box::new(OpenaiDecoder::default())
    }

    fn map_error(&self, status: u16, body: &[u8]) -> GatewayError {
        // llama-server's own context-size refusal (ladder design §2.1 fact 3,
        // §2.2 fact 14: "has no dedicated handling" until now) — the
        // backstop for when the gate's own pre-check undercounted (ladder
        // design §3.1: "climb to the smallest rung that fits … and retry
        // once"). Checked ahead of the generic `Upstream` fallback below so a
        // client always sees the stable `context_length_exceeded` code
        // instead of whatever wording that build of llama-server used.
        if status == 400 {
            if let Some(ec) = parse_exceed_context(body) {
                // `map_error` carries no model/alias — that identity lives on
                // the `Route` its caller already has, several layers above
                // this trait method. `model` is left empty here, and every
                // chat send site fills it from that route with
                // `crate::gate::attribute` before the error reaches a client
                // or a log line.
                return GatewayError::ContextExceeded {
                    model: String::new(),
                    prompt_tokens: ec.n_prompt_tokens,
                    max_output: None,
                    limit: ec.n_ctx,
                    top_rung: None,
                };
            }
        }
        let v: Value = serde_json::from_slice(body).unwrap_or_default();
        let message = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(500).collect());
        let provider_type = v
            .get("error")
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str)
            .map(String::from);
        GatewayError::Upstream {
            status,
            provider_type,
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
        let url = format!("{}/embeddings", up.base());
        let mut body = json!({"model": model, "input": req.inputs});
        if let Some(d) = req.dimensions {
            body["dimensions"] = json!(d);
        }
        let rb = http.post(url).json(&body);
        Ok(apply_bearer_auth(rb, up))
    }

    fn build_count_tokens(
        &self,
        http: &reqwest::Client,
        up: &Upstream,
        model: &str,
        text: &str,
    ) -> Result<CountPlan, GatewayError> {
        match up.kind {
            // llama-server exposes a native `/tokenize` at the server root
            // (not under /v1); count = number of returned token ids. A
            // single-model server ignores `model`, but forwarding it costs
            // nothing and keeps this identical to the chat path.
            UpstreamKind::LlamaServer => Ok(CountPlan::Request(tokenize_request(
                http,
                up,
                &json!({"model": model, "content": text}),
            ))),
            // Real OpenAI (and OAI-compatible servers without /tokenize) have
            // no token endpoint — their tokenizer is public, so count locally
            // with the model's tiktoken encoding. audio.cpp has no tokenize
            // route either (and its models aren't chat tokenizers anyway),
            // and sd-server takes a prompt but counts nothing.
            UpstreamKind::Generic | UpstreamKind::AudioCpp | UpstreamKind::SdCpp => {
                Ok(match tiktoken_count(model, text) {
                    (n, false) => CountPlan::Ready(n),
                    (n, true) => CountPlan::Guessed(n),
                })
            }
        }
    }

    fn parse_count(&self, body: &[u8]) -> Result<u64, GatewayError> {
        let v: Value = serde_json::from_slice(body)
            .map_err(|e| GatewayError::Transport(format!("invalid upstream JSON: {e}")))?;
        v.get("tokens")
            .and_then(Value::as_array)
            .map(|a| a.len() as u64)
            .ok_or_else(|| GatewayError::Transport("tokenize response without 'tokens'".into()))
    }

    fn parse_embeddings(&self, body: &[u8]) -> Result<EmbeddingsResponse, GatewayError> {
        let v: Value = serde_json::from_slice(body)
            .map_err(|e| GatewayError::Transport(format!("invalid upstream JSON: {e}")))?;
        let mut embeddings = Vec::new();
        for item in v.get("data").and_then(Value::as_array).unwrap_or(&vec![]) {
            let vec: Vec<f32> = item
                .get("embedding")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_f64)
                        .map(|f| f as f32)
                        .collect()
                })
                .unwrap_or_default();
            embeddings.push(vec);
        }
        Ok(EmbeddingsResponse {
            embeddings,
            usage: parse_usage(v.get("usage")),
            model: v
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    /// The Jina request shape, which is the one llama-server documents for
    /// `/v1/rerank`. `documents` rather than TEI's `texts`: the child
    /// auto-detects both, and picking one keeps the response shape predictable
    /// (TEI's is a bare array).
    fn build_rerank(
        &self,
        http: &reqwest::Client,
        up: &Upstream,
        model: &str,
        req: &RerankRequest,
    ) -> Result<reqwest::RequestBuilder, GatewayError> {
        let mut body = json!({
            "model": model,
            "query": req.query,
            "documents": req.documents,
        });
        if let Some(n) = req.top_n {
            body["top_n"] = json!(n);
        }
        let rb = http.post(format!("{}/rerank", up.base())).json(&body);
        Ok(apply_bearer_auth(rb, up))
    }

    /// Accepts both response shapes a rerank backend may answer with: Jina's
    /// `{"results": [{"index", "relevance_score"}]}` and TEI's bare
    /// `[{"index", "score"}]`. Only the *request* shape is ours to choose; the
    /// response is whatever the configured upstream returns.
    fn parse_rerank(&self, body: &[u8]) -> Result<RerankResponse, GatewayError> {
        let v: Value = serde_json::from_slice(body)
            .map_err(|e| GatewayError::Transport(format!("invalid upstream JSON: {e}")))?;
        let rows = match v.get("results").and_then(Value::as_array) {
            Some(rows) => rows.clone(),
            None => v.as_array().cloned().ok_or_else(|| {
                GatewayError::Transport(
                    "rerank response has neither a 'results' array nor a top-level array".into(),
                )
            })?,
        };
        let results = rows
            .iter()
            .map(|r| {
                let score = r
                    .get("relevance_score")
                    .or_else(|| r.get("score"))
                    .and_then(Value::as_f64)
                    .unwrap_or_default() as f32;
                RerankScore {
                    index: r.get("index").and_then(Value::as_u64).unwrap_or_default() as usize,
                    score,
                }
            })
            .collect();
        Ok(RerankResponse {
            results,
            usage: parse_usage(v.get("usage")),
            model: v
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }
}

fn parse_usage(v: Option<&Value>) -> Usage {
    match v {
        Some(u) => {
            // `prompt_tokens` already includes the cached subset here — OpenAI
            // reports `prompt_tokens_details.cached_tokens` as a *detail of* the
            // total, which is the IR's meaning too, so nothing is adjusted.
            let detail = |k: &str, f: &str| u.get(k).and_then(|d| d.get(f)).and_then(Value::as_u64);
            Usage {
                prompt_tokens: u.get("prompt_tokens").and_then(Value::as_u64),
                completion_tokens: u.get("completion_tokens").and_then(Value::as_u64),
                cached_input_tokens: detail("prompt_tokens_details", "cached_tokens"),
                // OpenAI's prompt caching is automatic and writes are not
                // billed separately, so there is no write counter to read.
                cache_write_tokens: None,
                reasoning_tokens: detail("completion_tokens_details", "reasoning_tokens"),
            }
        }
        None => Usage::default(),
    }
}

/// Parse llama.cpp's `timings` block. Returns `None` unless `prompt_n` is
/// present (the marker of a genuine llama-server timings object), so cloud
/// upstreams — which never emit this field — yield nothing. Also what the
/// benchmark engine reads `/completion`'s final chunk with, on both engines
/// (ik sends no `cache_n`).
pub(crate) fn parse_timings(v: Option<&Value>) -> Option<Timings> {
    let t = v?;
    let f = |k: &str| t.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    Some(Timings {
        prompt_n: t.get("prompt_n").and_then(Value::as_u64)?,
        prompt_ms: f("prompt_ms"),
        prompt_per_second: f("prompt_per_second"),
        predicted_n: t.get("predicted_n").and_then(Value::as_u64).unwrap_or(0),
        predicted_ms: f("predicted_ms"),
        predicted_per_second: f("predicted_per_second"),
        cache_n: t.get("cache_n").and_then(Value::as_u64),
        draft_n: t.get("draft_n").and_then(Value::as_u64),
        draft_n_accepted: t.get("draft_n_accepted").and_then(Value::as_u64),
    })
}

/// Count `text` with the model's tiktoken encoding (OpenAI's tokenizer is
/// public). Unknown / future model names fall back to `o200k_base` — the
/// encoding of the current GPT-4o/o-series family — and say so: the `bool` is
/// `true` when the encoding was that guess rather than the model's own, which
/// [`CountPlan::Guessed`] carries to the response (api-docs design §5.1).
fn tiktoken_count(model: &str, text: &str) -> (u64, bool) {
    let (n, guessed) = match tiktoken_rs::bpe_for_model(model) {
        Ok(bpe) => (bpe.encode_ordinary(text).len(), false),
        Err(_) => {
            tracing::debug!("tiktoken: unknown model '{model}', counting with o200k_base");
            let n = match tiktoken_rs::o200k_base() {
                Ok(bpe) => bpe.encode_ordinary(text).len(),
                Err(e) => {
                    tracing::error!("tiktoken: o200k_base unavailable: {e}");
                    0
                }
            };
            (n, true)
        }
    };
    (n as u64, guessed)
}

/// `POST /tokenize` at a llama-server's root (not under `/v1`), with the
/// upstream's bearer and extra headers, carrying `body` as given.
///
/// One builder for both callers, so the root derivation and the auth cannot
/// drift apart: the universal counter's `{model, content}` count
/// ([`Egress::build_count_tokens`]) and the llama.cpp-compatible `/tokenize`
/// route, which forwards the client's own object with only `model` rewritten
/// (api-docs design §5.3).
pub(crate) fn tokenize_request(
    http: &reqwest::Client,
    up: &Upstream,
    body: &Value,
) -> reqwest::RequestBuilder {
    let base = up.base().trim_end_matches("/v1");
    apply_bearer_auth(http.post(format!("{base}/tokenize")).json(body), up)
}

#[derive(Default)]
pub struct OpenaiDecoder {
    /// Upstream tool-call indexes seen so far (to distinguish start vs args).
    started_tools: std::collections::HashSet<u64>,
}

impl EgressStreamDecoder for OpenaiDecoder {
    fn on_event(&mut self, ev: &SseEvent) -> Vec<StreamDelta> {
        let data = ev.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return vec![];
        }
        let v: Value = match serde_json::from_str(data) {
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
        if let Some(choice) = v
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        {
            let delta = choice.get("delta").cloned().unwrap_or_default();
            // Reasoning models stream their thoughts here (with `content` null
            // until the answer begins); surface it as a distinct delta.
            if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str) {
                if !r.is_empty() {
                    out.push(StreamDelta::ReasoningDelta(r.to_string()));
                }
            }
            if let Some(t) = delta.get("content").and_then(Value::as_str) {
                if !t.is_empty() {
                    out.push(StreamDelta::TextDelta(t.to_string()));
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for c in calls {
                    let idx = c.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let f = c.get("function").cloned().unwrap_or_default();
                    let name = f.get("name").and_then(Value::as_str).unwrap_or_default();
                    let args = f
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !self.started_tools.contains(&idx) && !name.is_empty() {
                        self.started_tools.insert(idx);
                        out.push(StreamDelta::ToolCallStart {
                            index: idx as usize,
                            id: c
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or(&format!("call_{idx}"))
                                .to_string(),
                            name: name.to_string(),
                        });
                    }
                    if !args.is_empty() {
                        out.push(StreamDelta::ToolCallArgsDelta {
                            index: idx as usize,
                            fragment: args.to_string(),
                        });
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                out.push(StreamDelta::Stop(finish_from_openai(reason)));
            }
        }
        if v.get("usage").is_some() && !v["usage"].is_null() {
            out.push(StreamDelta::Usage(parse_usage(v.get("usage"))));
        }
        // llama-server attaches a top-level `timings` block (per-chunk with
        // `timings_per_token`, else only on the final chunk). Surface it for
        // the Chat tab's stats panel.
        if let Some(t) = parse_timings(v.get("timings")) {
            out.push(StreamDelta::Timings(t));
        }
        out
    }
}
