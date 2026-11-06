//! Reading the platform's answers on the Responses wire (openai egress design
//! §7.2.3): a whole Response object, and the stream of its events, as the
//! IR's [`Completion`] and [`StreamDelta`]s.
//!
//! The stream, as the create reference's streaming events have it
//! (<https://developers.openai.com/api/reference/resources/responses/methods/create.md>,
//! checked 2026-10-24; hand-written in the tests until the live probe, WP6,
//! captures a stream):
//! - `response.output_text.delta` is text; `response.refusal.delta` is the
//!   model's refusal (`StreamDelta::RefusalDelta`, a whole Response's
//!   `refusal` part `ContentPart::Refusal`), which each client wire writes
//!   as its own refusal where it has one;
//! - `response.reasoning_summary_text.delta` is reasoning, with a blank line
//!   between two summary parts;
//! - `response.output_item.added` of a `function_call` starts a tool call
//!   (its `call_id` and `name`), `response.function_call_arguments.delta`
//!   carries its arguments; a call whose `added` never came, or whose
//!   arguments came only whole, is read from its `response.output_item.done`;
//! - the terminal event (`response.completed`, `.incomplete`, `.failed`)
//!   carries the Response object, read by `openai_wire::read_response`: its
//!   service tier, its usage, then the stop. The tier comes from there only:
//!   `response.created` and `response.in_progress` echo the requested one
//!   (a probe);
//! - `response.failed` and an `error` event end the stream with an error.
//!
//! The stop: `content_filter` for `incomplete_details.reason:
//! "content_filter"`, `length` for every other incomplete one
//! (`max_output_tokens`, `max_messages`, `steered`, a reason added later, or
//! none; the raw reason logged), so an incomplete response's calls are not
//! run; otherwise tool calls when a `function_call` item was output,
//! otherwise stop.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::egress::call_ids::CallIds;
use crate::egress::openai_wire::responses_items::{event, item, part};
use crate::egress::openai_wire::{read_response, OpenaiDecoder};
use crate::egress::EgressStreamDecoder;
use crate::error::{FailedAnswer, GatewayError};
use crate::ir::{
    encrypted_sha256, Completion, ContentPart, FinishReason, Seal, SealScope, StreamDelta,
    UpstreamItem, UpstreamItems,
};
use crate::sse::SseEvent;

/// Whether `v` is a Responses stream event rather than a chat chunk: every
/// Responses event has a `type`, and no chat chunk has one.
pub fn is_responses_event(v: &Value) -> bool {
    v.get("type").and_then(Value::as_str).is_some()
}

/// Whether a whole answer is a Response object rather than a chat
/// completion.
pub fn is_response_object(v: &Value) -> bool {
    v.get("choices").is_none()
        && (v.get("object").and_then(Value::as_str) == Some("response")
            || v.get("output").is_some_and(Value::is_array))
}

/// The stop of a finished Response object (module doc). `called`: a
/// `function_call` item was output.
fn finish(response: &Value, called: bool) -> FinishReason {
    if response.get("status").and_then(Value::as_str) == Some("incomplete") {
        return match response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
        {
            Some("max_output_tokens") => FinishReason::Length,
            Some("content_filter") => FinishReason::ContentFilter,
            // `max_messages`, `steered`, a reason added later, or none: the
            // answer was cut short, which every client's vocabulary says
            // as `length`; the raw reason is logged, never sent on as a
            // finish reason no client wire defines.
            other => {
                tracing::info!(
                    reason = other.unwrap_or("(none)"),
                    "an incomplete Responses answer with a reason lmgw has no stop for, \
                     stopped as length"
                );
                FinishReason::Length
            }
        };
    }
    if called {
        FinishReason::ToolUse
    } else {
        FinishReason::Stop
    }
}

/// A `function_call` item's arguments, parsed as the Chat wire's are: a
/// string that is not JSON stays the string.
fn arguments(raw: &str) -> Value {
    crate::ir::parse_call_args(raw)
}

/// A call's own id, when the upstream sent a non-empty one.
fn call_id(item: &Value) -> Option<String> {
    item.get("call_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// The text of an error event or a failed Response's `error`: its message,
/// and its code beside it when there is one.
fn error_text(error: &Value, fallback: &str) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or(fallback);
    match error.get("code").and_then(Value::as_str) {
        Some(code) if !code.is_empty() => format!("{message} ({code})"),
        _ => message.to_string(),
    }
}

/// A 2xx whole Response with `status: "failed"` as the error it is:
/// OpenAI's own code and message, with the usage and tier it reported for
/// the row (decision 22). `failed` is `read_response`'s words, for a
/// Response without an `error` message.
fn failed_answer(
    v: &Value,
    failed: &str,
    report: crate::egress::openai_wire::ResponseReport,
) -> GatewayError {
    let error = v.get("error").filter(|e| e.is_object());
    let text = |k: &str| {
        error
            .and_then(|e| e.get(k))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
    };
    GatewayError::UpstreamFailed(Box::new(FailedAnswer {
        code: text("code").map(str::to_string),
        message: text("message").unwrap_or(failed).to_string(),
        usage: report.usage,
        service_tier: report.service_tier,
    }))
}

/// A `reasoning` item's seal (§7.3.1): its `encrypted_content` and its id,
/// for the upstream row `scope` names, as configured when it answered. `None`
/// for an item without either: nothing to send back.
fn seal_of(it: &Value, scope: &SealScope) -> Option<Seal> {
    let text = |k: &str| it.get(k).and_then(Value::as_str).filter(|t| !t.is_empty());
    Some(Seal::openai(scope, text("id")?, text("encrypted_content")?))
}

/// A whole Response object from the upstream row `scope` names as the IR's
/// completion. Its reasoning items are parts of the content where they
/// stood, each with its seal, when any came sealed (§7.3.1); otherwise the
/// content is its messages and calls, the trace apart, as before seals.
pub fn parse_response(v: &Value, scope: &SealScope) -> Result<Completion, GatewayError> {
    let report = read_response(v);
    if let Some(failed) = report.failed.clone() {
        return Err(failed_answer(v, &failed, report));
    }
    let mut content = Vec::new();
    let mut summaries: Vec<String> = Vec::new();
    let mut ids = CallIds::default();
    let mut called = false;
    let mut sealed = false;
    for it in v
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match it.get("type").and_then(Value::as_str) {
            Some(item::MESSAGE) => {
                for p in it
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let text = |k: &str| {
                        p.get(k)
                            .and_then(Value::as_str)
                            .filter(|t| !t.is_empty())
                            .map(str::to_string)
                    };
                    match p.get("type").and_then(Value::as_str) {
                        Some(part::OUTPUT_TEXT) => {
                            content.extend(text("text").map(ContentPart::text));
                        }
                        Some(part::REFUSAL) => {
                            content
                                .extend(text("refusal").map(|text| ContentPart::Refusal { text }));
                        }
                        _ => {}
                    }
                }
            }
            Some(item::FUNCTION_CALL) => {
                called = true;
                let raw = it.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                content.push(ContentPart::ToolUse {
                    id: call_id(it).unwrap_or_else(|| ids.mint()),
                    name: it
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    args: arguments(raw),
                });
            }
            Some(item::REASONING) => {
                let texts = summary_texts(it);
                let seal = seal_of(it, scope);
                sealed |= seal.is_some();
                if seal.is_some() || !texts.is_empty() {
                    content.push(ContentPart::Reasoning {
                        text: texts.join("\n\n"),
                        signature: None,
                        sealed: seal,
                        signature_scope: None,
                    });
                }
                summaries.extend(texts);
            }
            _ => {}
        }
    }
    if !sealed {
        content.retain(|p| !matches!(p, ContentPart::Reasoning { .. }));
    }
    Ok(Completion {
        content,
        reasoning: summaries.join("\n\n"),
        finish_reason: finish(v, called),
        usage: report.usage,
        model: v
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        timings: None,
        upstream_items: upstream_items(&report),
        service_tier: report.service_tier,
        context_management: None,
        usage_iterations: None,
    })
}

/// What the upstream calls a finished answer and its output items, for the
/// send site to record under the request's key (§7.2.3); `None` for an
/// answer without an id, which names nothing to record.
fn upstream_items(report: &crate::egress::openai_wire::ResponseReport) -> Option<UpstreamItems> {
    let id = report.id.as_deref().filter(|id| !id.is_empty())?;
    Some(UpstreamItems {
        response_id: id.to_string(),
        items: report.items.clone(),
    })
}

/// `items` (the terminal event's record) with each item a seal was taken
/// for recorded by the seal's own data, and one the terminal `output` left
/// out added: a stream's seals come from each item's `done`, and a client
/// sends back what it got (WP8 verify V-F4).
fn with_sealed(mut items: UpstreamItems, sealed: Vec<UpstreamItem>) -> UpstreamItems {
    for s in sealed {
        match items.items.iter_mut().find(|i| i.id == s.id) {
            Some(i) => i.encrypted_sha256 = s.encrypted_sha256,
            None => items.items.push(s),
        }
    }
    items
}

/// The summary texts of a `reasoning` item, in order.
fn summary_texts(it: &Value) -> Vec<String> {
    it.get("summary")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("text").and_then(Value::as_str))
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// The platform's stream decoder (`OpenaiEgress::new_decoder`): a send takes
/// either wire ([`super::wire_for`]), and each event says which it is
/// ([`is_responses_event`]), so one decoder reads both.
pub struct PlatformDecoder {
    chat: OpenaiDecoder,
    responses: ResponsesDecoder,
}

impl PlatformDecoder {
    /// The decoder of one answer from the upstream row `scope` names, which
    /// its seals name (§7.3.1). The chat chunks' `service_tier` is read too
    /// (P5).
    pub fn for_scope(scope: SealScope) -> Self {
        Self {
            chat: OpenaiDecoder::reading_tier(),
            responses: ResponsesDecoder {
                scope,
                ..Default::default()
            },
        }
    }
}

impl Default for PlatformDecoder {
    /// A decoder no send site made (a test's): its seals name no row (`0`).
    fn default() -> Self {
        Self::for_scope(SealScope::default())
    }
}

impl EgressStreamDecoder for PlatformDecoder {
    fn on_event(&mut self, ev: &SseEvent) -> Vec<StreamDelta> {
        match serde_json::from_str::<Value>(ev.data.trim()) {
            Ok(v) if is_responses_event(&v) => self.responses.on_value(&v),
            _ => self.chat.on_event(ev),
        }
    }

    fn on_end(&mut self) -> Vec<StreamDelta> {
        let mut out = self.chat.on_end();
        out.extend(self.responses.on_end());
        out
    }
}

/// What a Responses stream cut before its terminal event ends with: an
/// error, never a clean stop (the stream's own end is that event).
pub const CUT_BEFORE_TERMINAL: &str = "upstream stream ended before its terminal event";

/// The stream decoder of one Responses-wire answer.
#[derive(Default)]
pub struct ResponsesDecoder {
    /// The tool call ordinal of each `function_call` item, by its
    /// `output_index`: the IR numbers calls, the wire numbers every item.
    calls: HashMap<u64, usize>,
    /// The calls whose arguments came as deltas.
    streamed_args: HashSet<u64>,
    /// Ids for calls the upstream sent without one.
    ids: CallIds,
    /// The `(output_index, summary_index)` of the last summary text, for the
    /// blank line between two parts, of one item or of two: the stream's
    /// reasoning deltas join into the whole trace, as a whole answer's
    /// `reasoning` does. A consumer that keeps each item apart (a
    /// `/v1/messages` thinking block, a `/v1/responses` reasoning item, the
    /// in-process assembler's parts) starts an item without it; an unsealed
    /// item has no other boundary a joiner could read.
    last_summary: Option<(u64, u64)>,
    /// A Responses event came: the stream is this wire's.
    started: bool,
    /// Its terminal event (or an `error` event) came: its end is a clean
    /// one.
    ended: bool,
    /// The upstream row the answer comes from, as configured, which its
    /// seals name.
    pub scope: SealScope,
    /// Each sealed item as it is recorded, from the data its seal was taken
    /// from (an item's `done`), so the hash a client's seal is checked
    /// against is the hash of what the client got (WP8 verify V-F4).
    sealed: Vec<UpstreamItem>,
}

impl ResponsesDecoder {
    /// The body's end: a stream on this wire that had no terminal event was
    /// cut, which is an error ([`CUT_BEFORE_TERMINAL`]); its relay counts
    /// what came of it as any cut stream (§13.6).
    pub fn on_end(&mut self) -> Vec<StreamDelta> {
        match self.started && !self.ended {
            true => vec![StreamDelta::Error(CUT_BEFORE_TERMINAL.into())],
            false => vec![],
        }
    }

    /// The deltas of one Responses event `v`.
    pub fn on_value(&mut self, v: &Value) -> Vec<StreamDelta> {
        self.started = true;
        let index = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        let delta = || {
            v.get("delta")
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
        };
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            event::OUTPUT_TEXT_DELTA => delta()
                .map(|d| vec![StreamDelta::TextDelta(d.to_string())])
                .unwrap_or_default(),
            event::REFUSAL_DELTA => delta()
                .map(|d| vec![StreamDelta::RefusalDelta(d.to_string())])
                .unwrap_or_default(),
            event::REASONING_SUMMARY_TEXT_DELTA => {
                let Some(d) = delta() else {
                    return vec![];
                };
                let at = (index("output_index"), index("summary_index"));
                let mut text = String::new();
                if self.last_summary.is_some_and(|last| last != at) {
                    text.push_str("\n\n");
                }
                self.last_summary = Some(at);
                text.push_str(d);
                vec![StreamDelta::ReasoningDelta(text)]
            }
            event::OUTPUT_ITEM_ADDED => {
                let it = v.get("item").cloned().unwrap_or_default();
                if it.get("type").and_then(Value::as_str) != Some(item::FUNCTION_CALL) {
                    return vec![];
                }
                self.start_call(index("output_index"), &it)
                    .into_iter()
                    .collect()
            }
            event::FUNCTION_CALL_ARGUMENTS_DELTA => {
                let at = index("output_index");
                let (Some(&ordinal), Some(d)) = (self.calls.get(&at), delta()) else {
                    return vec![];
                };
                self.streamed_args.insert(at);
                vec![StreamDelta::ToolCallArgsDelta {
                    index: ordinal,
                    fragment: d.to_string(),
                }]
            }
            event::OUTPUT_ITEM_DONE => {
                let it = v.get("item").cloned().unwrap_or_default();
                self.item_done(index("output_index"), &it)
            }
            t @ (event::COMPLETED | event::INCOMPLETE | event::FAILED) => {
                self.ended = true;
                let response = v.get("response").cloned().unwrap_or_default();
                self.terminal(t, &response)
            }
            event::ERROR => {
                self.ended = true;
                vec![StreamDelta::Error(error_text(v, "upstream error"))]
            }
            // `response.created` and `.in_progress` (their tier is the
            // requested one), the `.done` and `.added` events of parts, and
            // every event lmgw has no reading for.
            _ => vec![],
        }
    }

    /// A `function_call` item's start, once per item.
    fn start_call(&mut self, at: u64, it: &Value) -> Option<StreamDelta> {
        if self.calls.contains_key(&at) {
            return None;
        }
        let ordinal = self.calls.len();
        self.calls.insert(at, ordinal);
        Some(StreamDelta::ToolCallStart {
            index: ordinal,
            id: call_id(it).unwrap_or_else(|| self.ids.mint()),
            name: it
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    /// An item's `done`: the complete copy. A call that did not start, or
    /// whose arguments did not stream, is read from it. A `reasoning` item's
    /// copy here is the complete one ("may be incomplete" in `added`, the
    /// reference says), which is where its seal is read (§7.3.1): after the
    /// item's summary deltas, so it closes that item's part.
    fn item_done(&mut self, at: u64, it: &Value) -> Vec<StreamDelta> {
        match it.get("type").and_then(Value::as_str) {
            Some(item::FUNCTION_CALL) => {}
            Some(item::REASONING) => {
                let seal = seal_of(it, &self.scope);
                if let Some(s) = &seal {
                    self.sealed.push(UpstreamItem {
                        id: s.item_id.clone(),
                        encrypted_sha256: Some(encrypted_sha256(&s.data)),
                    });
                }
                return seal.map(StreamDelta::ReasoningSeal).into_iter().collect();
            }
            _ => return vec![],
        }
        let mut out: Vec<StreamDelta> = self.start_call(at, it).into_iter().collect();
        let args = it
            .get("arguments")
            .and_then(Value::as_str)
            .filter(|a| !a.is_empty());
        if let (false, Some(a)) = (self.streamed_args.contains(&at), args) {
            self.streamed_args.insert(at);
            out.push(StreamDelta::ToolCallArgsDelta {
                index: self.calls[&at],
                fragment: a.to_string(),
            });
        }
        out
    }

    /// The terminal event `t` and its Response object: the tier, the usage,
    /// then the stop or the error.
    fn terminal(&mut self, t: &str, response: &Value) -> Vec<StreamDelta> {
        let report = read_response(response);
        let mut out = Vec::new();
        if let Some(tier) = report.service_tier.clone() {
            out.push(StreamDelta::ServiceTier(tier));
        }
        if report.usage.reported() {
            out.push(StreamDelta::Usage(report.usage));
        }
        if t == event::FAILED || report.failed.is_some() {
            let fallback = report
                .failed
                .as_deref()
                .unwrap_or("the response failed upstream");
            let text = response
                .get("error")
                .filter(|e| e.is_object())
                .map(|e| error_text(e, fallback))
                .unwrap_or_else(|| fallback.to_string());
            out.push(StreamDelta::Error(text));
            return out;
        }
        let called = !self.calls.is_empty()
            || response
                .get("output")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|i| i.get("type").and_then(Value::as_str) == Some(item::FUNCTION_CALL));
        if let Some(items) = upstream_items(&report) {
            out.push(StreamDelta::UpstreamItems(with_sealed(
                items,
                std::mem::take(&mut self.sealed),
            )));
        }
        out.push(StreamDelta::Stop(finish(response, called)));
        out
    }
}

#[cfg(test)]
mod seal_tests;
#[cfg(test)]
mod tests;
