//! The cancel from lmgw (MCP Tasks design §1.5): [`McpManager::cancel_task`]
//! for a route, the follower's `tasks/cancel`, and the owed cancel's
//! follower.
//!
//! - **The route's check is here**: a row is cancelled only for the thread
//!   it belongs to, and only while it is `open`.
//! - **A server that does not declare `tasks.cancel`** is not sent one: the
//!   cancel is refused, saying so, and the task goes on being followed.
//! - **A `tasks/cancel` answered with a JSON-RPC error** is refused with the
//!   server's message; the task goes on being followed. Only one that got no
//!   answer (no session, or none within the bound) is owed.
//! - **An owed cancel** is sent at the server's next connection, whatever it
//!   answers, and the row then goes (or stays `ended` while its result waits
//!   for delivery); to a server that does not declare `tasks.cancel` it is
//!   dropped unsent.

use std::sync::Weak;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::state::{AppState, SharedState};
use crate::store::mcp_tasks::{self, McpTaskRow};

use super::super::McpManager;
use super::follow::{end_row, poll_setting, Follow};
use super::registry::{Inbox, Nudge};
use super::wire::{self, WireError};
use super::{no_longer_knows, Ending, TaskStatus};

/// What a cancel from lmgw did (§1.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The server cancelled it; the row ended `cancelled` "by <who>".
    Cancelled,
    /// The work finished first; the row ended with its real result.
    FinishedFirst,
    /// The server was not connected: the row ended `cancelled` at once, and
    /// the cancel is sent when it connects.
    Owed,
    /// The server answered the cancel without ending the task: it goes on
    /// being followed.
    Requested,
}

/// Why lmgw did not cancel a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelRefusal {
    /// No such row in that thread.
    NotFound,
    /// The task already ended.
    Ended,
    /// The server does not declare `tasks.cancel`: why, in a sentence. The
    /// task goes on being followed.
    Unsupported(String),
    /// The server answered `tasks/cancel` with a JSON-RPC error: its
    /// message. The task goes on being followed.
    Server(String),
}

impl McpManager {
    /// Cancel task row `id` of thread `thread_id` for `by` (§1.5): the
    /// owner's or a device's cancel, as the route names them. A row of
    /// another thread is `NotFound`, one no longer `open` is `Ended`.
    pub async fn cancel_task(
        &self,
        thread_id: i64,
        id: i64,
        by: &str,
    ) -> Result<CancelOutcome, CancelRefusal> {
        let Some(app) = self.app() else {
            return Err(CancelRefusal::NotFound);
        };
        let row = match mcp_tasks::get(&app.db, id).await {
            Ok(Some(row)) if row.thread_id == Some(thread_id) => row,
            _ => return Err(CancelRefusal::NotFound),
        };
        if row.state != mcp_tasks::OPEN {
            return Err(CancelRefusal::Ended);
        }
        let key = (row.server_id, row.task_id.clone());
        let ask = || {
            let (reply, answer) = oneshot::channel();
            (
                Nudge::Cancel {
                    by: by.to_string(),
                    reply,
                },
                answer,
            )
        };
        let (nudge, mut answer) = ask();
        if self.tasks.nudge(&key, nudge).is_err() {
            // Not followed yet (a resume racing the route): follow it.
            self.resume_tasks().await;
            let (nudge, again) = ask();
            if self.tasks.nudge(&key, nudge).is_err() {
                return Err(CancelRefusal::Ended);
            }
            answer = again;
        }
        answer.await.unwrap_or(Err(CancelRefusal::Ended))
    }
}

/// The cancel of open row `id` (§1.5), by its follower.
pub(super) async fn cancel_open(
    state: &SharedState,
    id: i64,
    follow: &mut Follow,
    by: String,
) -> Result<CancelOutcome, CancelRefusal> {
    let server = state.snapshot().mcp_servers.get(&follow.server_id).cloned();
    let peer = match &server {
        Some(s) => state.mcp.task_peer(s).await,
        None => None,
    };
    let owed = |by: String| Ending::Cancelled {
        by: Some(by),
        told: false,
    };
    let (Some(server), Some(peer)) = (server, peer) else {
        end_row(state, id, follow, &owed(by)).await;
        return Ok(CancelOutcome::Owed);
    };
    if !wire::can_cancel(&peer) {
        return Err(CancelRefusal::Unsupported(format!(
            "server '{}' does not declare `tasks.cancel`, so its jobs cannot be cancelled from \
             lmgw; job {} goes on",
            server.name, follow.task_id
        )));
    }
    let bound = Duration::from_millis(server.timeout_ms);
    match wire::cancel(&peer, &follow.task_id, bound).await {
        Ok(info) if info.status == TaskStatus::Cancelled => {
            let ending = Ending::Cancelled {
                by: Some(by),
                told: true,
            };
            end_row(state, id, follow, &ending).await;
            Ok(CancelOutcome::Cancelled)
        }
        Ok(info) if info.status.is_terminal() => {
            follow.status = info.status;
            Ok(finished_first(state, id, follow, &peer, bound).await)
        }
        Ok(info) => {
            follow.status = info.status;
            Ok(CancelOutcome::Requested)
        }
        // Already terminal: the work finished first (§1.5).
        Err(WireError::Unknown(_)) => {
            if !follow.status.is_terminal() {
                follow.status = TaskStatus::Completed;
            }
            Ok(finished_first(state, id, follow, &peer, bound).await)
        }
        Err(WireError::Rpc(message)) => Err(CancelRefusal::Server(message)),
        Err(WireError::Link(e)) => {
            tracing::info!(
                "mcp task {}: tasks/cancel did not reach '{}' ({e}): owed",
                follow.task_id,
                server.name
            );
            end_row(state, id, follow, &owed(by)).await;
            Ok(CancelOutcome::Owed)
        }
    }
}

/// The task ended before the cancel: its real result.
async fn finished_first(
    state: &SharedState,
    id: i64,
    follow: &mut Follow,
    peer: &rmcp::service::Peer<rmcp::RoleClient>,
    bound: Duration,
) -> CancelOutcome {
    let r = wire::result(peer, &follow.task_id, Some(bound)).await;
    let ending = follow.ending_of(state, r).unwrap_or_else(|| {
        Ending::Abandoned(no_longer_knows(
            &state
                .snapshot()
                .mcp_servers
                .get(&follow.server_id)
                .map(|s| s.name.clone())
                .unwrap_or_default(),
        ))
    });
    end_row(state, id, follow, &ending).await;
    CancelOutcome::FinishedFirst
}

/// Send the owed cancel of `row` at its server's next connection, whatever
/// it answers, then let the row go.
pub(super) async fn follow_owed(app: Weak<AppState>, row: McpTaskRow, mut inbox: Inbox) {
    let mut due = tokio::time::Instant::now();
    loop {
        tokio::select! {
            n = inbox.recv() => match n {
                None => return,
                // The id was reused: the claim dropped the owed cancel.
                Some(Nudge::Reused) => break,
                Some(Nudge::Cancel { reply, .. }) => {
                    let _ = reply.send(Ok(CancelOutcome::Owed));
                    continue;
                }
                Some(Nudge::Linked | Nudge::Recheck) => {}
                Some(Nudge::Status(_)) => continue,
            },
            () = tokio::time::sleep_until(due) => {}
        }
        let Some(state) = app.upgrade() else {
            return;
        };
        due = tokio::time::Instant::now() + poll_setting(&state);
        match mcp_tasks::get(&state.db, row.id).await {
            Ok(Some(now)) if now.state == mcp_tasks::CANCEL_OWED => {}
            Ok(_) => break,
            Err(_) => continue,
        }
        let Some(server) = state.snapshot().mcp_servers.get(&row.server_id).cloned() else {
            continue;
        };
        let Some(peer) = state.mcp.task_peer(&server).await else {
            continue;
        };
        if wire::can_cancel(&peer) {
            let bound = Duration::from_millis(server.timeout_ms);
            match wire::cancel(&peer, &row.task_id, bound).await {
                Err(WireError::Link(e)) => {
                    tracing::debug!("mcp task {}: owed tasks/cancel: {e}", row.task_id);
                    continue;
                }
                answered => tracing::info!(
                    "mcp task {}: the owed tasks/cancel reached '{}' ({})",
                    row.task_id,
                    server.name,
                    match answered {
                        Ok(info) => info.status.as_str().to_string(),
                        Err(e) => format!("{e:?}"),
                    }
                ),
            }
        } else {
            tracing::info!(
                "mcp task {}: '{}' does not declare `tasks.cancel`: its owed cancel is dropped \
                 unsent",
                row.task_id,
                server.name
            );
        }
        if let Err(e) = mcp_tasks::owed_cancel_sent(&state.db, row.id).await {
            tracing::error!("mcp task {}: clearing its owed cancel: {e}", row.task_id);
        }
        break;
    }
    if let Some(state) = app.upgrade() {
        state.mcp.tasks.forget(&inbox.ticket);
    }
}
