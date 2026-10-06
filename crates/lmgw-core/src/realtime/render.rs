//! Items → the chat model's request (realtime design §7.2).
//!
//! Before each response the conversation is rendered into an
//! [`ir::ChatRequest`](crate::ir::ChatRequest):
//! - the instructions become the system message — for a spoken response
//!   with, as a paragraph of its own after them, the sounds its voice can
//!   make ([`Input::speech_hint`], WP10 D7);
//! - a user item becomes its text (audio: its transcript);
//! - an assistant item becomes its text, or for audio what the model wrote
//!   up to what was heard ([`Input::written`], §7.3) — code and markdown
//!   the voice left out included, where the transcript has only what was
//!   said (B2 review 2);
//! - function calls and their outputs become assistant tool calls and tool
//!   results, and so do server-side MCP calls with their results (`mcp`,
//!   realtime-server-tools §2.6); an `mcp_list_tools` item is never
//!   rendered — its tools go as tool definitions, after the client's
//!   functions (§2.1).
//!
//! **Normalized for strict templates.** Gemma- and Qwen-class templates (via
//! llama-server) and OpenAI-style upstreams refuse sequences a conversation
//! can legitimately contain, and the egress does not repair them, so this
//! does:
//! - system **items** join the instructions in the one leading system
//!   message — those templates raise on a system message anywhere else;
//! - consecutive user items are merged into one user message, and so are
//!   consecutive assistant contributions (a text, then the calls the same
//!   response made, become one assistant message — templates want roles to
//!   alternate);
//! - user and assistant items with nothing in them are dropped (an empty
//!   transcript, a response cancelled before its first word);
//! - **a tool result directly follows its call.** Anything that arrived
//!   while the call was pending — the user speaking again — goes after the
//!   result; a result that has not arrived yet is filled with `"(no result
//!   yet)"`;
//! - each tool result carries its call's name (Gemini correlates by name);
//! - **the model speaks after a user.** A conversation that opens with the
//!   assistant (the agent greeted first) gets [`CONVERSATION_START`] as a
//!   user turn before it — strict templates want the user first — and one
//!   that ends with the assistant (a second `response.create` with nothing
//!   said in between) gets [`CONTINUE`] after it: llama-server and
//!   Anthropic take a trailing assistant message as a prefill to extend,
//!   which llama-server refuses outright with reasoning on. Both are
//!   logged when used.
//!
//! Two cases the spec leaves open are decided here, visibly: a function call
//! that never completed (cancelled or failed mid-arguments) is not rendered —
//! it was never made — and an output whose call is not in the conversation
//! (the call was deleted, or never existed) is rendered as a user message
//! naming the call, rather than dropped.
//!
//! Client tools go out as they were given, with one change: `$schema` keys
//! are taken out of their parameter schemas — `@openai/agents` writes one
//! into every tool, and some providers refuse it. Everything else (`anyOf`
//! nullable unions included) is the client's and stays.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use super::protocol::{
    ContentPart, Item, ItemStatus, MaxOutputTokens, Role as ItemRole, Tool, ToolChoice,
    ToolChoiceMode,
};
use crate::ir::{
    self, ChatRequest, Message, Params, ReasoningControl, Role, ToolDef, ToolResultBlock,
};

mod mcp;

pub(crate) use mcp::of_session as mcp_of_session;
pub use mcp::{McpInput, McpRendered};

/// The synthetic result of a call whose output has not arrived (§7.2).
pub const NO_RESULT_YET: &str = "(no result yet)";

/// The synthetic user turn before a conversation the assistant opened.
pub const CONVERSATION_START: &str = "(the conversation starts)";

/// The synthetic user turn after a conversation that ends with the
/// assistant, so the model answers rather than extends its own turn.
pub const CONTINUE: &str = "(continue)";

/// What one response renders from: the conversation and the response's
/// snapshot of the session config (§4.2 step 4).
pub struct Input<'a> {
    pub alias: &'a str,
    pub instructions: &'a str,
    pub items: &'a [Item],
    pub tools: &'a [Tool],
    pub tool_choice: Option<&'a ToolChoice>,
    pub parallel_tool_calls: Option<bool>,
    pub max_output_tokens: Option<MaxOutputTokens>,
    /// The session's `reasoning` object, as the client sent it.
    pub reasoning: Option<&'a Value>,
    /// The tag hint of a spoken response (WP10 D7): a paragraph after the
    /// instructions — never part of `session.instructions`, which a client
    /// sends back, so it would double.
    pub speech_hint: Option<&'a str>,
    /// What the model wrote of an assistant audio item, by item id, up to
    /// what was heard (§7.3); `None` renders the item's transcript — an
    /// item the client made, which has no other text.
    pub written: &'a dyn Fn(&str) -> Option<String>,
    /// The session's MCP tools this response offers, and how its
    /// `mcp_call` items render (`mcp`).
    pub mcp: McpInput<'a>,
}

/// Render one response's request.
pub fn render(input: &Input<'_>) -> ChatRequest {
    let calls = renderable_calls(input.items);
    let mut b = Builder::new(input.items, &calls);

    let mut system: Vec<String> = Vec::new();
    if !input.instructions.trim().is_empty() {
        system.push(input.instructions.to_string());
    }
    if let Some(hint) = input.speech_hint.filter(|h| !h.trim().is_empty()) {
        system.push(hint.to_string());
    }
    for (pos, item) in input.items.iter().enumerate() {
        match item {
            Item::Message(m) => {
                let written = match m.role {
                    ItemRole::Assistant => m.id.as_deref().and_then(input.written),
                    _ => None,
                };
                let text = written.unwrap_or_else(|| message_text(&m.content));
                match m.role {
                    ItemRole::System if !text.trim().is_empty() => system.push(text),
                    ItemRole::User if !text.trim().is_empty() => b.user(text),
                    ItemRole::Assistant if !text.trim().is_empty() => b.assistant_text(text),
                    _ => {}
                }
            }
            Item::FunctionCall(c) => {
                let Some(call_id) = c.call_id.as_deref() else {
                    continue;
                };
                if calls.get(call_id) == Some(&pos) {
                    b.call(call_id, &c.name, &c.arguments);
                }
            }
            Item::FunctionCallOutput(o) => {
                if b.consumed.contains(&pos) {
                    continue;
                }
                match calls.get(o.call_id.as_str()) {
                    // The call comes later: its output is pulled up to it.
                    Some(&at) if at > pos => {}
                    // The call is waiting for its results right now.
                    Some(_) if b.pending.contains(&o.call_id) => {
                        b.flush();
                        if !b.consumed.contains(&pos) {
                            b.orphan(&o.call_id, &o.output);
                        }
                    }
                    // A second output for an answered call, or no call at all.
                    _ => b.orphan(&o.call_id, &o.output),
                }
            }
            // A server-side call and its result (`mcp`).
            Item::McpCall(c) => b.mcp_call((input.mcp.call)(c), &c.arguments),
            // A listing is never rendered: its tools go as tool definitions
            // (realtime-server-tools §1.2).
            Item::McpListTools(_) => {}
        }
    }
    b.flush();

    let mut turns = b.messages;
    if turns.first().is_some_and(|m| m.role == Role::Assistant) {
        tracing::info!(
            "realtime render for '{}': the conversation opens with the assistant; a user turn \
             {CONVERSATION_START:?} goes before it",
            input.alias
        );
        turns.insert(0, Message::text(Role::User, CONVERSATION_START));
    }
    if turns.last().is_some_and(|m| m.role == Role::Assistant) {
        tracing::info!(
            "realtime render for '{}': the conversation ends with the assistant; a user turn \
             {CONTINUE:?} goes after it",
            input.alias
        );
        turns.push(Message::text(Role::User, CONTINUE));
    }
    let mut messages = Vec::with_capacity(turns.len() + 1);
    if !system.is_empty() {
        messages.push(Message::text(Role::System, system.join("\n\n")));
    }
    messages.extend(turns);

    let functions: Vec<ToolDef> = input.tools.iter().filter_map(tool_def).collect();
    let tools = mcp::offered(functions, &input.mcp);
    let mut passthrough = Map::new();
    if let (Some(p), false) = (input.parallel_tool_calls, tools.is_empty()) {
        passthrough.insert("parallel_tool_calls".into(), Value::Bool(p));
    }
    ChatRequest {
        model_alias: input.alias.to_string(),
        messages,
        params: Params {
            max_tokens: match input.max_output_tokens {
                Some(MaxOutputTokens::Count(n)) => Some(n),
                _ => None,
            },
            reasoning: Some(reasoning_control(input.reasoning)),
            ..Default::default()
        },
        // A tool choice with no tools is refused by some upstreams.
        tool_choice: (!tools.is_empty())
            .then(|| {
                let choice = input.mcp.choice.cloned();
                choice.or_else(|| input.tool_choice.and_then(tool_choice))
            })
            .flatten(),
        tools,
        stream: true,
        passthrough,
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    }
}

/// Reasoning for a voice response (§7.6): **off**, unless the session's
/// `reasoning` names an effort — the only way that object says otherwise.
/// Spelled through lmgw's own control, which each egress turns into what its
/// engine understands (llama-server's `enable_thinking` kwarg included).
pub fn reasoning_control(session: Option<&Value>) -> ReasoningControl {
    let effort = session
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|e| !e.is_empty());
    match effort {
        Some(e) => ReasoningControl {
            effort: Some(e.to_string()),
            ..Default::default()
        }
        .normalised(),
        None => ReasoningControl {
            enabled: Some(false),
            ..Default::default()
        },
    }
}

/// Take every `$schema` key out of a JSON Schema, at every depth — but only
/// where a schema stands: a *property* named `$schema` is the client's, and
/// so is anything inside `enum`, `const`, `default` or `examples`.
pub fn strip_schema_keys(schema: &mut Value) {
    let Value::Object(m) = schema else {
        return;
    };
    m.remove("$schema");
    for (k, v) in m.iter_mut() {
        match k.as_str() {
            // One schema.
            "items"
            | "additionalProperties"
            | "additionalItems"
            | "not"
            | "if"
            | "then"
            | "else"
            | "contains"
            | "propertyNames"
            | "unevaluatedItems"
            | "unevaluatedProperties"
            | "contentSchema" => match v {
                // `items` may be a list of schemas (draft-07 tuples).
                Value::Array(a) => a.iter_mut().for_each(strip_schema_keys),
                _ => strip_schema_keys(v),
            },
            // A list of schemas.
            "anyOf" | "oneOf" | "allOf" | "prefixItems" => {
                if let Value::Array(a) = v {
                    a.iter_mut().for_each(strip_schema_keys);
                }
            }
            // A map of names to schemas.
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" => {
                if let Value::Object(named) = v {
                    named.values_mut().for_each(strip_schema_keys);
                }
            }
            // draft-07: each value is a schema, or a list of property names
            // (data, left alone — `strip_schema_keys` only touches objects).
            "dependencies" => {
                if let Value::Object(named) = v {
                    named.values_mut().for_each(strip_schema_keys);
                }
            }
            _ => {}
        }
    }
}

/// A function tool as the model is offered it. An `mcp` entry is not a tool
/// of its own: it names a label whose listed tools are offered
/// (realtime-server-tools §2.1).
fn tool_def(t: &Tool) -> Option<ToolDef> {
    let Tool::Function {
        name,
        description,
        parameters,
    } = t
    else {
        return None;
    };
    let mut parameters = parameters
        .clone()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    strip_schema_keys(&mut parameters);
    Some(ToolDef {
        name: name.clone(),
        description: description.clone(),
        parameters,
    })
}

/// An `mcp` choice is looked up in the session's tool table when the
/// response is created and launched (`mcp_tools::McpTable::offer`), and
/// comes as [`McpInput::choice`]; here it says nothing.
fn tool_choice(c: &ToolChoice) -> Option<ir::ToolChoice> {
    Some(match c {
        ToolChoice::Mode(ToolChoiceMode::Auto) => ir::ToolChoice::Auto,
        ToolChoice::Mode(ToolChoiceMode::None) => ir::ToolChoice::None,
        ToolChoice::Mode(ToolChoiceMode::Required) => ir::ToolChoice::Required,
        ToolChoice::Function(f) => ir::ToolChoice::Tool {
            name: f.name.clone(),
        },
        ToolChoice::Mcp(_) => return None,
    })
}

/// A message item's text: typed text, or an audio part's transcript.
fn message_text(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|p| match p {
            ContentPart::InputText { text } | ContentPart::OutputText { text } => {
                Some(text.as_str())
            }
            ContentPart::InputAudio { transcript, .. }
            | ContentPart::OutputAudio { transcript, .. } => transcript.as_deref(),
        })
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The function calls that are rendered — every one that was not left
/// incomplete — by `call_id`, at the position of its first item.
fn renderable_calls(items: &[Item]) -> HashMap<&str, usize> {
    let mut calls = HashMap::new();
    for (pos, item) in items.iter().enumerate() {
        if let Item::FunctionCall(c) = item {
            if c.status == Some(ItemStatus::Incomplete) {
                continue;
            }
            if let Some(id) = c.call_id.as_deref() {
                calls.entry(id).or_insert(pos);
            }
        }
    }
    calls
}

/// The arguments a model streamed, as the IR's JSON value: none at all is
/// `{}`; text that is not JSON is kept as a string, as the tool loop's own
/// accumulator does.
fn arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

struct Builder<'a> {
    items: &'a [Item],
    calls: &'a HashMap<&'a str, usize>,
    messages: Vec<Message>,
    /// Calls of the last assistant message still owed their results, in
    /// call order.
    pending: Vec<String>,
    names: HashMap<String, String>,
    /// Output items already rendered as some call's result.
    consumed: HashSet<usize>,
    /// The results of the server-side calls rendered so far (`mcp`).
    mcp: mcp::McpResults,
}

impl<'a> Builder<'a> {
    fn new(items: &'a [Item], calls: &'a HashMap<&'a str, usize>) -> Self {
        Self {
            items,
            calls,
            messages: Vec::new(),
            pending: Vec::new(),
            names: HashMap::new(),
            consumed: HashSet::new(),
            mcp: mcp::McpResults::default(),
        }
    }

    fn user(&mut self, text: String) {
        self.flush();
        match self.messages.last_mut() {
            Some(m) if m.role == Role::User => append_text(m, &text),
            _ => self.messages.push(Message::text(Role::User, text)),
        }
    }

    fn assistant_text(&mut self, text: String) {
        self.flush();
        match self.messages.last_mut() {
            Some(m) if m.role == Role::Assistant && !has_calls(m) => append_text(m, &text),
            _ => self.messages.push(Message::text(Role::Assistant, text)),
        }
    }

    fn call(&mut self, call_id: &str, name: &str, raw_args: &str) {
        let part = ir::ContentPart::ToolUse {
            id: call_id.to_string(),
            name: name.to_string(),
            args: arguments(raw_args),
        };
        // Calls join the assistant message before them: the text the same
        // response spoke first, or the calls made beside them.
        let joins = !self.pending.is_empty()
            || self
                .messages
                .last()
                .is_some_and(|m| m.role == Role::Assistant && !has_calls(m));
        match self.messages.last_mut() {
            Some(m) if joins => m.content.push(part),
            _ => self.messages.push(Message {
                role: Role::Assistant,
                content: vec![part],
            }),
        }
        self.pending.push(call_id.to_string());
        self.names.insert(call_id.to_string(), name.to_string());
    }

    /// Give every pending call its result, in call order: a server-side
    /// call's own, or the first output for it anywhere in the conversation
    /// not yet used, or the synthetic one.
    fn flush(&mut self) {
        for call_id in std::mem::take(&mut self.pending) {
            debug_assert!(self.calls.contains_key(call_id.as_str()) || self.mcp.has(&call_id));
            if let Some((content, is_error)) = self.mcp.take(&call_id) {
                self.messages.push(Message {
                    role: Role::Tool,
                    content: vec![ir::ContentPart::ToolResult {
                        name: self.names.get(&call_id).cloned(),
                        id: call_id,
                        content,
                        is_error,
                    }],
                });
                continue;
            }
            let found = self
                .items
                .iter()
                .enumerate()
                .find_map(|(pos, item)| match item {
                    Item::FunctionCallOutput(o)
                        if o.call_id == call_id && !self.consumed.contains(&pos) =>
                    {
                        Some((pos, o.output.clone()))
                    }
                    _ => None,
                });
            let output = match found {
                Some((pos, output)) => {
                    self.consumed.insert(pos);
                    output
                }
                None => NO_RESULT_YET.to_string(),
            };
            self.messages.push(Message {
                role: Role::Tool,
                content: vec![ir::ContentPart::ToolResult {
                    name: self.names.get(&call_id).cloned(),
                    id: call_id,
                    content: ToolResultBlock::one(output),
                    is_error: false,
                }],
            });
        }
    }

    /// An output with no call to follow: said as the user's, naming the
    /// call, so the model still sees it and the template sees no stray
    /// tool result.
    fn orphan(&mut self, call_id: &str, output: &str) {
        self.user(format!("[result of function call {call_id}]\n{output}"));
    }
}

fn has_calls(m: &Message) -> bool {
    m.content
        .iter()
        .any(|p| matches!(p, ir::ContentPart::ToolUse { .. }))
}

/// Merge `text` into a one-text-part message, one line apart.
fn append_text(m: &mut Message, text: &str) {
    match m.content.iter_mut().find_map(|p| match p {
        ir::ContentPart::Text { text } => Some(text),
        _ => None,
    }) {
        Some(t) => {
            t.push('\n');
            t.push_str(text);
        }
        None => m.content.insert(0, ir::ContentPart::text(text)),
    }
}
