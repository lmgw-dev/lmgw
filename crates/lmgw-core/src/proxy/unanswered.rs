//! A request whose client went away before it was answered: the `499`
//! `client_disconnected` row, and the in-flight gauge closed either way.
//!
//! hyper drops a handler's future when its connection closes, so nothing the
//! handler would have written after its current await ever runs: no
//! `request_logs` row, and the `requests.active` gauge it opened counting a
//! request that is over. A chat that waited for a cold model and was given up
//! on mid-load left no trace at all, and neither did a count that started a
//! container (2026-10-06, `docs/design/2026-10-06-registry-owns-start.md`).
//!
//! [`Unanswered`] is opened where the handler opens the gauge, and learns as
//! the request goes on what its row would say — the alias, the route, the
//! stage it is in. The handler hands the row over right before it writes it
//! ([`Unanswered::logging`]); dropped while it still holds it, the guard
//! writes it from a task of its own: status `499` — the code for a client
//! that closed its request — and `client_disconnected`, naming the stage.
//! Not `canceled`: that is a response that began (a stream cut off, a
//! caller's stop) and says 200; this one never had a status to send.
//!
//! Two platform cases leave more than that row (openai egress design §13.4,
//! after the WP9 review). A non-streamed chat on an `openai` row reads its
//! answer in a task of its own (`chat/unary.rs`), which borrows the row
//! ([`Unanswered::lend`]): when the client is gone by the time the answer is
//! read, that task writes the `499` with the answer's usage. A stream on an
//! `openai` row whose client goes away after the request went out and
//! before the upstream's 2xx counts its prompt locally
//! ([`Unanswered::count_if_sent`]), the budgets' lower bound.
//!
//! The counters ([`super::handle_count_tokens`], `/v1/messages/count_tokens`,
//! `/tokenize`) write their rows through the guard too
//! ([`Unanswered::answered`]): a count is metadata and a cheap one leaves no
//! row (api-docs design §5.4), but one that failed, or that had to bring its
//! model up first, is traffic like any other.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::config::Route;
use crate::gate::FallbackReason;
use crate::ingress::ClientProto;
use crate::ir::{ChatRequest, Usage};
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::*;

/// The row's `error_kind` for a request its client gave up on before it was
/// answered.
pub const CLIENT_DISCONNECTED: &str = "client_disconnected";

/// The status that row carries: "client closed request".
pub const CLIENT_CLOSED_STATUS: u16 = 499;

/// One request's open row (module doc). Opening it opens the in-flight gauge.
pub(crate) struct Unanswered {
    open: Option<Pending>,
}

/// What the row says, so far.
struct Pending {
    state: SharedState,
    proto: ClientProto,
    ctx: RequestCtx,
    alias: String,
    started: Instant,
    class: RequestClass,
    streamed: bool,
    route: Option<Box<Route>>,
    fallback: Option<FallbackReason>,
    /// Where the request was when it ended unanswered — the end of "the
    /// client disconnected while …".
    stage: &'static str,
    /// For a counter: this count's admission had to bring its model up
    /// ([`Unanswered::cold_start`]), which makes even its success a row.
    cold: bool,
    /// A platform request whose prompt counts once it was sent
    /// ([`Unanswered::count_if_sent`]).
    count: Option<PromptCount>,
}

/// The request a platform stream sent, and whether it went out yet.
struct PromptCount {
    ir: Arc<ChatRequest>,
    sent: Arc<AtomicBool>,
}

/// A request's open row, lent to a task that may outlive its handler
/// ([`Unanswered::lend`]). Dropped without being written or given back (the
/// task panicked, or its answer was never taken), it closes the in-flight
/// gauge, which the row would have.
pub(crate) struct Lent(Option<Pending>);

impl Drop for Lent {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            p.state.telemetry.request_abandoned();
        }
    }
}

/// What a lent row says besides its status, usage and error.
#[derive(Default)]
pub(crate) struct LentRow {
    pub ttfb_ms: Option<i64>,
    pub timings: Option<crate::ir::Timings>,
    pub max_tokens_clamped: Option<u32>,
    pub rung: Option<i64>,
    pub degraded: Option<String>,
    pub quantities: crate::pricing::Quantities,
    pub service_tier: crate::pricing::ServedTier,
}

impl Lent {
    /// Write the row, which closes the gauge: the task's, for a request
    /// whose handler is gone.
    pub(crate) async fn record(
        mut self,
        status: u16,
        usage: Usage,
        error: Option<(&str, String)>,
        row: LentRow,
        local_count: Option<crate::pricing::LocalCount>,
    ) {
        let Some(p) = self.0.take() else {
            return;
        };
        record_counted(
            LogParams {
                state: &p.state,
                proto: p.proto,
                ctx: &p.ctx,
                alias: p.alias,
                route: p.route.as_deref(),
                started: p.started,
                streamed: p.streamed,
                class: p.class,
                timings: row.timings,
                max_tokens_clamped: row.max_tokens_clamped,
                fallback: p.fallback,
                rung: row.rung,
                degraded: row.degraded,
                quantities: row.quantities,
                service_tier: row.service_tier,
            },
            status,
            row.ttfb_ms,
            usage,
            error,
            local_count,
        )
        .await;
    }
}

impl Unanswered {
    /// Open the gauge for a request of `proto`, as far as it is known yet.
    pub(crate) fn open(
        state: &SharedState,
        proto: ClientProto,
        ctx: &RequestCtx,
        alias: &str,
        started: Instant,
        class: RequestClass,
    ) -> Self {
        state.telemetry.request_started();
        Self {
            open: Some(Pending {
                state: state.clone(),
                proto,
                ctx: ctx.clone(),
                alias: alias.to_string(),
                started,
                class,
                streamed: false,
                route: None,
                fallback: None,
                stage: "reading the request",
                cold: false,
                count: None,
            }),
        }
    }

    fn with(&mut self, f: impl FnOnce(&mut Pending)) {
        if let Some(p) = self.open.as_mut() {
            f(p);
        }
    }

    /// Where the request is now: what the row says it was doing, should the
    /// client go away here.
    pub(crate) fn stage(&mut self, stage: &'static str) {
        self.with(|p| p.stage = stage);
    }

    /// The alias the request names, once the body is read.
    pub(crate) fn alias(&mut self, alias: &str) {
        self.with(|p| p.alias = alias.to_string());
    }

    pub(crate) fn streamed(&mut self, streamed: bool) {
        self.with(|p| p.streamed = streamed);
    }

    /// The route the gate settled on, and the fallback answering, if one is.
    ///
    /// Not named `route`: `tests/it/route_walk.rs` reads every call of a
    /// method by that name in the source as a router registration.
    pub(crate) fn routed_to(&mut self, route: &Route, fallback: Option<FallbackReason>) {
        self.with(|p| {
            p.route = Some(Box::new(route.clone()));
            p.fallback = fallback;
        });
    }

    /// This count's model was not up when it arrived, and its admission
    /// brought it up: the count is a row whatever its outcome.
    pub(crate) fn cold_start(&mut self) {
        self.with(|p| p.cold = true);
    }

    /// A platform stream's request (`ir`), whose prompt the row counts
    /// locally once `sent` says it went out, should the client go away
    /// before the upstream's 2xx: the upstream may already be working on it
    /// and bills for it (openai egress design §13.4).
    pub(crate) fn count_if_sent(&mut self, ir: Arc<ChatRequest>, sent: Arc<AtomicBool>) {
        self.with(|p| p.count = Some(PromptCount { ir, sent }));
    }

    /// The row, lent to a task that writes it should the handler be gone
    /// when the task is done; [`Self::restore`] takes it back. `None` when
    /// it was written already.
    pub(crate) fn lend(&mut self) -> Option<Lent> {
        self.open.take().map(|p| Lent(Some(p)))
    }

    /// Take back a row [`Self::lend`] lent: the handler writes it after
    /// all, or the guard does should the client go away from here on.
    pub(crate) fn restore(&mut self, lent: Option<Lent>) {
        if let Some(mut lent) = lent {
            self.open = lent.0.take();
        }
    }

    /// The handler writes the request's row now, and that row closes the
    /// gauge: the guard has nothing left to do.
    pub(crate) fn logging(&mut self) {
        self.open = None;
    }

    /// A counter's answer: its row, written now — for a failure, and for a
    /// success that brought its model up — or, for a cheap success, no row
    /// and only the gauge closed.
    pub(crate) async fn answered(mut self, status: u16, error: Option<(&str, String)>) {
        let Some(p) = self.open.take() else {
            return;
        };
        if error.is_none() && !p.cold {
            p.state.telemetry.request_abandoned();
            return;
        }
        p.record(status, error).await;
    }
}

impl Unanswered {
    /// [`Self::answered`] for a counter that failed with `e`.
    pub(crate) async fn failed(self, e: &crate::error::GatewayError) {
        self.answered(e.http_status().as_u16(), Some((e.kind(), e.log_text())))
            .await;
    }
}

impl Drop for Unanswered {
    fn drop(&mut self) {
        let Some(p) = self.open.take() else {
            return;
        };
        // The handler's future is dropped inside the runtime (hyper's task),
        // so the spawn goes through; with no runtime — teardown — there is no
        // database write to make either, and only the gauge is closed.
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            p.state.telemetry.request_abandoned();
            return;
        };
        let message = format!(
            "the client disconnected while {}; nothing was answered",
            p.stage
        );
        rt.spawn(async move {
            p.record(CLIENT_CLOSED_STATUS, Some((CLIENT_DISCONNECTED, message)))
                .await
        });
    }
}

impl Pending {
    /// The row, which closes the gauge ([`record`]). A platform stream's
    /// request that went out counts its prompt locally (§13.4).
    async fn record(self, status: u16, error: Option<(&str, String)>) {
        let sent = self
            .count
            .as_ref()
            .filter(|c| c.sent.load(Ordering::Relaxed))
            .map(|c| c.ir.clone());
        let (local_count, service_tier) = match (sent, self.route.as_deref()) {
            (Some(ir), Some(route)) => (
                floor_count(
                    route,
                    self.state.snapshot().is_local_upstream(route.upstream.id),
                    &ir,
                    String::new(),
                )
                .await,
                crate::pricing::ServedTier::of(
                    route.upstream.protocol,
                    None,
                    ir.requested_service_tier(),
                ),
            ),
            _ => (None, Default::default()),
        };
        record_counted(
            LogParams {
                state: &self.state,
                proto: self.proto,
                ctx: &self.ctx,
                alias: self.alias,
                route: self.route.as_deref(),
                started: self.started,
                streamed: self.streamed,
                class: self.class,
                timings: None,
                max_tokens_clamped: None,
                fallback: self.fallback,
                rung: None,
                degraded: None,
                quantities: Default::default(),
                service_tier,
            },
            status,
            None,
            Usage::default(),
            error,
            local_count,
        )
        .await;
    }
}

/// Whether a count resolved to `route` would have to bring its model up: a
/// local model with nothing of it running or starting. Asked before
/// admission; a fallback the admission swaps to is not a start.
pub(crate) fn would_start(state: &SharedState, route: &Route) -> bool {
    crate::vram::classify(route).is_some_and(|t| !state.vram.is_up(state, &t))
}
