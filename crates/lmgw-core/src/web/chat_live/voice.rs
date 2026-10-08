//! The thread's bound realtime session (chat-voice design §8.1): **one per
//! thread**. A second bind of the same thread takes over — the older
//! session is told through its stop and closes with a reason naming the
//! binder ("voice mode moved to device 'phone'", "… to the dashboard";
//! client-apps design §1.7) — so two journals and two item maps never write
//! one history. Takeover rather than refusal, so a single owner can move
//! windows, and a conversation can move between devices.
//!
//! A binding is a guard ([`VoiceBinding`]) the session holds for its life;
//! dropping it frees the thread. While one is held a temporary thread is
//! not kept (`ChatRepo::keep`, `409 voice_session_active`): the session
//! writes into the temporary store by the id it bound.
//!
//! **The takeover's fence** (WP8 review m7). The older session drains its
//! journal after the newer one is live, and that drain may still write a
//! user message — so the newer session gets the older binding's fence, a
//! signal raised once that binding is gone, which its session drops only
//! after its journal drained. The newer journal writes nothing before it:
//! the two journals write one history one after the other, never
//! interleaved.
//!
//! **A bind that never takes over** (`takeover=never`, 2026-10-07): a
//! client's own automatic rebind — after an ongoing folder rolled over —
//! must not take a session from another device that followed the same
//! rollover. [`LiveTurns::bind_voice_unless_bound`] binds only a thread no
//! other session holds, decided under the slot's lock with the
//! registration, and names the binder that holds it otherwise. Two holders
//! are not "another session" (review F-5): one bound with the same key —
//! the device's own session, after a dropped link it has not been told of
//! yet — and one whose session is already ending ([`VoiceBinding::closing`]:
//! it left its loop, after its 1001, a revocation or a close, and only
//! writes its last turns now). Such a binding is taken over, its fence
//! kept, so the new journal still writes after the old one drained.
//!
//! **A device's session leaves with its thread's reach, after the write**
//! (client-apps design L3; §1.6's close-code note, 2026-10-07). A session a
//! device bound closes with `chat_thread_not_found`, then the neutral 4004,
//! when its thread leaves the device's reach — the self-admin toolset
//! attached, the device's level or the gateway's dropped — and when the
//! thread is deleted: a device cannot tell the two apart (review W4-18).
//! The close is raised only once the write that moved the thread is
//! committed, and for a level once the snapshot that says it is published
//! ([`LiveTurns::self_admin_changed`], [`LiveTurns::reach_changed`],
//! [`LiveTurns::discarded`]), and so is the cancel of a device's turn
//! there: a client that reads its folder's current thread, or binds again,
//! the moment it hears the close finds the new reach. What a write may do
//! before its commit is only narrow the live events devices hear
//! ([`LiveTurns::self_admin_attaching`], [`LiveTurns::discard`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::{lock, Inner, LiveTurns, Slot, SlotState};
use crate::proxy::{stop_pair, StopHandle, StopSignal};
use crate::web::chat_feed::{Ending, VoiceEnd, VoiceWatch};

/// One bound session's hold on its thread (module doc).
pub(crate) struct VoiceBinding {
    inner: Arc<Inner>,
    thread_id: i64,
    id: u64,
    slot: Option<Arc<Slot>>,
    /// Raises this binding's fence when it goes (module doc).
    _gone: StopHandle,
    /// The binding in the change feed: `voice.ended` when it goes, saying
    /// who took it over when another bind did (client-apps design §2.2).
    watch: Option<VoiceWatch>,
    /// Who took it over, once another bind did.
    taken_by: TakenBy,
    /// Set when its session closes because its thread left its binder's
    /// reach, or was deleted (module doc).
    out_of_reach: Arc<AtomicBool>,
}

impl VoiceBinding {
    /// Its session ends because its key was revoked (§1.6): what its
    /// `voice.ended` says.
    pub(crate) fn revoked(&self) {
        if let Some(w) = &self.watch {
            w.ending().set(VoiceEnd::Revoked);
        }
    }

    /// Its thread left its binder's reach, or was deleted (module doc): the
    /// session closes as out of reach for that thread, not as taken over.
    pub(crate) fn out_of_reach(&self) -> bool {
        self.out_of_reach.load(Ordering::Acquire)
    }

    /// Its session is ending (module doc): it left its loop and writes its
    /// last turns now. A `takeover=never` bind no longer counts it as
    /// holding the thread.
    pub(crate) fn closing(&self) {
        if let Some(slot) = &self.slot {
            if let Some(held) = lock(&slot.state).voice.as_mut().filter(|h| h.id == self.id) {
                held.closing = true;
            }
        }
    }
}

/// What a bind hands the session (module doc).
pub(crate) struct VoiceBind {
    pub guard: VoiceBinding,
    /// Raised when another window binds the thread.
    pub taken: StopSignal,
    /// Who did, once `taken` is raised.
    pub taken_by: TakenBy,
    /// The binding this one took over, raised once it is gone; `None` when
    /// there was none.
    pub fence: Option<StopSignal>,
}

/// Who took a binding over (client-apps design §1.7): written by the bind
/// that did, before it raises the older session's stop; read by that
/// session as it closes.
#[derive(Debug, Clone, Default)]
pub(crate) struct TakenBy(Arc<std::sync::Mutex<Option<String>>>);

impl TakenBy {
    /// The binder's name — "the dashboard", "device 'phone'" — once the
    /// binding was taken over.
    pub(crate) fn get(&self) -> Option<String> {
        lock(&self.0).clone()
    }

    fn set(&self, by: &str) {
        *lock(&self.0) = Some(by.to_string());
    }
}

/// A binding as its thread's slot keeps it.
pub(super) struct Held {
    id: u64,
    /// The stop that ends its session: another window took over, or the
    /// thread left its binder's reach or was deleted
    /// (`Held::close_out_of_reach`).
    stop: StopHandle,
    /// Raised once it is gone (module doc).
    fence: StopSignal,
    /// Where the binder that takes it over writes its name.
    taken_by: TakenBy,
    /// Where the thread's delete, or its leaving the binder's reach, says
    /// why it ended (`voice.ended`).
    ending: Ending,
    /// It was bound by a device.
    device: bool,
    /// Its thread's level now (`ChatThread::reach_level`): kept by
    /// `LiveTurns::self_admin_changed`.
    level: u8,
    /// The key it was bound with; `None` for an in-process binder.
    key_id: Option<i64>,
    /// Its session is ending ([`VoiceBinding::closing`]).
    closing: bool,
    /// Who bound it: "the dashboard", "device 'phone'".
    by: String,
    /// Shared with its binding: it closes as out of reach (module doc).
    out_of_reach: Arc<AtomicBool>,
}

impl Held {
    /// Whether it holds the thread against a `takeover=never` bind with
    /// `key_id` (module doc): not when its session is ending, nor when it
    /// was bound with the same key.
    fn holds_against(&self, key_id: Option<i64>) -> bool {
        !self.closing && !(key_id.is_some() && self.key_id == key_id)
    }
}

/// Who binds a session ([`LiveTurns::bind_voice_as`]): how they are named
/// ("the dashboard", "device 'phone'"), whether they are a device, and the
/// thread's level (`ChatThread::reach_level`: the owner binds a thread with
/// the self-admin toolset, and so does a device allowed lmgw's admin tools;
/// client-apps design L3).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Binder<'a> {
    pub by: &'a str,
    pub device: bool,
    pub level: u8,
    /// The key it binds with; `None` for an in-process binder.
    pub key_id: Option<i64>,
}

impl LiveTurns {
    /// Bind a realtime session to `thread_id` for `by` — how the binder is
    /// named, "the dashboard" or "device 'phone'" (module doc) — as the
    /// owner binds a plain thread. A session already bound is taken over
    /// now, and told by whom.
    #[cfg(test)]
    pub(crate) fn bind_voice(&self, thread_id: i64, by: &str) -> VoiceBind {
        self.bind_voice_as(
            thread_id,
            Binder {
                by,
                device: false,
                level: 0,
                key_id: None,
            },
        )
    }

    /// [`Self::bind_voice`] for `binder`.
    pub(crate) fn bind_voice_as(&self, thread_id: i64, binder: Binder<'_>) -> VoiceBind {
        match self.bind(thread_id, binder, true) {
            Ok(bind) => bind,
            Err(_) => unreachable!("a bind that takes over is never refused"),
        }
    }

    /// [`Self::bind_voice_as`] that never takes over (module doc): `Err`
    /// with the name of the binder that holds the thread, and nothing
    /// bound, when a session is bound to it.
    pub(crate) fn bind_voice_unless_bound(
        &self,
        thread_id: i64,
        binder: Binder<'_>,
    ) -> Result<VoiceBind, String> {
        self.bind(thread_id, binder, false)
    }

    fn bind(
        &self,
        thread_id: i64,
        binder: Binder<'_>,
        takeover: bool,
    ) -> Result<VoiceBind, String> {
        let by = binder.by;
        let slot = self.inner.slot(thread_id);
        let id = self.inner.next();
        let (handle, taken) = stop_pair();
        let (gone, fence) = stop_pair();
        let taken_by = TakenBy::default();
        let out_of_reach = Arc::new(AtomicBool::new(false));
        let previous = {
            // The check and the registration under one lock: two binds at
            // once never both find the thread free.
            let mut st = lock(&slot.state);
            if let Some(held) = st
                .voice
                .as_ref()
                .filter(|h| !takeover && h.holds_against(binder.key_id))
            {
                let holder = held.by.clone();
                drop(st);
                drop(slot);
                self.inner.release(thread_id);
                return Err(holder);
            }
            let watch = self.inner.feed.voice_bound(thread_id, by, binder.level);
            let previous = st.voice.replace(Held {
                id,
                stop: handle,
                fence,
                taken_by: taken_by.clone(),
                ending: watch.ending(),
                device: binder.device,
                level: binder.level,
                key_id: binder.key_id,
                closing: false,
                by: by.to_string(),
                out_of_reach: out_of_reach.clone(),
            });
            (previous, watch)
        };
        let (previous, watch) = previous;
        let fence = previous.map(|older| {
            // Named before it is woken, so it reads who as it closes. Out of
            // the lock: raising the stop wakes the older session. One that
            // was already ending (its own close, a 1001) ended by itself:
            // its `voice.ended` says so, not a takeover (review P-17).
            if !older.closing {
                older.taken_by.set(by);
            }
            older.stop.stop();
            older.fence
        });
        let guard = VoiceBinding {
            inner: self.inner.clone(),
            thread_id,
            id,
            slot: Some(slot),
            _gone: gone,
            watch: Some(watch),
            taken_by: taken_by.clone(),
            out_of_reach,
        };
        Ok(VoiceBind {
            guard,
            taken,
            taken_by,
            fence,
        })
    }

    /// Whether a realtime session is bound to `thread_id` now.
    pub(crate) fn voice_bound(&self, thread_id: i64) -> bool {
        let slot = lock(&self.inner.threads).get(&thread_id).cloned();
        slot.is_some_and(|s| lock(&s.state).voice.is_some())
    }

    /// `thread_id` was deleted: a session bound to it says so when it ends
    /// (`voice.ended {reason: "thread_gone"}`). A device's session closes
    /// ([`Self::thread_deleted`]); the owner's is left alone.
    pub(super) fn voice_thread_gone(&self, thread_id: i64) {
        let slot = lock(&self.inner.threads).get(&thread_id).cloned();
        if let Some(slot) = slot {
            if let Some(held) = &lock(&slot.state).voice {
                held.ending.set(VoiceEnd::ThreadGone);
            }
        }
    }

    /// `thread_id`'s level changed to `level` (client-apps design L3, review
    /// W3-1): the self-admin toolset was attached (`1`) or taken off (`0`).
    /// A session a device bound to it that does not reach `level`, as `snap`
    /// says now, ends — it closes as out of reach for that thread — and the
    /// change feed's live events about the thread reach only the readers
    /// that see it. Taken off, the thread is every device's again (a device
    /// binds anew).
    ///
    /// **Only once the write that moved it is committed** (module doc): the
    /// level is the one the store holds now. Before the commit an attaching
    /// write calls [`Self::self_admin_attaching`].
    pub(crate) fn self_admin_changed(
        &self,
        snap: &crate::config::Snapshot,
        thread_id: i64,
        level: u8,
    ) {
        self.inner.feed.thread_self_admin(thread_id, level);
        let slot = lock(&self.inner.threads).get(&thread_id).cloned();
        if let Some(slot) = slot {
            let mut st = lock(&slot.state);
            if let Some(held) = st.voice.as_mut() {
                if held.level != 2 {
                    held.level = level;
                }
                held.close_if_out_of_reach(snap);
            }
            if let Some(live) = st.live.as_mut() {
                if live.level != 2 {
                    live.level = level;
                }
            }
            self.cancel_if_out_of_reach(&mut st, snap);
        }
    }

    /// A write that attaches the self-admin toolset to `thread_id` (at
    /// `level`) is about to commit (reviews W4-3, W4-8): the thread's live
    /// events stop reaching the devices that will not see it, so none hears
    /// a turn of it once it is out of its reach. Only that: no session
    /// closes and no turn is cancelled before the commit — a device told
    /// its thread went must find it gone wherever it reads (module doc).
    /// [`Self::self_admin_changed`] follows the commit; a write that fails
    /// gives the flag back through it.
    pub(crate) fn self_admin_attaching(&self, thread_id: i64, level: u8) {
        self.inner.feed.thread_self_admin(thread_id, level);
    }

    /// `thread_id` was deleted, and the delete committed (or a temporary
    /// thread was discarded; [`Self::discarded`]): a session a device bound
    /// to it closes as one whose thread left the device's reach —
    /// `chat_thread_not_found`, then the neutral 4004 — since a device must
    /// not tell a deleted thread from one hidden from it (review W4-18;
    /// §1.6's close-code note, 2026-10-07). Its `voice.ended` still says
    /// `thread_gone` to the owner, as [`Self::voice_thread_gone`] set it.
    /// The owner's session is left alone, as the delete always left it.
    pub(super) fn thread_deleted(&self, thread_id: i64) {
        let slot = lock(&self.inner.threads).get(&thread_id).cloned();
        if let Some(slot) = slot {
            if let Some(held) = lock(&slot.state).voice.as_ref().filter(|h| h.device) {
                held.close_out_of_reach();
            }
        }
    }

    /// The sweep purged threads `ids` (`server::chat_upkeep`), its write
    /// committed, without a discard: each goes as a deleted thread goes —
    /// its live events reach no device from now (`LiveFeed::thread_going`),
    /// then under its lock its live turn is cancelled and a device's session
    /// on it closes ([`Self::discarded`]). Before the caller wakes the feed,
    /// so a device's stream never plays `thread.deleted` while a live event
    /// of the thread still reaches it. Temporary threads are never swept.
    pub(crate) async fn purged(&self, ids: &[i64]) {
        for id in ids {
            self.inner.feed.thread_going(*id, true);
        }
        for id in ids {
            let mut held = self.hold(*id).await;
            self.discarded(&mut held);
        }
    }

    /// A device's admin-tools switch changed (`ApiKey::self_admin`), or the
    /// gateway's level that caps it: every session a device bound to a
    /// thread it no longer reaches, as `snap` says now, ends as
    /// [`Self::self_admin_changed`] ends one, and every text turn it runs on
    /// such a thread is cancelled (review P-8). `snap` is the published
    /// snapshot that says the level (module doc): the routes read it.
    pub(crate) fn reach_changed(&self, snap: &crate::config::Snapshot) {
        let slots: Vec<Arc<Slot>> = lock(&self.inner.threads).values().cloned().collect();
        for slot in slots {
            let mut st = lock(&slot.state);
            if let Some(held) = st.voice.as_ref() {
                held.close_if_out_of_reach(snap);
            }
            self.cancel_if_out_of_reach(&mut st, snap);
        }
    }

    /// The live turn of a slot, when a device started it and its thread is
    /// out of that device's reach as `snap` says now: cancelled as a delete
    /// cancels it (`discard`) — the generation moves, so its partial reply
    /// is not saved onto a thread the device no longer reaches, and its
    /// reader is told the turn stopped (`superseded`). An `lmgw__*` call
    /// already running finishes; the next one is refused by the scope's
    /// per-call check.
    fn cancel_if_out_of_reach(&self, st: &mut SlotState, snap: &crate::config::Snapshot) {
        let out = st.live.as_ref().is_some_and(|live| {
            live.device
                .is_some_and(|key| !crate::devices::reach(snap, Some(key)).sees(live.level))
        });
        if out {
            st.generation = self.inner.next();
            if let Some(live) = st.live.take() {
                let _ = live.cancel.send(true);
            }
        }
    }
}

impl Held {
    /// A device's binding whose binder no longer reaches its thread's level,
    /// as `snap` says now: closed as out of reach.
    fn close_if_out_of_reach(&self, snap: &crate::config::Snapshot) {
        if self.device && !crate::devices::reach(snap, self.key_id).sees(self.level) {
            self.close_out_of_reach();
        }
    }

    /// Its session closes as out of reach for its thread (module doc): its
    /// `voice.ended` says `revoked` unless a reason was said first (a
    /// delete's `thread_gone`), and it is stopped.
    fn close_out_of_reach(&self) {
        self.ending.set(VoiceEnd::OutOfReach);
        self.out_of_reach.store(true, Ordering::Release);
        self.stop.stop();
    }
}

impl Drop for VoiceBinding {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            // Only its own: a session taken over leaves the newer binding.
            let mine = {
                let mut st = lock(&slot.state);
                st.voice
                    .take_if(|held| held.id == self.id)
                    .map(|held| held.stop)
            };
            drop(mine);
            drop(slot);
            self.inner.release(self.thread_id);
        }
        if let Some(watch) = self.watch.take() {
            watch.end(self.taken_by.get());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_second_bind_takes_over_and_the_first_one_s_end_leaves_it() {
        let live = LiveTurns::default();
        assert!(!live.voice_bound(3));
        let first = live.bind_voice(3, "the dashboard");
        assert!(first.fence.is_none(), "nothing to wait for");
        assert!(live.voice_bound(3));
        assert!(!first.taken.is_raised());
        assert_eq!(first.taken_by.get(), None);
        let second = live.bind_voice(3, "device 'phone'");
        assert!(first.taken.is_raised(), "the first session is told");
        assert_eq!(
            first.taken_by.get().as_deref(),
            Some("device 'phone'"),
            "and by whom"
        );
        assert!(!second.taken.is_raised());
        let fence = second.fence.clone().expect("the first binding's fence");
        assert!(!fence.is_raised(), "the first session still drains");
        // The first session ends: the thread stays bound to the second,
        // whose journal may write now.
        drop(first);
        assert!(fence.is_raised());
        assert!(live.voice_bound(3));
        assert!(!second.taken.is_raised());
        drop(second);
        assert!(!live.voice_bound(3));
        assert_eq!(live.slots(), 0, "nothing left behind");
    }

    fn binder(by: &str, key_id: Option<i64>) -> Binder<'_> {
        Binder {
            by,
            device: true,
            level: 0,
            key_id,
        }
    }

    /// Review F-5: `takeover=never` is refused by another key's live
    /// session only — its own key's binding, and a binding whose session is
    /// ending, are taken over, the fence kept.
    #[tokio::test]
    async fn a_bind_that_never_takes_over_counts_only_another_live_session() {
        let live = LiveTurns::default();
        let phone = live.bind_voice_as(4, binder("device 'phone'", Some(1)));
        let refused = live.bind_voice_unless_bound(4, binder("device 'desktop'", Some(2)));
        assert_eq!(refused.err().as_deref(), Some("device 'phone'"));
        assert!(!phone.taken.is_raised(), "the holder goes on");
        // The phone's own rebind (its link dropped, its session not yet
        // told): taken over.
        let again = live
            .bind_voice_unless_bound(4, binder("device 'phone'", Some(1)))
            .expect("its own key's binding is no other session");
        assert!(phone.taken.is_raised());
        let fence = again.fence.clone().expect("the older binding's fence");
        drop(phone);
        assert!(fence.is_raised());
        // The phone's session ends (its 1001): the desktop's rebind takes it.
        again.guard.closing();
        let desktop = live
            .bind_voice_unless_bound(4, binder("device 'desktop'", Some(2)))
            .expect("an ending session holds nothing against it");
        assert!(again.taken.is_raised());
        assert_eq!(
            again.taken_by.get(),
            None,
            "it ended by itself, not by a takeover (review P-17)"
        );
        assert!(desktop.fence.is_some(), "its journal waits for the drain");
        drop(again);
        drop(desktop);
        assert_eq!(live.slots(), 0);
    }
}
