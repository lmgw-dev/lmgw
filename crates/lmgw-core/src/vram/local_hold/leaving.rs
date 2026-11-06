//! A request that leaves its hold's model for its fallback, and the turns a
//! hold carries (openai egress design §14.3).
//!
//! A climb that cannot happen and a candidate picked again hand the request
//! to another model while it still holds its claim on the one it leaves. The
//! claim is **kept**: a loop's next turn finds the model there when it is
//! still loaded, and a claim shared with other sends goes on serving them.
//! What changes is the fallback's admission and, on the in-process paths,
//! the fallback's whole call: they run inside [`leaving`], so eviction may
//! take the model being left for them when this request's claim is its only
//! user ([`Freeable`]) — otherwise a fallback that fits only once that model
//! goes would queue behind the request's own claim until the queue timeout,
//! or for good with none. The call counts as well as the admission: a ladder
//! fallback climbs while it sends, and a fallback that cannot serve hands on
//! again, and either may need the room the left model takes.
//!
//! The claims left are a stack: a request that left A for B and then leaves
//! B for C admits C inside both, and either claim frees its own model.
//!
//! "Only user" needs the hold's own count of turns: one claim can carry
//! several model calls at once (an agent run's classify stage shares its
//! loop's claim), and the registry counts that as one claim in flight. Each
//! send counts itself on its hold from the start of its fit until its
//! response ends, or until it leaves the hold for another model
//! ([`LocalHold::turn`], held by the send's lease): a turn counted while a
//! request leaves is a sibling's — one still counting its prompt or waiting
//! for pool room among them — and keeps the model.
//! The count goes up under the registry's lock, the one an eviction's stop
//! decides in: a turn that starts while its model is being judged is seen by
//! that judgement, or comes after the model was marked stopping — as a turn
//! on a claim whose container a forced stop took does.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::LocalHold;
use crate::runtime::registry::Freeable;

tokio::task_local! {
    /// Every claim the running request has left, outermost first.
    static LEAVING: Vec<Freeable>;
}

/// One turn on a hold: counted from [`LocalHold::turn`] until dropped.
#[derive(Debug)]
pub struct HoldTurn(Arc<AtomicUsize>);

impl Drop for HoldTurn {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl LocalHold {
    /// Count one turn on this hold until the returned guard drops (module
    /// doc), counted under the registry's lock.
    pub fn turn(&self) -> HoldTurn {
        self.guard().count_turn(&self.turns);
        HoldTurn(Arc::clone(&self.turns))
    }

    /// This hold's claim as the request that leaves it sees it: freeable for
    /// that request's own fallback when it is its model's only user.
    fn freeable(&self) -> Freeable {
        Freeable::new(
            self.target.class,
            self.target.model_id.clone(),
            self.generation(),
            Arc::clone(&self.turns),
        )
    }
}

/// Run `work` — the request's walk to its fallback, or the fallback's call
/// — with `hold`'s claim counted as freeable by any eviction it makes, on
/// top of the claims every enclosing [`leaving`] counts (module doc).
pub async fn leaving<F: Future>(hold: &LocalHold, work: F) -> F::Output {
    let mut left = current();
    left.push(hold.freeable());
    LEAVING.scope(left, work).await
}

/// The claims every [`leaving`] the caller runs in has left; empty outside
/// one.
pub(in crate::vram) fn current() -> Vec<Freeable> {
    LEAVING.try_with(Clone::clone).unwrap_or_default()
}
