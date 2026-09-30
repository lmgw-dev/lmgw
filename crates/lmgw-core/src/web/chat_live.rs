//! One live turn per Chat thread, and the generation a turn's reply is saved
//! against (review R1 finding 1).
//!
//! A turn's worker is spawned and outlives its request: it runs until its
//! stream ends, then saves the reply. Nothing used to tie that save to the
//! thread as it then stood — a worker stopped before its first token (still
//! waiting on the GPU, a cold start, a long prefill) only noticed at its first
//! delta, and wrote a one-token reply to the *old* history after whatever the
//! owner had done meanwhile: an edited, resent message and its new answer
//! became `[user', stale reply, new reply]`, which a template with strict role
//! alternation refuses on every later turn. A continue outliving an edit of
//! its row overwrote the edit.
//!
//! Two things close that, for stored and temporary threads alike (both are
//! in-process, and so is every worker):
//!
//! - **A generation per thread.** Every write that rewrites a thread's
//!   history — a new user message, an edit, a delete, a truncation, the
//!   thread going away — moves it ([`LiveTurns::write`]), and so does starting
//!   a turn ([`LiveTurns::begin`]). A turn captures it when it starts; its
//!   reply is saved only while it has not moved ([`Ticket::save_lock`]),
//!   under the same per-thread lock the rewrites take, so the check and the
//!   write cannot be split by one. A reply that lost is dropped and logged.
//! - **At most one live turn.** Starting a turn cancels the thread's previous
//!   one ([`Ticket::superseded`]): its worker drops what it is waiting on —
//!   the retrieval, the GPU admission, the upstream request — which frees the
//!   GPU rather than letting a turn nobody will see run to its end.
//!
//! Generations come from one counter for the whole gateway, so a thread's
//! slot can be dropped once nothing refers to it and a new one never repeats
//! an old value.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{watch, OwnedMutexGuard};

/// Every thread's live turn and generation. `state.chat_live`.
#[derive(Default)]
pub struct LiveTurns {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The last generation or ticket id handed out; one counter for both.
    counter: AtomicU64,
    threads: Mutex<HashMap<i64, Arc<Slot>>>,
}

struct Slot {
    /// Serialises the thread's history rewrites with a reply's save.
    write: Arc<tokio::sync::Mutex<()>>,
    state: Mutex<SlotState>,
}

struct SlotState {
    generation: u64,
    live: Option<Live>,
}

/// The turn currently answering: its ticket id, and the switch that cancels
/// it.
struct Live {
    id: u64,
    cancel: watch::Sender<bool>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing here panics while holding a lock; a poisoned one still holds a
    // consistent map, so carry on rather than take the Chat down.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Inner {
    fn next(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn slot(&self, thread_id: i64) -> Arc<Slot> {
        let mut map = lock(&self.threads);
        map.entry(thread_id)
            .or_insert_with(|| {
                Arc::new(Slot {
                    write: Arc::new(tokio::sync::Mutex::new(())),
                    state: Mutex::new(SlotState {
                        generation: self.next(),
                        live: None,
                    }),
                })
            })
            .clone()
    }

    /// Drop the thread's slot once nothing refers to it but the map and no
    /// turn is live — called after a reference to it went.
    fn release(&self, thread_id: i64) {
        let mut map = lock(&self.threads);
        let idle = map
            .get(&thread_id)
            .is_some_and(|s| Arc::strong_count(s) == 1 && lock(&s.state).live.is_none());
        if idle {
            map.remove(&thread_id);
        }
    }
}

impl LiveTurns {
    /// A write that rewrites `thread_id`'s history: waits for the thread's
    /// lock (a reply being saved finishes first), moves its generation, so
    /// no turn that started before it saves its reply, and cancels the live
    /// turn at once — a turn still in its retrieval or waiting on the GPU
    /// must not go on for a history that is gone, and must not write onto the
    /// rewritten rows. Hold the guard for the length of the write.
    pub(crate) async fn write(&self, thread_id: i64) -> HistoryWrite {
        let slot = self.inner.slot(thread_id);
        let guard = slot.write.clone().lock_owned().await;
        {
            let mut st = lock(&slot.state);
            st.generation = self.inner.next();
            if let Some(live) = st.live.take() {
                let _ = live.cancel.send(true);
            }
        }
        HistoryWrite {
            inner: self.inner.clone(),
            thread_id,
            slot: Some(slot),
            _guard: Some(guard),
        }
    }

    /// The thread is going away (deleted, discarded or kept): its history
    /// moves on, and its live turn is cancelled. Hold the guard for the length
    /// of the write.
    pub(crate) async fn discard(&self, thread_id: i64) -> HistoryWrite {
        self.write(thread_id).await
    }

    /// Start a turn of `thread_id`: its generation moves (a turn still
    /// running can no longer save), the previous turn is cancelled, and this
    /// one becomes the live one. The ticket is the turn's for its lifetime;
    /// dropping it ends the turn.
    pub(crate) async fn begin(&self, thread_id: i64) -> Ticket {
        let slot = self.inner.slot(thread_id);
        let (cancel, cancelled) = watch::channel(false);
        let id = self.inner.next();
        let generation = {
            // The lock only orders this against a write or a save in flight.
            let _guard = slot.write.clone().lock_owned().await;
            let mut st = lock(&slot.state);
            st.generation = self.inner.next();
            if let Some(previous) = st.live.replace(Live { id, cancel }) {
                let _ = previous.cancel.send(true);
            }
            st.generation
        };
        Ticket {
            inner: self.inner.clone(),
            thread_id,
            id,
            generation,
            slot: Some(slot),
            cancelled,
        }
    }

    /// How many threads have a slot right now (tests: slots do not pile up).
    #[cfg(test)]
    fn slots(&self) -> usize {
        lock(&self.inner.threads).len()
    }
}

/// A history rewrite in progress ([`LiveTurns::write`]); the thread's lock is
/// released when it drops.
pub(crate) struct HistoryWrite {
    inner: Arc<Inner>,
    thread_id: i64,
    slot: Option<Arc<Slot>>,
    _guard: Option<OwnedMutexGuard<()>>,
}

impl Drop for HistoryWrite {
    fn drop(&mut self) {
        self._guard.take();
        self.slot.take();
        self.inner.release(self.thread_id);
    }
}

/// One turn's hold on its thread ([`LiveTurns::begin`]).
pub(crate) struct Ticket {
    inner: Arc<Inner>,
    thread_id: i64,
    id: u64,
    generation: u64,
    slot: Option<Arc<Slot>>,
    cancelled: watch::Receiver<bool>,
}

/// A reply being saved: the thread's lock, held while the generation is still
/// the turn's ([`Ticket::save_lock`]).
pub(crate) struct SaveGuard {
    _guard: OwnedMutexGuard<()>,
}

impl Ticket {
    /// Resolves when a newer turn of this thread started, or the thread went
    /// away; pends for as long as this turn is the live one.
    pub(crate) async fn superseded(&self) {
        let mut rx = self.cancelled.clone();
        if rx.wait_for(|c| *c).await.is_err() {
            // The switch was dropped without being thrown: this turn is no
            // longer registered (it ended) — nothing will ever cancel it.
            std::future::pending::<()>().await;
        }
    }

    /// Whether a newer turn started or the thread went away, right now.
    #[cfg(test)]
    pub(crate) fn is_superseded(&self) -> bool {
        *self.cancelled.borrow()
    }

    /// The thread's lock, when its history is still what this turn read —
    /// hold it while the reply is written. `None` when it moved on: the reply
    /// belongs to a history that no longer exists and must not be saved.
    pub(crate) async fn save_lock(&self) -> Option<SaveGuard> {
        let slot = self.slot.as_ref()?;
        let guard = slot.write.clone().lock_owned().await;
        (lock(&slot.state).generation == self.generation).then_some(SaveGuard { _guard: guard })
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            {
                let mut st = lock(&slot.state);
                if st.live.as_ref().is_some_and(|l| l.id == self.id) {
                    st.live = None;
                }
            }
            drop(slot);
            self.inner.release(self.thread_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_rewrite_or_a_newer_turn_refuses_the_older_reply() {
        let live = LiveTurns::default();
        let t1 = live.begin(7).await;
        assert!(t1.save_lock().await.is_some(), "nothing moved");
        drop(live.write(7).await);
        assert!(t1.save_lock().await.is_none(), "an edit moved the history");

        let t2 = live.begin(7).await;
        assert!(t2.save_lock().await.is_some());
        let t3 = live.begin(7).await;
        assert!(t2.is_superseded(), "the newer turn cancelled it");
        tokio::time::timeout(std::time::Duration::from_secs(1), t2.superseded())
            .await
            .expect("superseded resolves");
        assert!(t2.save_lock().await.is_none());
        assert!(t3.save_lock().await.is_some());
        assert!(!t3.is_superseded());

        // Another thread is its own.
        let other = live.begin(8).await;
        assert!(t3.save_lock().await.is_some());
        assert!(other.save_lock().await.is_some());
    }

    #[tokio::test]
    async fn a_history_write_cancels_the_live_turn_at_once() {
        let live = LiveTurns::default();
        let t = live.begin(5).await;
        assert!(!t.is_superseded());
        drop(live.write(5).await);
        assert!(
            t.is_superseded(),
            "cancelled by the rewrite, not the next begin"
        );
    }

    #[tokio::test]
    async fn a_discarded_thread_cancels_its_turn() {
        let live = LiveTurns::default();
        let t = live.begin(-3).await;
        drop(live.discard(-3).await);
        assert!(t.is_superseded());
        assert!(t.save_lock().await.is_none());
    }

    #[tokio::test]
    async fn a_save_holds_off_a_rewrite() {
        let live = Arc::new(LiveTurns::default());
        let t = live.begin(1).await;
        let save = t.save_lock().await.expect("current");
        let l2 = live.clone();
        let edit = tokio::spawn(async move {
            drop(l2.write(1).await);
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!edit.is_finished(), "the edit waits for the save");
        drop(save);
        edit.await.unwrap();
        assert!(
            t.save_lock().await.is_none(),
            "and moves the history after it"
        );
    }

    #[tokio::test]
    async fn slots_go_once_nothing_refers_to_them() {
        let live = LiveTurns::default();
        let t = live.begin(1).await;
        drop(live.write(2).await);
        assert_eq!(live.slots(), 1, "thread 2 had no turn: its slot went");
        drop(t);
        assert_eq!(live.slots(), 0);
        // A new slot never repeats a generation an old ticket could hold.
        let a = live.begin(1).await;
        let g = a.generation;
        drop(a);
        let b = live.begin(1).await;
        assert!(b.generation > g);
    }
}
