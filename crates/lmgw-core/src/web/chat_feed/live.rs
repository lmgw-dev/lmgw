//! The feed's live half (client-apps design §2.2, §2.3): `turn.*`,
//! `voice.*` and `hold`, which are never stored — after a restart nothing
//! claims a turn is still running — and the live state `hello` and `state`
//! carry.
//!
//! One lock orders everything: an event is published, and the registry it
//! changes is updated, under the same lock a subscriber takes to subscribe
//! and read the registry. So a subscriber's `hello` and the events after it
//! never disagree: a turn is either in `hello.turns` or its `turn.started`
//! follows, never both missing, and a `turn.done` never precedes the turn's
//! start.
//!
//! The broadcast holds `chat_feed_live_buffer` events for a subscriber that
//! reads slower than they happen. One that falls further behind is told
//! (`broadcast::error::RecvError::Lagged`) and sends its client a fresh
//! `state` instead: never a silent drop. A change of the setting replaces
//! the channel; the old one closes once drained, and its subscribers move
//! to the new one with a `state` too.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use lmgw_api_types::chat_feed::{
    self as dto, event, FeedHold, LiveState, LiveTurn, LiveVoice, TurnDone, TurnStarted,
    VoiceBound, VoiceEnded,
};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::config::SelfAdmin;
use crate::store::AdminThreads;

/// One live event as the broadcast carries it.
#[derive(Debug, Clone)]
pub(crate) struct Live {
    pub event: &'static str,
    pub data: Value,
    /// The level of the thread it is about (`ChatThread::reach_level`): a
    /// reader that does not see it never receives it (L3).
    pub level: u8,
}

impl Live {
    /// A thread's reach changed for devices (review W4-8): each device's
    /// stream answers it with a fresh `state` of its own, built when it
    /// reads it; the owner's ignores it, nothing changed for them.
    pub(crate) fn devices_refresh() -> Self {
        Self {
            event: event::STATE,
            data: Value::Null,
            level: 0,
        }
    }

    /// Whether it is [`Self::devices_refresh`].
    pub(crate) fn is_devices_refresh(&self) -> bool {
        self.event == event::STATE && self.data.is_null()
    }
}

/// What `hello` and `state` say is live now, as one subscriber may see it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Now {
    pub hold: FeedHold,
    pub voice: Vec<LiveVoice>,
    pub turns: Vec<LiveTurn>,
}

impl Now {
    /// As `state` says it, to a reader whose admin tools may do
    /// `self_admin` now — its own level capped by the gateway's (`off` for
    /// one that is no device).
    pub(crate) fn state(self, self_admin: SelfAdmin, reason: Option<String>) -> LiveState {
        LiveState {
            self_admin: self_admin.into(),
            hold: self.hold,
            voice: self.voice,
            turns: self.turns,
            reason,
        }
    }
}

/// The live half: cheap to clone, one per gateway (`AppState::chat_feed`),
/// shared with the live turns (`web::chat_live`), which register turns and
/// bound sessions in it.
#[derive(Clone)]
pub(crate) struct LiveFeed(Arc<Mutex<Inner>>);

struct Inner {
    tx: broadcast::Sender<Live>,
    capacity: usize,
    next: u64,
    hold: FeedHold,
    turns: BTreeMap<u64, TurnEntry>,
    voices: BTreeMap<u64, VoiceEntry>,
    /// The threads that came to carry the self-admin toolset (review W4-3):
    /// a turn or a session registered from a read made before the flip is
    /// reported at the thread's level now (`1`), never to a device that does
    /// not see it. Kept while the toolset is attached: a thread flipped
    /// back, deleted, discarded or kept leaves it (review W5-8), so the set
    /// is the threads with the toolset attached, not every thread that ever
    /// had it. A registration from a read made before a flip back stays at
    /// level 1 for its life: the safe side.
    flipped: BTreeSet<i64>,
    /// The threads being deleted, or deleted ([`LiveFeed::thread_going`]),
    /// as `flipped` for an attach: a turn or a session registered from a
    /// read made before the delete — a bind, a turn waiting for the
    /// thread's lock — is reported to the owner alone too. Thread ids are
    /// never reused (AUTOINCREMENT), so an id kept a while hides nothing
    /// new.
    going: BTreeMap<i64, Going>,
}

/// A thread in `going`. Two deletes of one thread may run at once — its
/// own and its folder's, each marking it before it waits for its lock —
/// and one that fails must not undo the other's mark (the branch review's
/// N-2): the mark goes only when no delete of it is under way and none
/// committed.
#[derive(Debug, Default)]
struct Going {
    /// The deletes that marked it and have neither committed nor failed.
    pending: u32,
    /// A delete of it committed ([`LiveFeed::thread_gone`]): marked for
    /// good, whatever another delete of it does.
    deleted: bool,
    /// A prune found it ([`LiveFeed::forget_gone`]).
    seen: bool,
}

struct TurnEntry {
    turn: LiveTurn,
    /// The thread's level: a reader that does not see it never hears of it.
    level: u8,
    /// Its thread is being deleted ([`LiveFeed::thread_going`]).
    gone: bool,
}

struct VoiceEntry {
    voice: LiveVoice,
    level: u8,
    gone: bool,
    /// Where its end is said, shared with its watch: a session bound while
    /// its thread was being deleted says `thread_gone`, taken back when the
    /// delete fails ([`LiveFeed::thread_going`]).
    ending: Ending,
}

/// The level a deleted thread's live turns and sessions are reported at
/// ([`LiveFeed::thread_going`]): Admin Chat's, the owner's alone.
const GONE: u8 = 2;

impl TurnEntry {
    /// The level it is reported at: [`GONE`] once its thread is being
    /// deleted.
    fn seen_at(&self) -> u8 {
        if self.gone {
            GONE
        } else {
            self.level
        }
    }
}

impl VoiceEntry {
    /// As [`TurnEntry::seen_at`].
    fn seen_at(&self) -> u8 {
        if self.gone {
            GONE
        } else {
            self.level
        }
    }
}

impl Default for LiveFeed {
    /// A feed for a `LiveTurns` built on its own (its unit tests): the
    /// default buffer, the hold off.
    fn default() -> Self {
        Self::new(256, FeedHold::default())
    }
}

impl LiveFeed {
    /// `capacity` is `chat_feed_live_buffer` (at least 1), `hold` the
    /// published snapshot's.
    pub(crate) fn new(capacity: u32, hold: FeedHold) -> Self {
        let capacity = (capacity as usize).max(1);
        let (tx, _) = broadcast::channel(capacity);
        Self(Arc::new(Mutex::new(Inner {
            tx,
            capacity,
            next: 0,
            hold,
            turns: BTreeMap::new(),
            voices: BTreeMap::new(),
            flipped: BTreeSet::new(),
            going: BTreeMap::new(),
        })))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Nothing here panics while holding the lock; a poisoned one still
        // holds a consistent registry.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Subscribe, and what is live at that moment as a reader that reaches
    /// as far as `reach` may see it (L3) — under one lock, so nothing
    /// published falls between the two.
    pub(crate) fn subscribe(&self, reach: AdminThreads) -> (broadcast::Receiver<Live>, Now) {
        let inner = self.lock();
        (inner.tx.subscribe(), inner.now(reach))
    }

    /// What is live now, for a reader that reaches as far as `reach`.
    pub(crate) fn now(&self, reach: AdminThreads) -> Now {
        self.lock().now(reach)
    }

    /// The live buffer's size now (`chat_feed_live_buffer`).
    pub(crate) fn capacity(&self) -> usize {
        self.lock().capacity
    }

    /// A published snapshot's settings: the hold, when it differs from the
    /// one before (§2.2, R17: a fallback changed through the settings patch
    /// is a change too), and the live buffer, replaced when its size moved.
    #[cfg(test)]
    pub(crate) fn published(&self, hold: FeedHold, capacity: u32) {
        self.published_from(|| (hold, capacity));
    }

    /// [`Self::published`] with the settings read under this feed's lock
    /// (review W4-23): two publishes in a row both read the snapshot
    /// published last, so the older one can never land after the newer.
    pub(crate) fn published_from(&self, read: impl FnOnce() -> (FeedHold, u32)) {
        let mut inner = self.lock();
        let (hold, capacity) = read();
        let capacity = (capacity as usize).max(1);
        if capacity != inner.capacity {
            // The old channel's subscribers read what it holds, then see it
            // closed and move to this one (`stream`).
            inner.tx = broadcast::channel(capacity).0;
            inner.capacity = capacity;
        }
        if hold != inner.hold {
            inner.hold = hold.clone();
            inner.publish(event::HOLD, json(&hold), 0);
        }
    }

    /// A turn of `thread_id` (at `level`, `ChatThread::reach_level`)
    /// started, by `by` (a principal's description, §1.7), as a bound
    /// session's voice turn or not. Its watch publishes `turn.done` when it
    /// drops. A temporary thread is never in the feed (L7): its watch is
    /// inert.
    pub(crate) fn turn_started(
        &self,
        thread_id: i64,
        level: u8,
        by: String,
        voice: bool,
    ) -> TurnWatch {
        if thread_id < 0 {
            return TurnWatch::inert();
        }
        let mut inner = self.lock();
        let level = inner.level_now(thread_id, level);
        inner.next += 1;
        let id = inner.next;
        let turn = LiveTurn {
            thread_id,
            by,
            voice,
        };
        let data = json(&TurnStarted {
            thread_id,
            by: turn.by.clone(),
            voice,
        });
        let gone = inner.going.contains_key(&thread_id);
        inner.turns.insert(id, TurnEntry { turn, level, gone });
        inner.publish(event::TURN_STARTED, data, if gone { GONE } else { level });
        TurnWatch {
            feed: Some(self.clone()),
            id,
            thread_id,
            level,
            outcome: Arc::default(),
        }
    }

    /// A realtime session bound `thread_id` (at `level`) for `by`: the
    /// owner, or a device allowed lmgw's admin tools, may bind one with the
    /// self-admin toolset; an Admin Chat thread never binds. Its watch
    /// publishes `voice.ended` when it drops. Inert for a temporary thread
    /// (L7).
    pub(crate) fn voice_bound(&self, thread_id: i64, by: &str, level: u8) -> VoiceWatch {
        if thread_id < 0 {
            return VoiceWatch::inert();
        }
        let mut inner = self.lock();
        let level = inner.level_now(thread_id, level);
        inner.next += 1;
        let id = inner.next;
        let voice = LiveVoice {
            thread_id,
            by: by.to_string(),
        };
        let data = json(&VoiceBound {
            thread_id,
            by: by.to_string(),
        });
        let gone = inner.going.contains_key(&thread_id);
        // Bound to a thread being deleted: its end says the thread went,
        // whatever closes it (a delete that fails takes it back,
        // [`Self::thread_going`]).
        let ending = Ending::default();
        if gone {
            ending.set(VoiceEnd::ThreadGone);
        }
        inner.voices.insert(
            id,
            VoiceEntry {
                voice,
                level,
                gone,
                ending: ending.clone(),
            },
        );
        inner.publish(event::VOICE_BOUND, data, if gone { GONE } else { level });
        VoiceWatch {
            feed: Some(self.clone()),
            id,
            thread_id,
            by: by.to_string(),
            ending,
        }
    }
}

impl LiveFeed {
    /// Thread `thread_id`'s level is now `level` (client-apps design L3,
    /// review W3-1): the self-admin toolset attached (`1`) or taken off
    /// (`0`); an Admin Chat thread's `2` never changes. Its live turns and
    /// bound sessions are reported to the readers that see that level from
    /// now on. When one of them changed level, every device's stream is told
    /// to send a fresh `state` (review W4-8): a turn that left its view has
    /// no `turn.done` for it, and one that came into it no `turn.started`.
    pub(crate) fn thread_self_admin(&self, thread_id: i64, level: u8) {
        let mut inner = self.lock();
        if level > 0 {
            inner.flipped.insert(thread_id);
        } else {
            inner.flipped.remove(&thread_id);
        }
        let mut moved = false;
        for t in inner.turns.values_mut() {
            if t.turn.thread_id == thread_id && t.level != level && t.level != 2 {
                t.level = level;
                moved = true;
            }
        }
        for v in inner.voices.values_mut() {
            if v.voice.thread_id == thread_id && v.level != level && v.level != 2 {
                v.level = level;
                moved = true;
            }
        }
        if moved {
            let _ = inner.tx.send(Live::devices_refresh());
        }
    }

    /// Thread `thread_id` is being deleted (`LiveTurns::discard`, the
    /// sweep's purge): its live turns and bound sessions are reported to the
    /// owner alone from now, their `turn.done` and `voice.ended` too, so a
    /// device hears of a deleted thread what it hears of one hidden from it
    /// (review W4-18; §1.6's close-code note, 2026-10-07) — the record's
    /// `thread.deleted`, and a fresh `state` without them when one was live,
    /// as an attach of the toolset sends it ([`Self::thread_self_admin`]).
    /// A turn or a session registered on it from now is reported so too
    /// (`going`). `going: false` is a delete of it that failed: the thread
    /// is the devices' again once no other delete of it is under way and
    /// none committed ([`Going`]), and a session bound meanwhile no longer
    /// says its thread went. A temporary thread has no live events (L7).
    pub(crate) fn thread_going(&self, thread_id: i64, going: bool) {
        if thread_id < 0 {
            return;
        }
        let mut inner = self.lock();
        let marked = if going {
            inner.going.entry(thread_id).or_default().pending += 1;
            true
        } else {
            let Some(mark) = inner.going.get_mut(&thread_id) else {
                return;
            };
            mark.pending = mark.pending.saturating_sub(1);
            if mark.pending > 0 || mark.deleted {
                return;
            }
            inner.going.remove(&thread_id);
            false
        };
        inner.mark_gone(thread_id, marked);
    }

    /// Thread `thread_id`'s delete committed (`LiveTurns::discarded`): the
    /// delete that marked it ([`Self::thread_going`]) is done, and the
    /// thread stays marked for good — a delete of it that fails after this
    /// one, or failed while this one waited for the thread's lock, takes
    /// nothing back (the branch review's N-2).
    pub(crate) fn thread_gone(&self, thread_id: i64) {
        if thread_id < 0 {
            return;
        }
        let mut inner = self.lock();
        let mark = inner.going.entry(thread_id).or_default();
        mark.pending = mark.pending.saturating_sub(1);
        mark.deleted = true;
        inner.mark_gone(thread_id, true);
    }

    /// A device's admin-tools switch changed: every device's stream sends a
    /// fresh `state` (the one whose switch it was sees more, or less, of
    /// what is live; another's sees the same).
    pub(crate) fn devices_refresh(&self) {
        let _ = self.lock().tx.send(Live::devices_refresh());
    }
}

impl LiveFeed {
    /// Thread `thread_id` is gone (deleted, discarded, or a temporary one
    /// kept under a new id): nothing will register for it again.
    pub(crate) fn forget_thread(&self, thread_id: i64) {
        self.lock().flipped.remove(&thread_id);
    }

    /// Forget the deleted threads no turn or session refers to that the
    /// prune before this one saw already (the hourly upkeep,
    /// `server::chat_upkeep`): a registration from a read made before a
    /// delete comes within moments of it, never a prune later.
    pub(crate) fn forget_gone(&self) {
        let mut inner = self.lock();
        let live: BTreeSet<i64> = inner
            .turns
            .values()
            .map(|t| t.turn.thread_id)
            .chain(inner.voices.values().map(|v| v.voice.thread_id))
            .collect();
        inner.going.retain(|id, mark| {
            mark.pending > 0 || live.contains(id) || !std::mem::replace(&mut mark.seen, true)
        });
    }
}

impl Inner {
    fn publish(&self, event: &'static str, data: Value, level: u8) {
        // No subscriber is no error: nobody is listening.
        let _ = self.tx.send(Live { event, data, level });
    }

    /// Thread `thread_id`'s live turns and sessions are reported to the
    /// owner alone (`gone`), or at their level again; every device's stream
    /// sends a fresh `state` when one moved. Taken back, a session's end no
    /// longer says its thread went: only a delete under way said so then
    /// (`LiveFeed::voice_bound`), whichever binding holds the thread now.
    fn mark_gone(&mut self, thread_id: i64, gone: bool) {
        let mut moved = false;
        for t in self.turns.values_mut() {
            if t.turn.thread_id == thread_id && t.gone != gone {
                let was = t.seen_at();
                t.gone = gone;
                moved |= t.seen_at() != was;
            }
        }
        for v in self.voices.values_mut() {
            if v.voice.thread_id != thread_id {
                continue;
            }
            if !gone {
                v.ending.unset(VoiceEnd::ThreadGone);
            }
            if v.gone != gone {
                let was = v.seen_at();
                v.gone = gone;
                moved |= v.seen_at() != was;
            }
        }
        if moved {
            let _ = self.tx.send(Live::devices_refresh());
        }
    }

    /// `level` as read, or `1` when the toolset was attached since
    /// (`flipped`).
    fn level_now(&self, thread_id: i64, level: u8) -> u8 {
        if self.flipped.contains(&thread_id) {
            level.max(1)
        } else {
            level
        }
    }

    fn now(&self, reach: AdminThreads) -> Now {
        Now {
            hold: self.hold.clone(),
            voice: self
                .voices
                .values()
                .filter(|v| reach.sees(v.seen_at()))
                .map(|v| v.voice.clone())
                .collect(),
            turns: self
                .turns
                .values()
                .filter(|t| reach.sees(t.seen_at()))
                .map(|t| t.turn.clone())
                .collect(),
        }
    }
}

fn json<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// How a turn ended, as its frames and its save said it.
#[derive(Debug, Default)]
struct Outcome {
    message_id: Option<i64>,
    saved: bool,
    code: Option<String>,
}

/// One running turn in the feed: registered while it lives, `turn.done`
/// when it drops. Held by the turn itself (`chat_turn::Turn`), so the turn's
/// end is the worker's end, whatever path it took.
pub(crate) struct TurnWatch {
    feed: Option<LiveFeed>,
    id: u64,
    thread_id: i64,
    level: u8,
    outcome: Arc<Mutex<Outcome>>,
}

impl TurnWatch {
    /// A watch that publishes nothing (a temporary thread's turn).
    pub(crate) fn inert() -> Self {
        Self {
            feed: None,
            id: 0,
            thread_id: 0,
            level: 0,
            outcome: Arc::default(),
        }
    }

    /// What the turn's frames tell this watch through (`chat_turn::Events`).
    pub(crate) fn observer(&self) -> TurnObserver {
        TurnObserver(self.feed.is_some().then(|| self.outcome.clone()))
    }

    /// The turn's save: the row it saved or continued (`0`: none).
    pub(crate) fn persisted(&self, message_id: i64) {
        let mut o = lock(&self.outcome);
        o.saved = message_id != 0;
        o.message_id = (message_id != 0).then_some(message_id);
    }
}

impl Drop for TurnWatch {
    fn drop(&mut self) {
        let Some(feed) = self.feed.take() else {
            return;
        };
        let done = {
            let o = lock(&self.outcome);
            TurnDone {
                thread_id: self.thread_id,
                message_id: o.message_id,
                saved: o.saved,
                code: o.code.clone(),
            }
        };
        let mut inner = feed.lock();
        // The thread's level now: the toolset may have been attached while
        // the turn ran.
        let level = inner
            .turns
            .remove(&self.id)
            .map_or(self.level, |t| t.seen_at());
        inner.publish(event::TURN_DONE, json(&done), level);
    }
}

/// A turn's frames as its watch reads them: the code of the last `error`
/// frame that carried one is `turn.done`'s `code`.
#[derive(Clone, Default)]
pub(crate) struct TurnObserver(Option<Arc<Mutex<Outcome>>>);

impl TurnObserver {
    /// A frame the turn says, `event` with its JSON `data`.
    pub(crate) fn frame(&self, event: &str, data: &str) {
        let Some(outcome) = &self.0 else {
            return;
        };
        if event != "error" {
            return;
        }
        let code = serde_json::from_str::<Value>(data)
            .ok()
            .and_then(|v| v.get("code").and_then(Value::as_str).map(str::to_string));
        if let Some(code) = code {
            lock(outcome).code = Some(code);
        }
    }
}

/// Why a bound session let its thread go (§2.2 `voice.ended`), when it was
/// not a takeover: said by whoever knew before the binding dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoiceEnd {
    /// The session's key was revoked (§1.6).
    Revoked,
    /// The thread was deleted while it was bound.
    ThreadGone,
    /// The thread left the binding device's reach: the self-admin toolset
    /// was attached to it (L3, review W3-1), or the device's admin-tools
    /// switch was turned off.
    OutOfReach,
}

impl VoiceEnd {
    fn as_str(self) -> &'static str {
        match self {
            // For the device, the thread is gone; nobody else hears of it.
            Self::Revoked | Self::OutOfReach => "revoked",
            Self::ThreadGone => "thread_gone",
        }
    }
}

/// Where a bound session's end is said before it drops: shared by the
/// binding's watch and the thread's slot (`chat_live::voice`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Ending(Arc<Mutex<Option<VoiceEnd>>>);

impl Ending {
    /// The first reason said wins: a revoked session whose thread is
    /// deleted while it closes ended by the revocation.
    pub(crate) fn set(&self, why: VoiceEnd) {
        lock(&self.0).get_or_insert(why);
    }

    pub(crate) fn get(&self) -> Option<VoiceEnd> {
        *lock(&self.0)
    }

    /// Take back `why` when it is the reason said (a delete that failed).
    pub(crate) fn unset(&self, why: VoiceEnd) {
        let mut end = lock(&self.0);
        if *end == Some(why) {
            *end = None;
        }
    }
}

/// One bound session in the feed: registered while it lives, `voice.ended`
/// when it drops.
pub(crate) struct VoiceWatch {
    feed: Option<LiveFeed>,
    id: u64,
    thread_id: i64,
    by: String,
    ending: Ending,
}

impl VoiceWatch {
    pub(crate) fn inert() -> Self {
        Self {
            feed: None,
            id: 0,
            thread_id: 0,
            by: String::new(),
            ending: Ending::default(),
        }
    }

    /// Where its end is said ([`Ending`]).
    pub(crate) fn ending(&self) -> Ending {
        self.ending.clone()
    }

    /// End it now, `taken_over_by` the binder that took the thread over
    /// (`None`: it was not taken over).
    pub(crate) fn end(mut self, taken_over_by: Option<String>) {
        self.finish(taken_over_by);
    }

    fn finish(&mut self, taken_over_by: Option<String>) {
        let Some(feed) = self.feed.take() else {
            return;
        };
        let reason = match (&taken_over_by, self.ending.get()) {
            (_, Some(why)) => why.as_str(),
            (Some(_), None) => "taken_over",
            (None, None) => "closed",
        };
        let ended = VoiceEnded {
            thread_id: self.thread_id,
            by: self.by.clone(),
            reason: reason.to_string(),
            taken_over_by,
        };
        let mut inner = feed.lock();
        let level = inner.voices.remove(&self.id).map_or(0, |v| v.seen_at());
        inner.publish(event::VOICE_ENDED, json(&ended), level);
    }
}

impl Drop for VoiceWatch {
    fn drop(&mut self) {
        self.finish(None);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The hold as the feed says it (`hello`, `state`, `hold`).
pub(crate) fn hold_of(h: &crate::config::HoldSettings) -> FeedHold {
    dto::FeedHold {
        active: h.active,
        fallback_alias: h.fallback_alias.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &mut broadcast::Receiver<Live>) -> Vec<(&'static str, Value, u8)> {
        let mut out = Vec::new();
        while let Ok(l) = rx.try_recv() {
            out.push((l.event, l.data, l.level));
        }
        out
    }

    #[test]
    fn a_turn_is_live_from_its_start_to_its_watch_s_drop() {
        let feed = LiveFeed::default();
        let (mut rx, now) = feed.subscribe(AdminThreads::Shown);
        assert!(now.turns.is_empty());
        let watch = feed.turn_started(7, 0, "device 'phone'".into(), false);
        watch
            .observer()
            .frame("error", r#"{"message":"m","code":"gpu_hold"}"#);
        watch
            .observer()
            .frame("error", r#"{"message":"a tool label"}"#);
        watch.persisted(0);
        assert_eq!(feed.now(AdminThreads::Shown).turns.len(), 1);
        drop(watch);
        assert!(feed.now(AdminThreads::Shown).turns.is_empty());
        let got = drain(&mut rx);
        assert_eq!(got[0].0, event::TURN_STARTED);
        assert_eq!(
            got[1],
            (
                event::TURN_DONE,
                serde_json::json!({"thread_id": 7, "message_id": null, "saved": false, "code": "gpu_hold"}),
                0
            )
        );
    }

    /// §1.6's close-code note, 2026-10-07 (the branch review's R-2): a
    /// thread being deleted reports its live turns and sessions to the
    /// owner alone, those registered after the mark too, and a session's
    /// end says `thread_gone`; a delete that fails takes it back, and the
    /// mark is forgotten at the second prune that finds it unused.
    #[test]
    fn a_thread_being_deleted_is_the_owner_s_alone_late_registrations_too() {
        let feed = LiveFeed::default();
        let (mut rx, _) = feed.subscribe(AdminThreads::Hidden);
        let running = feed.turn_started(5, 0, "device 'phone'".into(), false);
        drain(&mut rx);
        feed.thread_going(5, true);
        let got: Vec<Live> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(got.len() == 1 && got[0].is_devices_refresh(), "{got:?}");
        assert!(feed.now(AdminThreads::Hidden).turns.is_empty());
        assert_eq!(feed.now(AdminThreads::Shown).turns.len(), 1);
        // Registered from a read made before the delete.
        let late = feed.turn_started(5, 0, "device 'phone'".into(), true);
        let bound = feed.voice_bound(5, "device 'phone'", 0);
        assert_eq!(bound.ending().get(), Some(VoiceEnd::ThreadGone));
        assert!(feed.now(AdminThreads::Hidden).voice.is_empty());
        drop((running, late, bound));
        let got = drain(&mut rx);
        assert!(!got.is_empty());
        assert!(got.iter().all(|(_, _, level)| *level == GONE), "{got:?}");
        let ended = got
            .iter()
            .find(|(e, _, _)| *e == event::VOICE_ENDED)
            .unwrap();
        assert_eq!(ended.1["reason"], "thread_gone");

        // A delete that fails: what registers next is the devices' again.
        feed.thread_going(5, false);
        let again = feed.voice_bound(5, "device 'phone'", 0);
        assert_eq!(again.ending().get(), None);
        assert_eq!(feed.now(AdminThreads::Hidden).voice.len(), 1);
        drop(again);

        // Forgotten at the second prune that finds no entry of it, once its
        // delete is done.
        feed.thread_going(9, true);
        feed.forget_gone();
        feed.forget_gone();
        assert!(feed.lock().going.contains_key(&9), "a delete under way");
        feed.thread_gone(9);
        feed.forget_gone();
        assert!(feed.lock().going.contains_key(&9));
        feed.forget_gone();
        assert!(!feed.lock().going.contains_key(&9));
    }

    /// The branch review's N-2: a thread two deletes mark stays the owner's
    /// alone while either is under way, and for good once one committed —
    /// the other failing takes nothing back, before or after it. Only when
    /// every delete of it failed is it the devices' again, and then every
    /// session bound meanwhile, a binding taken over among them, no longer
    /// says its thread went.
    #[test]
    fn a_failed_delete_never_undoes_another_delete_s_mark() {
        let feed = LiveFeed::default();
        let devices = |feed: &LiveFeed| feed.now(AdminThreads::Hidden).voice.len();
        let _bound = feed.voice_bound(5, "device 'phone'", 0);

        // The thread's own delete and its folder's, the first failing.
        feed.thread_going(5, true);
        feed.thread_going(5, true);
        feed.thread_going(5, false);
        assert_eq!(devices(&feed), 0, "the other delete is under way");
        feed.thread_gone(5);
        assert_eq!(devices(&feed), 0);
        // One committed, then the other found nothing to take.
        feed.thread_going(5, true);
        feed.thread_going(5, true);
        feed.thread_gone(5);
        feed.thread_going(5, false);
        assert_eq!(devices(&feed), 0, "a failed delete after a committed one");
        let late = feed.voice_bound(5, "device 'phone'", 0);
        assert_eq!(late.ending().get(), Some(VoiceEnd::ThreadGone));

        // Every delete of thread 6 fails: a session bound meanwhile, and the
        // one that took it over, end as sessions do.
        feed.thread_going(6, true);
        feed.thread_going(6, true);
        let first = feed.voice_bound(6, "device 'phone'", 0);
        let second = feed.voice_bound(6, "the dashboard", 0);
        assert_eq!(first.ending().get(), Some(VoiceEnd::ThreadGone));
        feed.thread_going(6, false);
        assert_eq!(second.ending().get(), Some(VoiceEnd::ThreadGone));
        feed.thread_going(6, false);
        assert_eq!(first.ending().get(), None, "taken over meanwhile");
        assert_eq!(second.ending().get(), None);
        assert_eq!(
            feed.now(AdminThreads::Hidden)
                .voice
                .iter()
                .filter(|v| v.thread_id == 6)
                .count(),
            2
        );
    }

    #[test]
    fn an_admin_turn_is_marked_and_left_out_of_a_device_s_now() {
        let feed = LiveFeed::default();
        let _w = feed.turn_started(3, 2, "the dashboard".into(), false);
        assert_eq!(feed.now(AdminThreads::Shown).turns.len(), 1);
        assert!(feed.now(AdminThreads::Hidden).turns.is_empty());
        let (mut rx, _) = feed.subscribe(AdminThreads::Hidden);
        drop(_w);
        assert!(drain(&mut rx).iter().all(|(_, _, level)| *level == 2));
    }

    /// Reviews W4-3, W4-8: a flip moves what is live between the sides and
    /// tells devices to refresh; a turn or a session registered from a read
    /// made before the flip takes the thread's latest flag.
    #[test]
    fn a_flip_moves_live_entries_tells_devices_and_wins_over_a_stale_read() {
        let feed = LiveFeed::default();
        let (mut rx, _) = feed.subscribe(AdminThreads::Hidden);
        let running = feed.turn_started(7, 0, "the dashboard".into(), false);
        assert_eq!(feed.now(AdminThreads::Hidden).turns.len(), 1);
        drain(&mut rx);

        feed.thread_self_admin(7, 1);
        assert!(
            feed.now(AdminThreads::Hidden).turns.is_empty(),
            "out of a device's view"
        );
        assert_eq!(
            feed.now(AdminThreads::Shown).turns.len(),
            1,
            "the owner's stays"
        );
        let got: Vec<Live> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(got.len(), 1);
        assert!(got[0].is_devices_refresh());
        // No change, no refresh.
        feed.thread_self_admin(7, 1);
        assert!(rx.try_recv().is_err());

        // Registered from a read that still said "plain": at the toolset's
        // level all the same.
        let late = feed.turn_started(7, 0, "the dashboard".into(), false);
        let bound = feed.voice_bound(7, "the dashboard", 0);
        assert!(
            feed.now(AdminThreads::Hidden).turns.is_empty()
                && feed.now(AdminThreads::Hidden).voice.is_empty()
        );
        drop((late, bound));
        let ends = drain(&mut rx);
        assert!(ends.iter().all(|(_, _, level)| *level == 1), "{ends:?}");

        // Back: in view again, and devices refresh once more.
        feed.thread_self_admin(7, 0);
        assert_eq!(feed.now(AdminThreads::Hidden).turns.len(), 1);
        assert!(rx.try_recv().unwrap().is_devices_refresh());
        drop(running);
        let done = drain(&mut rx);
        assert_eq!((done[0].0, done[0].2), (event::TURN_DONE, 0));
    }

    /// The per-device switch (2026-10-07): a device allowed lmgw's admin
    /// tools sees what is live in a thread with the toolset, never in an
    /// Admin Chat thread.
    #[test]
    fn a_device_allowed_the_admin_tools_sees_the_toolset_s_threads_live() {
        let feed = LiveFeed::default();
        let tools = feed.turn_started(7, 1, "the dashboard".into(), false);
        let admin = feed.turn_started(8, 2, "the dashboard".into(), false);
        let ids =
            |reach| -> Vec<i64> { feed.now(reach).turns.iter().map(|t| t.thread_id).collect() };
        assert_eq!(ids(AdminThreads::Shown), [7, 8]);
        assert_eq!(ids(AdminThreads::ToolsShown), [7]);
        assert!(ids(AdminThreads::Hidden).is_empty());
        drop((tools, admin));
    }

    /// Review W5-8: a thread leaves the flipped set when it is flipped back
    /// or goes, so the set holds the threads with the toolset attached, not
    /// every thread that ever had it.
    #[test]
    fn the_flipped_set_holds_only_threads_that_drive_the_plane_now() {
        let feed = LiveFeed::default();
        feed.thread_self_admin(7, 1);
        feed.thread_self_admin(8, 1);
        assert_eq!(feed.lock().flipped.len(), 2);
        feed.thread_self_admin(7, 0);
        feed.forget_thread(8);
        assert!(feed.lock().flipped.is_empty());
        // Gone from the set, a fresh read decides again.
        drop(feed.turn_started(7, 0, "the dashboard".into(), false));
        assert!(
            feed.now(AdminThreads::Hidden).turns.is_empty(),
            "the watch ended it"
        );
    }

    #[test]
    fn a_temporary_thread_is_never_live_in_the_feed() {
        let feed = LiveFeed::default();
        let (mut rx, _) = feed.subscribe(AdminThreads::Shown);
        drop(feed.turn_started(-2, 0, "the dashboard".into(), false));
        drop(feed.voice_bound(-2, "the dashboard", 0));
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_binding_says_why_it_ended() {
        let feed = LiveFeed::default();
        let (mut rx, _) = feed.subscribe(AdminThreads::Shown);
        let a = feed.voice_bound(5, "the dashboard", 0);
        assert_eq!(feed.now(AdminThreads::Shown).voice.len(), 1);
        a.end(Some("device 'phone'".into()));
        let b = feed.voice_bound(5, "device 'phone'", 0);
        b.ending().set(VoiceEnd::Revoked);
        b.ending().set(VoiceEnd::ThreadGone);
        drop(b);
        let c = feed.voice_bound(5, "device 'phone'", 0);
        drop(c);
        let ended: Vec<Value> = drain(&mut rx)
            .into_iter()
            .filter(|(e, _, _)| *e == event::VOICE_ENDED)
            .map(|(_, d, _)| d)
            .collect();
        assert_eq!(ended[0]["reason"], "taken_over");
        assert_eq!(ended[0]["by"], "the dashboard");
        assert_eq!(ended[0]["taken_over_by"], "device 'phone'");
        assert_eq!(ended[1]["reason"], "revoked", "the first reason said wins");
        assert_eq!(ended[2]["reason"], "closed");
        assert!(feed.now(AdminThreads::Shown).voice.is_empty());
    }

    #[test]
    fn the_hold_is_published_only_when_it_changes_and_a_resize_closes_the_old_channel() {
        let feed = LiveFeed::new(4, FeedHold::default());
        let (mut rx, _) = feed.subscribe(AdminThreads::Shown);
        feed.published(FeedHold::default(), 4);
        assert!(drain(&mut rx).is_empty());
        let held = FeedHold {
            active: false,
            fallback_alias: Some("cloud".into()),
        };
        feed.published(held.clone(), 4);
        assert_eq!(drain(&mut rx)[0].0, event::HOLD);
        assert_eq!(feed.now(AdminThreads::Shown).hold, held);
        feed.published(held, 8);
        assert_eq!(feed.capacity(), 8);
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Closed)
        ));
    }

    #[test]
    fn a_slow_subscriber_is_told_it_lagged() {
        let feed = LiveFeed::new(2, FeedHold::default());
        let (mut rx, _) = feed.subscribe(AdminThreads::Shown);
        for t in 1..=4 {
            drop(feed.turn_started(t, 0, "the dashboard".into(), false));
        }
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
    }
}
