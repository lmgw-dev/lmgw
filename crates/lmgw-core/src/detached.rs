//! Work that outlives the request that started it.

/// `work` as a task of its own, in the current span, awaited by the caller
/// (*changed 2026-10-23*, moved here from `devices` the same day). An owner's
/// write to the store is followed by things that only this process does: the
/// snapshot reload (whose MCP reconcile can take seconds), revoked
/// connections, cache invalidations, a frame on the feed. hyper drops a
/// handler future when its client closes the connection, and everything after
/// the store commit would be lost with it: the snapshot would serve the
/// replaced row, a revoked connection would stay open, a cache would keep
/// answering for the old upstream. Awaiting the task's handle keeps the
/// answer; dropping the handle stops nothing. A task that failed (a panic) is
/// a warn log naming `what` and a 500 `internal`.
pub(crate) async fn detached<T, F>(what: &str, work: F) -> Result<T, crate::principal::Refusal>
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    use tracing::Instrument;
    match tokio::spawn(work.in_current_span()).await {
        Ok(done) => Ok(done),
        Err(e) => {
            tracing::warn!("{what}: the task failed: {e}");
            Err(crate::principal::Refusal {
                status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                code: "internal",
                message: format!("{what} failed; try again"),
            })
        }
    }
}
