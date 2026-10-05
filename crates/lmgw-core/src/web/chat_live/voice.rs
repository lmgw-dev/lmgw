//! The thread's bound realtime session (chat-voice design §8.1): **one per
//! thread**. A second bind of the same thread takes over — the older
//! session is told through its stop and closes with a reason ("voice mode
//! moved to another window") — so two journals and two item maps never
//! write one history. Takeover rather than refusal, so a single owner can
//! move windows.
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

use std::sync::Arc;

use super::{lock, Inner, LiveTurns, Slot};
use crate::proxy::{stop_pair, StopHandle, StopSignal};

/// One bound session's hold on its thread (module doc).
pub(crate) struct VoiceBinding {
    inner: Arc<Inner>,
    thread_id: i64,
    id: u64,
    slot: Option<Arc<Slot>>,
    /// Raises this binding's fence when it goes (module doc).
    _gone: StopHandle,
}

/// What a bind hands the session (module doc).
pub(crate) struct VoiceBind {
    pub guard: VoiceBinding,
    /// Raised when another window binds the thread.
    pub taken: StopSignal,
    /// The binding this one took over, raised once it is gone; `None` when
    /// there was none.
    pub fence: Option<StopSignal>,
}

/// A binding as its thread's slot keeps it: its id, the stop that tells
/// its session another window took over, and its fence.
pub(super) type Held = (u64, StopHandle, StopSignal);

impl LiveTurns {
    /// Bind a realtime session to `thread_id` (module doc). A session
    /// already bound is taken over now.
    pub(crate) fn bind_voice(&self, thread_id: i64) -> VoiceBind {
        let slot = self.inner.slot(thread_id);
        let id = self.inner.next();
        let (handle, taken) = stop_pair();
        let (gone, fence) = stop_pair();
        let previous = lock(&slot.state).voice.replace((id, handle, fence));
        let fence = previous.map(|(_, older, fence)| {
            // Out of the lock: raising the stop wakes the older session.
            older.stop();
            fence
        });
        let guard = VoiceBinding {
            inner: self.inner.clone(),
            thread_id,
            id,
            slot: Some(slot),
            _gone: gone,
        };
        VoiceBind {
            guard,
            taken,
            fence,
        }
    }

    /// Whether a realtime session is bound to `thread_id` now.
    pub(crate) fn voice_bound(&self, thread_id: i64) -> bool {
        let slot = lock(&self.inner.threads).get(&thread_id).cloned();
        slot.is_some_and(|s| lock(&s.state).voice.is_some())
    }
}

impl Drop for VoiceBinding {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            // Only its own: a session taken over leaves the newer binding.
            let mine = {
                let mut st = lock(&slot.state);
                st.voice
                    .take_if(|(id, _, _)| *id == self.id)
                    .map(|(_, handle, _)| handle)
            };
            drop(mine);
            drop(slot);
            self.inner.release(self.thread_id);
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
        let first = live.bind_voice(3);
        assert!(first.fence.is_none(), "nothing to wait for");
        assert!(live.voice_bound(3));
        assert!(!first.taken.is_raised());
        let second = live.bind_voice(3);
        assert!(first.taken.is_raised(), "the first session is told");
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
}
