//! A wake for a thread's bound session when the thread's MCP task results
//! may have moved (MCP Tasks design §3.4): a result entered the thread
//! (`chat_tasks::deliver`), or a turn of it let it go — its reply may have
//! answered them, or it was refused before it answered anything.
//!
//! The thread's id, nothing else, as [`super::ApprovalWake`]: the session
//! reads the results from the thread itself, so a session that lagged
//! behind the channel reads its own thread's then, and a missed wake loses
//! nothing.

use super::LiveTurns;

/// The wake's channel (module doc).
pub(crate) struct ResultWake(tokio::sync::broadcast::Sender<i64>);

impl Default for ResultWake {
    fn default() -> Self {
        // A wake is a thread id: a lag is read as a wake for every thread
        // (module doc), so the ring's size bounds nothing a session needs.
        Self(tokio::sync::broadcast::channel(256).0)
    }
}

impl LiveTurns {
    /// Thread `thread_id`'s MCP task results may have moved (module doc).
    pub(crate) fn results_moved(&self, thread_id: i64) {
        let _ = self.inner.results.0.send(thread_id);
    }

    /// The wakes of [`Self::results_moved`], from now on.
    pub(crate) fn result_wakes(&self) -> tokio::sync::broadcast::Receiver<i64> {
        self.inner.results.0.subscribe()
    }
}
