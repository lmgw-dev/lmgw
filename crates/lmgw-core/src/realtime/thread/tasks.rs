//! Late MCP task results in a session bound to their thread (MCP Tasks
//! design §3.4, §4.2).
//!
//! - **`lmgw.task.done`**: each result that entered the thread while the
//!   session is bound is said once, with the feed's `task.done` facts. A
//!   result enters only while no turn of the thread runs
//!   (`web::chat_tasks::deliver`), so one that ended during a response is
//!   said after that response's turn ended — or, entering with the
//!   session's next user message (before it) or at its next turn's start,
//!   while that turn answers it.
//! - **The owed set** ([`Owed`]): the result rows the thread holds with no
//!   reply stored after them — the ones a continuation answers. Read from
//!   the thread at the bind (`bind`), so a result that entered before the
//!   session is owed without being said again (the feed said it); read
//!   again at each wake (below); and cut by a response whose turn saved a
//!   reply ([`Owed::answered_through`]: a result stored before that reply was
//!   in its request) — unless a read since covered that reply, which then
//!   stands (it saw the reply deleted, say).
//! - **The watermark**: which results were said is the largest message id
//!   handed out at the last read (ids only grow, `AUTOINCREMENT`). Until a
//!   read succeeded it is unknown, and the first read that does sets it
//!   without saying anything: a bind whose read failed does not announce
//!   the thread's whole history at its first wake.
//! - **The wakes** (`web::chat_live`'s result wake): a result entered, or a
//!   turn of the thread let it go — a dashboard send that answered the
//!   results, a refused continuation. A history write (the approval wake's:
//!   an edit, a deleted reply, a reply nobody heard deleted by the journal)
//!   reads it again too, a result whose reply went being owed again — only
//!   while the last read found a result row in the thread: none can be owed
//!   again otherwise, and only a delivery writes one, which wakes the result
//!   wake. The session reads the thread itself, so a wake missed in a lag
//!   loses nothing.
//! - **The read** (`store::mcp_tasks::results_read`) is narrow — the newest
//!   reply's id and the result rows past it or the watermark, ranges of an
//!   index — and runs off the session's loop, which takes what it found as
//!   one more event: input audio never waits for it. One read runs at a
//!   time; wakes during it read once more after it.
//!
//! The continuation itself is `lifecycle::bound`'s: the client's own
//! `response.create` with no new words, and no commit of its own since the
//! last response, runs the turn `POST …/answer` runs while a result is
//! owed. A temporary thread has no tasks (they are bridged), and nothing
//! here reads it.

use std::collections::BTreeSet;

use lmgw_api_types::chat_feed::TaskDone;
use tokio::sync::mpsc;

use super::super::protocol::ServerEvent;
use super::super::session::Core;
use crate::ir::{ContentPart, SYNTHETIC_CALL_ID_PREFIX};
use crate::state::SharedState;
use crate::store::mcp_tasks::{results_read, ResultsRead};
use crate::store::ChatMessageRow;
use crate::web::chat_tasks::render;

/// A read of the thread's results, as it reports to the session's loop:
/// what it found, or why the store did not say.
pub(crate) type Read = Result<ResultsRead, String>;

/// Where the reads report: the session's loop (module doc).
pub(crate) type ReadTx = mpsc::UnboundedSender<Read>;

/// A bound session's view of its thread's results (module doc).
#[derive(Debug, Default)]
pub(crate) struct Owed {
    /// The result rows (their message ids) no reply after them answered.
    results: BTreeSet<i64>,
    /// The largest message id handed out when the session last read the
    /// thread (`ResultsRead::watermark`): a result row past it entered
    /// since, and is said. `None` until a read succeeded: the first one
    /// that does sets it and says nothing (the bind's, or the first wake's
    /// after a bind whose read failed).
    seen_through: Option<i64>,
    /// The newest reply a `done` frame said was saved: a read whose
    /// watermark is below it was taken before that reply, and the results
    /// stored before it are answered all the same.
    answered: i64,
    /// The last read found a result row in the thread, answered or not.
    any: bool,
    /// A read runs: a wake meanwhile reads once more after it ([`again`]).
    ///
    /// [`again`]: Self::again
    reading: bool,
    /// A wake came while a read ran.
    again: bool,
    /// The session loop's end of the reads (module doc); `None` before the
    /// loop listens.
    tx: Option<ReadTx>,
}

impl Owed {
    /// The thread's results as the bind finds them (module doc). A read
    /// that fails owes nothing and leaves the watermark unknown, logged:
    /// the next wake reads again, and says nothing of what it finds.
    pub(crate) async fn at_bind(state: &SharedState, thread_id: i64) -> Self {
        let mut owed = Self::default();
        if thread_id <= 0 {
            return owed;
        }
        match results_read(&state.db, thread_id, None).await {
            Ok(read) => {
                owed.take(&read);
            }
            Err(e) => tracing::warn!(
                thread = thread_id,
                "realtime: the thread's job results could not be read at the bind ({e}); \
                 none is owed until the next wake reads them"
            ),
        }
        owed
    }

    /// An owed set of result rows `ids`, for a test of what owes.
    #[cfg(test)]
    pub(crate) fn owing_for_tests(ids: &[i64]) -> Self {
        Self {
            results: ids.iter().copied().collect(),
            seen_through: ids.iter().copied().max(),
            any: !ids.is_empty(),
            ..Default::default()
        }
    }

    /// Whether no result waits for an answer.
    pub(crate) fn is_empty(&self) -> bool {
        self.results.is_empty()
    }

    /// A turn of the session saved reply `message_id`: every result stored
    /// before it was in its request, and is answered — unless a read since
    /// covered that reply (its id within the read's watermark): the read
    /// saw what became of it, a delete included, and stands.
    pub(crate) fn answered_through(&mut self, message_id: i64) {
        if self.seen_through.is_some_and(|seen| message_id <= seen) {
            return;
        }
        self.answered = self.answered.max(message_id);
        self.results.retain(|id| *id > message_id);
    }

    /// Whether a history write (the approval wake) can leave the owed set
    /// as it is: the last read found no result row in the thread, and only
    /// a delivery writes one — which wakes the result wake itself.
    fn unmoved_by_history(&self) -> bool {
        self.seen_through.is_some() && !self.any
    }

    /// Take `read` (the thread as it stands): the result rows that entered
    /// since the last read, in their order — none when this is the first
    /// read that succeeded; the owed set becomes the results no reply
    /// follows, less those a reply saved after the read answered.
    fn take<'a>(&mut self, read: &'a ResultsRead) -> Vec<&'a ChatMessageRow> {
        let new = match self.seen_through {
            Some(seen) => read.rows.iter().filter(|m| m.id > seen).collect(),
            None => Vec::new(),
        };
        // Ids only grow (`AUTOINCREMENT`): nothing at or below the read's
        // watermark can enter after it.
        self.seen_through = Some(
            self.seen_through
                .map_or(read.watermark, |s| s.max(read.watermark)),
        );
        self.any = read.any;
        self.results = read
            .rows
            .iter()
            .map(|m| m.id)
            .filter(|id| *id > read.last_reply)
            .collect();
        if self.answered > read.watermark {
            let answered = self.answered;
            self.results.retain(|id| *id > answered);
        }
        new
    }
}

/// The `lmgw.task.done` facts of result row `m` in thread `thread_id`;
/// `None` for a row whose task facts or synthetic call do not read (only a
/// hand-edited row: it is still owed, and not said).
fn done_of(thread_id: i64, m: &ChatMessageRow) -> Option<TaskDone> {
    let task = m.task.as_ref()?;
    let id = render::pair_of(m)?
        .iter()
        .flat_map(|msg| msg.content.iter())
        .find_map(|p| match p {
            ContentPart::ToolUse { id, .. } => id.strip_prefix(SYNTHETIC_CALL_ID_PREFIX),
            _ => None,
        })?
        .parse()
        .ok()?;
    Some(TaskDone {
        thread_id,
        message_id: m.id,
        id,
        task_id: task.task_id.clone(),
        server_label: task.server_label.clone(),
        tool: task.tool.clone(),
        status: task.status.clone(),
        by: task.ended_by.clone(),
    })
}

impl Core {
    /// The session's loop takes the reads of its thread's results at `tx`
    /// (module doc).
    pub(in crate::realtime) fn results_listen(&mut self, tx: ReadTx) {
        if let Some(b) = self.bound.as_mut() {
            b.tasks.tx = Some(tx);
        }
    }

    /// The thread's results may have moved (module doc): read them again,
    /// off the session's loop — once more after a read that runs, however
    /// many wakes came meanwhile. `history`: the wake is a history write's
    /// (the approval wake), which moves nothing while the thread holds no
    /// result row.
    pub(in crate::realtime) fn results_woken(&mut self, history: bool) {
        let Some(b) = self.bound.as_mut() else {
            return;
        };
        if b.thread_id <= 0 || (history && b.tasks.unmoved_by_history()) {
            return;
        }
        if b.tasks.reading {
            b.tasks.again = true;
            return;
        }
        let Some(tx) = b.tasks.tx.clone() else {
            return;
        };
        b.tasks.reading = true;
        let (state, thread_id, since) = (self.state.clone(), b.thread_id, b.tasks.seen_through);
        tokio::spawn(async move {
            let read = results_read(&state.db, thread_id, since)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(read);
        });
    }

    /// A read of the thread's results is in (module doc): each result that
    /// entered since the last read is said, and the owed set taken from it.
    /// A read that failed changes nothing, logged.
    pub(in crate::realtime) fn results_read(&mut self, read: Read) {
        let id = self.id().to_string();
        let Some(b) = self.bound.as_mut() else {
            return;
        };
        let thread_id = b.thread_id;
        b.tasks.reading = false;
        let said: Vec<ServerEvent> = match &read {
            Ok(read) => b
                .tasks
                .take(read)
                .into_iter()
                .filter_map(|m| done_of(thread_id, m))
                .map(|done| ServerEvent::LmgwTaskDone { done })
                .collect(),
            Err(e) => {
                tracing::warn!(
                    "realtime {id}: chat thread {thread_id}'s job results could not be read \
                     ({e}); the next wake reads them"
                );
                Vec::new()
            }
        };
        let again = std::mem::take(&mut b.tasks.again);
        for ev in said {
            self.ob.send(ev);
        }
        if again {
            self.results_woken(false);
        }
    }
}

#[cfg(test)]
mod tests;
