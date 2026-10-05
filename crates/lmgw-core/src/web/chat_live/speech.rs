//! The thread's speech (chat-voice design §6.4): every read-aloud running
//! for a thread registers its stop here, and `POST
//! /chat/api/threads/{id}/speech/stop` raises them — the speech ends, the
//! text of a turn read as it streams goes on.
//!
//! A registration is a guard ([`Speaking`]) the read-aloud's task holds
//! for as long as it speaks (`chat_voice::speech::start`), so `speech/stop`
//! counts the read-alouds still speaking — not one refused before it spoke,
//! or ended by a failed voice while its text goes on (WP4 review n3).
//! Dropping it drops the stop's handle, which raises it too. The page going
//! away stops the read-aloud through its reader instead (the read-aloud
//! sees it gone). It outlives the text turn's ticket on purpose: a reply is
//! still being read after its text is done.

use std::sync::Arc;

use super::{lock, Inner, LiveTurns, Slot};
use crate::proxy::{stop_pair, StopSignal};

/// One read-aloud's place in its thread's slot (module doc).
pub(crate) struct Speaking {
    inner: Arc<Inner>,
    thread_id: i64,
    id: u64,
    slot: Option<Arc<Slot>>,
}

impl LiveTurns {
    /// Register a read-aloud of `thread_id`: its guard, and the stop the
    /// speaker listens to — raised by `speech/stop` ([`Self::stop_speech`])
    /// or by dropping the guard.
    pub(crate) fn speaking(&self, thread_id: i64) -> (Speaking, StopSignal) {
        let slot = self.inner.slot(thread_id);
        let id = self.inner.next();
        let (handle, signal) = stop_pair();
        lock(&slot.state).speech.push((id, handle));
        let guard = Speaking {
            inner: self.inner.clone(),
            thread_id,
            id,
            slot: Some(slot),
        };
        (guard, signal)
    }

    /// Stop every read-aloud of `thread_id`; how many were running.
    pub(crate) fn stop_speech(&self, thread_id: i64) -> usize {
        let slot = lock(&self.inner.threads).get(&thread_id).cloned();
        let Some(slot) = slot else {
            return 0;
        };
        let stopped = {
            let st = lock(&slot.state);
            for (_, handle) in &st.speech {
                handle.stop();
            }
            st.speech.len()
        };
        // A read-aloud's last guard that dropped while this held the slot
        // left it in the map (review n2): let it go now if it is idle.
        drop(slot);
        self.inner.release(thread_id);
        stopped
    }
}

impl Drop for Speaking {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            // Out of the lock before it drops: dropping raises the stop.
            let handle = {
                let mut st = lock(&slot.state);
                let at = st.speech.iter().position(|(id, _)| *id == self.id);
                at.map(|at| st.speech.swap_remove(at))
            };
            drop(handle);
            drop(slot);
            self.inner.release(self.thread_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_stop_reaches_every_speech_of_the_thread_and_no_other() {
        let live = LiveTurns::default();
        let (a, sa) = live.speaking(4);
        let (_b, sb) = live.speaking(4);
        let (_c, sc) = live.speaking(5);
        assert_eq!(live.stop_speech(4), 2);
        assert!(sa.is_raised() && sb.is_raised());
        assert!(!sc.is_raised(), "another thread's speech goes on");
        assert_eq!(live.stop_speech(9), 0, "nothing runs there");
        drop(a);
        assert_eq!(live.stop_speech(4), 1, "a speech that ended is gone");
    }

    #[tokio::test]
    async fn dropping_the_guard_stops_the_speech_and_frees_the_slot() {
        let live = LiveTurns::default();
        let (guard, signal) = live.speaking(3);
        assert!(!signal.is_raised());
        // A turn of the thread coming and going leaves the speech alone.
        let t = live.begin(3).await;
        drop(t);
        assert!(!signal.is_raised());
        assert_eq!(live.slots(), 1, "the speech keeps the slot");
        drop(guard);
        assert!(signal.is_raised(), "the page went: the speech stops");
        assert_eq!(live.slots(), 0);
    }
}
