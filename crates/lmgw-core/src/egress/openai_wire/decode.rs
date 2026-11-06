//! Reading OpenAI-shaped responses: a whole completion, the SSE decoder, the
//! reasoning text, usage and llama.cpp's `timings` (llama-egress design §2).

use serde_json::Value;

use crate::egress::call_ids::CallIds;
use crate::egress::{finish_from_openai, EgressStreamDecoder};
use crate::error::GatewayError;
use crate::ir::{Completion, ContentPart, FinishReason, StreamDelta, Timings, Usage};
use crate::sse::SseEvent;

/// The reasoning text of an OpenAI-shaped message or delta: llama-server's
/// and DeepSeek's `reasoning_content`, else the plain `reasoning` string
/// OpenRouter-style gateways (Kilo) and newer vLLM send — which lmgw dropped
/// before, so a cloud model's reasoning never reached the thread. The first
/// wins when both come, so nothing is doubled.
fn reasoning_text(msg: &Value) -> Option<&str> {
    msg.get("reasoning_content")
        .and_then(Value::as_str)
        .or_else(|| msg.get("reasoning").and_then(Value::as_str))
}

/// An OpenAI `usage` object: a chat completion's, whole or streamed, the
/// auxiliary routes' and legacy `/v1/completions`'.
pub(crate) fn parse_usage(v: Option<&Value>) -> Usage {
    match v {
        Some(u) => {
            // `prompt_tokens` already includes both cache subsets here — OpenAI
            // reports `prompt_tokens_details.cached_tokens` and
            // `.cache_write_tokens` as *details of* the total, which is the
            // IR's meaning too, so nothing is adjusted.
            let detail = |k: &str, f: &str| u.get(k).and_then(|d| d.get(f)).and_then(Value::as_u64);
            Usage {
                prompt_tokens: u.get("prompt_tokens").and_then(Value::as_u64),
                completion_tokens: u.get("completion_tokens").and_then(Value::as_u64),
                cached_input_tokens: detail("prompt_tokens_details", "cached_tokens"),
                // Since GPT-5.6 OpenAI bills prompt-cache writes at their own
                // rate (1.25x input), in place of the input rate rather than on
                // top of it — the IR's cache-write subset exactly. On older
                // models a write costs the plain input rate, which is what a
                // price row without `price_cache_write` falls back to.
                cache_write_tokens: detail("prompt_tokens_details", "cache_write_tokens"),
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
/// (ik sends no `cache_n`). A rate llama.cpp writes as `null` is `None`; the
/// `is_finite` filter is defensive, since parsed JSON holds no non-finite
/// number.
pub(crate) fn parse_timings(v: Option<&Value>) -> Option<Timings> {
    let t = v?;
    // llama.cpp writes a non-finite rate as `null`: an unknown one stays
    // unknown, never a measured zero.
    let f = |k: &str| t.get(k).and_then(Value::as_f64).filter(|v| v.is_finite());
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

/// Whether a reader takes the answer's `service_tier` (openai egress design
/// §5.2): the platform egress does, the compatible and llama.cpp ones never
/// (§5.4: the dimension does not exist there, whatever a provider sends).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadTier {
    #[default]
    No,
    Yes,
}

/// The answer's top-level `service_tier`, when `read` says to take it.
fn service_tier(v: &Value, read: ReadTier) -> Option<String> {
    (read == ReadTier::Yes)
        .then(|| v.get("service_tier").and_then(Value::as_str))
        .flatten()
        .map(str::to_string)
}

pub fn parse_completion(body: &[u8]) -> Result<Completion, GatewayError> {
    parse_completion_with(body, ReadTier::No)
}

/// [`parse_completion`], taking the `service_tier` when `read` says so.
pub fn parse_completion_with(body: &[u8], read: ReadTier) -> Result<Completion, GatewayError> {
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
    // OpenAI's own field for the model's refusal (`message.refusal`).
    if let Some(text) = msg.get("refusal").and_then(Value::as_str) {
        if !text.is_empty() {
            content.push(ContentPart::Refusal { text: text.into() });
        }
    }
    if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
        let mut ids = CallIds::default();
        for c in calls {
            let f = c.get("function").cloned().unwrap_or_default();
            let args_raw = f.get("arguments").and_then(Value::as_str).unwrap_or("{}");
            content.push(ContentPart::ToolUse {
                id: upstream_id(c).unwrap_or_else(|| ids.mint()),
                name: f
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                args: crate::ir::parse_call_args(args_raw),
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
        reasoning: reasoning_text(&msg).unwrap_or_default().to_string(),
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
        service_tier: service_tier(&v, read),
        upstream_items: None,
        context_management: None,
        usage_iterations: None,
    })
}

#[derive(Default)]
pub struct OpenaiDecoder {
    /// Upstream tool-call indexes seen so far (to distinguish start vs args).
    started_tools: std::collections::HashSet<u64>,
    /// The ids of calls the upstream sent without one: unique across the
    /// tool loop's model calls, where a per-answer `call_{index}` repeated.
    ids: CallIds,
    /// Whether chunks' `service_tier` is read ([`ReadTier`]).
    read_tier: ReadTier,
    /// The last `service_tier` emitted, so it goes out once and again only
    /// when it changes.
    tier: Option<String>,
}

impl OpenaiDecoder {
    /// A decoder that reads the chunks' `service_tier`: the platform's.
    pub fn reading_tier() -> Self {
        Self {
            read_tier: ReadTier::Yes,
            ..Self::default()
        }
    }
}

/// A call's own id, when the upstream sent a non-empty one.
fn upstream_id(c: &Value) -> Option<String> {
    c.get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
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
        // Every chunk of the platform's carries it; it leads the chunk's
        // deltas, so an encoder that relays it has it before the text.
        if let Some(tier) = service_tier(&v, self.read_tier) {
            if self.tier.as_deref() != Some(tier.as_str()) {
                self.tier = Some(tier.clone());
                out.push(StreamDelta::ServiceTier(tier));
            }
        }
        if let Some(choice) = v
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        {
            let delta = choice.get("delta").cloned().unwrap_or_default();
            // Reasoning models stream their thoughts here (with `content` null
            // until the answer begins); surface it as a distinct delta.
            if let Some(r) = reasoning_text(&delta) {
                if !r.is_empty() {
                    out.push(StreamDelta::ReasoningDelta(r.to_string()));
                }
            }
            if let Some(t) = delta.get("content").and_then(Value::as_str) {
                if !t.is_empty() {
                    out.push(StreamDelta::TextDelta(t.to_string()));
                }
            }
            if let Some(t) = delta.get("refusal").and_then(Value::as_str) {
                if !t.is_empty() {
                    out.push(StreamDelta::RefusalDelta(t.to_string()));
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
                            id: upstream_id(c).unwrap_or_else(|| self.ids.mint()),
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
            // An empty one is none: a provider that sends `""` on every
            // chunk has not finished (the silence clock reads it so too).
            let reason = choice.get("finish_reason").and_then(Value::as_str);
            if let Some(reason) = reason.filter(|r| !r.is_empty()) {
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

#[cfg(test)]
mod tests;
