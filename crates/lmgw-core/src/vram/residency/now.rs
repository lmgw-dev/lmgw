//! What an audio container whose load is pending holds on the card right
//! now: read by the ledger at its own device read, and subtracted from the
//! pending figure ([`super::pending`], the module docs' "what it holds now").
//!
//! **Which containers, and what it costs.** Only a ready audio container on
//! the GPU that lmgw does not know to hold its model — the ones with a load
//! pending — and only through the PID cache: the init PID a `podman inspect`
//! already found (the reading at rest, the reading after an answer, the
//! outside-VRAM pass) and the processes under it from `/proc`. No inspect is
//! spawned and nothing is waited for, so the `vram` frame stays cheap: two
//! process lists on top of the device read it takes anyway, and none at all
//! while nothing is pending or no PID of one is cached. A container whose
//! PID is not cached is not read and is charged as before; its note says
//! why.
//!
//! **In this order** — the outside-VRAM pass's (`ledger::measure`, review
//! finding 2), with the other direction kept: the process list, the devices,
//! the process list again, and per container the **smaller** of its two
//! figures. Memory the container took around the device read may or may not
//! be in that read's `used`; the smaller figure leaves it pending, so free
//! memory is never counted for a load the device read had not seen yet. One
//! that shrank (an idle unload) is read after it, and the device read saw
//! that memory either freed (exact) or still used (kept free twice, until
//! the next ledger).
//!
//! **Not a load mark, and nothing learned.** A reading that covers the
//! expected residency leaves nothing pending, and that is all it does:
//! - The generation is not marked loaded. Its `last_answered` is lmgw's
//!   idle-unload clock ([`super::holds`]) and the realtime warm's "loaded"
//!   ([`crate::vram::VramScheduler::audio_model_loaded`]). A load lmgw did
//!   not route happened at a time lmgw does not know, so a mark at the
//!   reading would start that clock late and take the model as held after
//!   audio.cpp had unloaded it — leaving its reload unreserved, the mirror of
//!   the double count this reading removes. It needs no mark either: every
//!   ledger reads again while lmgw does not know the model is held. The
//!   realtime warm still sends its warm-up request, which marks the
//!   generation the usual way once answered.
//! - Nothing is learned. A learned figure belongs to a request lmgw routed —
//!   read right after its answer and sampled while it ran ([`super::learn`],
//!   [`super::sample`]) — while a ledger's reading can fall in the middle of
//!   a load or of a generation nobody announced, and storing it would put a
//!   database write on the ledger's path. The next request lmgw routes
//!   teaches the figure.

use std::collections::HashMap;

use super::super::nvml::{GpuMemory, ProcessMemory};
use super::super::VramScheduler;
use super::read::own_share;

/// One container the ledger asks about: its generation, and its init PID
/// from the PID cache — or why the cache has none.
pub(in crate::vram) struct Ask {
    pub(in crate::vram) generation: u64,
    pub(in crate::vram) root: Result<u32, String>,
}

/// The ledger's device read, and what each container asked about holds at
/// it — or why that could not be read.
pub(in crate::vram) struct DeviceRead {
    pub(in crate::vram) devices: Result<Vec<GpuMemory>, String>,
    pub(in crate::vram) held: HashMap<u64, Result<u64, String>>,
}

impl VramScheduler {
    /// The devices, and — for every container in `asked` — what its
    /// processes hold, read around them on one blocking thread (module
    /// docs). With nothing to read — nothing asked, or no PID cached for
    /// any of it — it is the plain device read, and each container asked
    /// about keeps why its PID is missing.
    pub(in crate::vram) async fn devices_and_held(&self, asked: Vec<Ask>) -> DeviceRead {
        if asked.iter().all(|a| a.root.is_err()) {
            return DeviceRead {
                devices: self.devices().await,
                held: asked
                    .into_iter()
                    .filter_map(|a| a.root.err().map(|why| (a.generation, Err(why))))
                    .collect(),
            };
        }
        let probe = self.probe();
        let tree = self.procs.read().unwrap().clone();
        // The process tree is read from `/proc`: on the blocking thread too.
        tokio::task::spawn_blocking(move || {
            let members: Vec<(u64, Result<Vec<u32>, String>)> = asked
                .into_iter()
                .map(|a| {
                    let pids = a.root.and_then(|root| tree.tree_of(root));
                    (a.generation, pids)
                })
                .collect();
            let own: Vec<u32> = members
                .iter()
                .filter_map(|(_, pids)| pids.as_ref().ok())
                .flatten()
                .copied()
                .collect();
            let list = || {
                probe
                    .processes(&own, &[])
                    .map_err(|e| format!("no per-process GPU memory right now: {e}"))
            };
            let first = list();
            let devices = probe.devices();
            let second = list();
            let held = members
                .into_iter()
                .map(|(generation, pids)| {
                    (
                        generation,
                        pids.and_then(|pids| smaller_held(&pids, &first, &second)),
                    )
                })
                .collect();
            DeviceRead { devices, held }
        })
        .await
        .unwrap_or_else(|e| DeviceRead {
            devices: Err(format!("GPU probe thread failed: {e}")),
            held: HashMap::new(),
        })
    }
}

/// What `pids` hold by two process lists taken around one device read: the
/// smaller of the two sums (module docs). None of them listed is nothing
/// held; a list that failed, or a process listed without a figure, is no
/// figure at all.
pub(in crate::vram) fn smaller_held(
    pids: &[u32],
    first: &Result<Vec<ProcessMemory>, String>,
    second: &Result<Vec<ProcessMemory>, String>,
) -> Result<u64, String> {
    let sum = |list: &Result<Vec<ProcessMemory>, String>| -> Result<u64, String> {
        let list = list.as_ref().map_err(Clone::clone)?;
        Ok(own_share(pids, list)?.unwrap_or(0))
    };
    Ok(sum(first)?.min(sum(second)?))
}
