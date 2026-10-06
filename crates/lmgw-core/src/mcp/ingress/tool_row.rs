//! The `request_logs` row of **one tool call**, from every producer: the
//! northbound `/mcp` `tools/call`, and the in-process executors a model's
//! run calls through (`/v1/responses`, the Chat, agents, `/v1/realtime`;
//! §21). They differ only in `ingress_proto`, so the row shape — NULL
//! tokens, a synthesized status, live-feed broadcast — cannot drift between
//! them.
//!
//! **A dropped call** (realtime-server-tools design §2.5): an executor
//! writes its row when the call returns, and a future its run's cancel
//! dropped never does. Whoever dropped it writes [`record_tool_canceled`]
//! instead: `canceled`, with the status a model call stopped by its
//! consumer gets (200, `proxy::row_status`) and the time it ran.
//!
//! **Exactly one row per call.** The cancel may come while the executor is
//! already writing its own row — the call returned. Dropped then, that row
//! may or may not be written, and a `canceled` one beside it would be a
//! second. So a caller that may drop a call runs it under a [`RowWatch`]:
//! the row's write says so as it starts, and the caller lets such a call
//! finish rather than drop it. Every executor's row goes through here, so
//! none of them needs to know about the stop.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::store::{self, NewRequestLog};

/// How a call ended, as its row says it: status, error kind, message.
type Ending = (i64, Option<String>, Option<String>);

tokio::task_local! {
    /// Raised as a row's write starts, inside a [`RowWatch`]'s scope.
    static WRITING: Arc<AtomicBool>;
}

/// Whether the call run in [`Self::scope`] began writing its row (module
/// doc).
#[derive(Debug, Default)]
pub(crate) struct RowWatch(Arc<AtomicBool>);

impl RowWatch {
    /// `call`, its row's write watched.
    pub fn scope<F: Future>(&self, call: F) -> impl Future<Output = F::Output> {
        WRITING.scope(self.0.clone(), call)
    }

    /// The call returned, and its executor is writing its row: let it
    /// finish.
    pub fn writing(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Write the row of one tool call that returned: `error` is its tool error,
/// a synthesized 502 (module doc).
///
/// `ingress_proto` must be one the telemetry bus excludes from the token
/// aggregates ([`crate::telemetry::counts_in_token_stats`]); a tool call has no
/// tokens and a synthesized status, so counting it would skew both.
pub(crate) async fn record_tool_call(
    state: &SharedState,
    ctx: &RequestCtx,
    ingress_proto: &str,
    exposed_tool: &str,
    server_name: Option<String>,
    started: Instant,
    error: Option<String>,
) {
    let ending = match error {
        Some(msg) => (502, Some("tool_error".to_string()), Some(msg)),
        None => (200, None, None),
    };
    write(
        state,
        ctx,
        ingress_proto,
        (exposed_tool, server_name),
        started,
        ending,
    )
    .await
}

/// Write the row of one tool call its run's cancel dropped while it ran
/// (module doc): `canceled`, `note` saying what became of it.
pub(crate) async fn record_tool_canceled(
    state: &SharedState,
    ctx: &RequestCtx,
    ingress_proto: &str,
    (exposed_tool, server_name): (&str, Option<String>),
    started: Instant,
    note: &str,
) {
    let ending = (200, Some("canceled".to_string()), Some(note.to_string()));
    write(
        state,
        ctx,
        ingress_proto,
        (exposed_tool, server_name),
        started,
        ending,
    )
    .await
}

async fn write(
    state: &SharedState,
    ctx: &RequestCtx,
    ingress_proto: &str,
    (exposed_tool, server_name): (&str, Option<String>),
    started: Instant,
    (status, error_kind, error_msg): Ending,
) {
    debug_assert!(
        !crate::telemetry::counts_in_token_stats(ingress_proto),
        "tool-call rows must be excluded from the token stats"
    );
    // Outside a watch there is nobody to tell.
    let _ = WRITING.try_with(|w| w.store(true, Ordering::SeqCst));
    let total_ms = started.elapsed().as_millis() as i64;
    let row = NewRequestLog {
        client_key: ctx.client_key.clone(),
        ingress_proto: ingress_proto.to_string(),
        requested_alias: exposed_tool.to_string(),
        upstream_id: None,
        upstream_name: server_name,
        upstream_model: None,
        mcp_tool: Some(exposed_tool.to_string()),
        egress_proto: Some("mcp".to_string()),
        status,
        // ttfb is meaningless for a single tool call; total_ms is the latency.
        ttfb_ms: None,
        total_ms: Some(total_ms),
        // Tokens are meaningless for tools/call (§10 fix 2) — NULL, excluded.
        prompt_tokens: None,
        completion_tokens: None,
        streamed: false,
        error_kind,
        error_msg,
        // A tool call is not a model call: no tokens, no price, and the rollup
        // keeps it out of the unpriced remainder for that reason (§2.3).
        key_id: state.snapshot().key_id_for_name(
            ctx.client_key.as_deref().unwrap_or_else(|| {
                crate::telemetry::internal_identity(ingress_proto).unwrap_or("")
            }),
        ),
        class: crate::telemetry::RequestClass::Tool,
        ..Default::default()
    };
    let log_id = store::insert_request_log(&state.db, &row)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("failed to write MCP tool-call log: {e}");
            0
        });
    // Broadcast onto the live feed like any request (the §10 observability
    // claim) — request_finished excludes `mcp` from the token/error counters.
    state
        .telemetry
        .request_finished(crate::telemetry::RequestSummary {
            log_id,
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            client_key: row.client_key.clone(),
            ingress_proto: row.ingress_proto,
            requested_alias: row.requested_alias,
            upstream_name: row.upstream_name,
            upstream_model: row.mcp_tool.clone(),
            egress_proto: row.egress_proto,
            status: status as u16,
            ttfb_ms: row.ttfb_ms,
            total_ms: row.total_ms,
            prompt_tokens: row.prompt_tokens,
            completion_tokens: row.completion_tokens,
            cached_in_tokens: row.cached_in_tokens,
            cache_write_tokens: row.cache_write_tokens,
            streamed: row.streamed,
            error_kind: row.error_kind,
            error_msg: row.error_msg,
            // A tool execution has no money dimension at all — not an unpriced
            // one. The rollup keeps it out of the remainder for the same reason.
            cost_micro: None,
            class: row.class.as_str().to_string(),
            key_id: row.key_id,
            fallback_reason: None,
            rung: None,
            degraded: None,
        });
}
