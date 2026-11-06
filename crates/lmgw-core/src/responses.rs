//! `POST /v1/responses` (§21) — the HTTP handler.
//!
//! Three pieces meet here and nothing else lives in this file:
//! [`ingress::responses`](crate::ingress::responses) owns the wire format,
//! [`agent`](crate::agent) owns the loop, and [`mcp::exec`](crate::mcp::exec)
//! owns tool execution. This is the wiring plus the two decisions that need the
//! request's actual route: whether to forward natively, and what the run's
//! budget is.
//!
//! ## Native passthrough
//!
//! An upstream that [forwards `/v1/responses`](crate::config::Upstream::forwards_responses)
//! — the OpenAI platform, or an `openai_compatible` row with the flag — gets
//! the body forwarded verbatim (bar the model name and the bare call ids
//! of gateway design §7.1). Synthesizing the API on top of a provider
//! that already implements it would be strictly *worse* than proxying: the
//! provider round-trips its own reasoning items between tool calls, and
//! down-translating through `/v1/chat/completions` discards them. Everything
//! else — llama-server above all, which has no such endpoint at all — is
//! served by the loop, and so is a request only the loop can serve: one with
//! `mcp` tools lmgw serves, or one continuing a chain lmgw stored
//! ([`path`]). The passthrough's row carries the tokens and cost the
//! upstream reported, written when its answer ends ([`native`]).
//!
//! ## What a run looks like in the Logs tab
//!
//! One row per **model turn** (`ingress_proto = "responses"`, real tokens,
//! counted in stats) and one per **tool call** (`"responses-tool"`, no tokens,
//! excluded). Deliberately no parent row: it would either double-count the
//! tokens of every turn or report zero, and both are worse than showing the
//! work as it actually happened.
//!
//! ## Stored conversations (stage 2)
//!
//! `store` (default on) persists each response, which is what makes
//! `previous_response_id` — and therefore the `mcp_approval_request` round trip
//! — work. What is replayed is the **IR** conversation, not the wire items: a
//! stored `mcp_call.output` is a flattened string by schema, while the model
//! received the tool's real blocks, so replaying the wire form would degrade
//! every chained turn. Eviction is [chain-aware](crate::store::gc_responses)
//! and lives in Settings + the Responses tab.
//!
//! A stored response is the key's that made it ([`reader`]): an owner key
//! reads and continues every one, any other caller only its own, and another
//! key's id answers as an unknown one. `GET`/`DELETE` and `input_items` also
//! reach a response an upstream made natively ([`retrieve`]).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use tokio_stream::wrappers::ReceiverStream;

use crate::agent::claim::media::MediaClaim;
use crate::agent::claim::{CallOn, LoopClaim};
use crate::agent::{
    self, Budget, DecidedCall, DeltaSink, EventSink, LoopEvent, PendingCall, ResolvedTool,
    RunConfig, RunResult, StopReason, TurnRunner,
};
use crate::config::Route;
use crate::erased::Erased;
use crate::error::GatewayError;
use crate::gate::FallbackReason;
use crate::ingress::responses::{self as wire, ResponsesEncoder, ResponsesRequest};
use crate::ir::{ChatRequest, Completion, Message, Role};
use crate::mcp::exec::{DocsExecutor, McpExecutor, SelfAdminExecutor, SplitExecutor};
use crate::mcp::scope::{ScopedExecutor, ToolScope};
use crate::proxy::{self, RequestCtx};
use crate::state::SharedState;
use crate::store::{self, StoredResponse};
use crate::telemetry::RESPONSES_PROTO;

mod loop_items;
mod native;
mod path;
mod references;
mod retrieve;
mod seals;
mod serve;
mod unseen;

pub use retrieve::{delete_response, get_response, list_input_items};

/// Whose stored responses `ctx` may read and continue (openai egress design
/// §6): an owner key every one, any other caller its own.
pub(crate) fn reader(ctx: &RequestCtx) -> store::ResponseReader {
    if ctx.principal.is_owner() {
        store::ResponseReader::Owner
    } else {
        store::ResponseReader::Key(ctx.principal.key_id())
    }
}

/// `POST /v1/responses`.
pub async fn handle_responses(state: SharedState, ctx: RequestCtx, body: Value) -> Response {
    let started = Instant::now();
    let alias_hint = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();

    // The gate's routing stages (`gate::resolve`: the hold swap, so a local
    // model under a GPU hold is re-routed to its fallback — or refused with
    // `gpu_hold` — before the run is set up; the synthesized loop below
    // carries the route in its runner, so a swap made any later would only
    // reach the first turn, gpu-hold design §4). The media check runs here
    // too, for the same reason the other text routes refuse it before
    // admission: an image or audio alias would start a pipeline for a loop it
    // cannot take one turn of. Admission itself waits until the budget and
    // the stored chain have been checked, so neither refusal starts a
    // container.
    let mut req = match wire::parse_request(&body) {
        Ok(req) => req,
        Err(e) => return reject(&state, &ctx, &alias_hint, None, started, e).await,
    };
    req.ir.anthropic_beta = ctx.anthropic_beta.clone();
    // Whose traffic the run is holds for every turn of it: one admission
    // serves the run (per-request background design §7).
    let routed = match crate::gate::resolve(
        &state,
        &req.ir.model_alias,
        crate::gate::RouteCheck::Text("/v1/responses"),
        ctx.origin(),
    )
    .await
    {
        Ok(r) => r,
        Err(f) => return reject(&state, &ctx, &alias_hint, None, started, f.error).await,
    };
    // A candidate alias refuses a facet it does not enable before anything
    // is set up — here, because the native passthrough below never reaches
    // admission (candidate-aliases §4.6). Taken again after the stored chain
    // is prepended, for its images.
    let reasoning = ctx.reasoning_control();
    let routed = match routed.using(uses(&req, reasoning.as_ref())) {
        Ok(r) => r,
        Err(f) => return reject(&state, &ctx, &alias_hint, None, started, f.error).await,
    };
    // A structured output the model is known not to produce is refused
    // before either path (structured-output design §7).
    let format = req.ir.response_format.as_ref().and_then(|f| f.kind());
    let routed = match routed.serves_format(&state, format).await {
        Ok(r) => r,
        Err(f) => {
            let fallback = f.headers.fallback_reason();
            let resp = reject(&state, &ctx, &alias_hint, fallback, started, f.error).await;
            return f.headers.stamp(resp);
        }
    };

    // Scope and budget (usage-analytics §4.2). A `/v1/responses` run is the
    // single most expensive shape this gateway serves — a tool loop can spend
    // for minutes on one request — so it is exactly the traffic a budget is
    // there to bound, and the check belongs before the loop is set up.
    if let Err(e) = crate::policy::check_alias(
        &state.policy,
        &state.db,
        &state.snapshot(),
        ctx.client_key.as_deref(),
        &req.ir.model_alias,
    )
    .await
    {
        return reject(&state, &ctx, &alias_hint, None, started, e).await;
    }
    // A reasoning seal the client sends back must be one its key was given
    // (openai egress design §7.3.2), on either path.
    if let Err(e) = proxy::seals::check(&state.db, reader(&ctx), &req.ir.messages).await {
        return reject(&state, &ctx, &alias_hint, None, started, e).await;
    }
    serve::serve_routed(
        state,
        ctx,
        req,
        body,
        serve::Ready::Routed(routed),
        started,
        &alias_hint,
        None,
    )
    .await
}

/// The capability facets a run uses (candidate-aliases §4.6): its messages
/// and parameters, tool calls whenever the model is given tools — the
/// client's own function tools or the MCP blocks the loop runs — and what
/// the parts only a native upstream serves use (a hosted tool is a tool
/// call, an image by `file_id` is vision), which the IR does not carry.
fn uses(
    req: &ResponsesRequest,
    header: Option<&crate::ir::ReasoningControl>,
) -> crate::candidates::FacetSet {
    let uses = crate::gate::request_facets(&req.ir, header).union(req.native_only.facets());
    if req.client_tools.is_empty() && req.mcp_tools.is_empty() {
        uses
    } else {
        uses.insert(crate::candidates::Facet::ToolCalls)
    }
}

/// What a native `/v1/responses` upstream cannot be told (§5.5), and what
/// its sends did with an off (`native::reasoning_off`). That API has exactly
/// one reasoning knob — `reasoning.effort` — so a request asking for a
/// budget, or for thinking to be switched on with no level, is reported to
/// the client instead of being quietly dropped into a body that has no field
/// for it. An off goes as `effort: "none"`, and is reported only when it went
/// out in another form, fitted to the model (`fitted`).
fn native_annotations(
    c: &crate::ir::ReasoningControl,
    fitted: &proxy::reasoning_fit::Fitted,
) -> proxy::Annotations {
    let mut ignored = Vec::new();
    if c.enabled == Some(true) && c.effort.is_none() {
        ignored.push("enabled");
    }
    if c.budget_tokens.is_some() {
        ignored.push("budget");
    }
    fitted.report(&mut ignored);
    proxy::Annotations {
        ignored,
        params_refused: fitted.params_refused(),
        ..Default::default()
    }
}

/// `fallback`: the fallback that was already answering when the request was
/// refused — logged, as it is stamped on the response by the caller.
async fn reject(
    state: &SharedState,
    ctx: &RequestCtx,
    alias: &str,
    fallback: Option<FallbackReason>,
    started: Instant,
    e: GatewayError,
) -> Response {
    let status = e.http_status();
    let payload = wire::serialize_error(&e);
    proxy::record_request_failure(
        state,
        ctx,
        RESPONSES_PROTO,
        alias,
        None,
        fallback,
        started,
        &e,
    )
    .await;
    let mut resp = (status, axum::Json(payload)).into_response();
    e.stamp_rate_hints(&mut resp);
    resp
}

// ---------------------------------------------------------------------------
// Continuing a stored conversation
// ---------------------------------------------------------------------------

/// `req` with the conversation its `previous_response_id` continues in
/// front, as a run is sent it ([`load_chain`]) — for a count of what that run
/// would send (`POST /v1/responses/input_tokens`). Nothing is settled or
/// stored, and no approval is decided: a stored response whose pending calls
/// have no verdict yet is counted all the same. Returns whether it has
/// pending calls, whose results are not in the history (the count is
/// approximate then). A request without the id is left as it is.
pub(crate) async fn with_stored_history(
    state: &SharedState,
    ctx: &RequestCtx,
    req: &mut ResponsesRequest,
) -> Result<bool, GatewayError> {
    let Some(prev_id) = req.previous_response_id.clone() else {
        return Ok(false);
    };
    let (stored, _) = load_chain(state, &prev_id, req, None, reader(ctx)).await?;
    // `load_chain` read the record already; an unreadable one failed there.
    Ok(stored
        .pending
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<PendingCall>>(raw).ok())
        .is_some_and(|p| !p.is_empty()))
}

/// The answer for an id that is not the caller's: unknown, evicted, or
/// another key's — one wording, so none of them tells the others apart.
pub(crate) fn no_stored_response(id: &str) -> GatewayError {
    GatewayError::NotFound(format!(
        "no stored response '{id}' — it may have been evicted (see the \
         Responses tab and the retention settings), or it was created while \
         storing was off"
    ))
}

/// Load the response a request continues from, splice its conversation in front
/// of the new turn, and pair its pending calls with the client's verdicts
/// (`by`: who decides them; `None` for a count, which decides nothing).
async fn load_chain(
    state: &SharedState,
    prev_id: &str,
    req: &mut ResponsesRequest,
    by: Option<&crate::agent::approval::Approver>,
    reader: store::ResponseReader,
) -> Result<(StoredResponse, Vec<DecidedCall>), GatewayError> {
    if !state.snapshot().settings.responses_store {
        return Err(GatewayError::Unsupported(
            "'previous_response_id': storing responses is switched off in this gateway \
             (Settings → Retention), so there is nothing to continue from — enable it, or send \
             the full conversation in 'input'"
                .into(),
        ));
    }
    // Another key's response is answered as an unknown one.
    let stored = store::get_response(&state.db, prev_id, reader)
        .await
        .map_err(|e| GatewayError::Internal(format!("reading stored response {prev_id}: {e}")))?;
    let Some(stored) = stored else {
        return Err(no_stored_response(prev_id));
    };

    let history: Vec<Message> = serde_json::from_str(&stored.messages).map_err(|e| {
        GatewayError::Internal(format!("stored response {prev_id} is unreadable: {e}"))
    })?;

    // Settle what the previous run stopped at, before anything else is said.
    let pending: Vec<PendingCall> = match &stored.pending {
        None => Vec::new(),
        Some(raw) => serde_json::from_str(raw).map_err(|e| {
            GatewayError::Internal(format!("stored response {prev_id} is unreadable: {e}"))
        })?,
    };
    // `by: None` is a count, which decides nothing.
    let decided = match by {
        Some(by) => decide(&pending, req, by)?,
        None => Vec::new(),
    };

    // OpenAI does not carry `instructions` across a chained call, so that a
    // client can swap the system prompt. Honored when the client *says*
    // something — including an explicit empty string, which clears it — but a
    // continuation that simply omits the field keeps the prompt the
    // conversation has been running under. Dropping it silently would change
    // the model's behavior mid-conversation with nothing in the request to
    // explain why.
    let mut messages: Vec<Message> = match &req.instructions {
        Some(_) => history
            .into_iter()
            .skip_while(|m| m.role == Role::System)
            .collect(),
        None => history,
    };
    let new_turn = std::mem::take(&mut req.ir.messages);
    match &req.instructions {
        // A new system prompt goes in front of the resumed history, where a
        // system message belongs.
        Some(_) => {
            let (sys, rest): (Vec<Message>, Vec<Message>) =
                new_turn.into_iter().partition(|m| m.role == Role::System);
            let mut out = sys;
            out.append(&mut messages);
            out.extend(rest);
            req.ir.messages = out;
        }
        None => {
            messages.extend(new_turn);
            req.ir.messages = messages;
        }
    }
    Ok((stored, decided))
}

/// Pair each pending call with the client's verdict.
///
/// A gated call with no verdict is an error, not a silent denial: the client
/// asked to be consulted, and pretending it said "no" would hide the fact that
/// its request was incomplete. Denying explicitly (`approve: false`) is how a
/// caller abandons a call it does not want to run.
fn decide(
    pending: &[PendingCall],
    req: &ResponsesRequest,
    by: &crate::agent::approval::Approver,
) -> Result<Vec<DecidedCall>, GatewayError> {
    if pending.is_empty() {
        if let Some(a) = req.approvals.first() {
            return Err(GatewayError::BadRequest(format!(
                "mcp_approval_response '{}' does not match any pending call on the \
                 response being continued",
                a.approval_request_id
            )));
        }
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(pending.len());
    let mut unanswered: Vec<&str> = Vec::new();
    for p in pending {
        let verdict = req
            .approvals
            .iter()
            .find(|a| a.approval_request_id == p.approval_id);
        if p.needs_approval && verdict.is_none() {
            unanswered.push(&p.approval_id);
            continue;
        }
        let approved = verdict.is_none_or(|v| v.approve);
        let denial = match verdict.and_then(|v| v.reason.clone()) {
            Some(r) => format!("The user declined this tool call: {r}"),
            None => "The user declined this tool call.".to_string(),
        };
        // A verdict on a gated call is a decision, by whoever sent it; a
        // sibling held beside one carries nobody (client-apps design §6.3).
        let by = (p.needs_approval && verdict.is_some()).then(|| by.clone());
        out.push(DecidedCall {
            call: p.clone(),
            approved,
            denial,
            by,
        });
    }
    if !unanswered.is_empty() {
        return Err(GatewayError::BadRequest(format!(
            "this response is waiting on approval for {}; send an \
             mcp_approval_response input item for each (approve: false abandons the call)",
            unanswered
                .iter()
                .map(|id| format!("'{id}'"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// The synthesized path
// ---------------------------------------------------------------------------

/// Drives one model turn through the proxy's logged in-process helpers.
struct GatewayRunner {
    state: SharedState,
    route: Route,
    /// Why `route` is a fallback, when it is — every turn's row says so.
    fallback: Option<FallbackReason>,
    /// The caller's key, with its id: every turn's row is recorded against
    /// the key it is, even after a rename mid-loop (A2 review 4).
    key: proxy::KeyRef,
    stream: bool,
    /// GPU admission for the *run*, not for a turn (§9b). A tool loop is many
    /// turns against one model with tool calls in between; the model has to
    /// survive those gaps, and lmgw's in-flight count has to say so. Let go
    /// before an image or audio tool call that needs the GPU, and taken again
    /// by the next turn (`agent::claim`); dropped when the runner is, which
    /// is when the loop ends.
    claim: std::sync::Arc<LoopClaim>,
    /// Whether a unary turn was handed to the fallback by a ladder climb:
    /// the unary answer's headers say so. A stream's headers left before its
    /// first turn, so they keep the rung stamped at open.
    fell_back: proxy::FallbackNote,
    /// The sampling parameters the run's own sends left out because their
    /// model refused them, now or earlier (`x-lmgw-params-refused` of a
    /// unary answer): what *these* sends did, not the route's record, which
    /// a concurrent request may have added to or a fallback's turn may not
    /// share.
    params_refused: std::sync::Mutex<Vec<&'static str>>,
    /// The verbatim Anthropic blocks the run's unary answers carried, which
    /// the Responses wire has no item for ([`Self::note_answer_blocks`]).
    answer_blocks: std::sync::Mutex<Vec<String>>,
    /// The run's images that went to a fallback that cannot see as
    /// placeholders: the WARN names each once per run, not once per turn
    /// (`gate::fallback_images::Announced`).
    announced: crate::gate::fallback_images::Announced,
    /// What the turn [`PerRoute::request`] last gave left out, for its row
    /// (`request_logs.degraded`).
    marker: std::sync::Mutex<Option<String>>,
    /// What every turn carries to be handed on at the send (openai egress
    /// design §14.4): the `/v1/responses` route check, the run's origin — a
    /// guest's run hands on as a guest, so a local fallback is admitted only
    /// where it disturbs nobody — and the chain the run was routed along.
    hand: crate::proxy::reasoning_fit::Handover,
}

/// A fallback that cannot see — the run's route, or one the gate re-routes
/// a turn to — gets the conversation's images as placeholders, put in here
/// so the WARN names each image once per run; the send's own check
/// (`gate::fit_chat`) then finds none left, and the turn's row is marked
/// from here ([`PerRoute::degraded`]).
impl proxy::PerRoute for GatewayRunner {
    fn request<'a>(
        &'a self,
        route: &'a Route,
        _hold: Option<&'a crate::vram::LocalHold>,
        _rerouted: Option<&'a crate::gate::GateHeaders>,
        ir: &'a ChatRequest,
    ) -> Erased<'a, Result<Option<ChatRequest>, GatewayError>> {
        Box::pin(async move {
            let unseen = crate::gate::fallback_images::decide(&self.state, route, ir).await;
            let mut marker = self.marker.lock().unwrap_or_else(|e| e.into_inner());
            let Some(u) = unseen else {
                *marker = None;
                return Ok(None);
            };
            let (sent, n) = self.announced.without_images(ir, &route.upstream, &u);
            *marker = Some(u.marker(n));
            Ok(Some(sent))
        })
    }

    fn fitted(&self, fitted: &crate::proxy::reasoning_fit::Fitted) {
        let mut seen = self
            .params_refused
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for name in fitted.params_refused() {
            if !seen.contains(&name) {
                seen.push(name);
            }
        }
    }

    fn degraded(&self) -> Option<String> {
        self.marker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn handover(
        &self,
        rerouted: Option<&crate::gate::GateHeaders>,
    ) -> crate::proxy::reasoning_fit::Handover {
        let mut hand = self.hand.clone();
        if let Some(h) = rerouted {
            hand.chain = h.chain().clone();
        }
        hand
    }
}

impl TurnRunner for GatewayRunner {
    fn run_turn<'a>(
        &'a self,
        ir: &'a ChatRequest,
        deadline: Duration,
        sink: &'a mut dyn DeltaSink,
    ) -> Erased<'a, Result<Completion, GatewayError>> {
        Box::pin(async move {
            let started = Instant::now();
            // The run's claim, regained when a tool call let it go — or where
            // the gate sends the turn when a GPU block or a lost candidate
            // refused that (`agent::claim`).
            let call = match self.claim.call_on(&self.state, ir).await {
                Ok(call) => call,
                Err(r) => {
                    let route = r.route.as_deref().unwrap_or(&self.route);
                    let fallback = if r.gate { r.fallback } else { self.fallback };
                    crate::agent::claim::refused::record(
                        &self.state,
                        (self.key.clone(), &ir.model_alias),
                        RESPONSES_PROTO,
                        route,
                        fallback,
                        started,
                        &r.error,
                    )
                    .await;
                    return Err(r.error);
                }
            };
            let (hold, route, fallback, rerouted) = match &call {
                CallOn::Claim(hold) => (hold.as_ref(), &self.route, self.fallback, None),
                CallOn::Rerouted(o) => (
                    o.hold.as_ref(),
                    &o.route,
                    o.headers.fallback_reason(),
                    Some(&o.headers),
                ),
            };
            let deadline = deadline.saturating_sub(started.elapsed());
            let answered = if self.stream {
                proxy::stream_once_on(
                    &self.state,
                    hold,
                    route,
                    fallback,
                    ir,
                    RESPONSES_PROTO,
                    self.key.clone(),
                    deadline,
                    sink,
                    Some((self, rerouted)),
                )
                .await
            } else {
                proxy::sample_once_noting(
                    &self.state,
                    hold,
                    route,
                    fallback,
                    ir,
                    RESPONSES_PROTO,
                    self.key.clone(),
                    deadline,
                    Some(&self.fell_back),
                    Some((self, rerouted)),
                )
                .await
            };
            if let Ok(c) = &answered {
                self.note_answer_blocks(c);
            }
            answered
        })
    }
}

impl GatewayRunner {
    /// The verbatim Anthropic blocks of a turn's answer, which the
    /// Responses wire has no item for: the run's record keeps them (for a
    /// later turn to the same row), the client is not given them. A unary
    /// answer's headers name them (`answer.<type>` in
    /// `x-lmgw-params-ignored`); a stream's headers left before its first
    /// turn, so a stream's are a log line.
    fn note_answer_blocks(&self, c: &Completion) {
        let names = crate::ir::ignored_names("answer", &c.content);
        if names.is_empty() {
            return;
        }
        if self.stream {
            tracing::info!(
                blocks = %names.join(","),
                "a streamed answer's Anthropic blocks have no /v1/responses item; not relayed"
            );
            return;
        }
        let mut seen = self.answer_blocks.lock().unwrap_or_else(|e| e.into_inner());
        for name in names {
            if !seen.contains(&name) {
                seen.push(name);
            }
        }
    }
}

/// Feeds loop events into the encoder, and the encoder's frames to the client
/// when streaming. `tx: None` is the non-streaming path: the encoder still runs
/// (so it builds the same `output` array) and simply emits nothing.
struct EncoderSink<'a> {
    enc: &'a mut ResponsesEncoder,
    tx: Option<tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>>,
}

impl EventSink for EncoderSink<'_> {
    fn emit<'a>(&'a mut self, ev: LoopEvent) -> Erased<'a, bool> {
        Box::pin(async move {
            let frames = self.enc.on_event(&ev);
            match &self.tx {
                None => true,
                Some(tx) => {
                    if frames.is_empty() {
                        return true;
                    }
                    // A closed channel means the client hung up; returning false
                    // aborts the loop instead of running more turns and tool calls
                    // for a response nobody is reading.
                    tx.send(Ok(Bytes::from(frames))).await.is_ok()
                }
            }
        })
    }
}

/// The run's budget: the gateway's ceiling, which the request may lower.
///
/// Lower but never raise — the Settings values are what the machine's owner
/// agreed to spend, and a client should not be able to talk the gateway into
/// a longer run than that.
fn budget_for(state: &SharedState, req: &ResponsesRequest) -> Budget {
    let s = &state.snapshot().settings;
    Budget {
        max_tool_calls: req.max_tool_calls.map_or(s.responses_max_tool_calls, |r| {
            r.min(s.responses_max_tool_calls)
        }),
        wall_clock: Duration::from_secs(s.responses_timeout_seconds.max(1)),
    }
}

/// Whether this response will be persisted: the gateway's switch, which the
/// request may turn off but not on.
fn stores(state: &SharedState, req: &ResponsesRequest) -> bool {
    state.snapshot().settings.responses_store && req.store.unwrap_or(true)
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    state: SharedState,
    ctx: RequestCtx,
    req: ResponsesRequest,
    route: Route,
    fallback: Option<FallbackReason>,
    body: Value,
    started: Instant,
    prior: Option<StoredResponse>,
    resume: Vec<DecidedCall>,
    admission: Option<crate::vram::LocalHold>,
    headers: &mut crate::gate::GateHeaders,
    params_refused: &mut Vec<&'static str>,
    answer_blocks: &mut Vec<String>,
) -> Response {
    let budget = budget_for(&state, &req);
    // Who is asking decides which of the attached labels' tools the run gets:
    // a client key's tool scope, an agent token's manifest, and the `lmgw`
    // toolset for an owner credential only.
    let scope = ToolScope::of_request(&state, &ctx).await;
    let resolved = crate::mcp::exec::resolve(&state, &req.mcp_tools, &scope).await;

    let mut tools: Vec<ResolvedTool> = resolved.tools;
    tools.extend(
        req.client_tools
            .iter()
            .cloned()
            .map(ResolvedTool::client_side),
    );

    let store_it = stores(&state, &req);
    let echo = serde_json::json!({
        "instructions": req.instructions,
        "metadata": req.metadata,
        "max_tool_calls": budget.max_tool_calls,
        "parallel_tool_calls": req.parallel_tool_calls,
        "tools": body.get("tools").cloned().unwrap_or(Value::Array(vec![])),
        "tool_choice": body.get("tool_choice").cloned().unwrap_or(Value::String("auto".into())),
        "temperature": body.get("temperature").cloned().unwrap_or(Value::Null),
        "top_p": body.get("top_p").cloned().unwrap_or(Value::Null),
        "max_output_tokens": body.get("max_output_tokens").cloned().unwrap_or(Value::Null),
        "previous_response_id": req.previous_response_id,
        "store": store_it,
    });

    let stream = req.stream;
    let mut enc = ResponsesEncoder::new(&req.ir.model_alias, echo, stream);
    // A guest's run (per-request background design §7): sent with
    // `x-lmgw-background: on`, or admitted as one through a background
    // candidate alias. Its streamed run ends at its client's hang-up, a
    // tool call in flight included; the owner's stops its model call then
    // and lets a tool call in flight finish (§11.16).
    let guest = ctx.origin() == crate::runtime::registry::Origin::Background
        || admission
            .as_ref()
            .is_some_and(|h| h.origin() == crate::runtime::registry::Origin::Background);
    let runner = GatewayRunner {
        state: state.clone(),
        route,
        fallback,
        // The client's own request: its answers' items are recorded under
        // its key, none included (`proxy::answer_items`).
        key: proxy::KeyRef {
            public: true,
            ..ctx.key_ref()
        },
        stream,
        claim: std::sync::Arc::new(LoopClaim::new(admission)),
        fell_back: proxy::FallbackNote::default(),
        params_refused: Default::default(),
        answer_blocks: Default::default(),
        announced: Default::default(),
        marker: Default::default(),
        hand: crate::proxy::reasoning_fit::Handover {
            check: crate::gate::RouteCheck::Text("/v1/responses"),
            origin: match guest {
                true => crate::runtime::registry::Origin::Background,
                false => ctx.origin(),
            },
            chain: headers.chain().clone(),
            deferred: None,
        },
    };
    // One executor across all three planes. The built-in half only ever fires
    // for names this request's own `{"type":"mcp"}` blocks resolved (nothing is
    // attached implicitly), so an ordinary request cannot reach `lmgw__*` by
    // having the model guess a name.
    let docs_exec = {
        let e =
            DocsExecutor::new(state.clone(), ctx.clone()).charged_to(crate::devices::charged(&ctx));
        // The gateway key's name is the only identity a `/v1/responses` caller
        // has, and `docs__request` exists to tell the owner who asked.
        match &ctx.client_key {
            Some(key) => e.with_client(key.clone()),
            None => e,
        }
    };
    let exec = ScopedExecutor::new(
        SplitExecutor::new(
            SelfAdminExecutor::new(state.clone(), ctx.clone()),
            docs_exec,
            resolved.builtin.clone(),
            // A name runs where this request listed it from, or not at all.
            McpExecutor::new(state.clone(), ctx.clone()).with_listed(resolved.servers),
        )
        // A call that leaves out `model` runs on the default the listed
        // tool named (chat-media-tools §3.8).
        .with_media_defaults(resolved.media_defaults)
        // An image or audio call that needs the GPU gets the run's room
        // (chat-media-tools §4.3).
        .with_media_hooks(std::sync::Arc::new(MediaClaim::new(
            state.clone(),
            runner.claim.clone(),
        ))),
        state.clone(),
        ctx.clone(),
    );
    // The tools the gateway runs: a stored response closes the ones a stop
    // left unmade (`persist`).
    let server_tools: HashSet<String> = tools
        .iter()
        .filter(|t| t.server_label.is_some())
        .map(|t| t.def.name.clone())
        .collect();
    let cfg = RunConfig {
        tools,
        budget,
        parallel_tool_calls: req.parallel_tool_calls,
        resume,
        // A unary run is ended by its client going away with the handler
        // future, which hyper drops; a streamed run gets its hang-up as a
        // cancel below. There is no job behind either to press Cancel on.
        cancel: agent::Cancel::none(),
    };
    let save = SaveCtx {
        enabled: store_it,
        alias: req.ir.model_alias.clone(),
        chain_id: prior.as_ref().map(|p| p.chain_id.clone()),
        previous_response_id: req.previous_response_id.clone(),
        input_items: req.input_items.clone(),
        server_tools,
        key_id: ctx.principal.key_id(),
    };

    if !stream {
        let mut sink = EncoderSink {
            enc: &mut enc,
            tx: None,
        };
        for (label, defs) in &resolved.listed {
            sink.enc.push_mcp_list_tools(label, defs);
        }
        for f in &resolved.failed {
            sink.enc.push_mcp_list_failed(&f.label, &f.why);
        }
        let outcome = agent::run(req.ir, cfg, &runner, &exec, &mut sink).await;
        // A turn a ladder climb handed to the fallback (review finding 13):
        // the unary answer leaves only now, so it can say so — the fallback
        // replaces the rung stamped at open.
        if let Some(chain) = runner.fell_back.take() {
            headers.handed_on(chain);
        }
        *params_refused = std::mem::take(
            &mut *runner
                .params_refused
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        *answer_blocks = std::mem::take(
            &mut *runner
                .answer_blocks
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        // The run's claim goes before its bookkeeping: nothing below calls a
        // model.
        drop(exec);
        drop(runner);
        return match outcome {
            Ok(result) => {
                persist(&state, &save, &enc, &result).await;
                (StatusCode::OK, axum::Json(enc.snapshot())).into_response()
            }
            Err(e) => {
                // The turns themselves already logged; this records the
                // request-level outcome the client actually saw.
                let e = e.error;
                let status = e.http_status();
                proxy::record_request_failure(
                    &state,
                    &ctx,
                    RESPONSES_PROTO,
                    &save.alias,
                    None,
                    fallback,
                    started,
                    &e,
                )
                .await;
                // A non-2xx answer carries OpenAI's error envelope (`message`,
                // `type`, `param`, `code`), which is what the platform sends
                // and what every SDK reads one as (api-compat design §9,
                // finding 10; *changed 2026-11-05*: it was the failed run's
                // Response object, which only a structured-output refusal did
                // not get, structured-output design §11.15). The failed
                // Response is a stream's `response.failed`. Nothing is
                // stored: the client was given no response id to fetch.
                // An upstream rate refusal keeps its own headers, as on the
                // chat routes (openai egress design §8).
                let mut resp = (status, axum::Json(wire::serialize_error(&e))).into_response();
                e.stamp_rate_hints(&mut resp);
                resp
            }
        };
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    // The client closing its stream is the run's cancel, raised at once
    // (per-request background design §9, §11.16). The sink alone saw the
    // hang-up only at the next frame it failed to send, which a long tool
    // call, a long prefill or a model writing the client nothing put far
    // away, while the model went on generating.
    //
    // A guest's run ends then and there (*changed 2026-10-22*): the cancel
    // is raced against every await in it — a model call, and a tool call
    // that can take minutes — so its claim on the model, and a tool's own
    // image or audio claim, go at once; a guest's claim held that long
    // blocks the owner it promised not to disturb. The cost is the guest's:
    // a tool call that does not take the stop (a self-admin op, `docs__*`,
    // `kb__*`, an MCP server's) is dropped mid-way — it writes no row, and
    // the stored response has it abandoned instead of its result.
    //
    // The owner's run stops its model call at once and lets a tool call in
    // flight finish with its rows and result, then starts no other call and
    // no other turn (*changed 2026-10-24*, the owner's decision).
    let (hang_up, hung_up) = proxy::stop_pair();
    let cancel = agent::Cancel::signal(hung_up);
    let cfg = cfg.with_cancel(if guest {
        cancel
    } else {
        cancel.sparing_calls()
    });
    // A device's connection gets `TCP_USER_TIMEOUT` for as long as the
    // stream runs, and its accept-time timers back after it (device link
    // design §4.3).
    let timers = crate::server::device_listener::chat_stream_timers(
        ctx.conn.as_ref(),
        &state.snapshot().settings,
    );
    tokio::spawn(async move {
        let _timers = timers;
        let send = |s: String| {
            let tx = tx.clone();
            async move { s.is_empty() || tx.send(Ok(Bytes::from(s))).await.is_ok() }
        };
        let opened = send(enc.start()).await;
        if !opened {
            return;
        }
        for (label, defs) in &resolved.listed {
            let f = enc.push_mcp_list_tools(label, defs);
            if !send(f).await {
                return;
            }
        }
        for failed in &resolved.failed {
            let f = enc.push_mcp_list_failed(&failed.label, &failed.why);
            if !send(f).await {
                return;
            }
        }
        let ir = req.ir;
        let result = {
            let mut sink = EncoderSink {
                enc: &mut enc,
                tx: Some(tx.clone()),
            };
            let run = agent::run(ir, cfg, &runner, &exec, &mut sink);
            tokio::pin!(run);
            tokio::select! {
                r = &mut run => r,
                // Raised, the run winds down — a model call and a tool call
                // that took its stop still write their rows, an owner's tool
                // call in flight finishes — and what it made is stored below.
                () = tx.closed() => {
                    hang_up.stop();
                    run.await
                }
            }
        };
        // The run's claim goes before its bookkeeping: nothing below calls a
        // model.
        drop(exec);
        drop(runner);
        match result {
            // Stored even when the client hung up mid-stream: the work was done
            // and paid for, and a chained retry should be able to pick it up.
            Ok(r) => persist(&state, &save, &enc, &r).await,
            Err(e) => {
                let f = enc.fail(&e.error);
                let _ = send(f).await;
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// What `persist` needs that the encoder and the run result don't carry.
struct SaveCtx {
    enabled: bool,
    alias: String,
    /// `Some` when continuing an existing conversation.
    chain_id: Option<String>,
    previous_response_id: Option<String>,
    input_items: Value,
    /// The run's server-side tools, by name.
    server_tools: HashSet<String>,
    /// The key that made the response ([`StoredResponse::key_id`]).
    key_id: Option<i64>,
}

/// Write the finished response to the store, so a client can `GET` it and chain
/// from it. A failure here is logged, never fatal: the client already has (or
/// is still reading) a perfectly good response, and turning a successful
/// generation into a 500 over a bookkeeping problem would be the worse outcome.
async fn persist(state: &SharedState, save: &SaveCtx, enc: &ResponsesEncoder, run: &RunResult) {
    if !save.enabled {
        return;
    }
    let id = enc.id().to_string();
    let usage = enc.usage();
    // A stop that left server-side calls unmade (the tool-call budget ran
    // out, the client went away while they were announced) closes them, so a
    // chained request replays every call with its result and a strict
    // upstream answers it (chat-voice design §7.3). A client's calls stay
    // open for the client, and calls handed back for a client tool or an
    // approval are settled by the next request.
    let mut messages = run.messages.clone();
    if !matches!(run.reason, StopReason::ClientTool | StopReason::Approval) {
        agent::close_trailing_calls(&mut messages, |name| {
            save.server_tools
                .contains(name)
                .then(|| agent::UNMADE_CALL.to_string())
        });
    }
    let row = StoredResponse {
        chain_id: save.chain_id.clone().unwrap_or_else(|| id.clone()),
        id,
        previous_response_id: save.previous_response_id.clone(),
        model: save.alias.clone(),
        status: enc.status().to_string(),
        body: enc.snapshot().to_string(),
        input_items: save.input_items.to_string(),
        messages: serde_json::to_string(&messages).unwrap_or_else(|_| "[]".into()),
        pending: match run.reason {
            StopReason::Approval => serde_json::to_string(&run.pending).ok(),
            _ => None,
        },
        input_tokens: usage.prompt_tokens.map(|v| v as i64),
        output_tokens: usage.completion_tokens.map(|v| v as i64),
        created_at: String::new(), // filled by the column default
        key_id: save.key_id,
    };
    if let Err(e) = store::insert_response(&state.db, &row).await {
        tracing::error!("storing response {}: {e}", row.id);
    }
}
