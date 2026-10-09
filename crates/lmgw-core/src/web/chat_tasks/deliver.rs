//! An ended task's result entering its thread (MCP Tasks design §3.1).
//!
//! **Only while no turn of the thread runs**, under the thread's history
//! lock (`LiveTurns`): a write while a turn runs would sit in front of a
//! reply that never saw it. Four moments deliver:
//! 1. **the task ends with the thread idle** ([`soon`], from the follower);
//! 2. **a turn ends** ([`when_idle`], once its reply is saved or dropped and
//!    it let the thread go);
//! 3. **a user message is written for a turn** (a send, a bound session's
//!    spoken turn): in the message's own transaction, under the history
//!    write it takes, after what it declines and **before the message**
//!    (`store::send_user_message_by`, from `ChatRepo::append_user_message`).
//!    A result that ended before the user wrote is stored before the
//!    message, so the model answers the message with the result as context
//!    (chronological order, design T11) rather than answering the result.
//!    From before that write until the message's turn has begun, the
//!    thread counts as running ([`Sent`]): a result that ends meanwhile
//!    waits for the turn's end, as one that ends while it runs;
//! 4. **any other fresh turn starts** ([`at_start`]: an answer, an edit, a
//!    regenerate — under its own save lock, before it reads the history),
//!    so a result that ended a moment before is in its history. An edit or
//!    a regenerate answers a message already stored, whose place is fixed:
//!    what enters then follows it. So does a bound session's transcript
//!    retry (a heard response whose audio attempt began and was refused,
//!    `realtime/thread/turn/audio.rs`): the user's row is stored by then,
//!    the send's hold went to the first attempt, and the retry starts as
//!    `Fresh {user_message_id: None}` with `sent: None`, so its start
//!    delivers after that row (design §3.1, amended).
//!
//! A continue and a resumed turn deliver nothing at their start: each
//! extends the thread's last reply, which must stay the last message; what
//! waits enters when they end. **A reply its approvals hold open** holds
//! results off too (design decision of WP2): calls that wait for a
//! decision, or decided calls whose record is still open because the turn
//! that runs them has not begun or not saved yet — the condition a new
//! message declines them on (`store::decline_waiting`). The decision
//! resumes that reply only while it is the thread's last message, so a
//! result written after it, before or after the decision's commit, would
//! close its calls as not run, and its open call could join the result's
//! call into one message with a call left unanswered. They enter once the
//! resumed turn ended, its calls were closed as not run
//! (`chat_approvals::unrun`, which delivers then), or a new message
//! declined them.
//!
//! **A turn that took the thread and ended before its worker** (refused
//! after `begin_as`) delivers too: the guard [`after_turn`] makes is taken
//! before the turn's ticket and dropped after it, whichever way the turn
//! ends.
//!
//! Delivery writes nothing that moves the thread's generation: no turn
//! runs to be cancelled, and a bound session's journal still finds its
//! spoken reply where it left it. After it, the thread's bound session is
//! woken (`LiveTurns::results_moved`) and says each result that entered as
//! `lmgw.task.done` (`realtime::thread::tasks`).

use crate::state::{AppState, SharedState};
use crate::store::{self, mcp_tasks};

use super::super::chat_live::Ticket;
use super::super::chat_repo::ChatRepo;

/// Deliver what waits for thread `thread_id` on a task of its own (the
/// follower's end, moment 1): the follower never waits for a thread's lock.
/// A turn that runs now delivers when it ends, so none is spawned for it.
pub(crate) fn soon(state: &SharedState, thread_id: i64) {
    if state.chat_live.running(thread_id) {
        return;
    }
    let s = state.clone();
    tokio::spawn(async move {
        when_idle(&s, thread_id).await;
    });
}

/// Deliver what waits for thread `thread_id` if no turn of it runs (moments
/// 1 and 2): how many results entered.
pub(crate) async fn when_idle(state: &AppState, thread_id: i64) -> usize {
    if !waits(state, thread_id).await {
        return 0;
    }
    let _held = state.chat_live.hold(thread_id).await;
    // Under the lock a turn's start takes too: none starts before this
    // write is done, and one that runs now delivers when it ends.
    if state.chat_live.running(thread_id) {
        return 0;
    }
    write(state, thread_id).await
}

/// A fresh turn of thread `thread_id` started (moment 4): what waits enters
/// before the turn reads the history, under the lock its reply is saved
/// under — unless a newer turn or a rewrite moved the thread on already.
pub(crate) async fn at_start(state: &AppState, ticket: &Ticket, thread_id: i64) {
    if !waits(state, thread_id).await {
        return;
    }
    let Some(_proof) = ticket.save_lock().await else {
        return;
    };
    write(state, thread_id).await;
}

/// Every thread with a waiting result, each if no turn of it runs: at the
/// gateway's start (no turn survives a restart), and after server rows went
/// (`super::servers_gone`).
pub(crate) async fn all(state: &AppState) {
    let threads = match mcp_tasks::waiting_threads(&state.db).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("chat: reading the MCP task results that wait failed: {e}");
            return;
        }
    };
    for thread_id in threads {
        when_idle(state, thread_id).await;
    }
}

/// What a waiting result of thread `thread_id` waits for, in the words of
/// the thread's task list; `None` when nothing holds it (it enters at
/// once).
pub(crate) async fn held_by(state: &AppState, thread_id: i64) -> Option<String> {
    if state.chat_live.running(thread_id) {
        return Some("the turn of the thread that is running".into());
    }
    if gated(state, thread_id).await {
        return Some(
            "the calls the thread's last reply waits on: their decision, and the turn that \
             runs them"
                .into(),
        );
    }
    None
}

/// Whether thread `thread_id` (stored) has a result waiting, read without
/// its lock: most turns have none, and take no lock for it.
async fn waits(state: &AppState, thread_id: i64) -> bool {
    if thread_id <= 0 {
        return false;
    }
    match mcp_tasks::waiting(&state.db, thread_id).await {
        Ok(rows) => !rows.is_empty(),
        Err(e) => {
            tracing::warn!("chat: thread {thread_id}'s waiting MCP task results: {e}");
            false
        }
    }
}

/// Whether the thread's last message is a reply its approvals hold open
/// (module doc): calls waiting for a decision, or decided ones whose record
/// still ends in their calls.
async fn gated(state: &AppState, thread_id: i64) -> bool {
    match store::last_chat_message(&state.db, thread_id).await {
        Ok(Some(m)) => {
            m.role == "assistant"
                && m.pending_approvals
                    .as_ref()
                    .is_some_and(|p| p.is_open() || store::record_open(m.ir_messages.as_deref()))
        }
        _ => false,
    }
}

/// Write every waiting result of thread `thread_id` into it, under its lock
/// with no other turn running: how many entered. Each is one transaction
/// with its `task.done`; the feed's readers are woken once after them.
async fn write(state: &AppState, thread_id: i64) -> usize {
    if gated(state, thread_id).await {
        return 0;
    }
    let rows = match mcp_tasks::waiting(&state.db, thread_id).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("chat: thread {thread_id}'s waiting MCP task results: {e}");
            return 0;
        }
    };
    let mut delivered = Vec::new();
    for row in rows {
        match ChatRepo::Db.deliver_task(state, thread_id, row.id).await {
            Ok(Some(d)) => delivered.push(d),
            Ok(None) => {}
            Err(e) => tracing::warn!(
                thread = thread_id,
                "mcp task {}: writing its result into the thread failed: {e}",
                row.task_id
            ),
        }
    }
    entered(state, thread_id, &delivered);
    delivered.len()
}

/// Results entered thread `thread_id` (committed): each logged, the feed's
/// readers woken once, and the thread's bound session told to say them
/// (`lmgw.task.done`).
pub(crate) fn entered(state: &AppState, thread_id: i64, delivered: &[mcp_tasks::Delivered]) {
    for d in delivered {
        tracing::info!(
            thread = thread_id,
            "mcp task {} ({}) {}: its result entered the thread as message {}",
            d.row.task_id,
            d.row.tool,
            d.row.status,
            d.message_id
        );
    }
    if !delivered.is_empty() {
        state.chat_feed.wake();
        state.chat_live.results_moved(thread_id);
    }
}

/// A user message written for a turn that has not begun yet (moment 3):
/// taken before the message's write and held until its turn has begun
/// (`TurnOpts::sent`) or was refused. While it is held the thread counts
/// as running (`LiveTurns::sending`), so a result that ends after the
/// message waits for the turn's end; when it drops with no turn live — the
/// write failed, the turn was refused before it began — what waits enters
/// as on an idle thread.
pub(crate) struct Sent {
    mark: Option<crate::web::chat_live::Sending>,
    state: SharedState,
    thread_id: i64,
}

/// The [`Sent`] of a user message about to be written into stored thread
/// `thread_id`; `None` for a temporary thread, which has no tasks.
pub(crate) fn sent(state: &SharedState, thread_id: i64, stored: bool) -> Option<Sent> {
    stored.then(|| Sent {
        mark: Some(state.chat_live.sending(thread_id)),
        state: state.clone(),
        thread_id,
    })
}

impl Drop for Sent {
    fn drop(&mut self) {
        drop(self.mark.take());
        if tokio::runtime::Handle::try_current().is_ok() {
            soon(&self.state, self.thread_id);
        }
    }
}

/// What a turn holds for its length (moment 2): taken before the turn's
/// ticket and dropped after it — at the worker's end, or with a refusal
/// after the ticket was taken (the start's reach re-check, a resumed reply
/// or a continued one gone, an `answer` with nothing left to answer) — it
/// delivers what waits on a task of its own, and wakes the thread's bound
/// session to read which results still wait for an answer. Nothing for a
/// temporary thread.
pub(crate) struct AfterTurn(Option<(SharedState, i64)>);

/// The [`AfterTurn`] of a turn of thread `thread_id`; `stored`: the thread
/// is not a temporary one.
pub(crate) fn after_turn(state: &SharedState, thread_id: i64, stored: bool) -> AfterTurn {
    AfterTurn(stored.then(|| (state.clone(), thread_id)))
}

impl Drop for AfterTurn {
    fn drop(&mut self) {
        let Some((state, thread_id)) = self.0.take() else {
            return;
        };
        // The turn's reply may have answered the thread's results, or it
        // was refused before it answered any: the bound session reads which
        // still wait (MCP Tasks design §3.4).
        state.chat_live.results_moved(thread_id);
        if tokio::runtime::Handle::try_current().is_ok() {
            soon(&state, thread_id);
        }
    }
}

#[cfg(test)]
mod tests;
