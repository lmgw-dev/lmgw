//! The follower (MCP Tasks design §1.3–§1.6): every stored task in state
//! `open` is followed until its receiver says it ended, and every owed
//! cancel waits for its server.
//!
//! - **One follower per row**, a tokio task the registry
//!   ([`Tasks`](super::Tasks)) knows by `(server, task id)`; a bridged task
//!   registers too, for the notifications. No cap (T9): open tasks are as
//!   many as servers start.
//! - **Polling**: `tasks/get` every `pollInterval` (the task's own; `0`
//!   reads as not given), else every `mcp.task_poll_interval_s`, read at
//!   each poll (T8). While there is no session to ask (a device offline, a
//!   registered server that does not connect, a row gone) the next look is
//!   the setting's interval away, whatever the task's own. A device row is
//!   polled only while its link is `Ready`, and at once when it links again
//!   ([`McpManager::task_server_linked`]); a registered server is connected
//!   by the lazy path as a call would be.
//! - **Notifications** (`notifications/tasks/status`, via the handler) act
//!   at once, the latest of a burst only; polling continues beside them.
//! - **`input_required`** (T19): one `tasks/result` held open, polling beside
//!   it; its answer is the result. A follower that goes stops it.
//! - **The end**: a terminal status fetches `tasks/result`; `-32602` is
//!   abandoned; the row moves to `ended` with §1.4's result, and the end's
//!   request row is written (T20). lmgw never ends a task by its own clock or
//!   on a link drop (T7).
//! - **A reused id**: a server that answers a new task with the id of one
//!   lmgw still follows ended the older one, as far as lmgw can tell. The
//!   older row ends `abandoned` ("… reused its task id …"), its end's
//!   request row written, an owed cancel of it dropped; the new task is
//!   followed under a row of its own and never cancelled for it.
//! - **Status writes** are one `begin_write` each, never around a network
//!   call.
//! - **Cancel from lmgw**: [`cancel`](super::cancel).
//! - **At start** ([`McpManager::resume_tasks`]) every `open` and
//!   `cancel_owed` row gets its follower.

use std::sync::Weak;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

use crate::config::McpServer;
use crate::proxy::RequestCtx;
use crate::state::{AppState, SharedState};
use crate::store::mcp_tasks::{self, McpTaskRow};

use super::super::host::CallFrom;
use super::super::ingress::{record_tool_call, record_tool_canceled};
use super::super::{CallError, McpManager};
use super::cancel::{cancel_open, follow_owed, CancelOutcome};
use super::registry::{Inbox, Nudge};
use super::wire::{self, Payload, WireError};
use super::{ending_blocks, header, no_longer_knows, reused_id, started_text, Ending, Late};
use super::{LoggedAs, RowEnd, TaskInfo, TaskStatus};

/// One task as a follower or the bridge watches it.
pub(super) struct Follow {
    pub(super) server_id: i64,
    /// The server's name as the task's result names it: the row's, or the
    /// label the tool was offered under for a stored task.
    server: String,
    pub(super) task_id: String,
    pub(super) tool: String,
    pub(super) status: TaskStatus,
    pub(super) status_message: Option<String>,
    pub(super) poll_ms: Option<u64>,
    /// The `tasks/result` held open while `input_required` (T19); stopped
    /// when the follower goes.
    held: Option<JoinHandle<Result<Payload, WireError>>>,
    /// When the next `tasks/get` is due.
    due: tokio::time::Instant,
}

/// What [`Follow::next`] saw.
pub(super) enum Event {
    /// The task ended.
    Ended(Ending),
    /// Its status, status message or poll interval moved.
    Moved,
    /// A word the watcher handles itself (a cancel, a recheck, a reuse).
    Nudge(Nudge),
    /// The registry forgot it, or the gateway is going.
    Gone,
}

/// What one look at the task found.
enum Seen {
    Ended(Ending),
    Moved,
    Same,
}

/// `mcp.task_poll_interval_s`, as a wait (T8).
pub(super) fn poll_setting(state: &AppState) -> Duration {
    Duration::from_secs(u64::from(
        state.snapshot().settings.mcp.task_poll_interval_s.max(1),
    ))
}

impl Follow {
    pub(super) fn new((server_id, server): (i64, &str), tool: &str, info: &TaskInfo) -> Self {
        Self {
            server_id,
            server: server.to_string(),
            task_id: info.task_id.clone(),
            tool: tool.to_string(),
            status: info.status,
            status_message: info.status_message.clone(),
            poll_ms: info.poll_interval_ms,
            held: None,
            due: tokio::time::Instant::now(),
        }
    }

    /// Stored row `row`'s task, as it was last seen.
    pub(super) fn of_row(row: &McpTaskRow) -> Self {
        let info = TaskInfo {
            task_id: row.task_id.clone(),
            status: TaskStatus::parse(&row.status).unwrap_or(TaskStatus::Working),
            status_message: row.status_message.clone(),
            poll_interval_ms: row.poll_interval_ms.and_then(|v| u64::try_from(v).ok()),
            ttl_ms: row.ttl_ms.and_then(|v| u64::try_from(v).ok()),
        };
        Self::new((row.server_id, &row.server_label), &row.tool, &info)
    }

    /// The wait before the next poll: the task's own `pollInterval`, else
    /// the setting (T8). A `pollInterval` of 0 is no interval: as not given.
    fn interval(&self, state: &AppState) -> Duration {
        match self.poll_ms.filter(|&ms| ms > 0) {
            Some(ms) => Duration::from_millis(ms),
            None => poll_setting(state),
        }
    }

    /// Poll next after one interval from now.
    pub(super) fn poll_later(&mut self, state: &AppState) {
        self.due = tokio::time::Instant::now() + self.interval(state);
    }

    /// Wait for the next thing that happens to the task.
    pub(super) async fn next(&mut self, app: &Weak<AppState>, inbox: &mut Inbox) -> Event {
        enum Wake {
            Nudge(Option<Nudge>),
            Held(Result<Payload, WireError>),
            Due,
        }
        loop {
            let wake = tokio::select! {
                n = inbox.recv() => Wake::Nudge(n),
                r = async { self.held.as_mut().expect("guarded").await }, if self.held.is_some() => {
                    self.held = None;
                    Wake::Held(r.unwrap_or_else(|e| Err(WireError::Link(e.to_string()))))
                }
                () = tokio::time::sleep_until(self.due) => Wake::Due,
            };
            let Some(state) = app.upgrade() else {
                return Event::Gone;
            };
            let seen = match wake {
                Wake::Nudge(None) => return Event::Gone,
                Wake::Nudge(Some(Nudge::Linked)) => {
                    self.due = tokio::time::Instant::now();
                    continue;
                }
                Wake::Nudge(Some(Nudge::Status(info))) => self.look(&state, Some(info)).await,
                Wake::Nudge(Some(other)) => return Event::Nudge(other),
                Wake::Held(r) => match self.ending_of(&state, r) {
                    Some(e) => Seen::Ended(e),
                    // The held request went with its link: poll again.
                    None => Seen::Same,
                },
                Wake::Due => {
                    self.poll_later(&state);
                    self.look(&state, None).await
                }
            };
            match seen {
                Seen::Ended(e) => {
                    self.stop_held();
                    return Event::Ended(e);
                }
                Seen::Moved => return Event::Moved,
                Seen::Same => continue,
            }
        }
    }

    fn stop_held(&mut self) {
        if let Some(h) = self.held.take() {
            h.abort();
        }
    }

    /// Nothing to ask now (no session, or no server row): the next look is
    /// the setting's interval away, whatever the task's own — a server's
    /// short `pollInterval` is for asking it, not for waiting for it.
    fn wait_offline(&mut self, state: &AppState) {
        self.due = tokio::time::Instant::now() + poll_setting(state);
    }

    /// One look: `told` is what a notification said; `None` asks
    /// `tasks/get`. Offline, or a session that does not answer: nothing
    /// changes, and the next poll asks again.
    async fn look(&mut self, state: &AppState, told: Option<TaskInfo>) -> Seen {
        let Some(server) = state.snapshot().mcp_servers.get(&self.server_id).cloned() else {
            // Not in the snapshot: a row whose delete ended its tasks, or
            // one a reload has not published yet. Gone from the store too,
            // nothing will ever answer: the task ends abandoned (a row the
            // delete ended already stays as it ended).
            if matches!(
                mcp_tasks::server_stored(&state.db, self.server_id).await,
                Ok(false)
            ) {
                return Seen::Ended(Ending::Abandoned(super::removed(
                    &self.server,
                    "while lmgw followed the job",
                )));
            }
            self.wait_offline(state);
            return Seen::Same;
        };
        let Some(peer) = state.mcp.task_peer(&server).await else {
            self.wait_offline(state);
            return Seen::Same;
        };
        let bound = Duration::from_millis(server.timeout_ms);
        let info = match told {
            Some(info) => info,
            None => match wire::get(&peer, &self.task_id, bound).await {
                Ok(info) => info,
                Err(WireError::Unknown(_)) => {
                    return Seen::Ended(Ending::Abandoned(no_longer_knows(&server.name)))
                }
                Err(e) => {
                    tracing::debug!(
                        "mcp task {} on '{}': tasks/get: {e:?}",
                        self.task_id,
                        server.name
                    );
                    return Seen::Same;
                }
            },
        };
        let moved = info.status != self.status
            || info.status_message != self.status_message
            || (info.poll_interval_ms.is_some() && info.poll_interval_ms != self.poll_ms);
        self.status = info.status;
        self.status_message = info.status_message;
        if info.poll_interval_ms.is_some() {
            self.poll_ms = info.poll_interval_ms;
        }
        match self.status {
            TaskStatus::Cancelled => {
                return Seen::Ended(Ending::Cancelled {
                    by: None,
                    told: true,
                })
            }
            s if s.is_terminal() => {
                self.stop_held();
                let r = wire::result(&peer, &self.task_id, Some(bound)).await;
                return match self.ending_of(state, r) {
                    Some(e) => Seen::Ended(e),
                    None => Seen::Same,
                };
            }
            TaskStatus::InputRequired if self.held.is_none() => {
                let (peer, id) = (peer.clone(), self.task_id.clone());
                self.held = Some(tokio::spawn(
                    async move { wire::result(&peer, &id, None).await },
                ));
            }
            _ => {}
        }
        if moved {
            Seen::Moved
        } else {
            Seen::Same
        }
    }

    /// The ending a `tasks/result` answer makes; `None` when it did not
    /// arrive (the session went), to be asked again.
    pub(super) fn ending_of(
        &self,
        state: &AppState,
        r: Result<Payload, WireError>,
    ) -> Option<Ending> {
        let name = || {
            state
                .snapshot()
                .mcp_servers
                .get(&self.server_id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| format!("#{}", self.server_id))
        };
        Some(match r {
            Ok(Payload::Tool(result)) => Ending::Result {
                status: match (self.status, result.is_error) {
                    (TaskStatus::Failed, _) | (_, Some(true)) => TaskStatus::Failed,
                    _ => TaskStatus::Completed,
                },
                result,
            },
            Ok(Payload::Error(message)) => Ending::Error {
                message,
                status_message: self.status_message.clone(),
            },
            // A failed task's own JSON-RPC error may be -32602 too.
            Err(WireError::Unknown(message)) if self.status == TaskStatus::Failed => {
                Ending::Error {
                    message,
                    status_message: self.status_message.clone(),
                }
            }
            Err(WireError::Unknown(_)) => Ending::Abandoned(no_longer_knows(&name())),
            Err(WireError::Rpc(message)) => Ending::Error {
                message,
                status_message: self.status_message.clone(),
            },
            Err(WireError::Link(_)) => return None,
        })
    }
}

impl Drop for Follow {
    /// A follower or a bridged wait that goes leaves no `tasks/result`
    /// running behind it.
    fn drop(&mut self) {
        self.stop_held();
    }
}

/// How long ago row `row` was created, in milliseconds (for the end's row).
fn age_ms(row: &McpTaskRow) -> i64 {
    chrono::NaiveDateTime::parse_from_str(&row.created_at, "%Y-%m-%d %H:%M:%S")
        .map(|t| {
            (chrono::Utc::now().naive_utc() - t)
                .num_milliseconds()
                .max(0)
        })
        .unwrap_or(0)
}

/// The stored result of a row ended because `server` reused its task id.
pub(super) fn reused_result(server: &McpServer) -> impl Fn(&McpTaskRow) -> String + '_ {
    move |row| {
        let ending = Ending::Abandoned(reused_id(&server.name));
        serde_json::to_string(&ending_blocks(&ending, &row.task_id, &row.tool)).unwrap_or_default()
    }
}

/// The end's request rows (T20) of the open `rows` ended because `server`
/// reused their task id.
pub(super) async fn record_reused(state: &SharedState, server: &McpServer, rows: &[McpTaskRow]) {
    let ending = Ending::Abandoned(reused_id(&server.name));
    for row in rows {
        tracing::info!(
            "mcp task {} on '{}': the server reused its id for a new task; row {} ended \
             abandoned",
            row.task_id,
            server.name,
            row.id
        );
        let logged = LoggedAs {
            client_key: row.started_by.clone(),
            proto: crate::telemetry::CHAT_TOOL_PROTO,
        };
        record_end(state, &logged, &Follow::of_row(row), &ending, age_ms(row)).await;
        if let Some(thread_id) = row.thread_id {
            crate::web::chat_tasks::deliver::soon(state, thread_id);
        }
    }
}

impl McpManager {
    /// A late call was answered with a task (§1.2): the row inserted (an
    /// older row holding the same id ended as reused), its follower
    /// started, and the call answered `started, job <task id>`.
    pub(super) async fn start_late(
        &self,
        server: &McpServer,
        exposed: &str,
        created: wire::Created,
        from: &CallFrom,
        late: &Late,
    ) -> Result<(rmcp::model::CallToolResult, String), CallError> {
        let Some(app) = self.app() else {
            return Err(CallError::Upstream {
                server: server.name.clone(),
                detail: "the gateway is shutting down".into(),
            });
        };
        let info = &created.info;
        let label = super::super::exec::server_label(server);
        let new = mcp_tasks::NewMcpTask {
            server_id: server.id,
            server_label: &label,
            task_id: &info.task_id,
            thread_id: late.thread_id,
            tool: exposed,
            call_id: &late.call_id,
            started_by: from.logged_as.client_key.as_deref(),
            status: info.status.as_str(),
            status_message: info.status_message.as_deref(),
            poll_interval_ms: info.poll_interval_ms.and_then(|v| i64::try_from(v).ok()),
            ttl_ms: info.ttl_ms.and_then(|v| i64::try_from(v).ok()),
        };
        let removed = |row: &McpTaskRow| {
            super::removed_result(
                &row.task_id,
                &row.tool,
                &server.name,
                "while the call that started the job was answered",
            )
        };
        let inserted =
            match mcp_tasks::insert_reusing(&app.db, &new, reused_result(server), removed).await {
                Ok(done) => done,
                Err(e) => {
                    // Nothing may run unseen (K26): a task lmgw cannot record
                    // is cancelled.
                    if let Some(peer) = self.task_peer(server).await {
                        if wire::can_cancel(&peer) {
                            let bound = Duration::from_millis(server.timeout_ms);
                            let _ = wire::cancel(&peer, &info.task_id, bound).await;
                        }
                    }
                    return Err(CallError::Upstream {
                        server: server.name.clone(),
                        detail: format!(
                            "the server started job {}, but lmgw could not record it ({e}), so \
                             it was cancelled",
                            info.task_id
                        ),
                    });
                }
            };
        let mcp_tasks::Inserted {
            id,
            reused,
            server_gone,
        } = inserted;
        record_reused(&app, server, &reused).await;
        let text = started_text(&info.task_id, created.immediate.as_deref());
        let answer = || {
            Ok((
                rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                    text.clone(),
                )]),
                server.name.clone(),
            ))
        };
        if server_gone {
            // Removed while the call was in flight: the row ended abandoned
            // in its insert, and its result enters the thread as any ended
            // task's does (when the turn that made the call ends).
            tracing::info!(
                "mcp task {} on '{}': its server row was removed while the call was answered; \
                 the job ended abandoned",
                info.task_id,
                server.name
            );
            crate::web::chat_tasks::deliver::soon(&app, late.thread_id);
            return answer();
        }
        let inbox = self
            .tasks
            .claim((server.id, info.task_id.clone()), Some(id), true);
        let mut follow = Follow::new((server.id, &server.name), exposed, info);
        if !info.status.is_terminal() {
            follow.poll_later(&app);
        }
        tokio::spawn(follow_open(self.weak(), id, follow, inbox));
        answer()
    }

    pub(super) fn weak(&self) -> Weak<AppState> {
        self.state.get().cloned().unwrap_or_default()
    }

    /// Give every `open` and `cancel_owed` row its follower (§1.3: at start,
    /// and after a write outside the follower moved rows — a thread or a
    /// server gone); a row followed already reads itself again.
    pub async fn resume_tasks(&self) {
        let Some(app) = self.app() else {
            return;
        };
        let rows = match mcp_tasks::followed(&app.db).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!("mcp tasks: reading the open tasks failed: {e}");
                return;
            }
        };
        let known = self.tasks.rows();
        for tx in known.values() {
            let _ = tx.send(Nudge::Recheck);
        }
        for row in rows {
            if known.contains_key(&row.id) {
                continue;
            }
            let key = (row.server_id, row.task_id.clone());
            let open = row.state == mcp_tasks::OPEN;
            let Some(inbox) = self.tasks.register(key, Some(row.id), open) else {
                // Its key is taken: by this row's follower, started since
                // `known` was read, or by a newer task of the same id, whose
                // claim ended this row.
                continue;
            };
            if open {
                // Polled at once: whatever happened while lmgw was away.
                let follow = Follow::of_row(&row);
                tokio::spawn(follow_open(self.weak(), row.id, follow, inbox));
            } else {
                tokio::spawn(follow_owed(self.weak(), row, inbox));
            }
        }
    }

    /// A `notifications/tasks/status` from server `server_id`'s session
    /// (§1.3): to its task's follower, at once (the latest of a burst).
    pub(crate) fn on_task_status(
        &self,
        server_id: i64,
        params: rmcp::model::TaskStatusNotificationParam,
    ) {
        let info = wire::note(params);
        let key = (server_id, info.task_id.clone());
        self.tasks.status(&key, info);
    }

    /// Server `server_id` connected (a device linked again): its tasks are
    /// polled and its owed cancels sent at once.
    pub(crate) fn task_server_linked(&self, server_id: i64) {
        for tx in self.tasks.of_server(server_id) {
            let _ = tx.send(Nudge::Linked);
        }
    }

    /// Whether server `server_id` has an open task (T17).
    pub fn has_open_tasks(&self, server_id: i64) -> bool {
        self.tasks.has_open(server_id)
    }
}

/// Write how row `id` ended (§1.4) and its end's request row (T20): `false`
/// when the row was no longer open.
pub(super) async fn end_row(
    state: &SharedState,
    id: i64,
    follow: &Follow,
    ending: &Ending,
) -> bool {
    let blocks = ending_blocks(ending, &follow.task_id, &follow.tool);
    // The thread's readers get the result's `structuredContent` beside what
    // the model is given (`stored`).
    let structured = match ending {
        Ending::Result { result, .. } => result.structured_content.as_ref(),
        _ => None,
    };
    let result = super::stored::encode(&blocks, structured);
    let status = ending.status();
    let message = match ending {
        Ending::Error { status_message, .. } => status_message.as_deref(),
        _ => None,
    };
    let ended = mcp_tasks::Ended {
        status: status.as_str(),
        status_message: message,
        result: &result,
        ended_by: ending.ended_by(),
    };
    let owing = matches!(ending, Ending::Cancelled { told: false, .. });
    let moved = if owing {
        mcp_tasks::end_owing_cancel(&state.db, id, &ended).await
    } else {
        mcp_tasks::end(&state.db, id, &ended).await
    };
    match moved {
        Ok(true) => {}
        Ok(false) => return false,
        Err(e) => {
            tracing::error!("mcp task {}: storing its end failed: {e}", follow.task_id);
            return false;
        }
    }
    if let Ok(Some(row)) = mcp_tasks::get(&state.db, id).await {
        let logged = LoggedAs {
            client_key: row.started_by.clone(),
            proto: crate::telemetry::CHAT_TOOL_PROTO,
        };
        record_end(state, &logged, follow, ending, age_ms(&row)).await;
        // Into its thread now, if no turn runs there (MCP Tasks design
        // §3.1); a turn that runs takes it in when it ends.
        if let Some(thread_id) = row.thread_id {
            crate::web::chat_tasks::deliver::soon(state, thread_id);
        }
    }
    true
}

/// The end's request row (T20): under the starting principal, with the
/// task's status and how long it ran.
pub(super) async fn record_end(
    state: &SharedState,
    logged: &LoggedAs,
    follow: &Follow,
    ending: &Ending,
    total_ms: i64,
) {
    let server = state
        .snapshot()
        .mcp_servers
        .get(&follow.server_id)
        .map(|s| s.name.clone());
    let head = header(&follow.task_id, &follow.tool, ending.status());
    let ran = Duration::from_millis(u64::try_from(total_ms).unwrap_or(0));
    let started = Instant::now().checked_sub(ran).unwrap_or_else(Instant::now);
    let ctx = RequestCtx {
        client_key: logged.client_key.clone(),
        ..Default::default()
    };
    let tool = follow.tool.as_str();
    match ending.row_end(&head) {
        RowEnd::Done => {
            record_tool_call(state, &ctx, logged.proto, tool, server, started, None).await
        }
        RowEnd::Failed(why) => {
            record_tool_call(state, &ctx, logged.proto, tool, server, started, Some(why)).await
        }
        RowEnd::Canceled(note) => {
            record_tool_canceled(state, &ctx, logged.proto, (tool, server), started, &note).await
        }
    }
}

/// Follow open row `id` until it ends, or an owed cancel replaces it.
async fn follow_open(app: Weak<AppState>, id: i64, mut follow: Follow, mut inbox: Inbox) {
    loop {
        let event = follow.next(&app, &mut inbox).await;
        let Some(state) = app.upgrade() else {
            return;
        };
        let owes = match event {
            Event::Gone => break,
            // The id was reused: the claim of the key ended this row.
            Event::Nudge(Nudge::Reused) => break,
            Event::Moved => {
                let poll = follow.poll_ms.and_then(|v| i64::try_from(v).ok());
                let message = follow.status_message.as_deref();
                match mcp_tasks::set_status(&state.db, id, follow.status.as_str(), message, poll)
                    .await
                {
                    Ok(true) => None,
                    Ok(false) => match moved_away(&state, id).await {
                        Some(row) => Some(row),
                        None => break,
                    },
                    Err(e) => {
                        tracing::error!("mcp task {}: status write: {e}", follow.task_id);
                        None
                    }
                }
            }
            Event::Ended(ending) => {
                if end_row(&state, id, &follow, &ending).await {
                    break;
                }
                // Not open any more: its thread went meanwhile.
                match moved_away(&state, id).await {
                    Some(row) => Some(row),
                    None => break,
                }
            }
            Event::Nudge(Nudge::Recheck) => match moved_away(&state, id).await {
                Some(row) => Some(row),
                None if still_open(&state, id).await => None,
                None => break,
            },
            Event::Nudge(Nudge::Cancel { by, reply }) => {
                let outcome = cancel_open(&state, id, &mut follow, by).await;
                // Refused (the server cannot cancel, or said no) or only
                // requested: still followed.
                let done = matches!(
                    outcome,
                    Ok(CancelOutcome::Cancelled
                        | CancelOutcome::FinishedFirst
                        | CancelOutcome::Owed)
                );
                let owed = outcome == Ok(CancelOutcome::Owed);
                let _ = reply.send(outcome);
                let row = match owed {
                    true => mcp_tasks::get(&state.db, id).await.ok().flatten(),
                    false => None,
                };
                if row.is_none() && done {
                    break;
                }
                row
            }
            Event::Nudge(Nudge::Status(_) | Nudge::Linked) => None,
        };
        if let Some(row) = owes {
            state.mcp.tasks.owing(&inbox.ticket);
            drop(follow);
            return follow_owed(app, row, inbox).await;
        }
    }
    if let Some(state) = app.upgrade() {
        state.mcp.tasks.forget(&inbox.ticket);
    }
}

/// Row `id`, when it now owes a cancel (its thread went).
async fn moved_away(state: &AppState, id: i64) -> Option<McpTaskRow> {
    match mcp_tasks::get(&state.db, id).await {
        Ok(Some(row)) if row.state == mcp_tasks::CANCEL_OWED => Some(row),
        _ => None,
    }
}

async fn still_open(state: &AppState, id: i64) -> bool {
    matches!(mcp_tasks::get(&state.db, id).await, Ok(Some(row)) if row.state == mcp_tasks::OPEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn working(poll: Option<u64>) -> TaskInfo {
        TaskInfo {
            task_id: "t1".into(),
            status: TaskStatus::Working,
            status_message: None,
            poll_interval_ms: poll,
            ttl_ms: None,
        }
    }

    /// T17: the idle reaper skips a server with an open task, as it skips
    /// one with a call in flight.
    #[tokio::test]
    async fn a_server_with_an_open_task_is_never_reaped() {
        let m = McpManager::new();
        m.seed_ready_conn_for_tests(1, vec![("build".into(), serde_json::json!({}))])
            .await;
        if let Some(c) = m.conns.write().await.get_mut(&1) {
            c.last_used = Instant::now() - Duration::from_secs(60);
        }
        let mut snap = crate::config::Snapshot::default();
        let mut server = super::super::test_server();
        server.idle_seconds = 30;
        snap.mcp_servers.insert(1, server);
        let inbox = m.tasks.register((1, "t1".into()), Some(7), true).unwrap();
        assert!(!m.reap_idle(&snap).await, "an open task keeps it");
        m.tasks.forget(&inbox.ticket);
        assert!(m.reap_idle(&snap).await, "idle and nothing open: reaped");
    }

    /// A `pollInterval` of 0 is no interval: the setting's, never a poll
    /// due at once.
    #[tokio::test]
    async fn a_zero_poll_interval_reads_as_the_setting() {
        let state = AppState::init_for_tests().await.unwrap();
        let setting = poll_setting(&state);
        let interval = |poll| Follow::new((1, "x"), "x", &working(poll)).interval(&state);
        assert_eq!(interval(Some(0)), setting);
        assert_eq!(interval(None), setting);
        assert_eq!(interval(Some(250)), Duration::from_millis(250));
    }

    /// With no session to ask — a server that is not connected — the next
    /// look is the setting's interval away, however short the task's own;
    /// a server row gone from the store ends the task abandoned instead.
    #[tokio::test]
    async fn with_no_session_the_next_look_waits_the_setting() {
        let state = AppState::init_for_tests().await.unwrap();
        let setting = poll_setting(&state);
        // No server row, in the snapshot or the store: nothing will ever
        // answer, and the task ends abandoned.
        let mut f = Follow::new((999, "gone"), "x", &working(Some(1)));
        match f.look(&state, None).await {
            Seen::Ended(Ending::Abandoned(why)) => assert_eq!(
                why,
                "server 'gone' was removed (while lmgw followed the job): the job was \
                 abandoned; it may or may not have finished"
            ),
            _ => panic!("a server row gone from the store ends the task"),
        }

        // A disabled registered server: no session, none dialled.
        let row = crate::store::NewMcpServer {
            name: "off".into(),
            enabled: false,
            transport: crate::config::McpTransport::Http,
            command: None,
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            container_image: None,
            extra_run_args: Vec::new(),
            url: Some("http://127.0.0.1:1/mcp".into()),
            headers: Vec::new(),
            tool_prefix: "off".into(),
            timeout_ms: 1_000,
            autostart: false,
            idle_seconds: 0,
            allow_sampling: false,
            sampling_alias: None,
            agent_id: None,
        };
        let id = crate::store::insert_mcp_server(&state.db, &row)
            .await
            .unwrap();
        state.reload_snapshot().await.unwrap();
        let mut f = Follow::new((id, "off"), "x", &working(Some(1)));
        let before = tokio::time::Instant::now();
        assert!(matches!(f.look(&state, None).await, Seen::Same));
        assert!(f.due >= before + setting, "waits the setting, not 1 ms");
    }

    /// A follower (or a bridged wait) that goes stops its held
    /// `tasks/result`.
    #[tokio::test]
    async fn a_dropped_follow_stops_its_held_request() {
        struct Flag(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for Flag {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut f = Follow::new((1, "x"), "x", &working(None));
        f.held = Some(tokio::spawn(async move {
            let _flag = Flag(Some(tx));
            std::future::pending::<Result<Payload, WireError>>().await
        }));
        tokio::task::yield_now().await;
        drop(f);
        let stopped = tokio::time::timeout(Duration::from_secs(5), rx).await;
        assert!(stopped.is_ok(), "the held request was stopped");
    }
}
