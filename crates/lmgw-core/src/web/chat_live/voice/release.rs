//! A thread's release signal (realtime calls design §2.8): what a bind that
//! waits for its thread (`takeover=ask`) wakes on.
//!
//! The slot carries a counter ([`Slot::voice_changed`]) that moves each time
//! its voice binding changes: a bind registers (a new holder), the holder's
//! session starts to end (`VoiceBinding::closing`, after which no
//! `takeover=never` bind counts it), or its binding goes. A waiter
//! subscribes under the slot's lock, with its look at the binding, and
//! holds the slot until it drops the signal, so the counter it watches
//! lives as long as it does.

use std::sync::Arc;

use tokio::sync::watch;

use super::super::{Inner, Slot};

/// A thread's release signal, held by a bind that waits for the thread
/// (module doc).
pub(crate) struct VoiceRelease {
    inner: Arc<Inner>,
    thread_id: i64,
    /// Keeps the slot, and with it the counter, until the signal drops.
    slot: Option<Arc<Slot>>,
    rx: watch::Receiver<u64>,
}

impl VoiceRelease {
    pub(super) fn new(
        inner: Arc<Inner>,
        thread_id: i64,
        slot: Arc<Slot>,
        rx: watch::Receiver<u64>,
    ) -> Self {
        Self {
            inner,
            thread_id,
            slot: Some(slot),
            rx,
        }
    }

    /// The thread it watches.
    pub(crate) fn thread_id(&self) -> i64 {
        self.thread_id
    }

    /// Resolves once the thread's binding changed since the bind that handed
    /// this signal out looked at it, or since the last time this resolved:
    /// its holder started to end or went, or another bind took the thread.
    /// Bind again then: the thread may be free, or held by someone else.
    pub(crate) async fn changed(&mut self) {
        if self.rx.changed().await.is_err() {
            // The counter lives in the slot this signal holds: never.
            std::future::pending::<()>().await;
        }
    }

    /// Whether the thread's binding changed and [`Self::changed`] would
    /// resolve at once.
    #[cfg(test)]
    pub(crate) fn has_changed(&self) -> bool {
        self.rx.has_changed().unwrap_or(false)
    }
}

impl Drop for VoiceRelease {
    fn drop(&mut self) {
        // Its reference goes first, so the slot drops with it when nothing
        // else holds it.
        if let Some(slot) = self.slot.take() {
            drop(slot);
            self.inner.release(self.thread_id);
        }
    }
}

/// The thread's voice binding changed (module doc): wake its waiters.
pub(super) fn changed(slot: &Slot) {
    slot.voice_changed.send_modify(|n| *n = n.wrapping_add(1));
}
