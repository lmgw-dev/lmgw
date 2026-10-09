//! The followers by `(server id, task id)` (MCP Tasks design §1.3): the
//! receivers make task ids unique among their own tasks only.
//!
//! - **One registration per key.** [`Tasks::register`] (the resume) takes a
//!   free key only; [`Tasks::claim`] (a task the server just created) takes
//!   it over: a server that answers a new task with the id of one lmgw still
//!   follows reused it, and the older follower is told so
//!   ([`Nudge::Reused`]) and stops.
//! - **A [`Ticket`] per registration**, so a follower that stops forgets its
//!   own registration and never a newer one for the same key.
//! - **Status notifications coalesce** per registration: the follower reads
//!   the latest one ([`Inbox::recv`]), however many arrived meanwhile.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use tokio::sync::{mpsc, oneshot, watch};

use super::cancel::{CancelOutcome, CancelRefusal};
use super::TaskInfo;

/// `(server id, task id)`.
pub(super) type Key = (i64, String);

/// A word for a follower.
pub(super) enum Nudge {
    /// The latest `notifications/tasks/status` for its task.
    Status(TaskInfo),
    /// Its server connected: poll now, send an owed cancel now.
    Linked,
    /// Its row may have moved outside the follower (a thread or server
    /// gone): read it again.
    Recheck,
    /// The server answered a new task with this task's id: an open row was
    /// ended already by the one who claimed the key, an owed cancel dropped
    /// (it would cancel the new task); a bridged wait ends abandoned.
    Reused,
    /// Cancel the task for `by` (§1.5).
    Cancel {
        by: String,
        reply: oneshot::Sender<Result<CancelOutcome, CancelRefusal>>,
    },
}

struct Entry {
    /// The row followed; `None` for a bridged task.
    row: Option<i64>,
    /// An `open` row's follower (not an owed cancel, not the bridge).
    open: bool,
    gen: u64,
    tx: mpsc::UnboundedSender<Nudge>,
    /// The latest status notification, latest wins.
    status: watch::Sender<Option<TaskInfo>>,
}

/// One registration of a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Ticket {
    pub(super) key: Key,
    gen: u64,
}

/// What a follower hears: its nudges and its task's latest status.
pub(super) struct Inbox {
    pub(super) ticket: Ticket,
    rx: mpsc::UnboundedReceiver<Nudge>,
    status: watch::Receiver<Option<TaskInfo>>,
}

impl Inbox {
    /// The next word, a status notification as the latest one; `None` once
    /// the registration is gone. Cancel-safe.
    pub(super) async fn recv(&mut self) -> Option<Nudge> {
        loop {
            tokio::select! {
                biased;
                n = self.rx.recv() => return n,
                changed = self.status.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                    if let Some(info) = self.status.borrow_and_update().clone() {
                        return Some(Nudge::Status(info));
                    }
                }
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct Tasks {
    followers: Mutex<HashMap<Key, Entry>>,
    gens: AtomicU64,
}

impl Tasks {
    fn entry(&self, key: Key, row: Option<i64>, open: bool) -> (Entry, Inbox) {
        let gen = self.gens.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        let (status, status_rx) = watch::channel(None);
        let entry = Entry {
            row,
            open,
            gen,
            tx,
            status,
        };
        let inbox = Inbox {
            ticket: Ticket { key, gen },
            rx,
            status: status_rx,
        };
        (entry, inbox)
    }

    /// Register a follower for `key`: its inbox, or `None` when one is
    /// registered already.
    pub(super) fn register(&self, key: Key, row: Option<i64>, open: bool) -> Option<Inbox> {
        let mut map = self.followers.lock().unwrap();
        if map.contains_key(&key) {
            return None;
        }
        let (entry, inbox) = self.entry(key.clone(), row, open);
        map.insert(key, entry);
        Some(inbox)
    }

    /// Register the follower of a task the server just created, taking
    /// `key` over from any older follower, which is told
    /// [`Nudge::Reused`].
    pub(super) fn claim(&self, key: Key, row: Option<i64>, open: bool) -> Inbox {
        let (entry, inbox) = self.entry(key.clone(), row, open);
        let old = self.followers.lock().unwrap().insert(key, entry);
        if let Some(old) = old {
            let _ = old.tx.send(Nudge::Reused);
        }
        inbox
    }

    /// The follower of `ticket` is done (a newer registration of its key
    /// stays).
    pub(super) fn forget(&self, ticket: &Ticket) {
        let mut map = self.followers.lock().unwrap();
        if map.get(&ticket.key).is_some_and(|e| e.gen == ticket.gen) {
            map.remove(&ticket.key);
        }
    }

    /// The follower of `ticket` follows an owed cancel now, not an open row.
    pub(super) fn owing(&self, ticket: &Ticket) {
        if let Some(e) = self.followers.lock().unwrap().get_mut(&ticket.key) {
            if e.gen == ticket.gen {
                e.open = false;
            }
        }
    }

    pub(super) fn nudge(&self, key: &Key, n: Nudge) -> Result<(), Nudge> {
        let map = self.followers.lock().unwrap();
        match map.get(key) {
            Some(e) => e.tx.send(n).map_err(|e| e.0),
            None => Err(n),
        }
    }

    /// A status notification for `key`'s task: the latest wins.
    pub(super) fn status(&self, key: &Key, info: TaskInfo) {
        if let Some(e) = self.followers.lock().unwrap().get(key) {
            e.status.send_replace(Some(info));
        }
    }

    /// Whether server `server_id` has an open task: never reaped then (T17).
    pub(crate) fn has_open(&self, server_id: i64) -> bool {
        self.open_count(server_id) > 0
    }

    /// Server `server_id`'s open tasks.
    pub(crate) fn open_count(&self, server_id: i64) -> usize {
        let map = self.followers.lock().unwrap();
        map.iter()
            .filter(|((s, _), e)| *s == server_id && e.open)
            .count()
    }

    pub(super) fn of_server(&self, server_id: i64) -> Vec<mpsc::UnboundedSender<Nudge>> {
        let map = self.followers.lock().unwrap();
        map.iter()
            .filter(|((s, _), _)| *s == server_id)
            .map(|(_, e)| e.tx.clone())
            .collect()
    }

    pub(super) fn rows(&self) -> HashMap<i64, mpsc::UnboundedSender<Nudge>> {
        let map = self.followers.lock().unwrap();
        map.values()
            .filter_map(|e| e.row.map(|r| (r, e.tx.clone())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::TaskStatus;
    use super::*;

    fn info(status: TaskStatus, message: &str) -> TaskInfo {
        TaskInfo {
            task_id: "a".into(),
            status,
            status_message: Some(message.into()),
            poll_interval_ms: None,
            ttl_ms: None,
        }
    }

    #[test]
    fn the_registry_counts_open_rows_only() {
        let t = Tasks::default();
        let a = t.register((1, "a".into()), Some(1), true).unwrap();
        let b = t.register((1, "b".into()), Some(2), false).unwrap();
        let _c = t.register((2, "c".into()), None, false).unwrap();
        assert!(t.register((1, "a".into()), Some(1), true).is_none());
        assert_eq!(t.open_count(1), 1);
        assert!(!t.has_open(2));
        t.owing(&a.ticket);
        assert!(!t.has_open(1));
        t.forget(&b.ticket);
        assert_eq!(t.rows().len(), 1);
    }

    /// A reused id: the newer task takes the key, the older follower is
    /// told, and its forget leaves the newer registration standing.
    #[tokio::test]
    async fn a_claim_takes_the_key_over() {
        let t = Tasks::default();
        let mut old = t.register((1, "a".into()), Some(1), true).unwrap();
        let new = t.claim((1, "a".into()), Some(2), true);
        assert!(matches!(old.recv().await, Some(Nudge::Reused)));
        assert!(old.recv().await.is_none(), "the old registration is gone");
        t.forget(&old.ticket);
        t.owing(&old.ticket);
        assert_eq!(t.rows().keys().copied().collect::<Vec<_>>(), vec![2]);
        assert!(t.has_open(1));
        t.forget(&new.ticket);
        assert!(t.rows().is_empty());
    }

    /// Status notifications that arrive faster than the follower reads them
    /// queue nothing: it reads the latest.
    #[tokio::test]
    async fn status_notifications_coalesce() {
        let t = Tasks::default();
        let key = (1, "a".to_string());
        let mut inbox = t.register(key.clone(), Some(1), true).unwrap();
        for n in 0..1000 {
            t.status(&key, info(TaskStatus::Working, &format!("step {n}")));
        }
        t.status(&key, info(TaskStatus::InputRequired, "which branch?"));
        let Some(Nudge::Status(got)) = inbox.recv().await else {
            panic!("a status")
        };
        assert_eq!(got, info(TaskStatus::InputRequired, "which branch?"));
        let again = tokio::time::timeout(std::time::Duration::from_millis(50), inbox.recv()).await;
        assert!(again.is_err(), "one status read, nothing queued");
    }
}
