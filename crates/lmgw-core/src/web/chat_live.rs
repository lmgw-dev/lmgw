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
//!
//! The slot also holds **the thread's speech** (chat-voice design §6.4,
//! [`speech`]): the stops of the read-aloud running for it — a reply read
//! as it streams, a stored one read again — which `speech/stop` raises
//! without touching the text; and **the thread's bound realtime session**
//! (§8.1, [`voice`]): one per thread, a second bind taking over.
//!
//! **A conditional write** ([`LiveTurns::write_if`], §8.3) is the bound
//! session's journal's: a cut or a delete of a spoken reply proceeds under
//! the thread's lock only while the generation is still the one its turn left
//! behind. It does not move the generation — no turn has started since (a
//! start moves it), so none is live to cancel — and a voice finalize thus
//! never cancels a text turn of another window. The session's binding keeps
//! the thread's slot, and with it that generation; a slot that went (and
//! came back with a new generation) refuses the write.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{watch, OwnedMutexGuard};

mod speech;
mod voice;

pub(crate) use voice::{Binder, TakenBy, VoiceBinding};

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
    /// The change feed's live half (client-apps design §2.3): the turns and
    /// bound sessions it reports, registered here as they start.
    feed: super::chat_feed::LiveFeed,
}

struct Slot {
    /// Serialises the thread's history rewrites with a reply's save.
    write: Arc<tokio::sync::Mutex<()>>,
    state: Mutex<SlotState>,
}

struct SlotState {
    generation: u64,
    live: Option<Live>,
    /// The thread's read-aloud, each by its id ([`speech`]).
    speech: Vec<(u64, crate::proxy::StopHandle)>,
    /// The thread's bound realtime session ([`voice`]): its binding's id,
    /// the stop that tells it another window took over, and its fence.
    voice: Option<voice::Held>,
}

/// The turn currently answering: its ticket id, and the switch that cancels
/// it.
struct Live {
    id: u64,
    cancel: watch::Sender<bool>,
    /// The device key that started it, and its thread's level
    /// (`ChatThread::reach_level`): a turn whose thread left that device's
    /// reach — the toolset attached, the device's admin-tools switch turned
    /// off — is cancelled as a delete cancels it (review P-8).
    device: Option<i64>,
    level: u8,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing here panics while holding a lock; a poisoned one still holds a
    // consistent map, so carry on rather than take the Chat down.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run `write` on a task of its own, to its end, and wait for it (the
/// branch review's N-3): a write that narrows what devices hear before its
/// commit — a delete ([`LiveTurns::discard`]), an attach of the self-admin
/// toolset ([`LiveTurns::self_admin_attaching`]) — then commits, then takes
/// the step that follows the commit or undoes its first one. The request
/// that waits for it is dropped when its client hangs up, and a write cut
/// in between left a thread hidden from devices after its commit rolled
/// back, or a commit with no close behind it. The locks it holds go with
/// it, so they are held to its end too. An `Err` only when the task did
/// not run to its end because the runtime is shutting down; a panic in it
/// is the caller's. Either way the narrowing goes back with its guard
/// (`Going` for a delete, `chat::Attaching` for an attach) unless the step
/// after the commit ran.
pub(crate) async fn to_its_end<T: Send + 'static>(
    write: impl std::future::Future<Output = T> + Send + 'static,
) -> Result<T, crate::error::GatewayError> {
    match tokio::spawn(write).await {
        Ok(out) => Ok(out),
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(crate::error::GatewayError::Internal(format!(
            "the write did not run to its end: {e}"
        ))),
    }
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
                        speech: Vec::new(),
                        voice: None,
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
    /// The live turns of a gateway whose change feed is `feed`
    /// (`AppState::chat_feed`): the turns and bound sessions it reports.
    pub(crate) fn with_feed(feed: super::chat_feed::LiveFeed) -> Self {
        Self {
            inner: Arc::new(Inner {
                feed,
                ..Inner::default()
            }),
        }
    }

    /// The change feed's live half these turns report to.
    pub(crate) fn feed(&self) -> &super::chat_feed::LiveFeed {
        &self.inner.feed
    }

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
            going: None,
        }
    }

    /// Whether a turn answers thread `thread_id` now (a send, an edit, a
    /// regenerate, a continue, a bound session's voice turn).
    pub(crate) fn running(&self, thread_id: i64) -> bool {
        lock(&self.inner.threads)
            .get(&thread_id)
            .is_some_and(|s| lock(&s.state).live.is_some())
    }

    /// The thread's lock alone: no generation moves and no turn is
    /// cancelled. What a settings write that may attach or remove the
    /// self-admin toolset holds (review W4-3), so a device's user message is
    /// written either before it, when the thread was still the device's,
    /// or after it, re-checked against what it has become; and what a
    /// device's move, pin, archive or delete holds from its re-check of the
    /// thread's reach to its write (review W6-6).
    pub(crate) async fn hold(&self, thread_id: i64) -> HistoryWrite {
        let slot = self.inner.slot(thread_id);
        let guard = slot.write.clone().lock_owned().await;
        HistoryWrite {
            inner: self.inner.clone(),
            thread_id,
            slot: Some(slot),
            _guard: Some(guard),
            going: None,
        }
    }

    /// A conditional history write (module doc): the thread's lock, while
    /// its generation is still `generation`; `None` once it moved — another
    /// turn started, or the history was rewritten, since. The generation is
    /// not moved. Hold the guard for the length of the write.
    pub(crate) async fn write_if(&self, thread_id: i64, generation: u64) -> Option<HistoryWrite> {
        let slot = self.inner.slot(thread_id);
        let guard = slot.write.clone().lock_owned().await;
        let current = lock(&slot.state).generation == generation;
        let write = HistoryWrite {
            inner: self.inner.clone(),
            thread_id,
            slot: Some(slot),
            _guard: Some(guard),
            going: None,
        };
        current.then_some(write)
    }

    /// The thread is about to go away (deleted, discarded or kept): its
    /// lock, held across the write, and from here its live events reach no
    /// device, as a hidden thread's (`LiveFeed::thread_going`). Nothing else
    /// before the commit (`voice`'s module doc): [`Self::discarded`] follows
    /// it under the same lock, and a delete that fails calls
    /// [`Self::delete_failed`].
    ///
    /// The mark goes with the guard ([`Going`]): one dropped before either
    /// — a panic in the write, or this wait for the lock cut short — takes
    /// it back as a failed delete does.
    pub(crate) async fn discard(&self, thread_id: i64) -> HistoryWrite {
        let going = Going::mark(&self.inner, thread_id);
        let mut held = self.hold(thread_id).await;
        held.going = Some(going);
        held
    }

    /// [`Self::discard`] under a lock the caller took with [`Self::hold`]
    /// (review W6-6: a device's delete re-checks the thread's reach under
    /// it first): the same, without taking the lock a second time.
    pub(crate) fn discard_held(&self, held: &mut HistoryWrite) {
        held.going = Some(Going::mark(&self.inner, held.thread_id));
    }

    /// The thread is gone, its delete committed, under the lock
    /// [`Self::discard`] took: its live events stay out of every device's
    /// reach for good, whatever another delete of it does
    /// (`LiveFeed::thread_gone`), its history moves on, its live turn is
    /// cancelled, a realtime session still bound to it will say, when it
    /// ends, that its thread went (`voice.ended {reason: "thread_gone"}`,
    /// client-apps design §2.2; to the owner alone), and a device's session
    /// closes as one out of its reach ([`Self::thread_deleted`]).
    pub(crate) fn discarded(&self, held: &mut HistoryWrite) {
        let thread_id = held.thread_id;
        if let Some(going) = held.going.take() {
            going.committed();
        }
        self.inner.feed.thread_gone(thread_id);
        if let Some(slot) = &held.slot {
            let mut st = lock(&slot.state);
            st.generation = self.inner.next();
            if let Some(live) = st.live.take() {
                let _ = live.cancel.send(true);
            }
        }
        self.voice_thread_gone(thread_id);
        self.inner.feed.forget_thread(thread_id);
        self.thread_deleted(thread_id);
    }

    /// A delete that [`Self::discard`] prepared failed: nothing moved but
    /// the live events' reach, which goes back unless another delete of the
    /// thread is under way or committed, and with it the reason a session
    /// registered meanwhile would have given (`voice.ended`'s
    /// `thread_gone`; `LiveFeed::thread_going`). The guard's own drop
    /// does the same: this says it where the write decides it.
    pub(crate) fn delete_failed(&self, held: &mut HistoryWrite) {
        drop(held.going.take());
    }

    /// `begin_as` for an owner's turn on a plain thread (tests).
    #[cfg(test)]
    pub(crate) async fn begin(&self, thread_id: i64) -> Ticket {
        self.begin_as(thread_id, None, 0).await
    }

    /// Start a turn of `thread_id`: its generation moves (a turn still
    /// running can no longer save), the previous turn is cancelled, and this
    /// one becomes the live one. The ticket is the turn's for its lifetime;
    /// dropping it ends the turn. `device` started it (`None`: the owner),
    /// on a thread at `level`: a device's turn whose thread leaves its reach
    /// while it runs is cancelled ([`Self::self_admin_changed`],
    /// [`Self::reach_changed`]).
    pub(crate) async fn begin_as(&self, thread_id: i64, device: Option<i64>, level: u8) -> Ticket {
        let slot = self.inner.slot(thread_id);
        let (cancel, cancelled) = watch::channel(false);
        let id = self.inner.next();
        let generation = {
            // The lock only orders this against a write or a save in flight.
            let _guard = slot.write.clone().lock_owned().await;
            let mut st = lock(&slot.state);
            st.generation = self.inner.next();
            let live = Live {
                id,
                cancel,
                device,
                level,
            };
            if let Some(previous) = st.live.replace(live) {
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
    /// A delete's mark on the thread ([`LiveTurns::discard`]) until its
    /// commit or its failure: taken back, under the lock still, when this
    /// drops before either.
    going: Option<Going>,
}

impl Drop for HistoryWrite {
    fn drop(&mut self) {
        self.going.take();
        self._guard.take();
        self.slot.take();
        self.inner.release(self.thread_id);
    }
}

/// A delete's narrowing of what devices hear of its thread
/// (`LiveFeed::thread_going`), made by [`LiveTurns::discard`]: a guard, so
/// that the mark is taken back whenever the delete does not reach its
/// post-commit step ([`LiveTurns::discarded`]). A failure says so
/// ([`LiveTurns::delete_failed`]); a panic between the mark and the commit
/// or the undo, which used to leave the mark set until the gateway
/// restarted, drops it.
struct Going {
    inner: Arc<Inner>,
    thread_id: i64,
    /// Still to be taken back on drop.
    armed: bool,
}

impl Going {
    fn mark(inner: &Arc<Inner>, thread_id: i64) -> Self {
        inner.feed.thread_going(thread_id, true);
        Self {
            inner: inner.clone(),
            thread_id,
            armed: true,
        }
    }

    /// The delete committed: the mark stays, and `LiveFeed::thread_gone`
    /// settles it.
    fn committed(mut self) {
        self.armed = false;
    }
}

impl Drop for Going {
    fn drop(&mut self) {
        if self.armed {
            self.inner.feed.thread_going(self.thread_id, false);
        }
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
    /// The thread's generation this turn started at: what its reply is saved
    /// against, and what a bound session's journal guards its later writes
    /// with ([`LiveTurns::write_if`]).
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

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
        let mut write = live.discard(-3).await;
        assert!(!t.is_superseded(), "nothing moves before the commit");
        live.discarded(&mut write);
        drop(write);
        assert!(t.is_superseded());
        assert!(t.save_lock().await.is_none());
    }

    /// The branch review's N-2: a folder's delete marks a thread and waits
    /// for its lock, which the thread's own delete holds. Whichever of the
    /// two fails, before the other commits or after it, the thread's live
    /// events stay out of every device's reach, and so does a turn that
    /// registers on it later.
    #[tokio::test]
    async fn two_deletes_of_one_thread_never_undo_each_other_s_narrowing() {
        use crate::store::AdminThreads;
        use futures::FutureExt;
        for own_fails in [true, false] {
            let live = LiveTurns::default();
            let devices = |live: &LiveTurns| live.feed().now(AdminThreads::Hidden).turns.len();
            let _running = live
                .feed()
                .turn_started(7, 0, "device 'phone'".into(), false);
            assert_eq!(devices(&live), 1);
            let mut own = live.hold(7).await;
            live.discard_held(&mut own);
            let folder = live.discard(7);
            tokio::pin!(folder);
            assert!(
                folder.as_mut().now_or_never().is_none(),
                "the folder's delete waits for the lock"
            );
            if own_fails {
                live.delete_failed(&mut own);
                drop(own);
                assert_eq!(devices(&live), 0, "the folder's delete is under way");
                let mut held = folder.await;
                live.discarded(&mut held);
            } else {
                live.discarded(&mut own);
                drop(own);
                let mut held = folder.await;
                // The thread was gone when its folder's delete ran.
                live.delete_failed(&mut held);
                drop(held);
            }
            assert_eq!(devices(&live), 0, "own fails: {own_fails}");
            let _late = live
                .feed()
                .turn_started(7, 0, "device 'phone'".into(), true);
            assert_eq!(devices(&live), 0, "own fails: {own_fails}");
        }
    }

    /// The last review's follow-up: a delete that panics between its
    /// narrowing and its commit or undo — here a write of the test's own,
    /// run as the routes run theirs (`to_its_end`) — takes the narrowing
    /// back as its guard drops. It used to stay until the gateway
    /// restarted, and a device never heard of the thread live again.
    #[tokio::test]
    async fn a_delete_that_panics_before_its_commit_takes_its_narrowing_back() {
        use crate::store::AdminThreads;
        let live = Arc::new(LiveTurns::default());
        let devices = |live: &LiveTurns| live.feed().now(AdminThreads::Hidden).turns.len();
        let _running = live
            .feed()
            .turn_started(7, 0, "device 'phone'".into(), false);
        assert_eq!(devices(&live), 1);
        let held = live.hold(7).await;
        let task = live.clone();
        let out = tokio::spawn(to_its_end(async move {
            let mut held = held;
            task.discard_held(&mut held);
            assert_eq!(devices(&task), 0, "narrowed before the commit");
            panic!("an injected failure between the narrowing and the commit");
        }))
        .await;
        assert!(out.is_err_and(|e| e.is_panic()));
        assert_eq!(devices(&live), 1, "the narrowing was taken back");
        drop(live.hold(7).await);

        // A delete waiting for the thread's lock that is cut short takes
        // its mark back too.
        let own = live.hold(7).await;
        {
            use futures::FutureExt;
            let folder = live.discard(7);
            tokio::pin!(folder);
            assert!(folder.as_mut().now_or_never().is_none());
            assert_eq!(devices(&live), 0, "marked while it waits");
        }
        assert_eq!(devices(&live), 1, "taken back as the wait went");
        drop(own);
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
    async fn a_conditional_write_holds_only_while_nothing_moved() {
        let live = LiveTurns::default();
        // The bound session's binding keeps the thread's slot, and with it
        // the generation its turns leave behind.
        let bound = live.bind_voice(6, "the dashboard").guard;
        let t = live.begin(6).await;
        let g = t.generation();
        drop(t);
        // Twice in a row: the write itself moves nothing.
        drop(live.write_if(6, g).await.expect("nothing moved"));
        assert!(live.write_if(6, g).await.is_some());
        // Another turn of the thread (a text send in another window).
        let other = live.begin(6).await;
        assert!(live.write_if(6, g).await.is_none());
        assert!(!other.is_superseded(), "a refused write cancels nothing");
        drop((other, bound));
        assert_eq!(live.slots(), 0, "a refused write leaves no slot behind");
        // A slot that went is no proof nothing moved: refused.
        let t = live.begin(6).await;
        let g = t.generation();
        drop(t);
        assert!(live.write_if(6, g).await.is_none());
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
