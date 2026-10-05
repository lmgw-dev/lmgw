//! What a speaking response's synthesis may read of the writer (realtime
//! design §8.2, §9.1): how much of its audio has left, and when the client
//! runs out of what it holds — nothing that sends.
//!
//! The speaker's back-pressure waits on the first, and judges the second to
//! tell a writer that keeps pace from one that cannot send (a client that
//! stopped reading): the pacer never lets the client run dry while audio is
//! waiting, so a playing window that ends with the answer's audio still
//! queued is a stall (`responder::speech::room`).

use std::sync::Arc;

use tokio::sync::watch;
use tokio::time::Instant;

use super::paced::{Released, Shared};

/// A read-only view of the writer's paced send, for one session's speakers.
/// Cheap to clone.
#[derive(Clone)]
pub(crate) struct Progress {
    releases: watch::Receiver<Released>,
    paced: Arc<Shared>,
}

impl Progress {
    pub(super) fn new(paced: Arc<Shared>) -> Self {
        Self {
            releases: paced.releases(),
            paced,
        }
    }

    /// Samples of generation `gen`'s audio the writer has taken for sending
    /// — marking them seen for [`Self::changed`]. `u64::MAX` once a later
    /// generation speaks: `gen` is over.
    pub fn sent(&mut self, gen: u64) -> u64 {
        let r = *self.releases.borrow_and_update();
        match r.gen.cmp(&gen) {
            std::cmp::Ordering::Equal => r.samples,
            std::cmp::Ordering::Greater => u64::MAX,
            std::cmp::Ordering::Less => 0,
        }
    }

    /// Wait for the next release. `false`: the writer is gone.
    pub async fn changed(&mut self) -> bool {
        self.releases.changed().await.is_ok()
    }

    /// When the client finishes playing what of `gen`'s audio has left —
    /// `None` before any has, or when another generation holds the window
    /// (`playback`).
    pub fn window_end(&self, gen: u64) -> Option<Instant> {
        self.paced
            .playback()
            .filter(|p| p.gen == gen)
            .and_then(|p| p.end())
    }
}
