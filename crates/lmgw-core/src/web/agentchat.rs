//! **Tool-enabled chat** (§21 stages 2–3) — the Chat-tab send that runs a tool
//! loop instead of a 1:1 upstream stream.
//!
//! Two flavors share it, because they differ only in which tools are attached:
//!
//! * **Admin Chat** — a thread with `kind = "admin"`, holding the built-in
//!   `lmgw__*` tools, so the owner can drive the gateway from the dashboard.
//! * **Any thread with MCP servers attached** — the registered southbound
//!   servers a thread names in `mcp_tools`, resolved through the same
//!   [`crate::mcp::exec::resolve`] the Responses API calls, so a thread and an
//!   API client see one tool surface rather than two that drift.
//!
//! A thread can be both. Everything else about either is an ordinary chat
//! thread: same sidebar, same model picker, same persistence, same Logs rows.
//!
//! **No approvals here.** `/v1/responses` can stop a run and hand a gated call
//! back to the client; the Chat tab has no round trip to resume one, so threads
//! resolve their servers with `require_approval: never` and the picker is where
//! the owner decides what a thread may reach.
//!
//! ## Why this exists rather than "point an agent at /mcp"
//!
//! It is the same tools, reached without opening a door. The self-admin plane
//! now lives on [`/mcp/admin`](crate::mcp::ingress) behind its own token
//! precisely so that a coding agent holding a gateway key *cannot* reconfigure
//! lmgw by accident. Admin Chat is the other half of that trade: the owner,
//! sitting at the dashboard, gets the capability in-process — no token, no
//! network hop, nothing exposed — while everything reaching in from outside
//! still has to be let in deliberately.
//!
//! **It is not a privilege escalation.** The `self_admin` Setting is the
//! capability gate and it applies here too: at `read_only` (the default) this
//! chat can inspect the gateway and nothing more, and the mutating tools are
//! not even listed. The page says so, rather than failing mysteriously.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;

use super::chat_knowledge::KbTools;
use super::chat_turn::{self, Events, Reply, Stopped, Turn, TurnFrame, NOT_SAVED};
use crate::agent::{
    self, Budget, Cancel, DeltaSink, EventSink, LoopEvent, ResolvedTool, RunConfig, StopReason,
    TurnRunner,
};
use crate::config::Route;
use crate::error::GatewayError;
use crate::ingress::responses::{ApprovalRule, McpToolSpec};
use crate::ir::{flatten_tool_result, ChatRequest, Completion, ContentPart, Message, Role, Usage};
use crate::mcp::exec::{
    self as mcp_exec, self_admin_tools, DocsExecutor, KbExecutor, McpExecutor, SelfAdminExecutor,
    SplitExecutor, KB_LABEL, SELF_ADMIN_LABEL,
};
use crate::proxy::{self, PerRoute, RequestCtx, StopSignal};
use crate::state::SharedState;
use crate::store::ThreadMcp;
use crate::telemetry::{ADMIN_PROTO, CHAT_TOOL_PROTO};

mod claim;
mod heard;
mod refused;

/// `chat_threads.kind` for an Admin Chat thread.
pub const ADMIN_KIND: &str = "admin";

/// Who a `docs__request` filed from a Chat thread is attributed to. `/mcp`
/// takes this from the client's `initialize`; a thread has to name itself, or
/// the owner's own request queue would read as anonymous.
const DOCS_CLIENT: &str = "dashboard chat";

/// What a thread brings to the loop: the self-admin plane, the MCP servers it
/// attaches, its knowledge bases in tool mode, or any mix. Built by
/// [`super::chat_turn::start_turn`] from the stored thread, so this module
/// never re-reads the row it was handed.
pub struct ToolPlan {
    /// `kind == "admin"`: attach the `lmgw__*` tools, subject to the
    /// `self_admin` mode gate.
    pub admin: bool,
    /// Registered MCP servers this thread attaches (`chat_threads.mcp_tools`).
    pub mcp: Vec<ThreadMcp>,
    /// Tool mode's knowledge bases (chat-complete design §9.3): the `kb__*`
    /// tools, reaching exactly these bases.
    pub kb: Option<KbTools>,
}

impl ToolPlan {
    /// Nothing to attach — the caller should take the plain 1:1 path instead.
    pub fn is_empty(&self) -> bool {
        !self.admin && self.mcp.is_empty() && self.kb.is_none()
    }
}

/// The preconfigured system prompt. A thread's own system prompt, if set, is
/// appended rather than replacing this — the point of the flavor is that the
/// tools are explained, and a user tweaking the persona should not silently
/// lose that.
pub fn system_prompt(extra: &str) -> String {
    let mut s = String::from(BASE_PROMPT);
    if !extra.trim().is_empty() {
        s.push_str("\n\n");
        s.push_str(extra.trim());
    }
    s
}

const BASE_PROMPT: &str = "\
You are the administrator's assistant for lmgw, a self-hosted LLM API gateway \
running on this machine. You are talking to its owner through the gateway's own \
dashboard.

You have tools named `lmgw__*` that inspect and configure this gateway. Use \
them rather than guessing or asking the user to read values out to you — \
`lmgw__status` is the right first call for almost any question about how things \
are, and `lmgw__models`, `lmgw__upstreams`, `lmgw__mcp_servers`, `lmgw__logs` \
and `lmgw__settings` cover the rest.

Some of the tools change configuration. They are only offered when the owner \
has set self-admin to 'full'; if a call is refused for that reason, say so \
plainly and point at Settings rather than trying to work around it. Before a \
change that would restart a container, evict a loaded model, or rewrite an \
upstream, say what you are about to do and why.

Report what the tools actually returned. If something is unavailable or a tool \
fails, say that instead of filling the gap with a plausible answer.";

/// Wall-clock ceiling for one Admin Chat turn.
///
/// Shares the `/v1/responses` Setting rather than inventing a second one: it
/// bounds the same thing (a tool loop, not a single generation), it is visible
/// and editable on the Settings page, and having two knobs for one behavior is
/// how they drift apart.
fn budget(state: &SharedState) -> Budget {
    let s = &state.snapshot().settings;
    Budget {
        max_tool_calls: s.responses_max_tool_calls,
        wall_clock: Duration::from_secs(s.responses_timeout_seconds.max(1)),
    }
}

/// Drives one model turn, logged as `ingress_proto = "admin"` for an Admin Chat
/// thread and `"chat"` for any other — a thread that merely attached a search
/// server is still ordinary chat traffic, and Logs should say so.
struct ChatRunner {
    state: SharedState,
    route: Route,
    /// Why `route` is a fallback, when it is — every turn's row says so.
    fallback: Option<crate::gate::FallbackReason>,
    proto: &'static str,
    /// GPU admission for the whole loop (§9b) — see
    /// [`crate::vram::LocalHold`] for why a tool loop holds one guard
    /// rather than one per turn — let go while a heard turn's tools wait
    /// for its user row ([`claim`]).
    claim: claim::LoopClaim,
    /// The turn continues the thread's last reply: the loop's first request
    /// ends with it, and says so ([`chat_turn::mark_continuation`]).
    continuing: bool,
    /// Every model call carries the user's speech as audio: each goes only
    /// to a route this lmgw runs (voice-audio-input design §3.4).
    local_only: bool,
    /// Who the admitted route answers as ([`chat_turn::answered_by`]).
    admitted_as: Option<String>,
    /// What the last model call went out as: the overrides its route
    /// dropped, and who answered — for the `done` event and the saved reply.
    answering: Mutex<Answering>,
}

#[derive(Default, Clone)]
struct Answering {
    ignored: Vec<&'static str>,
    answered_by: Option<String>,
    /// How the last model call's reasoning off went out, and whether it
    /// reasoned anyway (model-capabilities design §5.6).
    fitted: Option<proxy::reasoning_fit::Fitted>,
}

/// The loop's request as each route takes it — the admitted one, and every
/// one the gate re-routes a model call to (review R1 finding 2): the
/// thread's own sampling split for that route, a continue refused where
/// there is no prefill, and the continuation fields only where llama-server
/// answers. Always from the loop's own request (the thread's own params),
/// never from what another route was sent.
impl PerRoute for ChatRunner {
    fn request(
        &self,
        route: &Route,
        rerouted: Option<&crate::gate::GateHeaders>,
        ir: &ChatRequest,
    ) -> Result<Option<ChatRequest>, GatewayError> {
        // Only the first request of a continue ends with the reply; every
        // later one ends with tool results and goes out as it is.
        let continuing = self.continuing
            && ir
                .messages
                .last()
                .is_some_and(|m| m.role == Role::Assistant);
        let fit = chat_turn::fit_route(route, ir, (continuing, self.local_only))?;
        let answered_by = match rerouted {
            Some(h) => chat_turn::answered_by(&self.state.snapshot(), h),
            None => self.admitted_as.clone(),
        };
        *self.answering.lock().unwrap_or_else(|e| e.into_inner()) = Answering {
            ignored: fit.ignored,
            answered_by,
            fitted: None,
        };
        if !continuing && !fit.params_changed {
            // The request as the loop built it is what this route takes —
            // no copy of the whole history for nothing.
            return Ok(None);
        }
        let mut req = ir.clone();
        req.params = fit.params;
        if continuing {
            chat_turn::mark_continuation(route, &mut req);
        }
        Ok(Some(req))
    }

    /// An off the route took in another form joins the overrides it dropped.
    fn fitted(&self, fitted: &proxy::reasoning_fit::Fitted) {
        let mut answering = self.answering.lock().unwrap_or_else(|e| e.into_inner());
        fitted.report(&mut answering.ignored);
        answering.fitted = Some(fitted.clone());
    }
}

#[async_trait]
impl TurnRunner for ChatRunner {
    async fn run_turn(
        &self,
        ir: &ChatRequest,
        deadline: Duration,
        sink: &mut dyn DeltaSink,
    ) -> Result<Completion, GatewayError> {
        let started = Instant::now();
        let hold = match self.claim.for_call().await {
            Ok(hold) => hold,
            Err(e) => {
                // A claim let go and refused again: its request row, as any
                // refusal of the loop's (`refused`).
                let (alias, route) = (&ir.model_alias, &self.route);
                refused::record(
                    &self.state,
                    alias,
                    self.proto,
                    route,
                    self.fallback,
                    started,
                    &e,
                )
                .await;
                return Err(e);
            }
        };
        proxy::stream_once_on(
            &self.state,
            hold.as_ref(),
            &self.route,
            self.fallback,
            ir,
            self.proto,
            proxy::KeyRef::default(),
            deadline.saturating_sub(started.elapsed()),
            sink,
            Some((self, None)),
        )
        .await
    }
}

/// Turns loop events into the SSE frames the Chat island already speaks, and
/// accumulates the turn for persistence.
struct ChatSink {
    tx: Events,
    /// Raised the moment the turn is stopped. From then on no text is relayed
    /// or kept, so the partial reply is what the reader had been sent, and no
    /// frame waits for a reader that stopped reading.
    stop: StopSignal,
    text: String,
    reasoning: String,
    usage: Usage,
    first_at: Option<Instant>,
}

impl ChatSink {
    /// One of the loop's frames. Once the turn is stopped it waits for no
    /// reader: a frame that still has room goes out, any other is dropped.
    /// The turn's own last frame is [`Self::say`]'s.
    async fn emit_raw(&self, ev: &'static str, data: String) -> bool {
        let frame = TurnFrame::new(ev, data);
        if self.stop.is_raised() {
            return !matches!(
                self.tx.try_send(frame),
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_))
            );
        }
        tokio::select! {
            biased;
            sent = self.tx.send(frame) => sent.is_ok(),
            // The stop landed while a stalled reader held this frame up.
            () = self.stop.raised() => true,
        }
    }

    /// A frame the reader must get even after a stop (`error`, `done`): sent
    /// once the loop and its GPU admission are gone, so waiting on a slow
    /// reader holds nothing.
    async fn say(&self, ev: &'static str, data: String) {
        let _ = self.tx.send(TurnFrame::new(ev, data)).await;
    }
}

#[async_trait]
impl EventSink for ChatSink {
    async fn emit(&mut self, ev: LoopEvent) -> bool {
        match ev {
            // A new turn after tool calls continues the same visible answer:
            // the island renders one assistant bubble per send, with the tool
            // activity shown inline, which is what the transcript looks like.
            LoopEvent::TurnStarted { .. } => true,
            // Nothing said after a stop is relayed or kept.
            LoopEvent::Text(_) | LoopEvent::Reasoning(_) if self.stop.is_raised() => true,
            LoopEvent::Text(t) => {
                self.first_at.get_or_insert_with(Instant::now);
                self.text.push_str(&t);
                self.emit_raw("delta", json!({ "text": t }).to_string())
                    .await
            }
            LoopEvent::Reasoning(t) => {
                self.first_at.get_or_insert_with(Instant::now);
                self.reasoning.push_str(&t);
                self.emit_raw("reasoning", json!({ "text": t }).to_string())
                    .await
            }
            LoopEvent::CallStarted { index, name, .. } => {
                self.emit_raw(
                    "tool",
                    json!({"event": "start", "index": index, "name": name}).to_string(),
                )
                .await
            }
            LoopEvent::CallArgs { index, fragment } => {
                self.emit_raw(
                    "tool",
                    json!({"event": "args", "index": index, "fragment": fragment}).to_string(),
                )
                .await
            }
            LoopEvent::CallReady {
                index, name, args, ..
            } => {
                self.emit_raw(
                    "tool",
                    json!({"event": "ready", "index": index, "name": name,
                           "arguments": args})
                    .to_string(),
                )
                .await
            }
            LoopEvent::CallResult {
                index,
                name,
                blocks,
                is_error,
                ms,
                ..
            } => {
                let (text, _) = flatten_tool_result(&blocks);
                self.emit_raw(
                    "tool",
                    json!({"event": "result", "index": index, "name": name,
                           "output": text, "is_error": is_error, "ms": ms})
                    .to_string(),
                )
                .await
            }
            LoopEvent::Done { usage, reason } => {
                self.usage = usage;
                // A cancel is the turn being stopped, and the turn says so
                // itself (`report_stop`, or `done` to a caller that stopped
                // it): there is no budget to raise.
                let stopped = reason == StopReason::Incomplete(agent::REASON_CANCELED);
                if let (StopReason::Incomplete(why), false) = (reason, stopped) {
                    let msg = format!(
                        "the run stopped early: {why} (raise it under Settings → Agents & \
                         tools if this was too tight)"
                    );
                    self.emit_raw("error", json!({ "message": msg }).to_string())
                        .await;
                }
                true
            }
        }
    }
}

/// The spawned worker behind a tool-enabled turn: resolve → loop → persist.
/// Every wait before the loop gives way the moment the turn is stopped or
/// replaced by a newer one, and the loop itself is cancelled then (its
/// model and tool calls dropped where they stand); a page that stopped
/// reading still gets the partial turn saved (review R1 finding 1).
pub(super) async fn run_send(
    state: SharedState,
    turn: Turn,
    ir: ChatRequest,
    plan: ToolPlan,
    tx: Events,
) {
    let started = Instant::now();
    let snap = state.snapshot();
    // A refusal that is no gateway error (a thread with no usable tool):
    // its message alone. A gateway error's goes with its code
    // (`chat_turn::refuse`).
    let refuse = |message: String| {
        let tx = tx.clone();
        async move {
            let error = json!({ "message": message }).to_string();
            let _ = tx.send(TurnFrame::new("error", error)).await;
            let done = json!({ "aborted": true }).to_string();
            let _ = tx.send(TurnFrame::new("done", done)).await;
        }
    };
    // The gate's routing stages — the GPU hold's re-route/refusal, like every
    // other interactive path (gpu-hold design §2). The runner below carries
    // this route for the whole tool loop, so the swap has to happen here or
    // not at all. Admission waits until the tools are resolved, so a thread
    // with nothing to call starts no container.
    let resolved = turn
        .or_stop(
            &tx,
            crate::gate::resolve(&state, &ir.model_alias, crate::gate::RouteCheck::None),
        )
        .await;
    let routed = match resolved {
        Ok(Ok(r)) => r,
        Ok(Err(f)) => {
            // `held` for the GPU hold or a benchmark, as the plain path says.
            super::chat_voice::held_at_resolve(&tx, &ir.model_alias, &f.error).await;
            return chat_turn::refuse(&tx, &f.error).await;
        }
        Err(why) => return turn.report_stop(why, &tx).await,
    };

    let mut tools: Vec<ResolvedTool> = Vec::new();
    // Names the loop must run in-process rather than against a southbound
    // server: an admin thread's automatic `lmgw__*` wiring, plus whatever
    // built-in toolset the thread attached by label below.
    let mut builtin_names: Vec<String> = Vec::new();
    if plan.admin {
        let defs = self_admin_tools(&snap);
        builtin_names.extend(defs.iter().map(|d| d.name.clone()));
        tools.extend(
            defs.into_iter()
                .map(|d| ResolvedTool::server_side(SELF_ADMIN_LABEL, d)),
        );
    }

    // The thread's attached MCP servers, resolved exactly as `/v1/responses`
    // resolves an `{"type":"mcp"}` block. A server that cannot be resolved is
    // named to the owner instead of silently vanishing from the tool list —
    // "the model ignored my search tool" is otherwise indistinguishable from
    // "the server never connected". Tool mode's knowledge bases join as the
    // built-in `kb` label with its whole toolset (kb__list, kb__search,
    // kb__read); the executor below restricts them to the thread's bases,
    // and a `kb` label the thread also attached by hand is the same toolset,
    // attached once.
    let mut specs: Vec<McpToolSpec> = plan
        .mcp
        .iter()
        .filter(|m| plan.kb.is_none() || m.server_label != KB_LABEL)
        .map(|m| McpToolSpec {
            server_label: m.server_label.clone(),
            allowed_tools: m.allowed_tools.clone(),
            require_approval: ApprovalRule::Never,
        })
        .collect();
    if plan.kb.is_some() {
        specs.push(McpToolSpec {
            server_label: KB_LABEL.to_string(),
            allowed_tools: None,
            require_approval: ApprovalRule::Never,
        });
    }
    if !specs.is_empty() {
        // The owner's own thread: the owner attached these labels.
        let scope = crate::mcp::scope::ToolScope::gateway();
        let resolved = match turn
            .or_stop(&tx, mcp_exec::resolve(&state, &specs, &scope))
            .await
        {
            Ok(r) => r,
            Err(why) => return turn.report_stop(why, &tx).await,
        };
        for (label, why) in &resolved.failed {
            let message = if label == KB_LABEL {
                format!("knowledge-base tools: {why}")
            } else {
                format!("MCP server '{label}': {why}")
            };
            let error = json!({ "message": message }).to_string();
            let _ = tx.send(TurnFrame::new("error", error)).await;
        }
        // A thread may attach the same self-admin toolset an admin thread gets
        // automatically; the set is what dispatches, so duplicates are inert.
        builtin_names.extend(resolved.builtin.iter().cloned());
        tools.extend(resolved.tools);
    }

    // No tools at all: either self-admin is off, or every attached server
    // failed (each already reported above). Say which rather than letting the
    // model improvise an answer about a gateway or a search it cannot reach.
    if tools.is_empty() {
        let msg = if plan.admin {
            "self-admin tools are switched off — set Self-admin tools under Settings → \
             Network & access to 'read only' or 'full' to use Admin Chat"
        } else if plan.mcp.is_empty() {
            "the knowledge-base tools are not available (the error above says why) — \
             switch this thread's knowledge mode to auto, or enable the kb__* tools again"
        } else {
            "none of this thread's MCP servers offered a usable tool — check \
             them on the MCP page, or detach them under thread settings"
        };
        return refuse(msg.to_string()).await;
    }

    // GPU admission (§9b), held for the whole loop. A refusal is reported to
    // the island the same way an unresolvable alias is. A local route comes
    // back on the port its container answers on (§5), and the runner carries
    // it, so every turn of the loop forwards to the same held container. Each
    // turn still takes its own per-send lease (`proxy::stream_once`).
    // As in the plain chat: an outside-VRAM swap must not hand the thread's
    // images to a fallback that cannot see them, and a candidate alias
    // refuses a facet it does not enable — tool calls among them, since this
    // loop always gives the model tools (`Routed::using`).
    let uses = crate::gate::request_facets(&ir, None).insert(crate::candidates::Facet::ToolCalls);
    // The loop's request rows' protocol, a refusal's below too.
    let proto = if plan.admin { ADMIN_PROTO } else { "chat" };
    let admitted = match routed.using(uses) {
        Ok(routed) => {
            let admit = super::chat_voice::admit_reporting(&state, routed, &ir.model_alias, &tx);
            turn.or_stop(&tx, admit).await
        }
        Err(f) => Ok(Err(f)),
    };
    let crate::gate::Opened {
        route,
        hold: admission,
        headers,
    } = match admitted {
        Ok(Ok(o)) => o,
        Ok(Err(f)) => {
            // Its request row, as the plain path writes one (`refused`).
            if let Some(route) = &f.route {
                let fallback = f.headers.fallback_reason();
                let e = &f.error;
                refused::record(&state, &ir.model_alias, proto, route, fallback, started, e).await;
            }
            let sent = chat_turn::SentAs::of(chat_turn::answered_by(&snap, &f.headers), &ir);
            return chat_turn::refuse_sent(&tx, &f.error, sent).await;
        }
        Err(why) => return turn.report_stop(why, &tx).await,
    };
    // A continue on the route admission settled on — see the plain path's
    // twin of this check in `chat::relay`; every re-route inside the loop is
    // checked again by the runner ([`PerRoute`]).
    let first = chat_turn::fit_route(&route, &ir, (turn.is_continue(), turn.local_only()));
    let first = match first {
        Ok(fit) => fit,
        Err(e) => {
            let fallback = headers.fallback_reason();
            refused::record(
                &state,
                &ir.model_alias,
                proto,
                &route,
                fallback,
                started,
                &e,
            )
            .await;
            let sent = chat_turn::SentAs::of(chat_turn::answered_by(&snap, &headers), &ir);
            return chat_turn::refuse_sent(&tx, &e, sent).await;
        }
    };
    let runner = ChatRunner {
        state: state.clone(),
        route,
        fallback: headers.fallback_reason(),
        proto,
        claim: claim::LoopClaim::new(admission),
        continuing: turn.is_continue(),
        local_only: turn.local_only(),
        admitted_as: chat_turn::answered_by(&snap, &headers),
        answering: Mutex::new(Answering {
            ignored: first.ignored,
            answered_by: chat_turn::answered_by(&snap, &headers),
            fitted: None,
        }),
    };
    // One executor for both planes, always: with no `lmgw__*` tools attached
    // the admin half simply never matches, which is cheaper than two code
    // paths that have to stay in agreement about which tool goes where.
    let mut exec = SplitExecutor::new(
        SelfAdminExecutor::new(state.clone(), RequestCtx::default()),
        DocsExecutor::new(state.clone(), RequestCtx::default()).with_client(DOCS_CLIENT),
        builtin_names,
        McpExecutor::new(state.clone(), RequestCtx::default()).with_proto(CHAT_TOOL_PROTO),
    );
    // Tool mode: the thread's bases and nothing else, whatever their
    // `mcp_visible` switch says (§9.4), with the thread's budget as the
    // tools' default.
    if let Some(kb) = &plan.kb {
        exec = exec.with_kb(
            KbExecutor::new(state.clone(), RequestCtx::default())
                .only(kb.ids.iter().copied())
                .with_default_budget(Some(kb.budget))
                .with_proto(CHAT_TOOL_PROTO),
        );
    }
    // The thread's own tool names: a call to any other name is the model's
    // invention, and its record says so (`close_trailing_calls` below).
    let known: HashSet<String> = tools.iter().map(|t| t.def.name.clone()).collect();
    // The loop races every model and tool call and every frame against this
    // stop; it is raised the moment the turn is stopped or replaced, and a
    // model call in flight then ends at its next await and still writes its
    // row (chat-voice design §7.2).
    let (stop_run, stop_signal) = proxy::stop_pair();
    let cfg = RunConfig::new(tools, budget(&state), true)
        .with_cancel(Cancel::signal(stop_signal.clone()));
    // A heard voice turn's tools wait for its user row (`heard`), the
    // loop's claim let go meanwhile (`claim`).
    let exec = heard::HeardTools::new(&exec, turn.user_row(), &stop_run, &runner.claim);
    let base_len = ir.messages.len();
    // What the loop's refusal below says its requests carried
    // (`TurnFrame::sent`).
    let images = crate::gate::media_parts(&ir).images > 0;
    let mut sink = ChatSink {
        tx: tx.clone(),
        stop: stop_signal,
        text: String::new(),
        reasoning: String::new(),
        usage: Usage::default(),
        first_at: None,
    };

    let mut stopped = None;
    let result = {
        let run = agent::run(ir, cfg, &runner, &exec, &mut sink);
        tokio::pin!(run);
        // The stop first: once it is raised the loop is not polled again
        // before it hears of it.
        tokio::select! {
            biased;
            why = turn.stopped(&tx) => {
                stopped = Some(why);
                stop_run.stop();
                run.await
            }
            r = &mut run => r,
        }
    };
    let (mut messages, err) = match result {
        Ok(r) => (r.messages[base_len.min(r.messages.len())..].to_vec(), None),
        // A turn that failed after tools ran keeps their record: the calls
        // happened, and the model must learn that they did, or it makes a
        // side-effecting call again.
        Err(e) => {
            sink.usage = e.usage;
            let record = e.messages[base_len.min(e.messages.len())..].to_vec();
            (record, Some(e.error))
        }
    };
    // The loop is over. Its GPU admission goes now, before the last frames,
    // which a reader that stopped reading could hold up.
    let answering = runner
        .answering
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // Whether the tool gate stopped the turn (`heard`), read before the
    // executor goes with the claim it borrows.
    let held_off = exec.held_off();
    drop(exec);
    drop(runner);
    if let Some(e) = err.as_ref().filter(|_| stopped.is_none()) {
        // With its code, as every refusal of the turn (WP11 server review
        // M1): a context overflow, a budget, a lost candidate — and who the
        // last model call went to.
        let sent = chat_turn::SentAs {
            answered_by: answering.answered_by.clone(),
            images,
        };
        let _ = sink.tx.send(TurnFrame::error_sent(e, sent)).await;
    }

    // A continue whose continuation went into the tool loop: the model's
    // first message of the record carries what it wrote after the prefill,
    // but the prefill itself sat in the request before the record — put it
    // there, so the row's `content` (prefill + everything streamed) starts
    // with the record's text and a replay neither repeats nor loses it
    // (review R1 finding 4).
    if let Some(prefix) = turn.continued_text() {
        prefix_record(&mut messages, prefix);
    }
    // Calls the turn ended before making (the tool-call budget ran out, the
    // page left as they were announced, the model named a tool the thread
    // does not have) get a result too: a record ending in a call with none
    // is one no later request can replay. A thread's loop never hands a
    // call back to a client, so every trailing call was never made.
    agent::close_trailing_calls(&mut messages, |name| {
        Some(if known.contains(name) {
            agent::UNMADE_CALL.to_string()
        } else {
            format!("not run: this thread has no tool named '{name}'")
        })
    });

    // Persist the assistant turn: the visible text plus the IR the loop
    // produced, so the *next* turn replays the tool calls it actually made.
    // A turn that ran no tool leaves an empty record once its answer is
    // trimmed off: that is no record, and stored as none — the reply then
    // replays like a plain one, reasoning included, and can be continued.
    let ir_json = Some(trim_trailing_text(&messages, &sink.text))
        .filter(|record| !record.is_empty())
        .and_then(|record| serde_json::to_string(&record).ok());
    let wrote_nothing = sink.text.is_empty() && sink.reasoning.is_empty() && ir_json.is_none();
    let saved = turn
        .persist(
            &state,
            Reply {
                text: &sink.text,
                reasoning: &sink.reasoning,
                prompt_tokens: sink.usage.prompt_tokens.map(|v| v as i64),
                completion_tokens: sink.usage.completion_tokens.map(|v| v as i64),
                ir_messages: ir_json.as_deref(),
                answered_by: answering.answered_by.clone(),
                stopped: stopped.is_some(),
                failed: err.is_some(),
            },
        )
        .await;

    // A caller that raised its own stop still reads: it gets `done`, which
    // names the partial turn just saved.
    if let Some(why) = stopped.filter(|w| *w != Stopped::Interrupted) {
        return turn.report_stop(why, &tx).await;
    }
    // A turn that failed before it said or ran anything saves nothing, and
    // says so as the plain path does: `done {aborted}`, no message id. Its
    // user message stays owed, and the next send merges with it (§7.4).
    if err.is_some() && wrote_nothing {
        sink.say("done", json!({ "aborted": true }).to_string())
            .await;
        return;
    }
    let ttfb = sink
        .first_at
        .map(|t| t.duration_since(started).as_millis() as i64);
    if saved.refused {
        sink.say(
            "error",
            json!({ "message": NOT_SAVED, "code": "not_saved" }).to_string(),
        )
        .await;
    }
    sink.say(
        "done",
        json!({
            "message_id": saved.id,
            "saved": saved.saved(),
            "model": turn.model(),
            "answered_by": answering.answered_by,
            "prompt_tokens": sink.usage.prompt_tokens,
            "completion_tokens": sink.usage.completion_tokens,
            "ttfb_ms": ttfb,
            "total_ms": started.elapsed().as_millis() as i64,
            "aborted": err.is_some() || stopped.is_some() || held_off,
            "timings": serde_json::Value::Null,
            "reasoning_ignored": answering.ignored,
            // The model reasoned although off was asked, in a sentence.
            "reasoning_note": answering
                .fitted
                .as_ref()
                .and_then(|f| f.note(answering.answered_by.as_deref().unwrap_or(turn.model()))),
        })
        .to_string(),
    )
    .await;
}

/// Put a continue's prefill `prefix` in front of the record's first assistant
/// text (review R1 finding 4). A record with no assistant message (nothing
/// ran) is left alone.
fn prefix_record(record: &mut [Message], prefix: &str) {
    if prefix.is_empty() {
        return;
    }
    let Some(first) = record.iter_mut().find(|m| m.role == Role::Assistant) else {
        return;
    };
    match first.content.iter_mut().find_map(|p| match p {
        ContentPart::Text { text } => Some(text),
        _ => None,
    }) {
        Some(text) => text.insert_str(0, prefix),
        None => {
            // Only reasoning or tool calls: the prefill goes before them.
            let at = first
                .content
                .iter()
                .position(|p| !matches!(p, ContentPart::Reasoning { .. }))
                .unwrap_or(first.content.len());
            first
                .content
                .insert(at, ContentPart::text(prefix.to_string()));
        }
    }
}

/// The stored IR must not repeat the final answer that `content` already holds,
/// or a reopened thread shows it twice. Only the trailing text is dropped — the
/// assistant's tool calls and their results, which `content` cannot represent,
/// are exactly what this column is for.
fn trim_trailing_text(turn: &[Message], text: &str) -> Vec<Message> {
    let mut out = turn.to_vec();
    if text.is_empty() {
        return out;
    }
    if let Some(last) = out.last_mut() {
        if last.role == Role::Assistant {
            last.content
                .retain(|p| !matches!(p, ContentPart::Text { .. }));
            if last.content.is_empty() {
                out.pop();
            }
        }
    }
    out
}
