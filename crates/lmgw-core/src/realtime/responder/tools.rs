//! A response's server-side calls (realtime-server-tools design §2.3, §2.4):
//! the model's calls of the MCP tools the response offered, noted as the
//! stream passes ([`Collect`]) and run once it has ended — inside the chat
//! future, not after the speaker, and before the response's
//! [`Msg::Finished`], so `response.done` follows every call and its audio.
//!
//! **Which run** (§2.3): none after a stream that failed — the core closes
//! them as never made — or that stopped for `Length` or `ContentFilter`. A
//! call whose arguments are not a JSON object fails at once with the parse
//! error and runs nothing; arguments the model left empty are `{}`, as the
//! rendering reads them. Everything else runs.
//!
//! **How** (§2.4): through the `/v1/responses` executor stack with the
//! session's `RequestCtx` — the key's tool scope re-read per call, then the
//! built-in toolsets in-process and the registered servers over MCP —
//! every layer logging under `realtime-tool`. The executors write the rows;
//! nothing else does. The calls run concurrently, or one at a time in model
//! order with `parallel_tool_calls: false`, each raced against the sink's
//! stop: the response's own, or in speech mode the chat's, which the
//! speaker raises when the response is stopped or its voice fails — a
//! failed voice does not wait for the tools.
//!
//! **Reports** ([`Report`]): [`Msg::ToolRunning`] as a call starts and
//! [`Msg::ToolDone`] with its outcome, by the call's upstream ordinal. In
//! speech mode they go through the speaker's queue, behind the call's own
//! deltas and the clauses before them, so a fast call is never reported
//! before its item exists; the call itself runs at once, and the core hears
//! that it was sent straight away ([`Msg::ToolSent`], §2.5) — with its id,
//! name and arguments, so a core whose item for it never came (a cancel
//! while the clauses before it are made, a voice that failed with them
//! queued) can still announce the call and close it as abandoned. A speaker
//! that is gone (its voice failed) cannot take the reports, and they go to
//! the core directly.
//!
//! **A call the stop drops** (§2.5) is the core's to close: it abandons the
//! response's open items itself, and these reports would come too late. Its
//! executor writes no row for a future that never returns, so it gets a
//! `canceled` one here (`dropped`). The stop races the call only up to its
//! executor's own row ([`RowWatch`]): a call whose row is being written has
//! returned, and finishes — its one row the executor's, its result reported
//! as any other's (a cancel's core drops it as an old generation's; a
//! failed voice's still takes it).

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use serde_json::{Map, Value};
use tokio::sync::mpsc;

use super::speech::Work;
use super::{Msg, Tx};
use crate::agent::{DeltaSink, ToolExecutor, ToolOutcome};
use crate::ir::{FinishReason, StreamDelta, Usage};
use crate::mcp::exec::{DocsExecutor, McpExecutor, SelfAdminExecutor, SplitExecutor};
use crate::mcp::ingress::RowWatch;
use crate::mcp::scope::ScopedExecutor;
use crate::proxy::{RequestCtx, StopSignal};
use crate::state::SharedState;
use crate::telemetry::REALTIME_TOOL_PROTO;

mod dropped;

/// What a response runs server-side (§2.1, §2.4).
#[derive(Debug, Clone, Default)]
pub(crate) struct ServerTools {
    /// The exposed names it offered: a call of one of them is the
    /// gateway's to run, any other the client's.
    pub names: HashSet<String>,
    /// The session's names a built-in toolset runs in-process: the split
    /// executor's in-process half.
    pub builtin: Vec<String>,
    /// The offered names of registered servers, and the server each was
    /// listed from: a call runs there or not at all.
    pub servers: HashMap<String, i64>,
    /// `parallel_tool_calls`: `false` runs the calls one at a time, in the
    /// model's order.
    pub parallel: bool,
}

/// One call of an offered name, as the stream made it.
#[derive(Debug)]
pub(super) struct Call {
    /// The upstream's ordinal: what the core knows the call's item by.
    index: usize,
    /// The upstream's own id for it.
    id: String,
    name: String,
    /// As streamed: a JSON text.
    args: String,
}

/// A call that went to its server, as [`Msg::ToolSent`] tells the core: all
/// it needs to announce the call's item itself (module doc).
#[derive(Debug, Clone)]
pub(crate) struct SentCall {
    /// The upstream's ordinal, as the call's deltas name it.
    pub index: usize,
    /// The upstream's own id for it, as its `ToolCallStart` gave it.
    pub id: String,
    /// The exposed name the model called.
    pub name: String,
    /// Its arguments, as streamed.
    pub args: String,
}

/// The stream's sink, with the calls of offered names noted as they pass.
pub(super) struct Collect<'a> {
    inner: &'a mut dyn DeltaSink,
    tools: &'a ServerTools,
    calls: Vec<Call>,
}

impl<'a> Collect<'a> {
    pub fn new(inner: &'a mut dyn DeltaSink, tools: &'a ServerTools) -> Self {
        Self {
            inner,
            tools,
            calls: Vec::new(),
        }
    }

    /// The calls the stream made, in its order.
    pub fn into_calls(self) -> Vec<Call> {
        self.calls
    }
}

impl DeltaSink for Collect<'_> {
    fn on_delta(&mut self, d: &StreamDelta) {
        match d {
            StreamDelta::ToolCallStart { index, id, name } if self.tools.names.contains(name) => {
                self.calls.push(Call {
                    index: *index,
                    id: id.clone(),
                    name: name.clone(),
                    args: String::new(),
                })
            }
            StreamDelta::ToolCallArgsDelta { index, fragment } => {
                if let Some(c) = self.calls.iter_mut().find(|c| c.index == *index) {
                    c.args.push_str(fragment);
                }
            }
            _ => {}
        }
        self.inner.on_delta(d);
    }

    fn stop(&self) -> Option<StopSignal> {
        self.inner.stop()
    }

    fn flush(&mut self) {
        self.inner.flush();
    }

    fn billed(&mut self, usage: &Usage) {
        self.inner.billed(usage);
    }

    fn billed_cost(&mut self, row: crate::pricing::RowCost) {
        self.inner.billed_cost(row);
    }
}

/// Where a call's reports go (module doc).
pub(super) struct Report<'a> {
    gen: u64,
    tx: &'a Tx,
    /// The speaker's queue; `None` in text mode.
    queue: Option<mpsc::UnboundedSender<Work>>,
}

impl<'a> Report<'a> {
    /// Text mode: straight to the core.
    pub fn direct(gen: u64, tx: &'a Tx) -> Self {
        Self {
            gen,
            tx,
            queue: None,
        }
    }

    /// Speech mode: behind everything already handed to the speaker.
    pub fn queued(gen: u64, tx: &'a Tx, queue: mpsc::UnboundedSender<Work>) -> Self {
        Self {
            gen,
            tx,
            queue: Some(queue),
        }
    }

    /// `call` starts: the core knows at once that it went to its server,
    /// and hears its `in_progress` in stream order (module doc).
    fn running(&self, call: &Call) {
        if self.queue.is_some() {
            let sent = SentCall {
                index: call.index,
                id: call.id.clone(),
                name: call.name.clone(),
                args: call.args.clone(),
            };
            let _ = self.tx.send((self.gen, Msg::ToolSent(sent)));
        }
        self.send(Msg::ToolRunning {
            index: call.index,
            at: Instant::now(),
        });
    }

    fn send(&self, msg: Msg) {
        let msg = match &self.queue {
            None => msg,
            Some(q) => match q.send(Work::Report(msg)) {
                Ok(()) => return,
                Err(mpsc::error::SendError(Work::Report(msg))) => msg,
                Err(_) => return,
            },
        };
        let _ = self.tx.send((self.gen, msg));
    }
}

/// Run `calls`, made by a stream that ended for `reason` (module doc).
/// `stop`: the sink's.
pub(super) async fn run(
    (state, ctx): (&SharedState, &RequestCtx),
    tools: &ServerTools,
    calls: Vec<Call>,
    reason: &FinishReason,
    stop: Option<StopSignal>,
    report: &Report<'_>,
) {
    if calls.is_empty() {
        return;
    }
    if matches!(reason, FinishReason::Length | FinishReason::ContentFilter) {
        tracing::debug!(
            "realtime: a response cut off ({reason:?}) runs none of its {} MCP calls",
            calls.len()
        );
        return;
    }
    let exec = executor(state, ctx, tools);
    let stop = stop.as_ref();
    let rows = dropped::Rows {
        state,
        ctx,
        builtin: &tools.builtin,
    };
    if tools.parallel {
        let all = calls.iter().map(|c| one(&exec, c, stop, report, &rows));
        futures::future::join_all(all).await;
    } else {
        for c in &calls {
            one(&exec, c, stop, report, &rows).await;
        }
    }
}

/// One call, reported as it starts and ends — unless the stop ends it
/// before its executor writes its row: a `canceled` row then, once it was
/// sent (module doc).
async fn one(
    exec: &dyn ToolExecutor,
    call: &Call,
    stop: Option<&StopSignal>,
    report: &Report<'_>,
    rows: &dropped::Rows<'_>,
) {
    if stop.is_some_and(StopSignal::is_raised) {
        return;
    }
    let args = match arguments(&call.args) {
        Ok(a) => a,
        Err(why) => {
            return report.send(Msg::ToolDone {
                index: call.index,
                outcome: ToolOutcome::error(why),
                at: Instant::now(),
            })
        }
    };
    report.running(call);
    let started = Instant::now();
    let watch = RowWatch::default();
    let run = watch.scope(exec.call(&call.name, &args));
    tokio::pin!(run);
    let outcome = tokio::select! {
        biased;
        () = crate::proxy::stopped(stop) => {
            if !watch.writing() {
                return rows.record(&call.name, started).await;
            }
            run.await
        }
        o = &mut run => o,
    };
    report.send(Msg::ToolDone {
        index: call.index,
        outcome,
        at: Instant::now(),
    });
}

/// A call's arguments as its tool gets them: a JSON object — none at all is
/// `{}` — or why it is not run.
fn arguments(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let kind = match serde_json::from_str::<Value>(raw) {
        Ok(v @ Value::Object(_)) => return Ok(v),
        Ok(Value::Array(_)) => "an array",
        Ok(Value::String(_)) => "a string",
        Ok(Value::Number(_)) => "a number",
        Ok(Value::Bool(_)) => "a boolean",
        Ok(Value::Null) => "null",
        Err(e) => {
            return Err(format!(
                "not run: the arguments the model wrote are not valid JSON ({e}); a tool takes a \
                 JSON object"
            ))
        }
    };
    Err(format!(
        "not run: the arguments the model wrote are {kind}, and a tool takes a JSON object"
    ))
}

/// The `/v1/responses` executor stack (module doc), every layer under
/// `realtime-tool`.
fn executor(
    state: &SharedState,
    ctx: &RequestCtx,
    tools: &ServerTools,
) -> ScopedExecutor<SplitExecutor> {
    let docs = DocsExecutor::new(state.clone(), ctx.clone())
        .with_proto(REALTIME_TOOL_PROTO)
        .charged_to(crate::devices::charged(ctx));
    // `docs__request` tells the owner who asked: the session's key, as on
    // `/v1/responses`.
    let docs = match &ctx.client_key {
        Some(key) => docs.with_client(key.clone()),
        None => docs,
    };
    ScopedExecutor::new(
        SplitExecutor::new(
            SelfAdminExecutor::new(state.clone(), ctx.clone()).with_proto(REALTIME_TOOL_PROTO),
            docs,
            tools.builtin.iter().cloned(),
            McpExecutor::new(state.clone(), ctx.clone())
                .with_proto(REALTIME_TOOL_PROTO)
                .with_listed(tools.servers.clone()),
        ),
        state.clone(),
        ctx.clone(),
    )
    .with_proto(REALTIME_TOOL_PROTO)
}

#[cfg(test)]
mod tests;
