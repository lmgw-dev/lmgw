//! The row of a server-side call the response's stop dropped
//! (realtime-server-tools design §2.5): a cancel, a barge-in, the session's
//! end. The executors write a call's row when it returns, and a dropped
//! future never does — one dropped is one whose row was not begun
//! (`RowWatch`) — so the responder writes it — `canceled`, as a model
//! call its consumer stopped is (status 200), with the time it ran and the
//! words its item gets ([`ABANDONED_CALL`]: it was sent, so it may still
//! have run). Under `realtime-tool` and the session's key, naming the
//! server the executors' rows would name, so it sits beside its finished
//! siblings in Logs. A call the stop ended before it was sent writes none:
//! no server ever saw it.

use std::time::Instant;

use crate::agent::ABANDONED_CALL;
use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::telemetry::REALTIME_TOOL_PROTO;

/// What a dropped call's row is written with.
pub(in crate::realtime::responder) struct Rows<'a> {
    pub state: &'a SharedState,
    pub ctx: &'a RequestCtx,
    /// The names a built-in toolset runs in-process (`ServerTools`).
    pub builtin: &'a [String],
}

impl Rows<'_> {
    /// The call of `name`, sent at `started`, was dropped.
    pub async fn record(&self, name: &str, started: Instant) {
        let builtin = self.builtin.iter().any(|b| b == name);
        let server = crate::mcp::exec::server_of(self.state, name, builtin).await;
        tracing::debug!(
            "realtime: the call of '{name}' was dropped by the response's stop after {} ms; \
             its row says canceled",
            started.elapsed().as_millis()
        );
        crate::mcp::ingress::record_tool_canceled(
            self.state,
            self.ctx,
            REALTIME_TOOL_PROTO,
            (name, server),
            started,
            ABANDONED_CALL,
        )
        .await;
    }
}
