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
//! An upstream flagged [`supports_responses`](crate::config::Upstream::supports_responses)
//! gets the body forwarded verbatim (bar the model name and the bare call ids
//! of gateway design §7.1). Synthesizing the API on top of a provider
//! that already implements it would be strictly *worse* than proxying: the
//! provider round-trips its own reasoning items between tool calls, and
//! down-translating through `/v1/chat/completions` discards them. Everything
//! else — llama-server above all, which has no such endpoint at all — is
//! served by the loop.
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

use std::collections::HashSet;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use crate::agent::{
    self, Budget, DecidedCall, DeltaSink, EventSink, LoopEvent, PendingCall, ResolvedTool,
    RunConfig, RunResult, StopReason, TurnRunner,
};
use crate::config::Route;
use crate::egress::{apply_bearer_auth, for_protocol};
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

mod unseen;

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
    let routed = match crate::gate::resolve(
        &state,
        &req.ir.model_alias,
        crate::gate::RouteCheck::Text("/v1/responses"),
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
    if routed.resolved().upstream.supports_responses {
        // The provider implements the API — including its own store — so
        // `previous_response_id` and approvals are its business, not ours.
        //
        // The control is resolved through all three tiers here too (§5.2): the
        // alias' `param_defaults.reasoning` is as much a part of what this
        // alias means on a native upstream as on a synthesized one, and
        // reading only the headers would have made the alias default apply
        // everywhere except the one route that forwards the body verbatim.
        let route = routed.resolved();
        let control = proxy::resolve_params(&req.ir, &ctx, route).reasoning_control();
        let fallback = routed.headers().fallback_reason();
        // A fallback that cannot see gets the body's images as placeholders,
        // as every other send gets the IR's (`gate::fallback_images`).
        let mut body = body;
        let omitted = unseen::fit(&state, route, &req.ir.model_alias, &mut body).await;
        let resp = native_passthrough(
            &state,
            &ctx,
            (route, fallback),
            &body,
            started,
            &control,
            omitted.as_ref().map(|o| o.marker.clone()),
        )
        .await;
        let annotations = proxy::Annotations {
            images_omitted: omitted.map(|o| o.count),
            ..native_annotations(&control)
        };
        return proxy::with_annotations(routed.headers().stamp(resp), &annotations);
    }

    // Continue a stored conversation, if asked. This both prepends its history
    // and settles whatever calls it stopped at.
    let mut prior: Option<StoredResponse> = None;
    let mut resume: Vec<DecidedCall> = Vec::new();
    if let Some(prev_id) = req.previous_response_id.clone() {
        // Whoever continues the response decides its pending calls: the
        // request's own principal.
        let by = crate::mcp::host::CallFrom::of(&ctx).caller;
        match load_chain(&state, &prev_id, &mut req, &by).await {
            Ok((stored, decided)) => {
                prior = Some(stored);
                resume = decided;
            }
            // Refused *after* the hold already picked a fallback, so the
            // rejection is attributed to it like every other error on this
            // route (gpu-hold design §4, `proxy::Failed`).
            Err(e) => {
                let fallback = routed.headers().fallback_reason();
                let resp = reject(&state, &ctx, &alias_hint, fallback, started, e).await;
                return routed.headers().stamp(resp);
            }
        }
    }

    // GPU admission (§9b) for the whole synthesized run — the gate's last
    // per-request stage: make room for a local model that is not resident, or
    // refuse by name, before the first turn. A local route comes back on the
    // port its container answers on (§5); the turn loop below carries this
    // route in its runner, so that reaches every turn, not only the first.
    // The stored chain is prepended by now, so this sees every image the
    // run will send (`Routed::using`).
    let admitted = match routed.using(uses(&req, reasoning.as_ref())) {
        Ok(routed) => routed.admit(&state).await,
        Err(f) => Err(f),
    };
    let crate::gate::Opened {
        mut route,
        hold: admission,
        mut headers,
    } = match admitted {
        Ok(o) => o,
        Err(f) => {
            let fallback = f.headers.fallback_reason();
            let resp = reject(&state, &ctx, &alias_hint, fallback, started, f.error).await;
            return f.headers.stamp(resp);
        }
    };

    // §5.2/§5.4, once for the whole run: every turn the loop makes reads the
    // same `req.ir.params` and the same `route.param_defaults`, so resolving
    // here reaches turn 7 as surely as turn 1.
    let max_tokens_defaulted =
        proxy::anthropic_max_tokens_default(&state, &mut route, &req.ir.params, req.stream).await;
    let resolved = proxy::resolve_params(&req.ir, &ctx, &route);
    let mut annotations = proxy::Annotations {
        ignored: proxy::reasoning_ignored(
            route.upstream.protocol,
            &resolved.reasoning_control(),
            crate::egress::openai::has_reasoning_object(&req.ir),
        ),
        max_tokens_defaulted,
        max_tokens_raised: proxy::anthropic_max_tokens_raised(&route, &resolved),
        // The headers leave before the first turn runs, so this is what every
        // turn's own per-send clamp will be — they all send these `params`.
        // Each turn logs its own value as it runs.
        max_tokens_clamped: crate::gate::planned_clamp(admission.as_ref(), &req.ir, &resolved),
        // Likewise the images the run opened with that go to a fallback
        // that cannot see as placeholders (`gate::fallback_images`).
        images_omitted: crate::gate::fallback_images::decide(&state, &route, &req.ir)
            .await
            .map(|_| crate::gate::fallback_images::count_images(&req.ir)),
    };
    // Likewise `x-lmgw-rung`: the rung the model runs as the run opens
    // (ladder design §6). A turn that climbs logs the rung it climbed to; a
    // unary run one of whose turns the fallback answered says so instead
    // (`run_loop`).
    headers.set_rung(crate::gate::planned_rung(admission.as_ref()));
    req.ir.params = resolved;

    let fallback = headers.fallback_reason();
    let resp = run_loop(
        state.clone(),
        ctx,
        req,
        route,
        fallback,
        body,
        started,
        prior,
        resume,
        admission,
        &mut headers,
    )
    .await;
    // A unary run a ladder climb handed to the fallback mid-loop (`run_loop`
    // set the fallback, which was not there at open): that turn was sent the
    // client's own body, never clamped, so the clamp planned at open does not
    // describe the answer — like the rung, it goes (ladder §12 entry 72).
    if fallback.is_none() && headers.fallback_reason().is_some() {
        annotations.max_tokens_clamped = None;
    }
    proxy::with_annotations(headers.stamp(resp), &annotations)
}

/// The capability facets a run uses (candidate-aliases §4.6): its messages
/// and parameters, and tool calls whenever the model is given tools — the
/// client's own function tools or the MCP blocks the loop runs.
fn uses(
    req: &ResponsesRequest,
    header: Option<&crate::ir::ReasoningControl>,
) -> crate::candidates::FacetSet {
    let uses = crate::gate::request_facets(&req.ir, header);
    if req.client_tools.is_empty() && req.mcp_tools.is_empty() {
        uses
    } else {
        uses.insert(crate::candidates::Facet::ToolCalls)
    }
}

/// What a native `/v1/responses` upstream cannot be told (§5.5). That API has
/// exactly one reasoning knob — `reasoning.effort` — so a request asking for a
/// budget, or for thinking to be switched off wholesale, is reported to the
/// client instead of being quietly dropped into a body that has no field for
/// it.
fn native_annotations(c: &crate::ir::ReasoningControl) -> proxy::Annotations {
    let mut ignored = Vec::new();
    if c.enabled.is_some() && c.effort.is_none() {
        ignored.push("enabled");
    }
    if c.budget_tokens.is_some() {
        ignored.push("budget");
    }
    proxy::Annotations {
        ignored,
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
    (status, axum::Json(payload)).into_response()
}

// ---------------------------------------------------------------------------
// Continuing a stored conversation
// ---------------------------------------------------------------------------

/// Load the response a request continues from, splice its conversation in front
/// of the new turn, and pair its pending calls with the client's verdicts.
async fn load_chain(
    state: &SharedState,
    prev_id: &str,
    req: &mut ResponsesRequest,
    by: &crate::agent::approval::Approver,
) -> Result<(StoredResponse, Vec<DecidedCall>), GatewayError> {
    if !state.snapshot().settings.responses_store {
        return Err(GatewayError::Unsupported(
            "'previous_response_id': storing responses is switched off in this gateway \
             (Settings → Retention), so there is nothing to continue from — enable it, or send \
             the full conversation in 'input'"
                .into(),
        ));
    }
    let stored = store::get_response(&state.db, prev_id)
        .await
        .map_err(|e| GatewayError::Internal(format!("reading stored response {prev_id}: {e}")))?;
    let Some(stored) = stored else {
        return Err(GatewayError::NotFound(format!(
            "no stored response '{prev_id}' — it may have been evicted (see the \
             Responses tab and the retention settings), or it was created while \
             storing was off"
        )));
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
    let decided = decide(&pending, req, by)?;

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
// Native passthrough
// ---------------------------------------------------------------------------

/// Forward the body to an upstream that implements `/v1/responses` itself,
/// rewriting only the model name, the reasoning effort and any input item's
/// `call_id` that carries a Gemini thought signature (to its bare id). Streams the response through untouched, so
/// reasoning items, hosted-tool events and anything the provider adds later
/// reach the client intact — the whole reason for not synthesizing here.
/// `degraded`: what the body's content lost to a fallback that cannot see,
/// for its row (`request_logs.degraded`).
async fn native_passthrough(
    state: &SharedState,
    ctx: &RequestCtx,
    (route, fallback): (&Route, Option<FallbackReason>),
    body: &Value,
    started: Instant,
    control: &crate::ir::ReasoningControl,
    degraded: Option<String>,
) -> Response {
    let alias = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let streamed = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let mut upstream_body = body.clone();
    upstream_body["model"] = Value::String(route.upstream_model.clone());
    // An earlier step answered by Gemini (a fallback, an alias switched
    // mid-conversation) left its thought signatures in the call ids; they
    // stay with Gemini, as on every synthesized send (gateway design §7.1).
    crate::ir::wire_call_ids_in_responses_body(&mut upstream_body);
    // The resolved control — headers over body over alias defaults (§5.2).
    // `reasoning.effort` is the only control this API can carry, so it is the
    // only one written in; the rest are named in the response's
    // `x-lmgw-reasoning-ignored` by the caller. Writing it back is not a no-op
    // even when it came from the body: a header or an alias default may have
    // replaced the level the body asked for.
    if let Some(effort) = control.effort.clone() {
        match upstream_body.get_mut("reasoning") {
            Some(Value::Object(o)) => {
                o.insert("effort".into(), Value::String(effort));
            }
            _ => upstream_body["reasoning"] = json!({"effort": effort}),
        }
    }

    let url = format!("{}/responses", route.upstream.base());
    let rb = apply_bearer_auth(state.http.post(&url).json(&upstream_body), &route.upstream);
    // `None` — the upstream's ceiling is 0, the maximum possible — leaves the
    // send unbounded here; the run as a whole is still bounded by
    // `responses_timeout_seconds` one level up, which is where a passthrough
    // /v1/responses call gets its visible ceiling either way.
    let result = match route.upstream.request_timeout() {
        Some(d) => tokio::time::timeout(d, rb.send()).await,
        None => Ok(rb.send().await),
    };

    let resp = match result {
        Err(_) => {
            return passthrough_error(
                state,
                ctx,
                &alias,
                (route, fallback),
                started,
                GatewayError::Timeout,
                degraded,
            )
            .await
        }
        Ok(Err(e)) => {
            return passthrough_error(
                state,
                ctx,
                &alias,
                (route, fallback),
                started,
                GatewayError::from(e),
                degraded,
            )
            .await
        }
        Ok(Ok(r)) => r,
    };
    let status = resp.status();
    if !status.is_success() {
        let bytes = resp.bytes().await.unwrap_or_default();
        // `map_error`'s backstop can return `GatewayError::ContextExceeded`
        // with an empty `model` — it has no route to name one. Fill it in
        // the same way `gate::fit::attribute` does at every other local chat
        // site (review finding 6): this is native passthrough, but the model
        // still overflowed *this* row's context.
        let e = crate::gate::attribute(
            for_protocol(route.upstream.protocol).map_error(status.as_u16(), &bytes),
            route,
        );
        return passthrough_error(state, ctx, &alias, (route, fallback), started, e, degraded)
            .await;
    }

    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(if streamed {
            "text/event-stream"
        } else {
            "application/json"
        })
        .to_string();
    let ttfb = started.elapsed().as_millis() as i64;

    // Usage is inside the provider's own body; parsing the stream just to count
    // tokens would mean buffering it, so the row records latency and status and
    // leaves tokens NULL rather than guessing.
    proxy::record_passthrough(
        state,
        ctx,
        RESPONSES_PROTO,
        &alias,
        route,
        fallback,
        started,
        Some(ttfb),
        streamed,
        StatusCode::OK.as_u16(),
        None,
        degraded,
    )
    .await;

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap()
}

async fn passthrough_error(
    state: &SharedState,
    ctx: &RequestCtx,
    alias: &str,
    (route, fallback): (&Route, Option<FallbackReason>),
    started: Instant,
    e: GatewayError,
    degraded: Option<String>,
) -> Response {
    let status = e.http_status();
    let payload = wire::serialize_error(&e);
    proxy::record_passthrough(
        state,
        ctx,
        RESPONSES_PROTO,
        alias,
        route,
        fallback,
        started,
        None,
        false,
        status.as_u16(),
        Some((e.kind().to_string(), e.to_string())),
        degraded,
    )
    .await;
    (status, axum::Json(payload)).into_response()
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
    /// survive those gaps, and lmgw's in-flight count has to say so. Dropped
    /// when the runner is, which is when the loop ends.
    _admission: Option<crate::vram::LocalHold>,
    /// Whether a unary turn was handed to the fallback by a ladder climb:
    /// the unary answer's headers say so. A stream's headers left before its
    /// first turn, so they keep the rung stamped at open.
    fell_back: proxy::FallbackNote,
    /// The run's images that went to a fallback that cannot see as
    /// placeholders: the WARN names each once per run, not once per turn
    /// (`gate::fallback_images::Announced`).
    announced: crate::gate::fallback_images::Announced,
    /// What the turn [`PerRoute::request`] last gave left out, for its row
    /// (`request_logs.degraded`).
    marker: std::sync::Mutex<Option<String>>,
}

/// A fallback that cannot see — the run's route, or one the gate re-routes
/// a turn to — gets the conversation's images as placeholders, put in here
/// so the WARN names each image once per run; the send's own check
/// (`gate::fit_chat`) then finds none left, and the turn's row is marked
/// from here ([`PerRoute::degraded`]).
#[async_trait]
impl proxy::PerRoute for GatewayRunner {
    async fn request(
        &self,
        route: &Route,
        _hold: Option<&crate::vram::LocalHold>,
        _rerouted: Option<&crate::gate::GateHeaders>,
        ir: &ChatRequest,
    ) -> Result<Option<ChatRequest>, GatewayError> {
        let unseen = crate::gate::fallback_images::decide(&self.state, route, ir).await;
        let mut marker = self.marker.lock().unwrap_or_else(|e| e.into_inner());
        let Some(u) = unseen else {
            *marker = None;
            return Ok(None);
        };
        let (sent, n) = self.announced.without_images(ir, &u);
        *marker = Some(u.marker(n));
        Ok(Some(sent))
    }

    fn degraded(&self) -> Option<String> {
        self.marker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[async_trait]
impl TurnRunner for GatewayRunner {
    async fn run_turn(
        &self,
        ir: &ChatRequest,
        deadline: Duration,
        sink: &mut dyn DeltaSink,
    ) -> Result<Completion, GatewayError> {
        if self.stream {
            proxy::stream_once_on(
                &self.state,
                self._admission.as_ref(),
                &self.route,
                self.fallback,
                ir,
                RESPONSES_PROTO,
                self.key.clone(),
                deadline,
                sink,
                Some((self, None)),
            )
            .await
        } else {
            proxy::sample_once_noting(
                &self.state,
                self._admission.as_ref(),
                &self.route,
                self.fallback,
                ir,
                RESPONSES_PROTO,
                self.key.clone(),
                deadline,
                Some(&self.fell_back),
                Some((self, None)),
            )
            .await
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

#[async_trait]
impl EventSink for EncoderSink<'_> {
    async fn emit(&mut self, ev: LoopEvent) -> bool {
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
    let runner = GatewayRunner {
        state: state.clone(),
        route,
        fallback,
        key: ctx.key_ref(),
        stream,
        _admission: admission,
        fell_back: proxy::FallbackNote::default(),
        announced: Default::default(),
        marker: Default::default(),
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
        ),
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
        // A Responses run is ended by its client going away, which the encoder
        // sink reports by returning `false`; there is no job behind it to press
        // Cancel on.
        cancel: agent::Cancel::none(),
    };
    let save = SaveCtx {
        enabled: store_it,
        alias: req.ir.model_alias.clone(),
        chain_id: prior.as_ref().map(|p| p.chain_id.clone()),
        previous_response_id: req.previous_response_id.clone(),
        input_items: req.input_items.clone(),
        server_tools,
    };

    if !stream {
        let mut sink = EncoderSink {
            enc: &mut enc,
            tx: None,
        };
        for (label, defs) in &resolved.listed {
            sink.enc.push_mcp_list_tools(label, defs);
        }
        for (label, err) in &resolved.failed {
            sink.enc.push_mcp_list_failed(label, err);
        }
        let outcome = agent::run(req.ir, cfg, &runner, &exec, &mut sink).await;
        // A turn a ladder climb handed to the fallback (review finding 13):
        // the unary answer leaves only now, so it can say so — the fallback
        // replaces the rung stamped at open.
        if let Some((alias, reason)) = runner.fell_back.take() {
            headers.fall_back(alias, reason);
        }
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
                enc.fail(&e);
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
                (status, axum::Json(enc.snapshot())).into_response()
            }
        };
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    tokio::spawn(async move {
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
        for (label, err) in &resolved.failed {
            let f = enc.push_mcp_list_failed(label, err);
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
            agent::run(ir, cfg, &runner, &exec, &mut sink).await
        };
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
    };
    if let Err(e) = store::insert_response(&state.db, &row).await {
        tracing::error!("storing response {}: {e}", row.id);
    }
}

// ---------------------------------------------------------------------------
// The rest of the resource: GET / DELETE / input_items
// ---------------------------------------------------------------------------

fn not_found(id: &str) -> Response {
    let e = GatewayError::NotFound(format!(
        "no stored response '{id}' — it may have been evicted, or storing was off \
         when it was created"
    ));
    (e.http_status(), axum::Json(wire::serialize_error(&e))).into_response()
}

/// `GET /v1/responses/{id}` — the response object exactly as it was served.
pub async fn get_response(state: SharedState, Path(id): Path<String>) -> Response {
    match store::get_response(&state.db, &id).await {
        Ok(Some(r)) => match serde_json::from_str::<Value>(&r.body) {
            Ok(v) => (StatusCode::OK, axum::Json(v)).into_response(),
            Err(e) => {
                let e = GatewayError::Internal(format!("stored response {id} is unreadable: {e}"));
                (e.http_status(), axum::Json(wire::serialize_error(&e))).into_response()
            }
        },
        Ok(None) => not_found(&id),
        Err(e) => {
            let e = GatewayError::Internal(e.to_string());
            (e.http_status(), axum::Json(wire::serialize_error(&e))).into_response()
        }
    }
}

/// `DELETE /v1/responses/{id}`.
///
/// Deletes the one response, not its chain — that is what the API means, and
/// the Responses tab is where a whole conversation is dropped. Its descendants
/// keep their `chain_id`, so the conversation still lists and evicts as a unit.
pub async fn delete_response(state: SharedState, Path(id): Path<String>) -> Response {
    match store::delete_response(&state.db, &id).await {
        Ok(true) => (
            StatusCode::OK,
            axum::Json(json!({"id": id, "object": "response", "deleted": true})),
        )
            .into_response(),
        Ok(false) => not_found(&id),
        Err(e) => {
            let e = GatewayError::Internal(e.to_string());
            (e.http_status(), axum::Json(wire::serialize_error(&e))).into_response()
        }
    }
}

/// `GET /v1/responses/{id}/input_items` — the `input` this response was given.
pub async fn list_input_items(state: SharedState, Path(id): Path<String>) -> Response {
    let Ok(Some(r)) = store::get_response(&state.db, &id).await else {
        return not_found(&id);
    };
    let items: Vec<Value> = serde_json::from_str(&r.input_items).unwrap_or_default();
    let first = items.first().and_then(|i| i.get("id")).cloned();
    let last = items.last().and_then(|i| i.get("id")).cloned();
    (
        StatusCode::OK,
        axum::Json(json!({
            "object": "list",
            "data": items,
            "first_id": first,
            "last_id": last,
            // Everything is returned in one page: these are one turn's items on
            // a single-user gateway, not a cursor-worthy collection.
            "has_more": false,
        })),
    )
        .into_response()
}
