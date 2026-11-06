//! Following one's own wait for room (chat-media-tools design §5.3).
//!
//! The queue keeps a stage per waiter, typed as a [`WaitPhase`] beside its
//! words, and publishes them in the `vram` view for everyone. A request
//! that wants to say what its own admission is doing — an image tool call
//! showing "Unloading chat/qwen3-30b" — needs its waiter, not the queue: two
//! requests may wait for the same model, and the view cannot tell them
//! apart. So a caller runs its admission inside [`follow`], and the waiter
//! that admission enqueues carries the caller's [`StageWatch`]: every phase
//! the queue sets on it is sent there as well.
//!
//! Scoped to the caller's task: a waiter enqueued inside `follow`'s future
//! follows, any other does not. An admission that needs no wait — the model
//! is up, admission is switched off or has nothing to measure — enqueues no
//! waiter and sends nothing.

use std::future::Future;
use std::sync::{Arc, Mutex};

pub use lmgw_api_types::WaitPhase;
use tokio::sync::mpsc;

/// Where one caller's waiter sends its phases, and the last one it sent:
/// a phase is sent when it differs from that one, so words set between two
/// equal phases (which clear the waiter's phase, `set_stage`) do not repeat
/// it to the caller.
#[derive(Debug, Clone)]
pub struct StageWatch {
    tx: mpsc::UnboundedSender<WaitPhase>,
    last: Arc<Mutex<Option<WaitPhase>>>,
}

impl StageWatch {
    fn new(tx: mpsc::UnboundedSender<WaitPhase>) -> Self {
        Self {
            tx,
            last: Arc::default(),
        }
    }

    pub(super) fn send(&self, phase: &WaitPhase) {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.as_ref() == Some(phase) {
            return;
        }
        *last = Some(phase.clone());
        // A caller that stopped reading is no reason to fail an admission.
        let _ = self.tx.send(phase.clone());
    }
}

tokio::task_local! {
    static WATCH: StageWatch;
}

/// Run `admission` with its waiter's phases sent to `tx` (module doc).
pub async fn follow<F: Future>(tx: mpsc::UnboundedSender<WaitPhase>, admission: F) -> F::Output {
    WATCH.scope(StageWatch::new(tx), admission).await
}

/// The watch of the [`follow`] the caller runs in, if any.
pub(super) fn current() -> Option<StageWatch> {
    WATCH.try_with(Clone::clone).ok()
}

/// Say `phase` to the [`follow`] the caller runs in, as its waiter would: a
/// test's stand-in for an admission.
#[cfg(test)]
pub(crate) fn tell(phase: &WaitPhase) {
    if let Some(w) = current() {
        w.send(phase);
    }
}

/// The words the queue's view shows for `phase`, where the phase alone
/// says it.
pub(super) fn words(phase: &WaitPhase) -> Option<String> {
    match phase {
        WaitPhase::Downloading {
            image,
            done_bytes,
            total_bytes,
            ..
        } => Some(super::download::words(image, *done_bytes, *total_bytes)),
        WaitPhase::Gate => Some("waiting for the admission gate".into()),
        WaitPhase::Measuring => Some("measuring GPU memory".into()),
        WaitPhase::Evicting { victim } => Some(format!("evicting {victim}")),
        WaitPhase::Starting => Some("starting the container".into()),
        WaitPhase::WaitingForRoom { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_watch_hears_a_phase_once_until_it_changes() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let w = StageWatch::new(tx);
        let room = WaitPhase::WaitingForRoom {
            needs_bytes: 7,
            free_bytes: 2,
        };
        w.send(&room);
        w.send(&room);
        w.send(&WaitPhase::Measuring);
        w.send(&room);
        let mut got = Vec::new();
        while let Ok(p) = rx.try_recv() {
            got.push(p);
        }
        assert_eq!(got, vec![room.clone(), WaitPhase::Measuring, room]);
    }
}
