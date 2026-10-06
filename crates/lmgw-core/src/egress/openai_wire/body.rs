//! The OpenAI chat body and its request (llama-egress design §2): one builder
//! every OpenAI-shaped egress composes, with the two places a dialect differs
//! passed in — its [`ReasoningStep`] and its [`ToolResultRenderer`].

use serde_json::{json, Map, Value};

use super::messages::{messages_json, ToolResultRenderer};
use crate::config::Upstream;
use crate::egress::apply_bearer_auth;
use crate::ir::{ChatRequest, Params, ReasoningControl, ToolChoice};

/// One egress's spelling of the resolved reasoning control (§5.3) in an
/// OpenAI-shaped chat body. [`build_chat_body`] calls it only when the control
/// is not empty (nothing set means no reasoning keys at all), in two halves
/// around the passthrough loop.
pub trait ReasoningStep {
    /// Write the resolved control into `body`. Runs **before** the passthrough
    /// loop so that these keys, which the gateway resolved from every tier, win
    /// over a stale copy riding along in `passthrough` (that loop only fills
    /// keys the body does not already have).
    fn apply(&self, body: &mut Map<String, Value>, ir: &ChatRequest, c: &ReasoningControl);

    /// Rewrite this dialect's reasoning keys that rode in through passthrough
    /// so they cannot contradict the control. Runs **after** the loop, because
    /// they arrive *in* it. OpenRouter's `reasoning` object is the builder's
    /// own business, reconciled right after this on every egress.
    fn reconcile(&self, body: &mut Map<String, Value>, c: &ReasoningControl);
}

/// Did the client send an OpenRouter-shaped `reasoning` object? That object is
/// the vocabulary lmgw reconciles rather than replaces, and its presence
/// changes what the scalar keys are allowed to do (§5.3).
pub(crate) fn has_reasoning_object(ir: &ChatRequest) -> bool {
    ir.passthrough
        .get("reasoning")
        .is_some_and(Value::is_object)
}

/// The OpenAI-shaped chat body, in a fixed order: the modelled fields (the
/// messages with each tool result rendered by `tool_results`), then
/// `reasoning`'s [`apply`](ReasoningStep::apply), then the passthrough loop,
/// then `reasoning`'s [`reconcile`](ReasoningStep::reconcile) and the
/// OpenRouter object's. An egress wraps this in its own `chat_body`, the body
/// it posts and the gate counts.
pub fn build_chat_body(
    ir: &ChatRequest,
    model: &str,
    params: &Params,
    stream: bool,
    reasoning: &dyn ReasoningStep,
    tool_results: &dyn ToolResultRenderer,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert(
        "messages".into(),
        Value::Array(messages_json(ir, tool_results)),
    );
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
    let c = params.reasoning_control();
    if !c.is_empty() {
        reasoning.apply(&mut body, ir, &c);
    }
    // Re-emit verbatim the top-level fields the IR doesn't model
    // (`response_format`, `grammar`, extra llama.cpp sampler params, …). The
    // modeled fields above always win, so this only *adds* keys the gateway
    // would otherwise silently strip on the OpenAI→OpenAI "near pass-through".
    for (k, v) in &ir.passthrough {
        body.entry(k.clone()).or_insert_with(|| v.clone());
    }
    // After the loop, because the keys these rewrite arrive *in* that loop.
    if !c.is_empty() {
        reasoning.reconcile(&mut body, &c);
        reconcile_reasoning_object(&mut body, &c);
    }
    Value::Object(body)
}

/// Bring the client's OpenRouter `reasoning` object, when one rode in through
/// passthrough, in line with the control lmgw resolved (§5.3): its `effort`
/// and `enabled` become the control's, or go where the control leaves them
/// unset. Everything else in it (`exclude`, the object's `max_tokens`) is the
/// client's and stays untouched.
fn reconcile_reasoning_object(body: &mut Map<String, Value>, c: &ReasoningControl) {
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

/// `POST {base}/chat/completions` carrying `body`, with the upstream's bearer
/// and extra headers: the request an OpenAI-shaped egress's `build_chat`
/// sends.
pub fn chat_request(
    http: &reqwest::Client,
    up: &Upstream,
    body: &Value,
) -> reqwest::RequestBuilder {
    let url = format!("{}/chat/completions", up.base());
    apply_bearer_auth(http.post(url).json(body), up)
}
