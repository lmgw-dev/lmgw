//! Shared in-process call+record helper (§8). The public-API path logs via
//! `record`/`LogParams` in `recording.rs` (keyed on `ClientProto`); the
//! *in-process* callers — the Chat tab, the Workflows classifier, and the MCP
//! sampling handler — are not OpenAI/Anthropic ingress and each used to copy
//! its own `record_*` boilerplate (`record_chat_call`, `record_llm_call`). §8
//! calls that out as the reason sampling does NOT get a `request_logs` row "for
//! free", and the fix is to extract one shared helper so an in-process model
//! call logs like any request. `record_in_process` is that core; `sample_once`
//! wraps it with the egress call.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::http::StatusCode;

use crate::config::Route;
use crate::egress::for_protocol;
use crate::error::GatewayError;
use crate::gate::FallbackReason;
use crate::ir::{ChatRequest, Completion, Timings, Usage};
use crate::state::{AppState, SharedState};
use crate::store::{self, NewRequestLog};
use crate::telemetry::RequestClass;

use super::*;

#[cfg(test)]
mod tests;

/// Everything `record_in_process` needs that isn't the per-call status/usage.
/// A plain struct (not the `ClientProto`-keyed `LogParams`) because in-process
/// callers carry an arbitrary `ingress_proto` label ("chat" / "workflow" /
/// "mcp-sampling"), not one of the two public ingress protocols.
pub(crate) struct InProcessLog<'a> {
    /// Whom the row is charged to, if the in-process call originated from an
    /// authenticated request context (sampling carries the originating
    /// server's label, chat none, a realtime session its key's identity —
    /// [`KeyRef`]).
    pub key: KeyRef,
    /// The `request_logs.ingress_proto` label — e.g. `"mcp-sampling"` (§8/§10):
    /// real LLM traffic that **does** count in stats, unlike `"mcp"`.
    pub ingress_proto: &'a str,
    /// The alias the in-process caller asked for (router input), logged verbatim.
    pub alias: &'a str,
    pub route: &'a Route,
    pub started: Instant,
    pub streamed: bool,
    /// Defaults to `Chat` — every in-process caller today is a model turn.
    /// quickdoc's embed/rerank helpers set `Aux`.
    pub class: RequestClass,
    pub timings: Option<Timings>,
    /// [`Annotations::max_tokens_clamped`] for this call (ladder design §3.2,
    /// unified-KV design §3.3 step 1): the turn's
    /// [`crate::gate::TurnLease::max_tokens_clamped`].
    pub max_tokens_clamped: Option<u32>,
    /// Why a fallback answered instead of the model the caller asked for:
    /// the [`GateHeaders::fallback_reason`](crate::gate::GateHeaders::fallback_reason) of the admission the caller holds.
    pub fallback: Option<FallbackReason>,
    /// See [`LogParams::rung`]: the turn's own lease's rung.
    pub rung: Option<i64>,
    /// See [`LogParams::degraded`]: what this call's content lost to a model
    /// that lacks a capability.
    pub degraded: Option<String>,
    /// See [`LogParams::quantities`]: what the call processed besides
    /// tokens; `Default::default()` when nothing was measured.
    /// [`record_in_process`] fills `requests` when it is left `None`.
    pub quantities: crate::pricing::Quantities,
}

/// Write one `request_logs` row for an in-process model call and broadcast it on
/// the live feed — the shared core all in-process callers route through so they
/// log identically to a public request (§8 "one path instead of three copies").
/// Real `prompt/completion_tokens`, `status`, `error_kind`, and latency, exactly
/// like [`record`]; the only difference is the free-form `ingress_proto` label.
///
/// Returns what the row adds to a total — priced on `p.route`, the route that
/// answered — for a caller that tells its meter (billable-units §7).
pub(crate) async fn record_in_process(
    p: InProcessLog<'_>,
    status: u16,
    ttfb_ms: Option<i64>,
    usage: Usage,
    error: Option<(&str, String)>,
    state: &AppState,
) -> crate::pricing::RowCost {
    let total_ms = p.started.elapsed().as_millis() as i64;
    let q = with_requests_default(p.quantities, true, status, error.as_ref().map(|(k, _)| *k));
    let priced = price_call(
        state,
        p.alias,
        Some(p.route),
        &p.key,
        p.ingress_proto,
        &usage,
        &q,
    );
    let row = NewRequestLog {
        client_key: priced.client_key,
        ingress_proto: p.ingress_proto.to_string(),
        requested_alias: p.alias.to_string(),
        upstream_id: Some(p.route.upstream.id),
        upstream_name: Some(p.route.upstream.name.clone()),
        upstream_model: Some(p.route.upstream_model.clone()),
        // In-process LLM traffic never names an MCP tool (that's the tools/call
        // path); sampling is real model traffic, so the column stays NULL.
        mcp_tool: None,
        egress_proto: Some(p.route.upstream.protocol.as_str().to_string()),
        status: status as i64,
        ttfb_ms,
        total_ms: Some(total_ms),
        prompt_tokens: usage.prompt_tokens.map(|v| v as i64),
        completion_tokens: usage.completion_tokens.map(|v| v as i64),
        streamed: p.streamed,
        error_kind: error.as_ref().map(|(k, _)| k.to_string()),
        error_msg: error.as_ref().map(|(_, m)| m.clone()),
        key_id: priced.key_id,
        class: p.class,
        cached_in_tokens: usage.cached_input_tokens.map(|v| v as i64),
        cache_write_tokens: usage.cache_write_tokens.map(|v| v as i64),
        reasoning_tokens: usage.reasoning_tokens.map(|v| v as i64),
        cost: priced.cost,
        timings: p.timings,
        max_tokens_clamped: p.max_tokens_clamped,
        fallback_reason: p.fallback.map(|r| r.as_str().to_string()),
        rung: p.rung,
        degraded: p.degraded,
        audio_in_ms: quantity_column(q.audio_in_ms),
        chars_in: quantity_column(q.chars_in),
        images_out: quantity_column(q.images_out),
        // A model call: no approval decides one.
        approved_by: None,
    };
    let share = row.row_cost();
    // A caller stamped with a run (`X-Lmgw-Run` on a `/v1/responses` or a
    // realtime request) has its rows on that run's total, as `record` puts
    // a `/v1` request's there (§3.1).
    note_on_run(state, p.key.run, &row, &usage);
    let log_id = store::insert_request_log(&state.db, &row)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("failed to write in-process request log: {e}");
            0
        });
    state
        .telemetry
        .request_finished(row_summary(log_id, row, status));
    share
}

/// Run one **non-streaming** in-process model call through the existing egress
/// adapter path and log it like any request (§8). This is the helper the MCP
/// sampling handler (`mcp::handler::create_message`) calls to answer
/// `sampling/createMessage` from our own models, and the shape any other
/// non-streaming in-process caller (e.g. the Workflows classifier) can reuse.
///
/// - `ingress_proto` is the log label — `"mcp-sampling"` for sampling, which
///   §10 deliberately counts in stats (real tokens), unlike `"mcp"` tools/call.
/// - `fallback` is why `route` is a fallback rather than the model the caller
///   asked for — its admission's [`GateHeaders::fallback_reason`](crate::gate::GateHeaders::fallback_reason) — so the
///   turn's row records it (candidate-aliases design §4.7).
/// - `deadline` is the call's own wall-clock budget. For sampling this is the
///   **self-starvation** guard (§8): a `sampling_alias` pointing at a single-slot
///   local llama.cpp model that the triggering agent already occupies must fail
///   **loudly** here ([`GatewayError::Timeout`]) instead of hanging forever on a
///   GPU slot the caller owns. A visible named constant at the call site, not a
///   hidden cap.
///
/// On success returns the parsed [`Completion`] (assistant text + real token
/// usage + finish reason). Both success and every failure write a log row, so
/// an upstream-triggered sampling call is always observable (§8/§14).
///
/// **GPU admission is the caller's, deliberately.** This helper is one turn;
/// every caller of it is either a single call (MCP sampling) or a loop of many
/// (`/v1/responses`, the dashboard's agent chat, quickdoc's extraction and
/// generation runs). Admitting here would take and release a claim per turn,
/// which is the one shape that must not happen — see
/// [`crate::vram::LocalHold`]. So callers hold a guard across their own
/// lifetime and this stays a pure egress call. A new caller that skips it is a
/// bug: it forwards to the GPU without arbitration and leaves its own model
/// looking like the coldest eviction candidate on the box.
///
/// **The gate's per-send half is this helper's, for the mirror-image
/// reason.** A guarded model's pool reservation (`gate::fit_chat`: clamp,
/// count, reserve) belongs to one turn — its own prompt, its own max output —
/// and has to end with that turn's response, not with the caller's loop. So
/// it is taken here, per call, and released as soon as the answer is read.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn sample_once(
    state: &SharedState,
    hold: Option<&crate::vram::LocalHold>,
    route: &Route,
    fallback: Option<FallbackReason>,
    ir: &ChatRequest,
    ingress_proto: &str,
    client_key: Option<String>,
    deadline: Duration,
) -> Result<Completion, GatewayError> {
    sample_once_noting(
        state,
        hold,
        route,
        fallback,
        ir,
        ingress_proto,
        KeyRef::named(client_key),
        deadline,
        None,
        None,
    )
    .await
}

/// Where a caller of [`sample_once_noting`] learns that a turn was answered by
/// the request's fallback, because a ladder climb could not happen (ladder
/// design §12 entry 8) — `/v1/responses`, whose unary answer leaves after its
/// last turn and so can still say so (review finding 13). The first such
/// turn's alias and reason are kept.
#[derive(Debug, Default)]
pub(crate) struct FallbackNote(std::sync::Mutex<Option<(String, FallbackReason)>>);

impl FallbackNote {
    fn note(&self, alias: String, reason: FallbackReason) {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        slot.get_or_insert((alias, reason));
    }

    /// The fallback a turn handed over to, if any did.
    pub(crate) fn take(&self) -> Option<(String, FallbackReason)> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// [`sample_once`], telling `note` when the turn was handed to the fallback.
/// `key` is the caller's key — with its id when the caller has one, so a
/// loop that outlives a rename of its key records each turn against the key
/// it is (A2 review 4). `per_route` decides what goes out on each route, as
/// for [`stream_once_on`]: `ir` is always the caller's own request.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn sample_once_noting(
    state: &SharedState,
    hold: Option<&crate::vram::LocalHold>,
    route: &Route,
    fallback: Option<FallbackReason>,
    ir: &ChatRequest,
    ingress_proto: &str,
    key: KeyRef,
    deadline: Duration,
    note: Option<&FallbackNote>,
    per_route: Option<(&dyn PerRoute, Option<&crate::gate::GateHeaders>)>,
) -> Result<Completion, GatewayError> {
    let started = Instant::now();
    state.telemetry.request_started();
    let own = ir;

    // Budget and scope for work the gateway does on its own behalf (§4.4).
    // Before the call, so an unattended loop stops rather than discovering the
    // ceiling one request at a time.
    if let Err(e) = crate::policy::check_internal(
        &state.policy,
        &state.db,
        &state.snapshot(),
        ingress_proto,
        &ir.model_alias,
    )
    .await
    {
        record_in_process(
            InProcessLog {
                key: key.clone(),
                ingress_proto,
                alias: &ir.model_alias,
                route,
                started,
                streamed: false,
                class: RequestClass::Chat,
                timings: None,
                max_tokens_clamped: None,
                fallback,
                rung: None,
                degraded: None,
                quantities: Default::default(),
            },
            e.http_status().as_u16(),
            None,
            Usage::default(),
            Some((e.kind(), e.to_string())),
            state,
        )
        .await;
        return Err(e);
    }

    // What the caller sends on this route (`per_route`); a refusal of it is
    // this call's row, like any other.
    let prepared = match per_route {
        Some((p, rerouted)) => p.request(route, hold, rerouted, own).await,
        None => Ok(None),
    };
    let (ir, mut refused) = match prepared {
        Ok(Some(r)) => (std::borrow::Cow::Owned(r), None),
        Ok(None) => (std::borrow::Cow::Borrowed(own), None),
        Err(e) => (std::borrow::Cow::Borrowed(own), Some(e)),
    };
    let ir: &ChatRequest = &ir;
    let egress = for_protocol(route.upstream.protocol);
    let mut params = ir.params.clone().with_defaults(&route.param_defaults);
    // An off this cloud model cannot take as asked goes out in the form it
    // can (model-capabilities design §5.6).
    let mut fitted = super::reasoning_fit::fit(state, route, &mut params).await;

    // The caller's `deadline` is a bound on this whole call, the gate
    // included (second review, finding 7): it runs from `started`, and the gate's
    // count and pool wait end at it, so a queue for KV room can never stretch
    // MCP sampling's "fail fast" bound or an agent loop's wall clock. `None`
    // only for a deadline too far out for the clock to represent (a huge
    // `responses_timeout_seconds`), which bounds nothing anyway.
    let deadline_at = started.checked_add(deadline);

    let mut max_tokens_clamped = None;
    let mut rung = None;
    let mut lease_degraded = None;
    let result: Result<Turn<Completion>, GatewayError> = async {
        if let Some(e) = refused.take() {
            return Err(e);
        }
        // The gate's per-send half (unified-KV design §3.3): this turn's own
        // clamp, count and pool reservation. Per turn, not per hold — the
        // caller's hold may span a whole tool loop, and the lease must not.
        let fit =
            crate::gate::fit_chat(state, hold, route, ir, &mut params, false, deadline_at).await;
        let (mut lease, ir) = match fit {
            Ok(fit) => fit,
            // The clamp (step 1) already ran even though the reservation
            // (step 4) refused — this turn's own log row must still
            // carry it (review finding 7).
            Err(f) => {
                max_tokens_clamped = f.max_tokens_clamped;
                rung = f.rung.as_ref().map(crate::gate::RungTag::log);
                return Err(f.error);
            }
        };
        max_tokens_clamped = lease.max_tokens_clamped();
        lease_degraded = lease.degraded();
        // The send's effective timeout is the smaller of the upstream's own
        // timeout and what is left of the caller's `deadline` after the gate —
        // so the self-starvation guard can fail faster than a generous
        // per-upstream ceiling (a cold local model's 10 min) without ever
        // *raising* that bound. Both are visible, neither hidden. An upstream
        // with no ceiling of its own leaves the deadline standing alone, which
        // is still a bound: this path always has one.
        let left = deadline_at.map_or(deadline, |d| d.saturating_duration_since(Instant::now()));
        if left.is_zero() {
            return Err(GatewayError::Timeout);
        }
        let effective = match route.upstream.request_timeout() {
            Some(t) => left.min(t),
            None => left,
        };
        // One send, through the gate's send (§3.2's dead-container retry, and
        // on a ladder row the count beside it and the climb): with the hold
        // in hand this is the same turn re-issued against a fresh container
        // when the one we hold a claim on has died.
        let sent = super::reasoning_fit::send_chat(
            state,
            hold,
            route,
            &mut lease,
            &ir,
            &params,
            false,
            deadline_at,
            Some(effective),
            None,
            &mut fitted,
            super::reasoning_fit::RowAs::InProcess {
                key: key.clone(),
                ingress_proto,
                alias: &ir.model_alias,
                fallback,
            },
            |r, p| {
                Ok(egress
                    .build_chat(&state.http, &r.upstream, &r.upstream_model, &ir, p, false)?
                    .timeout(effective))
            },
        )
        .await;
        rung = lease.rung_log();
        let resp = match sent? {
            crate::gate::Sent::Upstream(resp) => resp,
            crate::gate::Sent::Rerouted(r) => return Ok(Turn::Rerouted(r)),
        };
        let status = resp.status();
        // A body that could not be read to its end (a timeout, a reset) drops
        // the lease here unfinished: released once llama-server lets go.
        let bytes = resp.bytes().await.map_err(GatewayError::from)?;
        // The answer is in: this turn no longer occupies the pool.
        lease.complete();
        if !status.is_success() {
            return Err(crate::gate::attribute(
                egress.map_error(status.as_u16(), &bytes),
                route,
            ));
        }
        egress.parse_completion(&bytes).map(Turn::Answered)
    }
    .await;
    let result = match result {
        Ok(Turn::Answered(c)) => Ok(c),
        Err(e) => Err(e),
        // The gate refused instead — a candidate alias's deferral: this
        // turn's row records it, on the candidate it was on.
        Ok(Turn::Rerouted(Err(f))) => Err(f.error),
        // The gate sent the turn elsewhere before anything was sent — the
        // request's fallback after a climb that could not happen (ladder
        // design §12 entry 8), or a candidate alias's next pick (§12 entry
        // 45): the turn is that call's, row and gauge included — this one
        // sent nothing and logs nothing.
        Ok(Turn::Rerouted(Ok(opened))) => {
            state.telemetry.request_abandoned();
            if let (Some(note), Some(fb), Some(reason)) = (
                note,
                opened.headers.fallback(),
                opened.headers.fallback_reason(),
            ) {
                note.note(fb.to_string(), reason);
            }
            let left =
                deadline_at.map_or(deadline, |d| d.saturating_duration_since(Instant::now()));
            return Box::pin(sample_once_noting(
                state,
                opened.hold.as_ref(),
                &opened.route,
                opened.headers.fallback_reason(),
                own,
                ingress_proto,
                key,
                left,
                note,
                per_route.map(|(p, _)| (p, Some(&opened.headers))),
            ))
            .await;
        }
    };

    if let Some((p, _)) = per_route {
        p.fitted(&fitted);
    }
    // One shared log row either way — the §8 "logs like any request" guarantee.
    let (status, usage, error): (u16, Usage, Option<(&str, String)>) = match &result {
        Ok(c) => (StatusCode::OK.as_u16(), c.usage, None),
        Err(e) => (
            e.http_status().as_u16(),
            Usage::default(),
            Some((e.kind(), e.to_string())),
        ),
    };
    record_in_process(
        InProcessLog {
            key,
            ingress_proto,
            alias: &ir.model_alias,
            route,
            started,
            streamed: false,
            class: RequestClass::Chat,
            timings: None,
            max_tokens_clamped,
            fallback,
            rung,
            degraded: crate::degraded::join([
                per_route.and_then(|(p, _)| p.degraded()),
                lease_degraded,
            ]),
            quantities: Default::default(),
        },
        status,
        Some(started.elapsed().as_millis() as i64),
        usage,
        error,
        state,
    )
    .await;

    result
}

/// What one send came to: its answer, or — before anything was sent — where
/// the gate sent the request instead ([`crate::gate::Sent::Rerouted`]): the
/// request's fallback, when a ladder climb could not happen (ladder design
/// §12 entry 8), or a candidate alias's next pick (candidate-aliases §12
/// entry 45). The request is then served again on that admission; `Err` is
/// the refusal to answer with instead.
///
/// `#[allow(large_enum_variant)]` like [`crate::gate::Sent`]: the route is
/// the point of the rerouted answer, and there is one of these per send.
#[allow(clippy::large_enum_variant)]
pub(super) enum Turn<T> {
    Answered(T),
    Rerouted(Result<crate::gate::Opened, crate::gate::OpenFailed>),
}

/// Balances [`TelemetryBus::request_started`] for a call whose future is
/// *dropped* before it can log a row.
///
/// Every in-process caller normally closes its own out through
/// `record_in_process`, and that is still the path. This is for the case that
/// has no path at all: a call dropped where it stands, so nothing after the
/// await ever runs — a unary turn of a cancelled agent run
/// (`agent::Cancel::guard`; a streamed turn is stopped cooperatively instead
/// and logs its row, `agent/relay.rs`), or a caller's own future dropped
/// whole. Without this the in-flight gauge would go on counting a call that
/// is over.
struct InFlight<'a> {
    telemetry: &'a crate::telemetry::TelemetryBus,
    armed: bool,
}

impl<'a> InFlight<'a> {
    fn armed(telemetry: &'a crate::telemetry::TelemetryBus) -> Self {
        Self {
            telemetry,
            armed: true,
        }
    }

    /// The row is being written; the recorder closes the gauge from here.
    fn logging(&mut self) {
        self.armed = false;
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.telemetry.request_abandoned();
        }
    }
}

/// Run one **streaming** in-process model call, forwarding each normalized IR
/// delta to `sink` as it arrives and folding the whole turn up into a
/// [`Completion`]. The streaming sibling of [`sample_once`], logged identically
/// (§8) — same `ingress_proto` label, same real token counts, same row on the
/// unified feed whether it succeeded or failed.
///
/// This is what lets the `/v1/responses` tool loop (§21) stream tokens to the
/// client *and* still hold the finished assistant message it needs before it can
/// run the turn's tools: `sink` sees the deltas live, the return value is the
/// same turn assembled.
///
/// GPU admission belongs to the caller here too, for the reason spelled out on
/// [`sample_once`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_once(
    state: &SharedState,
    hold: Option<&crate::vram::LocalHold>,
    route: &Route,
    fallback: Option<FallbackReason>,
    ir: &ChatRequest,
    ingress_proto: &str,
    key: KeyRef,
    deadline: Duration,
    sink: &mut dyn crate::agent::DeltaSink,
) -> Result<Completion, GatewayError> {
    stream_once_on(
        state,
        hold,
        route,
        fallback,
        ir,
        ingress_proto,
        key,
        deadline,
        sink,
        None,
    )
    .await
}

/// A caller's say in what a streamed call sends on each route it lands on:
/// the route [`stream_once_on`] was handed, and every one the gate re-routes
/// the send to before anything was sent (a ladder climb's fallback, a
/// candidate alias's next pick). The dashboard Chat's tool loop needs it —
/// which sampling fields a route takes, whether it can continue a reply, the
/// llama-server-only continuation fields — and must not decide it once for
/// the first route and send that to whichever answers (review R1 finding 2).
#[async_trait::async_trait]
pub(crate) trait PerRoute: Send + Sync {
    /// The request to send on `route` — `None`: `ir` as it is — or why the
    /// call cannot go there, which fails it like an upstream refusal would.
    /// `hold` is the claim the send goes out on (a local container's);
    /// `rerouted` is the gate's headers when the gate chose `route` for this
    /// send; `None` for the route the caller handed in. Async: whether a
    /// heard turn's model takes its audio is a capability lookup
    /// (`spoken::may_hear`).
    async fn request(
        &self,
        route: &Route,
        hold: Option<&crate::vram::LocalHold>,
        rerouted: Option<&crate::gate::GateHeaders>,
        ir: &ChatRequest,
    ) -> Result<Option<ChatRequest>, GatewayError>;

    /// How the request's reasoning off went out on the route [`Self::request`]
    /// was last asked about (model-capabilities design §5.6) — told once the
    /// send is over, a retry included, so a caller that reports the controls
    /// a route did not send can add it ([`super::reasoning_fit::Fitted::report`]).
    fn fitted(&self, _fitted: &super::reasoning_fit::Fitted) {}

    /// What the request [`Self::request`] last gave lost to a model that
    /// lacks a capability (images as placeholders, a turn's audio as its
    /// transcript), as the call's row says it (`request_logs.degraded`,
    /// [`crate::degraded`]); joined there with what the gate's own send left
    /// out. `None`: nothing.
    fn degraded(&self) -> Option<String> {
        None
    }
}

/// The error of a call its consumer stopped before the upstream answered
/// ([`DeltaSink::stop`](crate::agent::DeltaSink::stop)): nothing was read,
/// so its row carries no output — and the prompt only once the request went
/// out.
fn before_answer() -> GatewayError {
    super::canceled("stopped by the caller before the upstream answered")
}

/// [`stream_once`], with `per_route` deciding what goes out on each route
/// ([`PerRoute`]). `ir` is always the caller's own request: a re-route asks
/// `per_route` again from it, never from what the previous route was sent.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_once_on(
    state: &SharedState,
    hold: Option<&crate::vram::LocalHold>,
    route: &Route,
    fallback: Option<FallbackReason>,
    ir: &ChatRequest,
    ingress_proto: &str,
    key: KeyRef,
    deadline: Duration,
    sink: &mut dyn crate::agent::DeltaSink,
    per_route: Option<(&dyn PerRoute, Option<&crate::gate::GateHeaders>)>,
) -> Result<Completion, GatewayError> {
    let started = Instant::now();
    state.telemetry.request_started();
    let mut in_flight = InFlight::armed(&state.telemetry);
    // The consumer's cooperative stop (realtime design §4.3): every wait
    // below is raced against it, and a stopped call still writes its row.
    let stop = sink.stop();
    let stop = stop.as_ref();
    let mut produced = 0usize;
    // The answer carried reasoning — with an off asked, the model did not
    // take it (`Fitted::observe`).
    let mut reasoned = false;

    let own = ir;
    let prepared = match per_route {
        // Raced against the consumer's stop: the caller's check may read a
        // provider's catalog (voice-audio-input review V9).
        Some((p, rerouted)) => tokio::select! {
            biased;
            () = super::stopped(stop) => Err(before_answer()),
            r = p.request(route, hold, rerouted, own) => r,
        },
        None => Ok(None),
    };
    let (ir, mut refused) = match prepared {
        Ok(Some(r)) => (std::borrow::Cow::Owned(r), None),
        Ok(None) => (std::borrow::Cow::Borrowed(own), None),
        Err(e) => (std::borrow::Cow::Borrowed(own), Some(e)),
    };
    let ir: &ChatRequest = &ir;
    let egress = for_protocol(route.upstream.protocol);
    let mut params = ir.params.clone().with_defaults(&route.param_defaults);
    // As in `sample_once`: an off fitted to the model (§5.6) — not for a
    // route the caller refused, which gets nothing, not even a catalog read
    // (voice-audio-input WP2 review #8: a heard turn's audio refused on a
    // route whose model cannot take it).
    let mut fitted = match refused {
        Some(_) => super::reasoning_fit::Fitted::default(),
        None => super::reasoning_fit::fit(state, route, &mut params).await,
    };
    // Same rule as `sample_once`: the caller's deadline may tighten the
    // upstream's own ceiling but never loosen it, and an upstream with no
    // ceiling leaves the deadline standing alone. Both are visible. Between
    // chunks it is a stall bound, as before.
    let effective = match route.upstream.request_timeout() {
        Some(t) => deadline.min(t),
        None => deadline,
    };
    // …and up to the first byte it is a deadline that includes the gate
    // (second review, finding 7): the count and the pool wait end at it, and the wait
    // for response headers gets only what they left. (`None`: too far out for
    // the clock, as in `sample_once`.)
    let deadline_at = started.checked_add(deadline);

    let mut acc = crate::agent::TurnAccumulator::default();
    let mut outcome = StreamOutcome::default();
    let mut max_tokens_clamped = None;
    let mut rung = None;
    let mut lease_degraded = None;
    // The request went out and no answer has begun: a stop now still costs
    // the prompt the upstream may be working on (`stop::unanswered_usage`).
    // Set by the request's build, as each attempt goes out — not when the
    // send is first polled: the send can wait long before anything leaves
    // (a ladder climb in progress, a candidate's re-pick and its start), and
    // a stop there sent no prompt to bill (package A review #3). Atomic only
    // because the build closure is `Fn` and the call's future must be `Send`.
    let prompt_sent = AtomicBool::new(false);

    let result: Result<Turn<()>, GatewayError> = async {
        // The caller's refusal of this route: the turn's row, like any other.
        if let Some(e) = refused.take() {
            return Err(e);
        }
        // The gate's per-send half, per turn — see `sample_once`. The lease
        // lives until the stream below has been drained, and no longer.
        let fit = tokio::select! {
            biased;
            () = super::stopped(stop) => return Err(before_answer()),
            f = crate::gate::fit_chat(state, hold, route, ir, &mut params, true, deadline_at) => f,
        };
        let (mut lease, ir) = match fit {
            Ok(fit) => fit,
            // Same reason as `sample_once` (review finding 7): the clamp
            // already ran even though the reservation refused.
            Err(f) => {
                max_tokens_clamped = f.max_tokens_clamped;
                rung = f.rung.as_ref().map(crate::gate::RungTag::log);
                return Err(f.error);
            }
        };
        max_tokens_clamped = lease.max_tokens_clamped();
        lease_degraded = lease.degraded();
        let left = deadline_at.map_or(deadline, |d| d.saturating_duration_since(Instant::now()));
        if left.is_zero() {
            return Err(GatewayError::Timeout);
        }
        // The *connect* is retried through the shared dead-container path
        // (§3.2); the body below is not, and cannot be — by then deltas have
        // already reached the caller's sink. On a ladder row nothing reaches
        // the sink before the count's verdict on the rung that answered.
        // A re-pick clears the record (A2 review 2): what the candidate it
        // left was sent is not being worked on.
        let send = super::reasoning_fit::send_chat(
            state,
            hold,
            route,
            &mut lease,
            &ir,
            &params,
            true,
            deadline_at,
            Some(effective.min(left)),
            Some(&prompt_sent),
            &mut fitted,
            super::reasoning_fit::RowAs::InProcess {
                key: key.clone(),
                ingress_proto,
                alias: &ir.model_alias,
                fallback,
            },
            |r, p| {
                prompt_sent.store(true, Ordering::Relaxed);
                egress.build_chat(&state.http, &r.upstream, &r.upstream_model, &ir, p, true)
            },
        );
        // Dropping the send closes its connection, so a stop while the
        // upstream prefills ends the work there too.
        let sent = tokio::select! {
            biased;
            () = super::stopped(stop) => None,
            s = send => Some(s),
        };
        rung = lease.rung_log();
        let Some(sent) = sent else {
            return Err(before_answer());
        };
        let resp = match sent? {
            crate::gate::Sent::Upstream(resp) => resp,
            crate::gate::Sent::Rerouted(r) => return Ok(Turn::Rerouted(r)),
        };
        let status = resp.status();
        if !status.is_success() {
            // Raced against the stop like every other wait here: with no
            // deadline of the caller's (realtime passes `Duration::MAX`) and
            // an upstream `request_timeout` of 0, an error body that stalls
            // would otherwise pin this call — and its hold — for good.
            let bytes = tokio::select! {
                biased;
                () = super::stopped(stop) => None,
                b = resp.bytes() => Some(b),
            };
            let Some(bytes) = bytes else {
                lease.end(false);
                // An error answer: the upstream took no prompt to work on.
                prompt_sent.store(false, Ordering::Relaxed);
                return Err(before_answer());
            };
            // An error answer read to its end is a task the server is done
            // with; an unreadable one is released once `/slots` says so.
            lease.end(bytes.is_ok());
            let bytes = bytes.unwrap_or_default();
            return Err(crate::gate::attribute(
                egress.map_error(status.as_u16(), &bytes),
                route,
            ));
        }
        outcome = drive_upstream_until(
            resp,
            egress.new_decoder(),
            Some(effective),
            started,
            &state.telemetry,
            |delta| {
                acc.on_delta(&delta);
                produced += super::produced_chars(&delta);
                if matches!(&delta, crate::ir::StreamDelta::ReasoningDelta(r) if !r.is_empty()) {
                    reasoned = true;
                }
                sink.on_delta(&delta);
                // Checked after the delta went out, so a stop raised by the
                // sink itself ends the stream here.
                let go = !stop.is_some_and(crate::proxy::StopSignal::is_raised);
                async move { go }
            },
            super::stopped(stop),
        )
        .await;
        // `drive_upstream` consumed the response, so the connection is closed
        // by now: released at once after a normal end, once llama-server lets
        // go of the slot after anything else. (A cancelled turn — the agent
        // loop dropping this future — drops the lease unfinished too.)
        lease.end(outcome.completed);
        Ok(Turn::Answered(()))
    }
    .await;
    fitted.observe(route, reasoned);
    if let Some((p, _)) = per_route {
        p.fitted(&fitted);
    }
    let result = match result {
        Ok(Turn::Answered(())) => Ok(()),
        Err(e) => Err(e),
        // See `sample_once`: a refusal instead is this turn's row.
        Ok(Turn::Rerouted(Err(f))) => Err(f.error),
        // See `sample_once`: the rerouted call is the turn.
        Ok(Turn::Rerouted(Ok(opened))) => {
            in_flight.logging();
            state.telemetry.request_abandoned();
            let left =
                deadline_at.map_or(deadline, |d| d.saturating_duration_since(Instant::now()));
            return Box::pin(stream_once_on(
                state,
                opened.hold.as_ref(),
                &opened.route,
                opened.headers.fallback_reason(),
                own,
                ingress_proto,
                key,
                left,
                sink,
                per_route.map(|(p, _)| (p, Some(&opened.headers))),
            ))
            .await;
        }
    };

    // A mid-stream failure is reported by `drive_upstream` in the outcome rather
    // than as an Err, so both have to be folded into one status here. A
    // stream its consumer stopped (the only early end here) is the public
    // relay's `canceled` row — status 200 — with what it cost so far,
    // estimated where the upstream had not said yet; so is a call stopped
    // before the upstream answered (`stop::row_status`), whose prompt counts
    // once it was sent.
    let mut usage = acc.usage();
    let stopped = result.is_ok() && outcome.error.is_none() && outcome.aborted;
    let stopped_unanswered =
        prompt_sent.into_inner() && matches!(&result, Err(e) if super::is_canceled(e));
    let error: Option<(String, String)> = match &result {
        Err(_) if stopped_unanswered => {
            let (u, note) = super::unanswered_usage(ir);
            usage = u;
            Some(("canceled".to_string(), note))
        }
        Err(e) => Some((e.kind().to_string(), e.to_string())),
        Ok(()) if stopped => {
            let (u, note) = super::stopped_usage(usage, outcome.timings.as_ref(), ir, produced);
            usage = u;
            Some(("canceled".to_string(), note))
        }
        Ok(()) => outcome.error.clone(),
    };
    let status = match &result {
        Err(e) => super::row_status(e),
        Ok(()) if error.is_some() && !stopped => StatusCode::BAD_GATEWAY.as_u16(),
        Ok(()) => StatusCode::OK.as_u16(),
    };
    // `Ok` only once the response passed its status check: the upstream
    // answered, so the row counts its request — a stop or a failure
    // mid-stream included, which the writer's default would leave unknown
    // (billable-units §4.5). A stop before that stays unknown.
    let quantities = if result.is_ok() {
        crate::pricing::Quantities::answered()
    } else {
        crate::pricing::Quantities::default()
    };
    in_flight.logging();
    let share = record_in_process(
        InProcessLog {
            key,
            ingress_proto,
            alias: &ir.model_alias,
            route,
            started,
            streamed: true,
            class: RequestClass::Chat,
            timings: outcome.timings,
            max_tokens_clamped,
            fallback,
            rung,
            degraded: crate::degraded::join([
                per_route.and_then(|(p, _)| p.degraded()),
                lease_degraded,
            ]),
            quantities,
        },
        status,
        outcome.ttfb_ms,
        usage,
        error.as_ref().map(|(k, m)| (k.as_str(), m.clone())),
        state,
    )
    .await;
    // This route's row — a re-routed call's own, which the caller's meter
    // cannot price from the route it started on.
    sink.billed(&usage);
    sink.billed_cost(share);

    result?;
    // The consumer asked for the stop; what it got so far is all it gets.
    if stopped {
        return Err(super::canceled("stopped by the caller mid-stream"));
    }
    // A stream that died mid-flight has no usable turn — surfacing the partial
    // text as if the model had finished would make the loop act on a truncated
    // answer, so it fails loudly instead (§14).
    if let Some((kind, msg)) = error {
        return Err(match kind.as_str() {
            "timeout" => GatewayError::Timeout,
            "transport" => GatewayError::Transport(msg),
            _ => GatewayError::Upstream {
                status: 502,
                message: msg,
                provider_type: None,
            },
        });
    }
    Ok(acc.finish(route.upstream_model.clone()))
}
