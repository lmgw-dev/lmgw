//! One per-process reading of an audio container: what the driver lists for
//! its init and every process under it. Shared by the reading after an answer
//! ([`super::learn`]), the reading at rest ([`super::ready`]) and the sampler
//! ([`super::sample`]).

use std::collections::HashSet;

use crate::state::SharedState;

use super::super::attribution::{Fill, Member};
use super::super::nvml::ProcessMemory;
use super::super::scheduler::CONTROL_TIMEOUT;
use super::super::VramScheduler;

impl VramScheduler {
    /// The container's processes: its init (`podman inspect`, cached per
    /// generation) and everything under it.
    ///
    /// Every registered container goes into the PID plan, not only this one:
    /// the cache retires whatever a plan does not name into tombstones. A
    /// read in flight is joined, and one that failed is asked again
    /// ([`Fill::Verdict`]) — callers come once per answered request at most,
    /// so a failed `podman inspect` is retried by the next one rather than
    /// never (WP7 review, low).
    pub(super) async fn container_pids(
        &self,
        state: &SharedState,
        generation: u64,
        model_id: &str,
    ) -> Result<Vec<u32>, String> {
        let wait = self.pid_plan(state, Fill::Verdict);
        if !wait.is_empty() {
            let reads = futures::future::join_all(wait.into_iter().map(|r| r.done()));
            if tokio::time::timeout(CONTROL_TIMEOUT, reads).await.is_err() {
                return Err(format!(
                    "its container's PID was still being read after {}s",
                    CONTROL_TIMEOUT.as_secs()
                ));
            }
        }
        let root = self.pids.lock().unwrap().root(generation)?;
        let tree = self.procs.read().unwrap().clone();
        let label = format!("audio/{model_id}");
        let pids = tokio::task::spawn_blocking(move || {
            std::iter::once(root)
                .chain(tree.descendants(root))
                .collect::<Vec<u32>>()
        })
        .await
        .map_err(|e| format!("process walk failed: {e}"))?;
        // The tree it was read with, for its tombstone once it stops.
        self.pids.lock().unwrap().remember(&[Member {
            generation,
            label,
            pids: pids.clone(),
        }]);
        Ok(pids)
    }

    /// What the driver lists for `pids` right now. `Ok(None)`: none of them
    /// is listed — a container that holds nothing on the GPU (yet). `Err`:
    /// the probe could not answer, or listed one of them without a size.
    ///
    /// `settle`: ask about the tombstones too and drop the ones the driver no
    /// longer lists. The outside-VRAM pass settles them as well, but only
    /// while its trigger is armed; with it off, the PID plans these readings
    /// make would otherwise pile tombstones up for good (WP7 review, low).
    /// The sampler passes `false` — twenty times a second is no place for it.
    pub(super) async fn read_processes(
        &self,
        pids: &[u32],
        settle: bool,
    ) -> Result<Option<u64>, String> {
        let probe = self.probe();
        let tombstones = if settle {
            self.pids.lock().unwrap().tombstone_pids()
        } else {
            Vec::new()
        };
        let own = pids.to_vec();
        let asked: HashSet<u32> = own.iter().chain(&tombstones).copied().collect();
        let listed = tokio::task::spawn_blocking(move || {
            probe
                .processes(&own, &tombstones)
                .map_err(|e| format!("no per-process GPU memory right now: {e}"))
        })
        .await
        .map_err(|e| format!("GPU probe thread failed: {e}"))??;
        if settle {
            // Only the pruning is wanted here: what the tombstones still
            // hold is the outside-VRAM pass's figure, not this one's.
            let _ = self.pids.lock().unwrap().settle(&listed, &asked);
        }
        own_share(pids, &listed)
    }
}

/// The bytes the driver lists for `pids`.
pub(super) fn own_share(pids: &[u32], listed: &[ProcessMemory]) -> Result<Option<u64>, String> {
    let mut total: Option<u64> = None;
    for p in listed.iter().filter(|p| pids.contains(&p.pid)) {
        let bytes = p.bytes.ok_or_else(|| {
            format!(
                "the driver lists its process {} without saying how much memory it holds",
                p.pid
            )
        })?;
        total = Some(total.unwrap_or(0).saturating_add(bytes));
    }
    Ok(total)
}
