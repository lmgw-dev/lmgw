//! Server-side tool loop (§21) — the engine behind `/v1/responses`.
//!
//! Everything below `proxy.rs` is a **1:1 translator**: one client request
//! becomes one upstream request. This module is the one place that is 1:N. It
//! drives the model, executes the tools the model asks for, feeds the results
//! back, and repeats until the model stops asking — which is precisely what
//! OpenAI's Responses API does server-side, and what a bare
//! `/v1/chat/completions` shim over llama-server structurally cannot do.
//!
//! It sits *above* the proxy rather than inside it so that `/v1/chat/completions`
//! stays the 1:1 path it has always been, and so the loop is testable against a
//! fake [`ToolExecutor`] with no HTTP anywhere.
//!
//! **Protocol-agnostic on purpose.** The loop speaks IR ([`ChatRequest`],
//! [`Completion`]) and knows nothing about the Responses wire format, so it
//! works against `openai`, `anthropic` *and* `gemini` upstreams — a Responses
//! client driving Claude with server-side MCP execution routes through here
//! unchanged — and it can back the Chat tab's tool support and an Admin Chat
//! later without being reshaped.
//!
//! ## Ownership decides where a tool runs
//!
//! Each tool is resolved to either a **server-side** tool (an MCP tool the
//! gateway executes, tagged with the server's label) or a **client** tool (a
//! plain function the caller declared). When the model calls a client tool the
//! loop *stops* and hands the call back: with no stored state, the client
//! replays the conversation with the result appended, exactly as it would
//! against OpenAI.
//!
//! ## Budgets are visible, never guessed
//!
//! A tool loop that can't terminate is a runaway GPU. [`Budget`] bounds it by
//! tool calls and wall clock, both plumbed from request fields and Settings —
//! never a constant compiled in here. Exhausting either ends the run as
//! [`StopReason::Incomplete`] with the reason named, so the client is told the
//! answer was cut short instead of silently receiving a partial one.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use crate::error::GatewayError;
use crate::ir::{
    ChatRequest, Completion, ContentPart, FinishReason, Message, Role, StreamDelta, ToolDef,
    ToolResultBlock, Usage,
};

pub mod approval;
mod cancel;
mod record;
mod relay;
#[cfg(test)]
mod relay_tests;

pub use cancel::{Cancel, CANCEL_POLL};
pub use record::RunError;
pub(crate) use record::{close_trailing_calls, ABANDONED_CALL, UNMADE_CALL};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// A tool the model may call, plus where it runs.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTool {
    pub def: ToolDef,
    /// `Some(label)` — the gateway executes it and the loop continues.
    /// `None` — a client-declared function tool; the loop stops and returns it.
    pub server_label: Option<String>,
    /// The caller asked to approve this tool before it runs (`require_approval`).
    /// The loop stops with [`StopReason::Approval`] instead of calling it.
    pub needs_approval: bool,
}

impl ResolvedTool {
    pub fn server_side(label: impl Into<String>, def: ToolDef) -> Self {
        Self {
            def,
            server_label: Some(label.into()),
            needs_approval: false,
        }
    }

    pub fn client_side(def: ToolDef) -> Self {
        Self {
            def,
            server_label: None,
            needs_approval: false,
        }
    }

    /// Require approval before this tool runs.
    pub fn gated(mut self, yes: bool) -> Self {
        self.needs_approval = yes;
        self
    }
}

/// A tool call the loop stopped on rather than executed.
///
/// Either the caller gated it (`needs_approval`), or it merely shares a turn
/// with one that was gated: a model that asked for three tools expects three
/// results, so executing the ungated ones now and the gated one later would
/// reorder the transcript. Both kinds are handed back together and both are
/// settled together on the next request.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PendingCall {
    /// Stable id the caller quotes to approve or deny this call.
    pub approval_id: String,
    pub call_id: String,
    pub name: String,
    pub args: Value,
    pub server_label: String,
    pub needs_approval: bool,
}

/// The id a caller quotes to decide a gated call.
///
/// The model's own call id is already unique within the response and is what
/// the transcript is keyed on; the ordinal covers upstreams that emit none.
/// **One definition on purpose**: the loop stores this on the
/// [`PendingCall`] and the wire encoder renders it as the approval item's `id`,
/// so the two cannot drift into a mapping the client can never satisfy.
pub fn approval_id(call_id: &str, index: usize) -> String {
    if call_id.is_empty() {
        format!("call_{index}")
    } else {
        call_id.to_string()
    }
}

/// A [`PendingCall`] with the caller's verdict, replayed at the head of the
/// next run so the conversation continues exactly where it stopped.
#[derive(Debug, Clone, PartialEq)]
pub struct DecidedCall {
    pub call: PendingCall,
    pub approved: bool,
    /// Result fed to the model when `approved` is false. The model has to be
    /// *told* the call was refused — leaving the result out would strand it
    /// waiting for an answer that never comes.
    pub denial: String,
    /// Who decided it (client-apps design §6.3): `Some` only for a gated
    /// call a caller gave a verdict on — never for a sibling held beside
    /// one, which nobody approved. An approved call with `by` runs inside
    /// [`approval::scope`], so the call's row names the approver and a
    /// device-hosted server gets `_meta["lmgw/approval"]`.
    pub by: Option<approval::Approver>,
}

/// Hard bounds on one run. Both are supplied by the caller from a request field
/// or a Setting — this module deliberately defines no defaults, so a limit can
/// never be silently invented here (§14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Maximum server-side tool calls across the whole run. Also bounds turns,
    /// since a turn that calls no tool ends the loop.
    pub max_tool_calls: u32,
    /// Wall-clock ceiling for the run, checked between turns.
    pub wall_clock: Duration,
}

/// What ended the run.
#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    /// The model produced a final answer.
    Done(FinishReason),
    /// The model called a tool the gateway does not execute; the pending calls
    /// are in the transcript and the client must run them and come back.
    ClientTool,
    /// The model called a tool the caller gated behind an approval. The calls
    /// are in [`RunResult::pending`]; the run continues once they are decided.
    Approval,
    /// A [`Budget`] was exhausted. The payload is the field that ran out, used
    /// verbatim as the Responses `incomplete_details.reason`.
    Incomplete(&'static str),
}

pub const REASON_MAX_TOOL_CALLS: &str = "max_tool_calls";
pub const REASON_TIME_LIMIT: &str = "time_limit";
/// A [`Cancel`] was raised while the run was inside a model or tool call. Not a
/// budget: nothing ran out, the owner asked it to stop.
pub const REASON_CANCELED: &str = "canceled";

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

/// Outcome of one server-side tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    pub blocks: Vec<ToolResultBlock>,
    pub is_error: bool,
    /// An MCP result's `structuredContent`, kept apart for a client that
    /// renders it — an MCP Apps view (client-apps design §7.3). The model
    /// has it among `blocks` only when the result's `content` was empty
    /// (`mcp::exec::blocks_from_result`).
    pub structured: Option<Value>,
    /// An MCP result's `content` blocks as the server sent them (their
    /// resource URIs namespaced as `/mcp` sends them), for a client that
    /// hands the result to an MCP Apps view (client-apps design §7.5).
    /// `None` for a tool lmgw ran in-process and for a call that never
    /// reached a server: [`Self::mcp_content`] says those in MCP's shape.
    pub content: Option<Vec<Value>>,
}

impl ToolOutcome {
    pub fn ok(blocks: Vec<ToolResultBlock>) -> Self {
        Self {
            blocks,
            is_error: false,
            structured: None,
            content: None,
        }
    }

    /// A failed call. The message goes back to the *model* as the tool result —
    /// a tool erroring is normal control flow it can recover from, not a
    /// gateway failure that should abort the run.
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            blocks: ToolResultBlock::one(msg.into()),
            is_error: true,
            structured: None,
            content: None,
        }
    }

    /// The result's MCP `content` blocks: the server's own when it sent
    /// them ([`Self::content`]), else `blocks` said in MCP's shape.
    pub fn mcp_content(&self) -> Vec<Value> {
        match &self.content {
            Some(c) => c.clone(),
            None => crate::mcp::exec::content::of_blocks(&self.blocks),
        }
    }
}

/// Runs the tools the loop owns. Split from the loop so tests drive it with a
/// fake and so the MCP wiring (and later an Admin plane) stays out of here.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn call(&self, name: &str, args: &Value) -> ToolOutcome;
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// One observable step of a run, in emission order.
///
/// The loop reports *what happened*, not how any protocol renders it — the
/// Responses encoder turns these into `response.*` SSE events, the eventual
/// Chat tab into DOM updates.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopEvent {
    /// A model turn began. `turn` is 0-based.
    TurnStarted { turn: usize },
    /// Answer text from the model.
    Text(String),
    /// Reasoning trace, kept apart from the answer.
    Reasoning(String),
    /// A tool call started streaming. `index` is a run-global ordinal, not a
    /// per-turn one, so it can index the Responses `output` array directly.
    CallStarted {
        index: usize,
        call_id: String,
        name: String,
        /// `Some(label)` when the gateway will run it.
        server_label: Option<String>,
        /// The call is gated behind an approval and will not run in this pass.
        /// Known at the moment the call starts (it follows from the tool's
        /// name), so a consumer can render it as an approval request from the
        /// first frame rather than rewriting an already-emitted tool item.
        needs_approval: bool,
    },
    /// A fragment of call `index`'s JSON arguments.
    CallArgs { index: usize, fragment: String },
    /// Call `index`'s arguments are complete.
    CallReady {
        index: usize,
        call_id: String,
        name: String,
        args: Value,
        server_label: Option<String>,
        needs_approval: bool,
    },
    /// A server-side call finished. Never emitted for client tools, so the
    /// label is always known — it is carried here rather than remembered from
    /// the matching `CallStarted`, so a consumer can render the result item
    /// without holding per-call state.
    CallResult {
        index: usize,
        call_id: String,
        name: String,
        server_label: String,
        blocks: Vec<ToolResultBlock>,
        is_error: bool,
        ms: u64,
        /// The result's `structuredContent` ([`ToolOutcome::structured`]).
        structured: Option<Value>,
        /// The result's MCP `content` blocks ([`ToolOutcome::mcp_content`]).
        content: Vec<Value>,
    },
    /// Terminal event; exactly one is emitted per run.
    Done { reason: StopReason, usage: Usage },
}

/// Sink for [`LoopEvent`]s. Returning `false` aborts the run — that is how a
/// disconnected client stops the loop (and any further tool calls) instead of
/// leaving it burning GPU on a response nobody will read.
#[async_trait]
pub trait EventSink: Send {
    async fn emit(&mut self, ev: LoopEvent) -> bool;
}

/// Collects every event; the non-streaming path and the tests use it.
#[derive(Debug, Default)]
pub struct CollectSink(pub Vec<LoopEvent>);

#[async_trait]
impl EventSink for CollectSink {
    async fn emit(&mut self, ev: LoopEvent) -> bool {
        self.0.push(ev);
        true
    }
}

// ---------------------------------------------------------------------------
// Turn accumulation
// ---------------------------------------------------------------------------

/// Assembles one model turn out of IR stream deltas.
///
/// The loop needs the *complete* assistant message to append to the
/// conversation before it can run the tools, so even on the streaming path
/// every delta is folded up here as it passes through to the client.
#[derive(Debug, Default)]
pub struct TurnAccumulator {
    text: String,
    reasoning: String,
    calls: Vec<PartialCall>,
    /// IR tool-call ordinal → slot in `calls` (upstreams need not start at 0).
    slots: HashMap<usize, usize>,
    finish: Option<FinishReason>,
    usage: Usage,
}

#[derive(Debug, Clone, Default)]
struct PartialCall {
    id: String,
    name: String,
    args_json: String,
}

impl TurnAccumulator {
    /// Usage folded so far. Readable before [`finish`](Self::finish) consumes
    /// the accumulator, so a turn that failed mid-stream can still be logged
    /// with the tokens it did spend.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    pub fn on_delta(&mut self, d: &StreamDelta) {
        match d {
            StreamDelta::TextDelta(t) => self.text.push_str(t),
            StreamDelta::ReasoningDelta(r) => self.reasoning.push_str(r),
            StreamDelta::ToolCallStart { index, id, name } => {
                let slot = self.calls.len();
                self.slots.insert(*index, slot);
                self.calls.push(PartialCall {
                    id: id.clone(),
                    name: name.clone(),
                    args_json: String::new(),
                });
            }
            StreamDelta::ToolCallArgsDelta { index, fragment } => {
                if let Some(slot) = self.slots.get(index) {
                    self.calls[*slot].args_json.push_str(fragment);
                }
            }
            StreamDelta::Usage(u) => self.usage.merge(u),
            StreamDelta::Stop(r) => self.finish = Some(r.clone()),
            StreamDelta::Timings(_) | StreamDelta::Error(_) => {}
        }
    }

    /// Fold into a [`Completion`], parsing each call's accumulated argument
    /// JSON. Unparseable arguments become a JSON string rather than an error:
    /// the model, not the gateway, produced them, and the tool's own validation
    /// gives a far better message than a 500 here would.
    pub fn finish(self, model: String) -> Completion {
        let mut content: Vec<ContentPart> = Vec::new();
        if !self.text.is_empty() {
            content.push(ContentPart::text(self.text));
        }
        for c in self.calls {
            let args =
                serde_json::from_str(&c.args_json).unwrap_or(Value::String(c.args_json.clone()));
            content.push(ContentPart::ToolUse {
                id: c.id,
                name: c.name,
                args,
            });
        }
        let saw_tool = content
            .iter()
            .any(|p| matches!(p, ContentPart::ToolUse { .. }));
        Completion {
            content,
            reasoning: self.reasoning,
            finish_reason: self.finish.unwrap_or(if saw_tool {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            }),
            usage: self.usage,
            model,
            // A tool-loop turn is assembled from one upstream stream, but the
            // loop's *caller* sees the aggregate of several; attaching one
            // turn's timings to it would report the last turn's throughput as
            // the run's. The per-turn rows in Logs carry the real numbers.
            timings: None,
        }
    }
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// Receives one turn's raw IR deltas as the upstream produces them.
///
/// A named trait rather than `dyn FnMut(&StreamDelta)`: under `async_trait` the
/// closure form loses its higher-ranked lifetime and every caller is forced to
/// keep each delta alive for the whole call.
pub trait DeltaSink: Send {
    fn on_delta(&mut self, d: &StreamDelta);

    /// The consumer's cooperative stop ([`crate::proxy::StopSignal`], realtime
    /// design §4.3), if it has one: once raised, a streamed in-process call
    /// ends at its next await and still writes its row. `None`, the default,
    /// never stops a call.
    fn stop(&self) -> Option<crate::proxy::StopSignal> {
        None
    }

    /// Close whatever the sink holds open, as a tool call or the end of the
    /// stream would (chat-voice design §7.5): a speaking sink says its
    /// clause in progress now, so a preamble is heard before the tool runs.
    /// Nothing, by default.
    ///
    /// Call it only at a tool call or a turn boundary. A speaking sink resets
    /// its whole reading state here (code fence, list numbering, cues), so a
    /// flush inside an open code block would make the rest of it speakable.
    fn flush(&mut self) {}

    /// What the call's request row records it used, said once the row is
    /// written — on every path a streamed in-process call writes one,
    /// stopped and failed calls included (`proxy::stream_once_on`). A meter
    /// tallies it (`agents::batch`'s, WP11 server review n4). Nothing, by
    /// default.
    fn billed(&mut self, _usage: &crate::ir::Usage) {}

    /// What the same row adds to a total, said right after [`Self::billed`]:
    /// its cost, priced on the route that answered — a re-routed call's own,
    /// not the caller's (billable-units design §7) — or, unpriced, a gap
    /// where it did work ([`crate::store::NewRequestLog::row_cost`]). A run's
    /// meter sums it ([`crate::pricing::CostTotal`]). Nothing, by default.
    fn billed_cost(&mut self, _row: crate::pricing::RowCost) {}
}

impl<F: FnMut(&StreamDelta) + Send> DeltaSink for F {
    fn on_delta(&mut self, d: &StreamDelta) {
        self(d)
    }
}

/// Runs one model turn against the configured upstream. Implemented by the
/// caller (`proxy.rs` in production, a canned script in tests) so this module
/// carries no HTTP and no logging.
///
/// The `sink` receives the turn's raw IR deltas as they arrive; a non-streaming
/// implementation may simply not call it.
#[async_trait]
pub trait TurnRunner: Send + Sync {
    async fn run_turn(
        &self,
        ir: &ChatRequest,
        deadline: Duration,
        sink: &mut dyn DeltaSink,
    ) -> Result<Completion, GatewayError>;
}

/// Everything one run needs beyond the conversation itself.
pub struct RunConfig {
    pub tools: Vec<ResolvedTool>,
    pub budget: Budget,
    /// Run a turn's tool calls concurrently. The OpenAI request field of the
    /// same name; honored rather than ignored, because a client sets it false
    /// precisely when its tools are order-dependent or share a resource.
    pub parallel_tool_calls: bool,
    /// Calls left pending by an earlier run, now decided. Settled *before* the
    /// first model turn: `ir.messages` already ends with the assistant turn
    /// that made them, so the model must receive their results before it is
    /// asked anything else.
    pub resume: Vec<DecidedCall>,
    /// Stops the run from outside, mid-await. [`Cancel::none`] by default,
    /// which is a run only its own budget and its sink can end.
    pub cancel: Cancel,
}

impl RunConfig {
    /// The common case: no run to resume.
    pub fn new(tools: Vec<ResolvedTool>, budget: Budget, parallel_tool_calls: bool) -> Self {
        Self {
            tools,
            budget,
            parallel_tool_calls,
            resume: Vec::new(),
            cancel: Cancel::none(),
        }
    }

    /// Let `cancel` abandon this run's model and tool calls where they stand.
    pub fn with_cancel(mut self, cancel: Cancel) -> Self {
        self.cancel = cancel;
        self
    }
}

/// The finished run.
pub struct RunResult {
    pub reason: StopReason,
    pub usage: Usage,
    /// The conversation as it ended, including every tool exchange the loop
    /// performed. Callers that persist state store this.
    pub messages: Vec<Message>,
    /// Calls the run stopped on, when `reason` is [`StopReason::Approval`].
    /// Persist these with the conversation; they are what a resume replays.
    pub pending: Vec<PendingCall>,
    /// True if the sink asked to stop (client disconnected).
    pub aborted: bool,
}

/// Drive the model until it stops calling tools we own, a budget runs out, or
/// the client goes away. `Err` when a model turn failed; it keeps the record
/// of the turns before ([`RunError`]).
pub async fn run(
    mut ir: ChatRequest,
    cfg: RunConfig,
    runner: &dyn TurnRunner,
    exec: &dyn ToolExecutor,
    sink: &mut dyn EventSink,
) -> Result<RunResult, RunError> {
    let started = Instant::now();
    let owners = relay::Owners::new(&cfg.tools);
    let label_of = |name: &str| owners.label_of(name);
    let gated = |name: &str| owners.gated(name);

    ir.tools = cfg.tools.iter().map(|t| t.def.clone()).collect();
    // The loop consumes the turn boundaries itself; a client asking for a
    // stream is served by `sink`, not by the IR flag.
    ir.stream = false;

    let mut usage = Usage::default();
    let mut calls_made: u32 = 0;
    let mut next_index: usize = 0;
    let mut turn: usize = 0;

    /// The sink gave up: the client went away.
    macro_rules! gone {
        () => {
            return Ok(RunResult {
                reason: StopReason::Incomplete("client_disconnected"),
                usage,
                messages: ir.messages,
                pending: Vec::new(),
                aborted: true,
            })
        };
    }

    macro_rules! emit {
        ($ev:expr) => {
            if !sink.emit($ev).await {
                gone!()
            }
        };
    }

    /// The await it guards was abandoned. The terminal event still goes out —
    /// exactly one per run, whatever ended it — but its verdict is ignored:
    /// a sink whose owner just cancelled is about to say "stop" anyway, and
    /// the run is already stopping.
    ///
    /// `$closed` are the calls the cancel cut off, each with what its result
    /// says ([`record::Closed`]). They are reported before the terminal event
    /// rather than not at all: an apply that was writing labels when Cancel
    /// was pressed has to say which calls it had sent, because "no record"
    /// reads as "nothing happened" (§4.1). The record gets the same results,
    /// so every call in it still has its result and the conversation stays
    /// replayable (chat-voice design §7.3).
    macro_rules! canceled {
        ($closed:expr) => {{
            let closed: Vec<record::Closed> = $closed;
            if let Some(results) = record::results(&closed) {
                ir.messages.push(results);
            }
            for c in &closed {
                sink.emit(c.event()).await;
            }
            sink.emit(LoopEvent::Done {
                reason: StopReason::Incomplete(REASON_CANCELED),
                usage,
            })
            .await;
            return Ok(RunResult {
                reason: StopReason::Incomplete(REASON_CANCELED),
                usage,
                messages: ir.messages,
                pending: Vec::new(),
                aborted: true,
            });
        }};
    }

    // Settle the previous run's pending calls first. `ir.messages` ends with the
    // assistant turn that made them, so their results must be the very next
    // thing the model sees — before any new generation.
    if !cfg.resume.is_empty() {
        let approved: Vec<(String, String, Value)> = cfg
            .resume
            .iter()
            .filter(|d| d.approved)
            .map(|d| {
                (
                    d.call.call_id.clone(),
                    d.call.name.clone(),
                    d.call.args.clone(),
                )
            })
            .collect();
        // Who approved each of them, in the same order: a sibling held
        // beside a gated call ran because its turn was decided, not because
        // anyone approved it, and carries nobody.
        let approvers: Vec<Option<approval::Approver>> = cfg
            .resume
            .iter()
            .filter(|d| d.approved)
            .map(|d| d.by.clone())
            .collect();
        for (i, d) in cfg.resume.iter().enumerate() {
            emit!(LoopEvent::CallReady {
                index: i,
                call_id: d.call.call_id.clone(),
                name: d.call.name.clone(),
                args: d.call.args.clone(),
                server_label: Some(d.call.server_label.clone()),
                needs_approval: false,
            });
        }
        // The resume as a cancel leaves it: one result per entry, in the
        // model's order and at the index its `CallReady` had — each denial
        // kept, each approved call saying `says`.
        let cut = |says: &str| -> Vec<record::Closed> {
            cfg.resume
                .iter()
                .enumerate()
                .map(|(i, d)| record::Closed {
                    index: i,
                    call_id: d.call.call_id.clone(),
                    name: d.call.name.clone(),
                    server_label: d.call.server_label.clone(),
                    says: if d.approved {
                        says.to_string()
                    } else {
                        d.denial.clone()
                    },
                })
                .collect()
        };
        if cfg.cancel.is_raised() {
            canceled!(cut(UNMADE_CALL))
        }
        let Some(outcomes) = cfg
            .cancel
            .guard(execute_approved(
                exec,
                &approved,
                &approvers,
                cfg.parallel_tool_calls,
            ))
            .await
        else {
            canceled!(cut(ABANDONED_CALL))
        };
        let mut outcomes = outcomes.into_iter();

        let mut results: Vec<ContentPart> = Vec::with_capacity(cfg.resume.len());
        let mut reports = Vec::with_capacity(cfg.resume.len());
        for (i, d) in cfg.resume.iter().enumerate() {
            let (outcome, ms) = if d.approved {
                outcomes
                    .next()
                    .unwrap_or((ToolOutcome::error("not executed"), 0))
            } else {
                (ToolOutcome::error(d.denial.clone()), 0)
            };
            reports.push(LoopEvent::CallResult {
                index: i,
                call_id: d.call.call_id.clone(),
                name: d.call.name.clone(),
                server_label: d.call.server_label.clone(),
                blocks: outcome.blocks.clone(),
                is_error: outcome.is_error,
                ms,
                structured: outcome.structured.clone(),
                content: outcome.mcp_content(),
            });
            results.push(ContentPart::ToolResult {
                id: d.call.call_id.clone(),
                name: Some(d.call.name.clone()),
                content: outcome.blocks,
                is_error: outcome.is_error,
            });
        }
        calls_made += approved.len() as u32;
        next_index += cfg.resume.len();
        // Recorded before they are reported: a client gone mid-report leaves
        // a record in which every call has its result.
        ir.messages.push(Message {
            role: Role::Tool,
            content: results,
        });
        for ev in reports {
            emit!(ev);
        }
    }

    loop {
        // Budget check *before* the turn: a run that has already spent its time
        // must not start another (possibly minutes-long) generation.
        let elapsed = started.elapsed();
        if elapsed >= cfg.budget.wall_clock {
            emit!(LoopEvent::Done {
                reason: StopReason::Incomplete(REASON_TIME_LIMIT),
                usage,
            });
            return Ok(RunResult {
                reason: StopReason::Incomplete(REASON_TIME_LIMIT),
                usage,
                messages: ir.messages,
                pending: Vec::new(),
                aborted: false,
            });
        }
        let remaining = cfg.budget.wall_clock - elapsed;

        emit!(LoopEvent::TurnStarted { turn });

        // Stream the turn through as it arrives, folding it up as it goes so
        // the loop has the finished assistant message and the client has the
        // tokens live ([`relay`]).
        let end = relay::turn(
            runner,
            &ir,
            remaining,
            &cfg.cancel,
            &mut *sink,
            &owners,
            next_index,
        )
        .await;
        let (completion, acc, streamed) = match end {
            relay::TurnEnd::Returned(r) => {
                let relay::Returned { out, acc, streamed } = *r;
                match out {
                    Ok(completion) => (completion, acc, streamed),
                    // The turns before keep their record (`RunError`).
                    Err(error) => {
                        return Err(RunError {
                            error,
                            messages: ir.messages,
                            usage,
                        })
                    }
                }
            }
            // No calls of the cancelled turn are in the record.
            relay::TurnEnd::Canceled => canceled!(Vec::new()),
            relay::TurnEnd::SinkGone => gone!(),
        };
        // A unary runner never touches the sink, in which case the completion
        // *is* the turn; a streaming one has already folded everything up.
        let completion = if streamed {
            acc.finish(completion.model.clone())
        } else {
            completion
        };
        // Unary turns produced no deltas — surface their content now.
        if !streamed {
            if !completion.reasoning.is_empty() {
                emit!(LoopEvent::Reasoning(completion.reasoning.clone()));
            }
            for p in &completion.content {
                if let ContentPart::Text { text } = p {
                    if !text.is_empty() {
                        emit!(LoopEvent::Text(text.clone()));
                    }
                }
            }
        }

        // `add`, not `merge`: these are separate upstream calls, and each turn's
        // reported usage is its own total.
        usage.add(&completion.usage);
        turn += 1;

        let calls: Vec<(String, String, Value)> = completion
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolUse { id, name, args } => {
                    Some((id.clone(), name.clone(), args.clone()))
                }
                _ => None,
            })
            .collect();

        // Record the assistant turn before doing anything with its tool calls,
        // so the conversation stays well-formed on every exit path below. Its
        // reasoning leads it: a thinking model that called a tool expects to
        // see what it was thinking when the result comes back, and the
        // template (`--reasoning-preserve`) renders that only if the next
        // request carries it.
        let mut turn = Vec::with_capacity(completion.content.len() + 1);
        if !completion.reasoning.is_empty() {
            turn.push(ContentPart::reasoning(completion.reasoning.clone()));
        }
        turn.extend(completion.content.iter().cloned());
        ir.messages.push(Message {
            role: Role::Assistant,
            content: turn,
        });

        if calls.is_empty() {
            let reason = StopReason::Done(completion.finish_reason.clone());
            emit!(LoopEvent::Done {
                reason: reason.clone(),
                usage,
            });
            return Ok(RunResult {
                reason,
                usage,
                messages: ir.messages,
                pending: Vec::new(),
                aborted: false,
            });
        }

        // Announce every call's completed arguments, then classify.
        for (i, (id, name, args)) in calls.iter().enumerate() {
            emit!(LoopEvent::CallReady {
                index: next_index + i,
                call_id: id.clone(),
                name: name.clone(),
                args: args.clone(),
                server_label: label_of(name),
                needs_approval: gated(name),
            });
        }

        // One unowned call hands the whole turn back: the model expects a
        // result for *every* call it made, and we cannot answer the ones we
        // don't own, so a split execution would leave the conversation
        // malformed. The client runs them all and replays.
        let any_client_tool = calls.iter().any(|(_, name, _)| label_of(name).is_none());
        if any_client_tool {
            emit!(LoopEvent::Done {
                reason: StopReason::ClientTool,
                usage,
            });
            return Ok(RunResult {
                reason: StopReason::ClientTool,
                usage,
                messages: ir.messages,
                pending: Vec::new(),
                aborted: false,
            });
        }

        // Same rule, one step stronger: a gated call suspends its whole turn.
        // The ungated calls beside it are held too and settled together on
        // resume, for exactly the reason above — the model asked for N results
        // and must get them in one block, in the order it asked.
        if calls.iter().any(|(_, name, _)| gated(name)) {
            let pending: Vec<PendingCall> = calls
                .iter()
                .enumerate()
                .map(|(i, (id, name, args))| PendingCall {
                    approval_id: approval_id(id, next_index + i),
                    call_id: id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                    server_label: label_of(name).unwrap_or_default(),
                    needs_approval: gated(name),
                })
                .collect();
            emit!(LoopEvent::Done {
                reason: StopReason::Approval,
                usage,
            });
            return Ok(RunResult {
                reason: StopReason::Approval,
                usage,
                messages: ir.messages,
                pending,
                aborted: false,
            });
        }

        if calls_made + calls.len() as u32 > cfg.budget.max_tool_calls {
            emit!(LoopEvent::Done {
                reason: StopReason::Incomplete(REASON_MAX_TOOL_CALLS),
                usage,
            });
            return Ok(RunResult {
                reason: StopReason::Incomplete(REASON_MAX_TOOL_CALLS),
                usage,
                messages: ir.messages,
                pending: Vec::new(),
                aborted: false,
            });
        }

        // A cancel that is already raised made none of the calls; one that
        // lands while they run abandons them in flight.
        if cfg.cancel.is_raised() {
            canceled!(record::Closed::all(
                &calls,
                next_index,
                UNMADE_CALL,
                label_of
            ))
        }
        let Some(outcomes) = cfg
            .cancel
            .guard(execute(exec, &calls, cfg.parallel_tool_calls))
            .await
        else {
            canceled!(record::Closed::all(
                &calls,
                next_index,
                ABANDONED_CALL,
                label_of
            ))
        };

        let mut results: Vec<ContentPart> = Vec::with_capacity(calls.len());
        let mut reports = Vec::with_capacity(calls.len());
        for (i, ((id, name, _), (outcome, ms))) in calls.iter().zip(outcomes).enumerate() {
            reports.push(LoopEvent::CallResult {
                index: next_index + i,
                call_id: id.clone(),
                name: name.clone(),
                server_label: label_of(name).unwrap_or_default(),
                blocks: outcome.blocks.clone(),
                is_error: outcome.is_error,
                ms,
                structured: outcome.structured.clone(),
                content: outcome.mcp_content(),
            });
            results.push(ContentPart::ToolResult {
                id: id.clone(),
                name: Some(name.clone()),
                content: outcome.blocks,
                is_error: outcome.is_error,
            });
        }

        calls_made += calls.len() as u32;
        next_index += calls.len();
        // Recorded before they are reported: a client gone mid-report leaves
        // a record in which every call has its result.
        ir.messages.push(Message {
            role: Role::Tool,
            content: results,
        });
        for ev in reports {
            emit!(ev);
        }
    }
}

/// [`execute`] for a resume's approved calls: each runs inside
/// [`approval::scope`] with the approver it has (none for a sibling, nor
/// past the end of `approvers`).
async fn execute_approved(
    exec: &dyn ToolExecutor,
    calls: &[(String, String, Value)],
    approvers: &[Option<approval::Approver>],
    parallel: bool,
) -> Vec<(ToolOutcome, u64)> {
    // Each with its call id too, as [`execute`] runs one: an approved call
    // that becomes an MCP task records it (`_meta` then carries both
    // `lmgw/approval` and `lmgw/task`).
    let run_one = |id: &String, name: &String, args: &Value, by: Option<approval::Approver>| {
        let (id, name, args) = (id.clone(), name.clone(), args.clone());
        approval::scope(by, async move {
            let t0 = Instant::now();
            let outcome = with_call_id(id, exec.call(&name, &args)).await;
            (outcome, t0.elapsed().as_millis() as u64)
        })
    };
    let by = |i: usize| approvers.get(i).cloned().flatten();
    if parallel {
        futures::future::join_all(
            calls
                .iter()
                .enumerate()
                .map(|(i, (id, n, a))| run_one(id, n, a, by(i))),
        )
        .await
    } else {
        let mut seq = Vec::with_capacity(calls.len());
        for (i, (id, n, a)) in calls.iter().enumerate() {
            seq.push(run_one(id, n, a, by(i)).await);
        }
        seq
    }
}

/// Run a batch of `(call_id, name, args)` through the executor, returning each
/// outcome with its latency **in call order**.
///
/// Concurrent when the caller allows it — a turn asking for three independent
/// lookups should cost one lookup's latency — but the results are always
/// ordered as the model asked, so the transcript is deterministic.
///
/// Each call runs inside [`approval::scope`] with nobody as its approver,
/// whatever scope the run itself sits in: only a resume's approved calls
/// ([`execute_approved`]) carry one, and a run started inside another
/// call's scope (an agent a tool runs) cannot inherit it.
async fn execute(
    exec: &dyn ToolExecutor,
    calls: &[(String, String, Value)],
    parallel: bool,
) -> Vec<(ToolOutcome, u64)> {
    execute_approved(exec, calls, &[], parallel).await
}

tokio::task_local! {
    /// The id of the call an executor runs, inside [`execute`].
    static CALL_ID: String;
}

/// The model's id of the tool call the executor is running now, when it was
/// called from a run's loop: what an MCP task the call starts records as
/// the call that started it (MCP Tasks design §2.1).
pub fn current_call_id() -> Option<String> {
    CALL_ID.try_with(Clone::clone).ok()
}

/// Run `call`, an executor's call of the model's tool call `id`, with `id`
/// as its [`current_call_id`]: every loop that runs a model's calls (this
/// module's, realtime's responder) wraps each call in it.
pub async fn with_call_id<F: std::future::Future>(id: String, call: F) -> F::Output {
    CALL_ID.scope(id, call).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: None,
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn budget() -> Budget {
        Budget {
            max_tool_calls: 10,
            wall_clock: Duration::from_secs(60),
        }
    }

    fn user(text: &str) -> ChatRequest {
        ChatRequest {
            model_alias: "alias".into(),
            messages: vec![Message::text(Role::User, text)],
            params: Default::default(),
            tools: Vec::new(),
            tool_choice: None,
            stream: false,
            passthrough: Default::default(),
            llama_kwargs_enabled: None,
            anthropic_beta: Vec::new(),
        }
    }

    /// Replays a canned list of turns; records the IR it was handed each time.
    struct ScriptedRunner {
        turns: Mutex<Vec<Completion>>,
        seen: Mutex<Vec<ChatRequest>>,
        /// Emit each turn as deltas rather than returning it whole.
        streaming: bool,
    }

    impl ScriptedRunner {
        fn new(turns: Vec<Completion>, streaming: bool) -> Self {
            Self {
                turns: Mutex::new(turns),
                seen: Mutex::new(Vec::new()),
                streaming,
            }
        }
    }

    #[async_trait]
    impl TurnRunner for ScriptedRunner {
        async fn run_turn(
            &self,
            ir: &ChatRequest,
            _deadline: Duration,
            sink: &mut dyn DeltaSink,
        ) -> Result<Completion, GatewayError> {
            self.seen.lock().unwrap().push(ir.clone());
            let mut turns = self.turns.lock().unwrap();
            if turns.is_empty() {
                return Err(GatewayError::Internal("scripted runner exhausted".into()));
            }
            let c = turns.remove(0);
            if self.streaming {
                let mut idx = 0;
                for p in &c.content {
                    match p {
                        ContentPart::Text { text } => {
                            for ch in text.split_inclusive(' ') {
                                sink.on_delta(&StreamDelta::TextDelta(ch.to_string()));
                            }
                        }
                        ContentPart::ToolUse { id, name, args } => {
                            sink.on_delta(&StreamDelta::ToolCallStart {
                                index: idx,
                                id: id.clone(),
                                name: name.clone(),
                            });
                            sink.on_delta(&StreamDelta::ToolCallArgsDelta {
                                index: idx,
                                fragment: args.to_string(),
                            });
                            idx += 1;
                        }
                        _ => {}
                    }
                }
                sink.on_delta(&StreamDelta::Usage(c.usage));
                sink.on_delta(&StreamDelta::Stop(c.finish_reason.clone()));
            }
            Ok(c)
        }
    }

    struct EchoExec;

    #[async_trait]
    impl ToolExecutor for EchoExec {
        async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
            ToolOutcome::ok(ToolResultBlock::one(format!("{name}:{args}")))
        }
    }

    fn text_turn(s: &str) -> Completion {
        Completion {
            content: vec![ContentPart::text(s)],
            reasoning: String::new(),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                ..Default::default()
            },
            model: "m".into(),
            timings: None,
        }
    }

    fn call_turn(calls: &[(&str, &str)]) -> Completion {
        Completion {
            content: calls
                .iter()
                .map(|(id, name)| ContentPart::ToolUse {
                    id: (*id).into(),
                    name: (*name).into(),
                    args: json!({"q": 1}),
                })
                .collect(),
            reasoning: String::new(),
            finish_reason: FinishReason::ToolUse,
            usage: Usage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                ..Default::default()
            },
            model: "m".into(),
            timings: None,
        }
    }

    async fn run_with(
        turns: Vec<Completion>,
        tools: Vec<ResolvedTool>,
        budget: Budget,
        streaming: bool,
    ) -> (RunResult, Vec<LoopEvent>, Vec<ChatRequest>) {
        let runner = ScriptedRunner::new(turns, streaming);
        let mut sink = CollectSink::default();
        let res = run(
            user("go"),
            RunConfig::new(tools, budget, true),
            &runner,
            &EchoExec,
            &mut sink,
        )
        .await
        .unwrap();
        let seen = runner.seen.lock().unwrap().clone();
        (res, sink.0, seen)
    }

    #[tokio::test]
    async fn a_turn_with_no_tool_calls_ends_the_run() {
        let (res, events, seen) =
            run_with(vec![text_turn("hi")], Vec::new(), budget(), false).await;
        assert_eq!(res.reason, StopReason::Done(FinishReason::Stop));
        assert_eq!(seen.len(), 1);
        assert!(events.contains(&LoopEvent::Text("hi".into())));
        assert_eq!(res.usage.completion_tokens, Some(5));
    }

    #[tokio::test]
    async fn a_turns_reasoning_is_replayed_on_the_next_turn() {
        let mut call = call_turn(&[("c1", "search")]);
        call.reasoning = "need to search first".into();
        let (_res, _events, seen) = run_with(
            vec![call, text_turn("found it")],
            vec![ResolvedTool::server_side("web", tool("search"))],
            budget(),
            false,
        )
        .await;
        assert_eq!(seen.len(), 2);
        let asst = seen[1]
            .messages
            .iter()
            .find(|m| m.role == Role::Assistant)
            .expect("the tool-calling turn is replayed");
        // The trace leads the turn, then the call it led to.
        assert_eq!(
            asst.content[0],
            ContentPart::reasoning("need to search first")
        );
        assert!(matches!(asst.content[1], ContentPart::ToolUse { .. }));

        // A turn that had no trace does not grow an empty part.
        let (_res, _events, seen) = run_with(
            vec![call_turn(&[("c1", "search")]), text_turn("found it")],
            vec![ResolvedTool::server_side("web", tool("search"))],
            budget(),
            false,
        )
        .await;
        let asst = seen[1]
            .messages
            .iter()
            .find(|m| m.role == Role::Assistant)
            .unwrap();
        assert!(matches!(asst.content[0], ContentPart::ToolUse { .. }));
    }

    #[tokio::test]
    async fn a_server_side_call_is_executed_and_fed_back() {
        let (res, events, seen) = run_with(
            vec![call_turn(&[("c1", "search")]), text_turn("found it")],
            vec![ResolvedTool::server_side("web", tool("search"))],
            budget(),
            false,
        )
        .await;
        assert_eq!(res.reason, StopReason::Done(FinishReason::Stop));
        // Two upstream turns; the second saw the tool result appended.
        assert_eq!(seen.len(), 2);
        let tool_msg = seen[1]
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("tool result fed back to the model");
        assert!(matches!(
            &tool_msg.content[0],
            ContentPart::ToolResult { content, .. }
                if content == &ToolResultBlock::one(r#"search:{"q":1}"#)
        ));
        assert!(events.iter().any(|e| matches!(
            e,
            LoopEvent::CallResult { name, is_error: false, .. } if name == "search"
        )));
    }

    /// Usage must be the sum over every turn, not just the last one — a caller
    /// billing or budgeting on this would otherwise undercount an agentic run
    /// by however many tool round trips it took.
    #[tokio::test]
    async fn usage_accumulates_across_turns() {
        let (res, _, _) = run_with(
            vec![call_turn(&[("c1", "search")]), text_turn("done")],
            vec![ResolvedTool::server_side("web", tool("search"))],
            budget(),
            false,
        )
        .await;
        assert_eq!(res.usage.completion_tokens, Some(10));
        assert_eq!(res.usage.prompt_tokens, Some(20));
    }

    #[tokio::test]
    async fn an_unowned_tool_stops_the_run_for_the_client_to_handle() {
        let (res, events, seen) = run_with(
            vec![call_turn(&[("c1", "local_fs")])],
            vec![ResolvedTool::client_side(tool("local_fs"))],
            budget(),
            false,
        )
        .await;
        assert_eq!(res.reason, StopReason::ClientTool);
        assert_eq!(seen.len(), 1, "must not run a second turn");
        assert!(events.iter().any(|e| matches!(
            e,
            LoopEvent::CallReady { name, server_label: None, .. } if name == "local_fs"
        )));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, LoopEvent::CallResult { .. })),
            "the gateway must not execute a client tool"
        );
    }

    /// A turn mixing an owned and an unowned call must hand back *both*: the
    /// model needs a result for every call it made, so executing half would
    /// leave the replayed conversation malformed.
    #[tokio::test]
    async fn a_mixed_turn_hands_back_every_call() {
        let (res, events, _) = run_with(
            vec![call_turn(&[("c1", "search"), ("c2", "local_fs")])],
            vec![
                ResolvedTool::server_side("web", tool("search")),
                ResolvedTool::client_side(tool("local_fs")),
            ],
            budget(),
            false,
        )
        .await;
        assert_eq!(res.reason, StopReason::ClientTool);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, LoopEvent::CallResult { .. })),
            "no call may run when one of the turn's calls is the client's"
        );
    }

    #[tokio::test]
    async fn the_tool_call_budget_ends_the_run_as_incomplete() {
        let (res, events, seen) = run_with(
            vec![
                call_turn(&[("c1", "search")]),
                call_turn(&[("c2", "search")]),
                call_turn(&[("c3", "search")]),
            ],
            vec![ResolvedTool::server_side("web", tool("search"))],
            Budget {
                max_tool_calls: 2,
                wall_clock: Duration::from_secs(60),
            },
            false,
        )
        .await;
        assert_eq!(res.reason, StopReason::Incomplete(REASON_MAX_TOOL_CALLS));
        assert_eq!(seen.len(), 3);
        assert!(events
            .iter()
            .any(|e| matches!(e, LoopEvent::Done { reason, .. } if *reason
                == StopReason::Incomplete(REASON_MAX_TOOL_CALLS))));
    }

    #[tokio::test]
    async fn an_expired_wall_clock_ends_the_run_before_the_next_turn() {
        let (res, _, seen) = run_with(
            vec![call_turn(&[("c1", "search")]), text_turn("late")],
            vec![ResolvedTool::server_side("web", tool("search"))],
            Budget {
                max_tool_calls: 10,
                wall_clock: Duration::ZERO,
            },
            false,
        )
        .await;
        assert_eq!(res.reason, StopReason::Incomplete(REASON_TIME_LIMIT));
        assert!(seen.is_empty(), "must not start a turn it cannot afford");
    }

    /// The streaming path must fold the same conversation as the unary one —
    /// this is what keeps a streamed `/v1/responses` from diverging from a
    /// non-streamed one.
    #[tokio::test]
    async fn streamed_turns_assemble_the_same_conversation() {
        let (res, events, seen) = run_with(
            vec![call_turn(&[("c1", "search")]), text_turn("found it")],
            vec![ResolvedTool::server_side("web", tool("search"))],
            budget(),
            true,
        )
        .await;
        assert_eq!(res.reason, StopReason::Done(FinishReason::Stop));
        assert_eq!(seen.len(), 2);
        // Arguments streamed as fragments still reach the executor parsed.
        assert!(events.iter().any(|e| matches!(
            e,
            LoopEvent::CallReady { args, .. } if args == &json!({"q": 1})
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            LoopEvent::CallResult { blocks, .. }
                if blocks == &ToolResultBlock::one(r#"search:{"q":1}"#)
        )));
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                LoopEvent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "found it");
    }

    /// Call ordinals index the Responses `output` array, so they must keep
    /// counting across turns rather than restarting at 0 each time.
    #[tokio::test]
    async fn call_indices_are_run_global() {
        let (_, events, _) = run_with(
            vec![
                call_turn(&[("c1", "search")]),
                call_turn(&[("c2", "search")]),
                text_turn("done"),
            ],
            vec![ResolvedTool::server_side("web", tool("search"))],
            budget(),
            false,
        )
        .await;
        let indices: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                LoopEvent::CallResult { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(indices, vec![0, 1]);
    }

    /// A tool that fails is the model's problem to work around, not a 500 — the
    /// error text goes back as the result and the loop keeps going.
    #[tokio::test]
    async fn a_failing_tool_is_reported_to_the_model_and_the_run_continues() {
        struct FailExec;
        #[async_trait]
        impl ToolExecutor for FailExec {
            async fn call(&self, _name: &str, _args: &Value) -> ToolOutcome {
                ToolOutcome::error("upstream server is down")
            }
        }
        let runner = ScriptedRunner::new(
            vec![call_turn(&[("c1", "search")]), text_turn("oh well")],
            false,
        );
        let mut sink = CollectSink::default();
        let res = run(
            user("go"),
            RunConfig {
                tools: vec![ResolvedTool::server_side("web", tool("search"))],
                budget: budget(),
                parallel_tool_calls: true,
                resume: Vec::new(),
                cancel: Cancel::none(),
            },
            &runner,
            &FailExec,
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(res.reason, StopReason::Done(FinishReason::Stop));
        let tool_msg = res
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("the failure is fed back as a tool result");
        assert!(matches!(
            &tool_msg.content[0],
            ContentPart::ToolResult { is_error: true, .. }
        ));
    }

    /// A disconnected client must stop the loop, not leave it running turns and
    /// tool calls for a response nobody will read.
    #[tokio::test]
    async fn a_sink_that_gives_up_aborts_the_run() {
        struct DeadSink(usize);
        #[async_trait]
        impl EventSink for DeadSink {
            async fn emit(&mut self, _ev: LoopEvent) -> bool {
                self.0 = self.0.saturating_sub(1);
                self.0 > 0
            }
        }
        let runner = ScriptedRunner::new(
            vec![call_turn(&[("c1", "search")]), text_turn("never reached")],
            false,
        );
        let mut sink = DeadSink(2);
        let res = run(
            user("go"),
            RunConfig {
                tools: vec![ResolvedTool::server_side("web", tool("search"))],
                budget: budget(),
                parallel_tool_calls: true,
                resume: Vec::new(),
                cancel: Cancel::none(),
            },
            &runner,
            &EchoExec,
            &mut sink,
        )
        .await
        .unwrap();
        assert!(res.aborted);
        assert_eq!(runner.seen.lock().unwrap().len(), 1);
    }

    /// `parallel_tool_calls: false` must actually serialize. A client sets it
    /// when its tools share a resource or must run in order, so quietly running
    /// them concurrently anyway would corrupt exactly the case it protects.
    #[tokio::test]
    async fn parallel_tool_calls_false_runs_them_one_at_a_time() {
        /// Records enter/exit around a yield point, so overlap is visible.
        struct TraceExec(Mutex<Vec<String>>);
        #[async_trait]
        impl ToolExecutor for TraceExec {
            async fn call(&self, _name: &str, args: &Value) -> ToolOutcome {
                let tag = args["q"].to_string();
                self.0.lock().unwrap().push(format!("enter{tag}"));
                tokio::task::yield_now().await;
                self.0.lock().unwrap().push(format!("exit{tag}"));
                ToolOutcome::ok(ToolResultBlock::one("ok"))
            }
        }

        let two_calls = Completion {
            content: vec![
                ContentPart::ToolUse {
                    id: "c1".into(),
                    name: "search".into(),
                    args: json!({"q": 1}),
                },
                ContentPart::ToolUse {
                    id: "c2".into(),
                    name: "search".into(),
                    args: json!({"q": 2}),
                },
            ],
            reasoning: String::new(),
            finish_reason: FinishReason::ToolUse,
            usage: Usage::default(),
            model: "m".into(),
            timings: None,
        };

        let trace_for = |parallel: bool| {
            let two_calls = two_calls.clone();
            async move {
                let runner = ScriptedRunner::new(vec![two_calls, text_turn("done")], false);
                let exec = TraceExec(Mutex::new(Vec::new()));
                let mut sink = CollectSink::default();
                run(
                    user("go"),
                    RunConfig {
                        tools: vec![ResolvedTool::server_side("web", tool("search"))],
                        budget: budget(),
                        parallel_tool_calls: parallel,
                        resume: Vec::new(),
                        cancel: Cancel::none(),
                    },
                    &runner,
                    &exec,
                    &mut sink,
                )
                .await
                .unwrap();
                let trace = exec.0.lock().unwrap().clone();
                trace
            }
        };

        assert_eq!(
            trace_for(false).await,
            vec!["enter1", "exit1", "enter2", "exit2"],
        );
        assert_eq!(
            trace_for(true).await,
            vec!["enter1", "enter2", "exit1", "exit2"],
            "the default must still overlap"
        );
    }

    #[test]
    fn the_accumulator_folds_split_argument_fragments() {
        let mut acc = TurnAccumulator::default();
        acc.on_delta(&StreamDelta::ToolCallStart {
            index: 0,
            id: "c1".into(),
            name: "search".into(),
        });
        for frag in [r#"{"q""#, r#": "rust"#, r#"""#, "}"] {
            acc.on_delta(&StreamDelta::ToolCallArgsDelta {
                index: 0,
                fragment: frag.into(),
            });
        }
        acc.on_delta(&StreamDelta::Stop(FinishReason::ToolUse));
        let c = acc.finish("m".into());
        assert!(matches!(
            &c.content[0],
            ContentPart::ToolUse { args, .. } if args == &json!({"q": "rust"})
        ));
    }

    /// Malformed arguments from the model must reach the tool, which gives a
    /// better error than the gateway could — not abort the run.
    #[test]
    fn unparseable_arguments_survive_as_a_string() {
        let mut acc = TurnAccumulator::default();
        acc.on_delta(&StreamDelta::ToolCallStart {
            index: 0,
            id: "c1".into(),
            name: "search".into(),
        });
        acc.on_delta(&StreamDelta::ToolCallArgsDelta {
            index: 0,
            fragment: "{not json".into(),
        });
        let c = acc.finish("m".into());
        assert!(matches!(
            &c.content[0],
            ContentPart::ToolUse { args, .. } if args == &json!("{not json")
        ));
    }

    /// Records who each call's approval scope names.
    struct ApproverExec(Mutex<Vec<(String, Option<approval::Approver>)>>);

    #[async_trait]
    impl ToolExecutor for ApproverExec {
        async fn call(&self, name: &str, _args: &Value) -> ToolOutcome {
            tokio::task::yield_now().await;
            self.0
                .lock()
                .unwrap()
                .push((name.to_string(), approval::approved_by()));
            ToolOutcome::ok(ToolResultBlock::one("ok"))
        }
    }

    /// A call outside a resume's approved ones carries nobody, even when
    /// the run sits inside another call's approval scope (review finding 4:
    /// the stamp is never inherited); a resume's carries its own.
    #[tokio::test]
    async fn only_an_approved_call_carries_its_approver() {
        let by = |n: &str| approval::Approver {
            kind: lmgw_api_types::mcp_host::CallerKind::Device,
            name: n.into(),
        };
        let calls = vec![
            ("c1".to_string(), "a".to_string(), json!({})),
            ("c2".to_string(), "b".to_string(), json!({})),
        ];
        for parallel in [false, true] {
            let exec = ApproverExec(Mutex::new(Vec::new()));
            approval::scope(Some(by("outer")), execute(&exec, &calls, parallel)).await;
            let mut seen = exec.0.into_inner().unwrap();
            seen.sort_by(|x, y| x.0.cmp(&y.0));
            assert_eq!(
                seen,
                vec![("a".to_string(), None), ("b".to_string(), None)],
                "parallel {parallel}"
            );

            let exec = ApproverExec(Mutex::new(Vec::new()));
            let approvers = [Some(by("phone")), None];
            approval::scope(
                Some(by("outer")),
                execute_approved(&exec, &calls, &approvers, parallel),
            )
            .await;
            let mut seen = exec.0.into_inner().unwrap();
            seen.sort_by(|x, y| x.0.cmp(&y.0));
            assert_eq!(
                seen,
                vec![
                    ("a".to_string(), Some(by("phone"))),
                    ("b".to_string(), None)
                ],
                "parallel {parallel}"
            );
        }
    }
}
