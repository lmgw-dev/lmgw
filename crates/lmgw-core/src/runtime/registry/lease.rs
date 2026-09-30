//! A benchmark run's GPU lease, as the registry enforces it (benchmark design
//! §3.2, §13 decision 44).
//!
//! The lease lives on the snapshot, and every admission site asks it there
//! ([`crate::config::Snapshot::gpu_block`]). But a site asks *before* its
//! awaits — a footprint, a ledger read, the admission gate — and starts the
//! container after them, so a start that passed the check just before the
//! lease was taken could still reach `podman run` after the run's drain had
//! looked at an empty registry and gone on to measure. The drain cannot wait
//! for a start it cannot see.
//!
//! So the registry keeps a copy of the lease, and [`Registry::acquire_as`]
//! refuses to *create* an entry while one is held, deciding that under the
//! same map lock that inserts the entry. [`Registry::set_gpu_lease`] takes
//! that lock too, so once it returns, every start is either in the map
//! already — the drain sees it and waits for it — or will be refused. A claim
//! on an entry that exists (a hit, or a park on a start in flight) is not a
//! start and is left alone: the drain waits for it, as for any busy model.
//!
//! Only the lease. The GPU hold keeps its own semantics (gpu-hold design):
//! it drains and sweeps, it does not refuse at this level.

use std::collections::HashMap;
use std::sync::{Arc, MutexGuard};

use super::state::{Entry, Key};
use super::Registry;
use crate::bench::lease::GpuLease;

impl Registry {
    /// Take (`Some`) or release (`None`) the lease — called by
    /// [`crate::state::AppState::set_gpu_lease`] beside the snapshot's.
    pub fn set_gpu_lease(&self, lease: Option<Arc<GpuLease>>) {
        let _map = self.map();
        *self.gpu_lease.lock().unwrap_or_else(|e| e.into_inner()) = lease;
    }

    /// The lease this registry refuses starts under, if any.
    pub fn gpu_lease(&self) -> Option<Arc<GpuLease>> {
        let map = self.map();
        self.leased_under(&map)
    }

    /// The lease, read with the map lock `_map` held.
    pub(super) fn leased_under(
        &self,
        _map: &MutexGuard<'_, HashMap<Key, Entry>>,
    ) -> Option<Arc<GpuLease>> {
        self.gpu_lease
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}
