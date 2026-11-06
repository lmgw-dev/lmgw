//! Whether the model may be working out of sight: what decides if a
//! keep-alive counts as the upstream speaking ([`super::Silence::hears`]).
//!
//! A model may work out of sight before the stream's first visible output,
//! and inside a span where it thinks, or where a tool its provider runs is
//! running, with nothing to stream meanwhile:
//!
//! - **Anthropic thinking**: a `content_block_start` of type `thinking` or
//!   `redacted_thinking` until the `content_block_stop` of its index. With
//!   `thinking.display` omitted the block carries no text, only its
//!   signature at the end, and Anthropic sends `ping`s in between.
//! - **Anthropic compaction** (*changed 2026-11-05*): a `content_block_start`
//!   of type `compaction` until the `content_block_stop` of its index, and
//!   no visible output. At threshold compaction Anthropic writes the summary
//!   between the start and the block's single `compaction_delta`, with only
//!   `ping`s in between.
//! - **Anthropic server tools** (*2026-11-05*): a block whose type ends in
//!   `_tool_use` but is no client's `tool_use` (`server_tool_use` for web
//!   search, web fetch, code execution, tool search, …; the MCP connector's
//!   `mcp_tool_use`) until the `content_block_stop` of the block that
//!   answers it: the one whose type ends in `_tool_result`
//!   (`web_search_tool_result`, `code_execution_tool_result`,
//!   `mcp_tool_result`, …) and whose `tool_use_id` is the tool block's `id`.
//!   The tool runs between the two blocks, where only `ping`s come: "The
//!   result block arrives complete in a single `content_block_start` event,
//!   with no deltas"
//!   (<https://platform.claude.com/docs/en/agents-and-tools/tool-use/server-tools>).
//!   A client's `tool_use` block ends the wait for every result not begun
//!   yet: with a client call in the same turn the API runs no server tool,
//!   it returns (`stop_reason: "tool_use"`). *Changed 2026-11-05*: so does
//!   every other block that is neither a `_tool_use` nor a `_tool_result`
//!   one (text, thinking, compaction): the model visibly moved on, so a
//!   result whose type or id did not match no longer holds the wait open
//!   while text streams. Parallel server tools still pair, their blocks
//!   come in a row. A tool block without an `id`, or a result without a
//!   `tool_use_id`, pairs with the oldest wait it could be.
//! - **OpenAI Responses reasoning**: a `response.output_item.added` of type
//!   `reasoning` until the `response.output_item.done` of its
//!   `output_index`. An encrypted or unsummarized reasoning item streams
//!   nothing in between, and an aggregator in front sends its comments.
//! - **OpenAI Responses hosted tools** (*2026-11-05*): a
//!   `response.output_item.added` of a call OpenAI runs itself
//!   (`web_search_call`, `file_search_call`, `code_interpreter_call`,
//!   `image_generation_call`, `mcp_call`, `mcp_list_tools`, a hosted
//!   `shell_call`, a server-run `tool_search_call`) until its
//!   `response.output_item.done`, and on until the next item is added or
//!   the answer shows again: a hosted shell's and a server tool search's
//!   result is an item of its own (`shell_call_output`,
//!   `tool_search_output`), and the docs do not say in which of the two the
//!   run happens
//!   (<https://developers.openai.com/api/docs/guides/tools-shell.md>,
//!   <https://developers.openai.com/api/docs/guides/tools-tool-search.md>).
//!   Every item type that ends in `_call` is such a call but the ones the
//!   client runs, which end the turn ([`CLIENT_CALLS`], a `shell_call`
//!   whose `environment` is `local`, a `tool_search_call` whose `execution`
//!   is `client`): a span an unknown client call opens ends with its item
//!   anyway, a hosted run that opened none would be cut.
//!   *Changed 2026-11-05*: a call whose done never came (an aggregator
//!   dropped it) ends at the next added item that is no span (not a
//!   reasoning item, not another hosted call) or at visible text; a hosted
//!   call added while another is open leaves that one open, since the docs
//!   do not say whether hosted calls may overlap. The gap after a call's
//!   done counts for one more timeout at most (the row's own, no constant
//!   of its own): past it keep-alives are silence again, so a stall there
//!   is cut at the latest two timeouts after the done.
//!
//! Chat Completions (OpenAI-compatible, llama.cpp), legacy completions and
//! Gemini have no such span: their reasoning comes as deltas, which are
//! events and break a silence anyway, and Gemini sends no keep-alives.
//!
//! Visible output is any answer content: a text, refusal, tool-call or
//! function-call delta, an Anthropic block or a Responses item that is not
//! thinking, any other non-empty key of a chat delta but the reasoning ones,
//! a Gemini part that is not a thought. The recognizer reads every wire's
//! shapes at once (they do not overlap), so a relay needs not say which wire
//! it reads.
//!
//! Once the answer is finished (a chat `finish_reason`, a Gemini
//! `finishReason`, Anthropic's `message_delta` with a `stop_reason` or its
//! `message_stop`, a Responses terminal event) nothing is out of sight any
//! more, a span left open included (a `pause_turn` may end on a server tool
//! not run yet): what is left is the usage tail and the body's end, and
//! keep-alives there are silence (*2026-11-05*). A `message_delta` without
//! a `stop_reason` finishes nothing (*changed 2026-11-05*): the decoder
//! does not stop on it either (`egress::anthropic`), and an empty
//! `finish_reason` is none on both sides.
//!
//! Each event is read as little as it takes: a named event only when it may
//! open or close a span (rare), an unnamed one (a chat chunk) only until the
//! first visible output, or when it mentions a span or terminal event at
//! all (a wire whose events carry their name only in `type`).

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::time::Instant;

use crate::sse::SseEvent;

const BLOCK_START: &str = "content_block_start";
const BLOCK_STOP: &str = "content_block_stop";
const ITEM_ADDED: &str = "response.output_item.added";
const ITEM_DONE: &str = "response.output_item.done";

/// The Anthropic blocks the model works out of sight in until their stop,
/// which are no visible output.
const HIDDEN_BLOCKS: &[&str] = &["thinking", "redacted_thinking", "compaction"];

/// Anthropic's next to last event: the answer is finished when it carries a
/// `stop_reason` ([`Probe::stops`]).
const MESSAGE_DELTA: &str = "message_delta";

/// The named events whose body is read: they may open or close a span, or
/// finish the answer.
const READ_BY_NAME: &[&str] = &[
    BLOCK_START,
    BLOCK_STOP,
    ITEM_ADDED,
    ITEM_DONE,
    MESSAGE_DELTA,
];

/// The events after which the answer is finished: Anthropic's last one, the
/// Responses terminal ones.
const TERMINAL: &[&str] = &[
    "message_stop",
    "response.completed",
    "response.failed",
    "response.incomplete",
];

/// Named events that are visible output by their name alone: the Responses
/// deltas of an answer, for an upstream that sends them without the item's
/// `response.output_item.added` first.
const SHOWN_BY_NAME: &[&str] = &[
    "response.output_text.delta",
    "response.refusal.delta",
    "response.function_call_arguments.delta",
    "response.custom_tool_call_input.delta",
    "response.output_audio.delta",
    "response.output_audio_transcript.delta",
];

/// The Responses call items the client runs: they end the turn, nothing
/// runs out of sight. (A `shell_call` and a `tool_search_call` may be
/// either: [`Item::hosted`].)
const CLIENT_CALLS: &[&str] = &[
    "function_call",
    "custom_tool_call",
    "computer_call",
    "local_shell_call",
    "apply_patch_call",
];

/// The keys of a chat delta that are no answer content.
const NOT_SHOWN: &[&str] = &[
    "role",
    "reasoning_content",
    "reasoning",
    "reasoning_details",
    "reasoning_text",
    "thinking",
];

/// What one stream showed so far.
#[derive(Default)]
pub(super) struct Sight {
    /// Visible output has begun.
    shown: bool,
    /// The answer is finished.
    done: bool,
    /// The spans open where the model may work out of sight.
    open: Vec<Span>,
}

/// One span where the model may work out of sight.
#[derive(PartialEq, Debug)]
enum Span {
    /// Until the `content_block_stop` or `response.output_item.done` of this
    /// index: a thinking block, a reasoning item, a server tool's result
    /// block (`None` for one that came without).
    Until(Option<u64>),
    /// An Anthropic server tool whose result block has not begun yet, by the
    /// tool block's `id`.
    Tool(Option<String>),
    /// A hosted Responses call, by its `output_index`, until its done; then
    /// [`Span::Between`]. Also until an item that is no span is added or
    /// the answer shows again, for a done that never came.
    Hosted(Option<u64>),
    /// A hosted Responses call done at this instant, until the next item is
    /// added or the answer shows again, and for one timeout at most
    /// ([`Sight::may_be_thinking`]).
    Between(Instant),
}

impl Span {
    fn is_tool(&self) -> bool {
        matches!(self, Span::Tool(_))
    }

    fn is_hosted(&self) -> bool {
        matches!(self, Span::Hosted(_) | Span::Between(_))
    }
}

/// The parts of an event the recognizer reads; everything else is skipped.
#[derive(Deserialize)]
struct Probe {
    #[serde(rename = "type")]
    ty: Option<String>,
    index: Option<u64>,
    output_index: Option<u64>,
    content_block: Option<Block>,
    item: Option<Item>,
    /// Anthropic's `message_delta`'s (a Responses delta's is a string).
    delta: Option<Value>,
    choices: Option<Vec<Choice>>,
    candidates: Option<Vec<Candidate>>,
}

impl Probe {
    /// Whether a `message_delta` ends the answer: it carries a
    /// `stop_reason`, the one the decoder ends on (`egress::anthropic`).
    fn stops(&self) -> bool {
        let reason = self.delta.as_ref().and_then(|d| d.get("stop_reason"));
        reason.and_then(Value::as_str).is_some()
    }
}

/// An Anthropic content block's head.
#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    ty: Option<String>,
    id: Option<String>,
    tool_use_id: Option<String>,
}

/// A Responses output item's head.
#[derive(Deserialize)]
struct Item {
    #[serde(rename = "type")]
    ty: Option<String>,
    /// A `tool_search_call`'s: `server` or `client`.
    execution: Option<String>,
    /// A `shell_call`'s.
    environment: Option<Environment>,
}

#[derive(Deserialize)]
struct Environment {
    #[serde(rename = "type")]
    ty: Option<String>,
}

#[derive(Deserialize)]
struct Choice {
    delta: Option<Map<String, Value>>,
    /// Legacy completions' text.
    text: Option<String>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Candidate {
    content: Option<GeminiContent>,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct GeminiContent {
    parts: Option<Vec<Map<String, Value>>>,
}

impl Sight {
    /// Whether the model may be working where nothing of it streams at
    /// `now`: before any visible output, or inside a span, while the answer
    /// is not finished. The gap after a hosted call's done counts for one
    /// `timeout` after the done, no longer.
    pub(super) fn may_be_thinking(&self, now: Instant, timeout: Duration) -> bool {
        let counts = |s: &Span| match s {
            Span::Between(since) => now < *since + timeout,
            _ => true,
        };
        !self.done && (!self.shown || self.open.iter().any(counts))
    }

    /// Read one event that is more than a keep-alive, come at `now`.
    pub(super) fn observe(&mut self, ev: &SseEvent, now: Instant) {
        let named = ev.event.as_deref();
        if named.is_some_and(|n| TERMINAL.contains(&n)) {
            self.done = true;
            return;
        }
        let read_body = match named {
            Some(n) => READ_BY_NAME.contains(&n),
            None => !self.shown || mentions_span(&ev.data),
        };
        if !read_body {
            if named.is_some_and(|n| SHOWN_BY_NAME.contains(&n)) {
                self.show();
            }
            return;
        }
        let Ok(p) = serde_json::from_str::<Probe>(&ev.data) else {
            return;
        };
        match named.or(p.ty.as_deref()) {
            Some(BLOCK_START) => self.block_start(p.index, p.content_block),
            Some(BLOCK_STOP) => self.open.retain(|s| *s != Span::Until(p.index)),
            Some(ITEM_ADDED) => self.item_added(p.output_index, p.item),
            Some(ITEM_DONE) => {
                self.open.retain(|s| *s != Span::Until(p.output_index));
                for s in &mut self.open {
                    if *s == Span::Hosted(p.output_index) {
                        *s = Span::Between(now);
                    }
                }
            }
            Some(MESSAGE_DELTA) => self.done |= p.stops(),
            Some(n) if SHOWN_BY_NAME.contains(&n) => self.show(),
            Some(n) if TERMINAL.contains(&n) => self.done = true,
            // Another named event: Anthropic's `message_start`, a Responses
            // `response.created`, a chat chunk's `object` is no `type`.
            Some(_) if p.choices.is_none() && p.candidates.is_none() => {}
            _ => {
                let choices = || p.choices.iter().flatten();
                let candidates = || p.candidates.iter().flatten();
                if choices().any(Choice::shows) || candidates().any(Candidate::shows) {
                    self.shown = true;
                }
                let finished = |r: &Option<String>| r.as_deref().is_some_and(|r| !r.is_empty());
                if choices().any(|c| finished(&c.finish_reason))
                    || candidates().any(|c| finished(&c.finish_reason))
                {
                    self.done = true;
                }
            }
        }
    }

    /// Visible output: the answer goes on past every hosted call, its done
    /// come or not.
    fn show(&mut self) {
        self.shown = true;
        self.open.retain(|s| !s.is_hosted());
    }

    fn block_start(&mut self, index: Option<u64>, block: Option<Block>) {
        let (ty, id, answers) = match block {
            Some(b) => (b.ty.unwrap_or_default(), b.id, b.tool_use_id),
            None => Default::default(),
        };
        let server_tool = ty.ends_with("_tool_use");
        let result = ty.ends_with("_tool_result");
        if !server_tool && !result {
            // A client's `tool_use`, the model's text or thinking: the model
            // moved on, no server tool runs any more, one whose result never
            // came included.
            self.open.retain(|s| !s.is_tool());
        }
        if HIDDEN_BLOCKS.contains(&ty.as_str()) {
            self.open.push(Span::Until(index));
            return;
        }
        self.shown = true;
        if server_tool {
            self.open.push(Span::Tool(id));
        } else if result {
            // Its own tool's span by id; where either side has none, the
            // oldest one that could be it.
            let by_id =
                |s: &Span| matches!((s, &answers), (Span::Tool(Some(t)), Some(a)) if t == a);
            let unpaired =
                |s: &Span| matches!((s, &answers), (Span::Tool(None), _) | (Span::Tool(_), None));
            let waiting =
                (self.open.iter().position(by_id)).or_else(|| self.open.iter().position(unpaired));
            if let Some(i) = waiting {
                self.open[i] = Span::Until(index);
            }
        }
    }

    fn item_added(&mut self, index: Option<u64>, item: Option<Item>) {
        self.open.retain(|s| !matches!(s, Span::Between(_)));
        match item {
            Some(i) if i.ty.as_deref() == Some("reasoning") => self.open.push(Span::Until(index)),
            // A hosted call next to another one still open: both may run.
            Some(i) if i.hosted() => {
                self.shown = true;
                self.open.push(Span::Hosted(index));
            }
            // An item that is no span: the answer went on past every hosted
            // call, one whose done never came included.
            _ => self.show(),
        }
    }
}

impl Item {
    /// Whether OpenAI runs this call itself (the module docs).
    fn hosted(&self) -> bool {
        let ty = self.ty.as_deref().unwrap_or_default();
        let call = ty == "mcp_list_tools" || ty.ends_with("_call");
        let local_shell = ty == "shell_call"
            && self.environment.as_ref().and_then(|e| e.ty.as_deref()) == Some("local");
        let client_search = ty == "tool_search_call" && self.execution.as_deref() == Some("client");
        call && !CLIENT_CALLS.contains(&ty) && !local_shell && !client_search
    }
}

/// Whether an unnamed event's text names a span or terminal event: only then
/// is it read after the first visible output.
fn mentions_span(data: &str) -> bool {
    let finder = |needle: &str| memchr::memmem::find(data.as_bytes(), needle.as_bytes()).is_some();
    finder("content_block_st")
        || finder("output_item.")
        || finder("message_delta")
        || finder("message_stop")
        || finder("response.completed")
        || finder("response.failed")
        || finder("response.incomplete")
}

/// A JSON value that carries something: not null, not empty.
fn present(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    }
}

impl Choice {
    fn shows(&self) -> bool {
        let delta = self.delta.as_ref().is_some_and(|d| {
            d.iter()
                .any(|(k, v)| !NOT_SHOWN.contains(&k.as_str()) && present(v))
        });
        delta || self.text.as_deref().is_some_and(|t| !t.is_empty())
    }
}

impl Candidate {
    fn shows(&self) -> bool {
        let parts = self.content.as_ref().and_then(|c| c.parts.as_ref());
        parts.into_iter().flatten().any(|part| {
            part.get("thought") != Some(&Value::Bool(true))
                && part
                    .iter()
                    .any(|(k, v)| k != "thought" && k != "thoughtSignature" && present(v))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn named(name: &str, data: Value) -> SseEvent {
        SseEvent {
            event: Some(name.into()),
            data: data.to_string(),
        }
    }

    fn unnamed(data: Value) -> SseEvent {
        SseEvent {
            event: None,
            data: data.to_string(),
        }
    }

    const TIMEOUT: Duration = Duration::from_secs(1);

    /// `events`, all at one instant.
    fn seen(events: &[SseEvent], at: Instant) -> Sight {
        let mut s = Sight::default();
        for ev in events {
            s.observe(ev, at);
        }
        s
    }

    /// Whether the model may be thinking at once after `events`.
    fn after(events: &[SseEvent]) -> bool {
        let at = Instant::now();
        seen(events, at).may_be_thinking(at, TIMEOUT)
    }

    fn chat(delta: Value) -> SseEvent {
        unnamed(
            json!({"object": "chat.completion.chunk", "choices": [{"index": 0, "delta": delta}]}),
        )
    }

    fn block(i: u64, b: Value) -> SseEvent {
        named(
            BLOCK_START,
            json!({"type": BLOCK_START, "index": i, "content_block": b}),
        )
    }

    fn start(i: u64, ty: &str) -> SseEvent {
        block(i, json!({"type": ty}))
    }

    fn stop(i: u64) -> SseEvent {
        named(BLOCK_STOP, json!({"type": BLOCK_STOP, "index": i}))
    }

    fn added(i: u64, item: Value) -> SseEvent {
        named(
            ITEM_ADDED,
            json!({"type": ITEM_ADDED, "output_index": i, "item": item}),
        )
    }

    fn added_ty(i: u64, ty: &str) -> SseEvent {
        added(i, json!({"type": ty}))
    }

    fn done(i: u64) -> SseEvent {
        named(ITEM_DONE, json!({"type": ITEM_DONE, "output_index": i}))
    }

    #[test]
    fn a_chat_stream_shows_at_its_first_answer_content() {
        assert!(after(&[chat(json!({"role": "assistant", "content": ""}))]));
        assert!(after(&[chat(json!({"reasoning_content": "hmm"}))]));
        assert!(after(&[chat(json!({"content": null, "refusal": null}))]));
        assert!(!after(&[chat(json!({"content": "Hi"}))]));
        assert!(!after(&[chat(json!({"refusal": "no"}))]));
        assert!(!after(&[chat(
            json!({"tool_calls": [{"index": 0, "function": {"name": "f"}}]})
        )]));
        assert!(!after(&[unnamed(
            json!({"choices": [{"index": 0, "text": "Hi"}]})
        )]));
    }

    #[test]
    fn an_anthropic_thinking_block_is_a_span() {
        let msg = named("message_start", json!({"type": "message_start"}));
        assert!(after(&[msg.clone(), start(0, "thinking")]));
        assert!(!after(&[start(0, "thinking"), stop(0), start(1, "text")]));
        // After visible output, a thinking block opens a span again
        // (interleaved thinking), until its own stop.
        assert!(after(&[
            start(0, "text"),
            stop(0),
            start(1, "redacted_thinking")
        ]));
        assert!(!after(&[start(0, "text"), start(1, "thinking"), stop(1)]));
        // A compaction block is written out of sight too, and shows nothing.
        assert!(after(&[start(0, "compaction")]));
        assert!(after(&[start(0, "text"), stop(0), start(1, "compaction")]));
        assert!(!after(&[start(0, "text"), start(1, "compaction"), stop(1)]));
        assert!(!after(&[start(0, "tool_use")]));
        // Without the `event:` line, by its `type`.
        let mut bare = start(0, "text");
        bare.event = None;
        assert!(!after(&[bare]));
    }

    #[test]
    fn a_responses_reasoning_item_is_a_span() {
        let created = named("response.created", json!({"type": "response.created"}));
        assert!(after(&[created, added_ty(0, "reasoning")]));
        assert!(!after(&[
            added_ty(0, "reasoning"),
            done(0),
            added_ty(1, "message")
        ]));
        assert!(after(&[added_ty(0, "message"), added_ty(1, "reasoning")]));
        assert!(!after(&[added_ty(0, "function_call")]));
        let delta = named(
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "delta": "Hi"}),
        );
        assert!(!after(&[delta]));
    }

    #[test]
    fn a_gemini_thought_is_not_shown() {
        let parts = |p: Value| unnamed(json!({"candidates": [{"content": {"parts": p}}]}));
        assert!(after(&[parts(
            json!([{"thought": true, "text": "hmm", "thoughtSignature": "x"}])
        )]));
        assert!(!after(&[parts(json!([{"text": "Hi"}]))]));
        assert!(!after(&[parts(json!([{"functionCall": {"name": "f"}}]))]));
    }

    /// An Anthropic server tool runs from its block's start, across the gap
    /// after its stop, to the stop of the result block that answers it.
    #[test]
    fn an_anthropic_server_tool_is_a_span_until_its_result_block_stops() {
        let tool = |i: u64, ty: &str, id: &str| block(i, json!({"type": ty, "id": id}));
        let result = |i: u64, ty: &str, id: &str| block(i, json!({"type": ty, "tool_use_id": id}));
        let text = [start(0, "text"), stop(0)];
        let searching = [
            &text[..],
            &[tool(1, "server_tool_use", "srvtoolu_1"), stop(1)],
        ]
        .concat();
        assert!(after(&searching));
        let answered = [
            &searching[..],
            &[result(2, "web_search_tool_result", "srvtoolu_1")],
        ]
        .concat();
        assert!(after(&answered), "the result block is still open");
        assert!(!after(&[&answered[..], &[stop(2)]].concat()));
        // The MCP connector's, and a result of another type.
        assert!(after(
            &[&text[..], &[tool(1, "mcp_tool_use", "mcptoolu_1"), stop(1)]].concat()
        ));
        assert!(!after(
            &[
                &text[..],
                &[
                    tool(1, "mcp_tool_use", "mcptoolu_1"),
                    stop(1),
                    result(2, "mcp_tool_result", "mcptoolu_1"),
                    stop(2),
                ]
            ]
            .concat()
        ));
        // Two tools at once: one's result leaves the other's run open.
        let both = [
            &text[..],
            &[
                tool(1, "server_tool_use", "a"),
                stop(1),
                tool(2, "server_tool_use", "b"),
                stop(2),
                result(3, "web_fetch_tool_result", "b"),
                stop(3),
            ],
        ]
        .concat();
        assert!(after(&both));
        assert!(!after(
            &[
                &both[..],
                &[result(4, "web_search_tool_result", "a"), stop(4)]
            ]
            .concat()
        ));
        // A client call in the same turn: the API runs no server tool.
        assert!(!after(
            &[&searching[..], &[tool(2, "tool_use", "toolu_1"), stop(2)]].concat()
        ));
        // A client's `tool_result` is no server tool's.
        assert!(!after(&[start(0, "tool_result")]));
    }

    /// A Responses call OpenAI runs is a span until its item is done, and on
    /// until the next item; a call the client runs is none.
    #[test]
    fn a_responses_hosted_call_is_a_span() {
        let text = [added_ty(0, "message"), done(0)];
        for ty in [
            "web_search_call",
            "file_search_call",
            "code_interpreter_call",
            "image_generation_call",
            "mcp_call",
            "mcp_list_tools",
            "shell_call",
            "tool_search_call",
        ] {
            let running = [&text[..], &[added_ty(1, ty)]].concat();
            assert!(after(&running), "{ty}");
            let ran = [&running[..], &[done(1)]].concat();
            assert!(after(&ran), "{ty}: its output item may follow");
            assert!(
                !after(&[&ran[..], &[added_ty(2, "message")]].concat()),
                "{ty}"
            );
            let delta = named("response.output_text.delta", json!({"delta": "Hi"}));
            assert!(!after(&[&ran[..], &[delta]].concat()), "{ty}");
        }
        for item in [
            json!({"type": "function_call"}),
            json!({"type": "custom_tool_call"}),
            json!({"type": "computer_call"}),
            json!({"type": "local_shell_call"}),
            json!({"type": "apply_patch_call"}),
            json!({"type": "shell_call", "environment": {"type": "local"}}),
            json!({"type": "tool_search_call", "execution": "client"}),
            json!({"type": "mcp_approval_request"}),
        ] {
            assert!(
                !after(&[&text[..], &[added(1, item.clone())]].concat()),
                "{item}"
            );
        }
    }

    /// Once the answer is finished, nothing is out of sight: not before any
    /// output, not in a span left open.
    #[test]
    fn a_finished_answer_is_out_of_sight_no_more() {
        let finish =
            |r: &str| unnamed(json!({"choices": [{"index": 0, "delta": {}, "finish_reason": r}]}));
        assert!(!after(&[finish("stop")]));
        assert!(after(&[chat(json!({"role": "assistant"}))]));
        let gemini = unnamed(json!({"candidates": [{"finishReason": "STOP"}]}));
        assert!(!after(&[gemini]));
        let tool = block(0, json!({"type": "server_tool_use", "id": "srvtoolu_1"}));
        let message_delta = named(
            MESSAGE_DELTA,
            json!({"type": MESSAGE_DELTA, "delta": {"stop_reason": "pause_turn"}}),
        );
        let message_stop = named("message_stop", json!({"type": "message_stop"}));
        for ev in [message_delta.clone(), message_stop] {
            assert!(!after(&[tool.clone(), stop(0), ev.clone()]), "{ev:?}");
        }
        let mut bare = message_delta;
        bare.event = None;
        assert!(!after(&[start(0, "thinking"), bare]));
        for end in TERMINAL.iter().filter(|t| t.starts_with("response.")) {
            let ev = named(end, json!({"type": end}));
            assert!(!after(&[added_ty(0, "reasoning"), ev]), "{end}");
        }
    }

    /// A `message_delta` without a `stop_reason` (usage or context edits
    /// alone) does not finish the answer: the decoder does not stop on it
    /// either.
    #[test]
    fn a_message_delta_without_a_stop_reason_finishes_nothing() {
        let thinking = start(0, "thinking");
        for delta in [
            json!({"type": MESSAGE_DELTA, "usage": {"output_tokens": 3}}),
            json!({"type": MESSAGE_DELTA, "delta": {"stop_reason": null}}),
            json!({"type": MESSAGE_DELTA, "delta": {}}),
        ] {
            assert!(
                after(&[thinking.clone(), named(MESSAGE_DELTA, delta.clone())]),
                "{delta}"
            );
            let mut bare = named(MESSAGE_DELTA, delta.clone());
            bare.event = None;
            assert!(after(&[thinking.clone(), bare]), "bare {delta}");
        }
    }

    /// Any block that is neither a server tool's nor a result ends a wait
    /// for a result that never came: the model moved on.
    #[test]
    fn a_block_after_an_unanswered_server_tool_closes_its_span() {
        let tool = block(1, json!({"type": "server_tool_use", "id": "srvtoolu_1"}));
        let unanswered = [start(0, "text"), stop(0), tool, stop(1)];
        assert!(after(&unanswered));
        for ty in ["text", "thinking", "redacted_thinking"] {
            let s = seen(
                &[&unanswered[..], &[start(2, ty), stop(2)]].concat(),
                Instant::now(),
            );
            assert!(!s.open.iter().any(Span::is_tool), "{ty}: {:?}", s.open);
            assert!(
                !after(&[&unanswered[..], &[start(2, ty), stop(2)]].concat()),
                "{ty}"
            );
        }
        // A result of another tool's id leaves the wait open until then.
        let other = block(
            2,
            json!({"type": "web_search_tool_result", "tool_use_id": "x"}),
        );
        assert!(after(&[&unanswered[..], &[other, stop(2)]].concat()));
    }

    /// A server tool block without an `id` is answered by the oldest result
    /// with an id it could be.
    #[test]
    fn a_server_tool_without_an_id_pairs_with_the_oldest_result() {
        let tool = block(1, json!({"type": "server_tool_use"}));
        let result = block(
            2,
            json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1"}),
        );
        let ran = [start(0, "text"), stop(0), tool, stop(1), result];
        assert!(after(&ran), "the result block is still open");
        assert!(!after(&[&ran[..], &[stop(2)]].concat()));
    }

    /// A hosted call whose `output_item.done` never came ends with the next
    /// item that is no span or with visible text; a reasoning item or another
    /// hosted call next to it leaves it open.
    #[test]
    fn a_hosted_call_without_its_done_ends_with_the_next_item() {
        let running = [
            added_ty(0, "message"),
            done(0),
            added_ty(1, "web_search_call"),
        ];
        assert!(!after(&[&running[..], &[added_ty(2, "message")]].concat()));
        assert!(!after(
            &[&running[..], &[added_ty(2, "function_call")]].concat()
        ));
        let delta = named("response.output_text.delta", json!({"delta": "Hi"}));
        assert!(!after(&[&running[..], &[delta]].concat()));
        let reasons = [&running[..], &[added_ty(2, "reasoning"), done(2)]].concat();
        assert!(after(&reasons));
        let parallel = [&running[..], &[added_ty(2, "web_search_call"), done(2)]].concat();
        let s = seen(&parallel, Instant::now());
        assert!(s.open.contains(&Span::Hosted(Some(1))), "{:?}", s.open);
    }

    /// The gap after a hosted call's done counts for one timeout: past it a
    /// keep-alive is silence again.
    #[test]
    fn the_gap_after_a_hosted_call_lasts_one_timeout() {
        let at = Instant::now();
        let s = seen(
            &[
                added_ty(0, "message"),
                added_ty(1, "web_search_call"),
                done(1),
            ],
            at,
        );
        assert!(s.may_be_thinking(at, TIMEOUT));
        assert!(s.may_be_thinking(at + TIMEOUT - Duration::from_millis(1), TIMEOUT));
        assert!(!s.may_be_thinking(at + TIMEOUT, TIMEOUT));
        // A hosted call still running is no such gap.
        let running = seen(
            &[added_ty(0, "message"), added_ty(1, "web_search_call")],
            at,
        );
        assert!(running.may_be_thinking(at + TIMEOUT * 10, TIMEOUT));
    }
}
