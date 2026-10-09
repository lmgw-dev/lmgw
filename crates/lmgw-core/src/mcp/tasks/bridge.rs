//! The bridge (MCP Tasks design §1.7, T4): a task-augmented call for a
//! caller with no stored thread — `/mcp`, `/v1/responses`, an unbound
//! realtime session, an agent run, a temporary thread — followed inline
//! with the follower's own loop ([`Follow`]), and the caller answered with
//! the task's result as a normal `CallToolResult` (a JSON-RPC error stays an
//! error).
//!
//! At the row's `timeout_ms` from the call, or when the caller stops
//! waiting (its future dropped), lmgw sends `tasks/cancel` and the call
//! reports the timeout as a normal call's. Nothing is stored and no feed
//! record is written; the request rows are T20's: the call's own, and the
//! end's. On a device link `_meta["lmgw/task"]` says
//! `{"delivery": "wait", "thread_id": null}` (`meta`).
//!
//! The bridged task claims its key in the registry like any new task: an
//! open row of the same id ends as reused (`follow`), and a newer task that
//! reuses the bridged one's id ends the wait abandoned, cancelling nothing.
//! A server that does not declare `tasks.cancel` is sent none.

use std::sync::Weak;
use std::time::{Duration, Instant};

use rmcp::model::CallToolResult;

use crate::config::McpServer;
use crate::state::AppState;

use super::super::host::CallFrom;
use super::super::{CallError, McpManager};
use super::follow::{record_end, record_reused, reused_result, Event, Follow};
use super::registry::{Nudge, Ticket};
use super::wire::{self, Created};
use super::{bridged, reused_id, Ending};

/// Follow `created` until it ends or the call's `timeout_ms` passes.
pub(super) async fn wait(
    mgr: &McpManager,
    server: &McpServer,
    exposed: &str,
    created: Created,
    from: &CallFrom,
    started: Instant,
) -> Result<CallToolResult, CallError> {
    let app = mgr.state.get().cloned().unwrap_or_default();
    let Some(state) = app.upgrade() else {
        return Err(CallError::Upstream {
            server: server.name.clone(),
            detail: "the gateway is shutting down".into(),
        });
    };
    let info = created.info;
    // An open row holding the same id: the server reused it.
    match crate::store::mcp_tasks::reused(
        &state.db,
        server.id,
        &info.task_id,
        reused_result(server),
    )
    .await
    {
        Ok(rows) => record_reused(&state, server, &rows).await,
        Err(e) => tracing::error!(
            "mcp task {} on '{}': ending a row of a reused id failed: {e}",
            info.task_id,
            server.name
        ),
    }
    // Registered for the notifications.
    let mut inbox = mgr
        .tasks
        .claim((server.id, info.task_id.clone()), None, false);
    let mut guard = CancelOnDrop {
        app: app.clone(),
        server: server.clone(),
        task_id: Some(info.task_id.clone()),
        ticket: Some(inbox.ticket.clone()),
    };
    let mut follow = Follow::new((server.id, &server.name), exposed, &info);
    if !info.status.is_terminal() {
        follow.poll_later(&state);
    }
    drop(state);
    let deadline = started + Duration::from_millis(server.timeout_ms);
    let followed = tokio::time::timeout_at(deadline.into(), async {
        loop {
            match follow.next(&app, &mut inbox).await {
                Event::Ended(e) => return Some(e),
                Event::Gone => return None,
                Event::Nudge(Nudge::Reused) => {
                    return Some(Ending::Abandoned(reused_id(&server.name)))
                }
                Event::Moved | Event::Nudge(Nudge::Recheck | Nudge::Linked | Nudge::Status(_)) => {}
                Event::Nudge(Nudge::Cancel { reply, .. }) => {
                    // Not a stored task: nothing for a route to cancel.
                    drop(reply);
                }
            }
        }
    })
    .await;
    let Some(state) = app.upgrade() else {
        return Err(CallError::Upstream {
            server: server.name.clone(),
            detail: "the gateway is shutting down".into(),
        });
    };
    let ran = started.elapsed().as_millis() as i64;
    match followed {
        Ok(Some(ending)) => {
            guard.disarm(&state);
            record_end(&state, &from.logged_as, &follow, &ending, ran).await;
            bridged(&ending, &follow.task_id, exposed, &server.name)
        }
        Ok(None) => Err(CallError::Upstream {
            server: server.name.clone(),
            detail: "the gateway is shutting down".into(),
        }),
        Err(_) => {
            // The guard sends the cancel as it goes.
            let ending = Ending::Cancelled {
                by: Some("lmgw, which stopped waiting (the server's timeout_ms)".into()),
                told: true,
            };
            record_end(&state, &from.logged_as, &follow, &ending, ran).await;
            Err(CallError::Timeout {
                server: server.name.clone(),
                timeout_ms: server.timeout_ms,
            })
        }
    }
}

/// Sends `tasks/cancel` for a bridged task when dropped armed: the call
/// timed out, or its caller stopped waiting.
struct CancelOnDrop {
    app: Weak<AppState>,
    server: McpServer,
    task_id: Option<String>,
    ticket: Option<Ticket>,
}

impl CancelOnDrop {
    /// The task ended: nothing to cancel.
    fn disarm(&mut self, state: &AppState) {
        self.task_id = None;
        if let Some(ticket) = self.ticket.take() {
            state.mcp.tasks.forget(&ticket);
        }
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(state) = self.app.upgrade() else {
            return;
        };
        if let Some(ticket) = self.ticket.take() {
            state.mcp.tasks.forget(&ticket);
        }
        let Some(task_id) = self.task_id.take() else {
            return;
        };
        let server = self.server.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let Some(peer) = state.mcp.task_peer(&server).await else {
                    return;
                };
                if !wire::can_cancel(&peer) {
                    tracing::debug!(
                        "mcp task {task_id}: '{}' does not declare `tasks.cancel`: the bridge \
                         sends none",
                        server.name
                    );
                    return;
                }
                let bound = Duration::from_millis(server.timeout_ms);
                if let Err(e) = wire::cancel(&peer, &task_id, bound).await {
                    tracing::debug!("mcp task {task_id}: the bridge's tasks/cancel: {e:?}");
                }
            });
        }
    }
}
