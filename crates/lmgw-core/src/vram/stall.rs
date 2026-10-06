//! An admission that found nothing of lmgw's to evict (§4): one look for a
//! container the registry lost track of before it settles into waiting, and a
//! stage that says what the wait is for.
//!
//! Seen 2026-10-06 (`docs/design/2026-10-06-registry-owns-start.md`): a
//! model's container was running with no registry entry — a start abandoned
//! mid-load — so its memory read as outside use. The next request for that
//! very model never saw it resident, eviction found nothing, and the request
//! held the admission gate for the whole `vram.queue_timeout_seconds` under
//! the stage "waiting for a busy model to finish" — with no busy model
//! anywhere — before it was refused.

use std::time::{Duration, Instant};

use crate::hf::fmt_bytes;

use super::ledger::Ledger;
use super::{Target, VramScheduler};
use crate::runtime::lifecycle::REAP_INTERVAL;
use crate::runtime::registry::{PassWait, RuntimeState};
use crate::state::SharedState;

impl VramScheduler {
    /// `evict_one` came back empty for the first time in this wait: run a
    /// reconciliation pass ([`crate::runtime::lifecycle::readopt`]) — or
    /// wait for the one running — before settling into waiting. `true`: it
    /// changed what the registry holds; the target itself may be resident
    /// now, or a model the wait may evict.
    ///
    /// The caller has let the admission gate go: a pass asks podman, and the
    /// admissions queued behind this one have no reason to wait for that.
    /// Bounded by what is left of this wait's `vram.queue_timeout_seconds`,
    /// and by one reaper tick — a pass that takes longer is stuck on podman,
    /// and the tick's own pass carries on with it. The reaper tick's pass
    /// covers the rest of the wait.
    pub(super) async fn look_for_unheld(
        &self,
        state: &SharedState,
        waiter_id: u64,
        started: Instant,
        budget: Option<Duration>,
    ) -> bool {
        let within = budget.map_or(REAP_INTERVAL, |d| {
            d.saturating_sub(started.elapsed()).min(REAP_INTERVAL)
        });
        self.set_stage(waiter_id, "looking for containers lmgw lost track of");
        crate::runtime::lifecycle::readopt(state, PassWait::Join, within).await
    }
}

/// What a wait with nothing to evict is waiting for, from the ledger it
/// decided on: lmgw's own models still busy (or still starting), or — when
/// none of them is on the card at all — memory lmgw has no say over.
pub(super) fn waiting_stage(l: &Ledger, target: &Target, needs: u64, free: u64) -> String {
    let others: Vec<_> = l
        .entries
        .iter()
        .filter(|e| !(e.class == target.class && e.model_id == target.model_id))
        .collect();
    if others.is_empty() {
        return format!(
            "needs {}, {} free — nothing of lmgw's is on the GPU to evict; waiting for memory \
             held outside lmgw",
            fmt_bytes(needs),
            fmt_bytes(free)
        );
    }
    if others.iter().all(|e| e.state == RuntimeState::Starting) {
        return "waiting for a model that is still starting".into();
    }
    "waiting for a busy model to finish".into()
}
