//! The request-log row's write, and the in-flight gauge it closes, as a task
//! of its own.
//!
//! The write is an await — the insert — and the handler around it can be
//! dropped at any await: its client went away. Dropped mid-insert, the row
//! was lost and the `requests.active` gauge, which the row's
//! [`TelemetryBus::request_finished`] closes, kept counting a request that was
//! over. Spawned, the write finishes whatever happens to the handler; the
//! handler still waits for it, so a response never overtakes its own row.

use crate::state::SharedState;
use crate::store::{self, NewRequestLog};
use crate::telemetry::{RequestSummary, TelemetryBus};

/// Insert `row`, then close the gauge with it and put it on the live feed.
pub(super) async fn write_row(state: &SharedState, row: NewRequestLog, status: u16) {
    let st = state.clone();
    let task = tokio::spawn(async move {
        let log_id = store::insert_request_log(&st.db, &row)
            .await
            .unwrap_or_else(|e| {
                tracing::error!("failed to write request log: {e}");
                0
            });
        finished(&st.telemetry, log_id, row, status);
    });
    // A panic in the task has already been reported by the runtime; the
    // handler answers either way.
    let _ = task.await;
}

fn finished(telemetry: &TelemetryBus, log_id: i64, row: NewRequestLog, status: u16) {
    telemetry.request_finished(RequestSummary {
        log_id,
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        client_key: row.client_key,
        ingress_proto: row.ingress_proto,
        requested_alias: row.requested_alias,
        upstream_name: row.upstream_name,
        upstream_model: row.upstream_model,
        egress_proto: row.egress_proto,
        status,
        ttfb_ms: row.ttfb_ms,
        total_ms: row.total_ms,
        prompt_tokens: row.prompt_tokens,
        completion_tokens: row.completion_tokens,
        cached_in_tokens: row.cached_in_tokens,
        cache_write_tokens: row.cache_write_tokens,
        streamed: row.streamed,
        error_kind: row.error_kind,
        error_msg: row.error_msg,
        cost_micro: row.cost.total_micro,
        class: row.class.as_str().to_string(),
        key_id: row.key_id,
        fallback_reason: row.fallback_reason,
        rung: row.rung,
        degraded: row.degraded,
    });
}
