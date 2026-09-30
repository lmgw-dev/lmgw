//! Scheduler

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::runtime::registry::Origin;
use crate::runtime::Class;
use crate::state::SharedState;

use super::amdgpu::AmdSysfsProbe;
use super::attribution::{PidCache, ProcFs, ProcTree};
use super::nvml::{GpuMemory, GpuProbe, NvmlProbe};
use super::{background, broadcast, plan, Target, WaiterView};

/// The telemetry source for this host, picked once at startup.
///
/// NVML first, because it is the only one of the two that can answer for a
/// *discrete* card whatever else is in the box — a Ryzen laptop with an NVIDIA
/// dGPU is a host where both probes would find something, and the dGPU is the
/// one llama.cpp will be running on. amdgpu's sysfs counters are the fallback,
/// and they are only installed when a card actually exposes them, so the last
/// case keeps NVML's own failure text: "NVML unavailable: …" is the reason the
/// owner needs to see, and inventing a second one would bury it.
pub fn detect_probe() -> Arc<dyn GpuProbe> {
    let nvml = NvmlProbe::detect();
    if nvml.is_available() {
        return Arc::new(nvml);
    }
    match AmdSysfsProbe::detect() {
        Some(amd) => {
            tracing::info!(
                "amdgpu sysfs counters found — VRAM admission control has device telemetry"
            );
            Arc::new(amd)
        }
        None => Arc::new(nvml),
    }
}

pub(super) const MIB: u64 = 1024 * 1024;

/// How often the scheduler re-measures while it waits for room. A sampling
/// rate, not a bound on anything: nothing is skipped or capped because of it,
/// the loop simply looks again this often.
pub(super) const POLL: Duration = Duration::from_millis(250);

/// Timeout for the busy probe against a victim's own `/slots`. A loopback JSON
/// read against a container lmgw started, so a slow answer means it is wedged.
///
/// Shared with the hold sweep (gpu-hold design §5), which runs the same probe
/// against the same containers: two budgets for one question would mean a
/// container that counts as idle for eviction and as busy for the sweep.
///
/// Also the bound on the `podman inspect` that reads a container's PID for
/// the outside-VRAM verdict (candidate-aliases §12, review finding 4): a
/// control question about a container lmgw runs, asked on the request path.
/// A read that takes longer makes the verdict unavailable with a reason that
/// names this bound.
pub(crate) const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub(super) struct Waiter {
    pub(super) id: u64,
    pub(super) alias: String,
    pub(super) model: String,
    pub(super) class: Class,
    pub(super) needs: u64,
    pub(super) since: Instant,
    pub(super) stage: String,
    /// Whose wait it is: every start's admission is the owner's; a climb's
    /// is its hold's. Only the owner's make a guest give way
    /// ([`VramScheduler::owner_waiting`], candidate-aliases §12 entry 91).
    pub(super) origin: Origin,
}

/// Memory a decision has claimed but whose container has not started yet
/// (§4). Held from inside the gate until the start settles; see [`Reserved`].
#[derive(Debug, Clone)]
pub(super) struct Reservation {
    pub(super) id: u64,
    pub(super) class: Class,
    pub(super) model_id: String,
    pub(super) bytes: u64,
}

#[derive(Default)]
pub(super) struct Live {
    pub(super) queue: Vec<Waiter>,
    pub(super) reservations: Vec<Reservation>,
}

pub struct VramScheduler {
    probe: RwLock<Arc<dyn GpuProbe>>,
    pub(super) plans: plan::PlanCache,
    /// One admission *decision* at a time — not one load at a time. Requests
    /// that need room queue behind it, which is what makes the queue a real,
    /// observable thing rather than a race between concurrent free-memory
    /// reads; it is released before the container start so the loads overlap.
    pub(super) gate: tokio::sync::Mutex<()>,
    pub(super) live: Mutex<Live>,
    next_id: AtomicU64,
    /// Every admission that ever entered the queue ([`Self::waits_begun`]).
    waits: AtomicU64,
    /// The host's process tree, for a container's processes under its init
    /// (§4.7). `/proc`; a seam for the tests.
    pub(super) procs: RwLock<Arc<dyn ProcTree>>,
    /// Each container generation's `State.Pid`, and the tombstones of
    /// containers that left (§4.7, [`attribution`](crate::vram::attribution)). Shared with the task
    /// that records a `podman inspect`'s answer, which outlives a cancelled
    /// view.
    pub(super) pids: Arc<Mutex<PidCache>>,
    /// Whether boot has finished adopting what podman still runs and sweeping
    /// the router-mode leftovers (candidate-aliases §12, review finding 1).
    /// `lifecycle::boot` runs unawaited, so requests are served while
    /// reconciliation still reads podman; until it is done the registry does
    /// not hold the containers a previous lmgw left on the card, and their
    /// memory would read as outside use. Until then the outside-VRAM verdict
    /// is unavailable.
    boot_settled: AtomicBool,
    /// Background starts, one at a time per model ([`background::GuestTurns`],
    /// candidate-aliases §12 entry 91).
    pub(super) guest_turns: background::GuestTurns,
}

impl Default for VramScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl VramScheduler {
    pub fn new() -> Self {
        Self::with_probe(detect_probe())
    }

    pub fn with_probe(probe: Arc<dyn GpuProbe>) -> Self {
        Self {
            probe: RwLock::new(probe),
            plans: plan::PlanCache::default(),
            gate: tokio::sync::Mutex::new(()),
            live: Mutex::new(Live::default()),
            next_id: AtomicU64::new(1),
            waits: AtomicU64::new(0),
            procs: RwLock::new(Arc::new(ProcFs::default())),
            pids: Arc::new(Mutex::new(PidCache::default())),
            boot_settled: AtomicBool::new(false),
            guest_turns: background::GuestTurns::default(),
        }
    }

    /// Boot reconciliation is over (or never ran, in a test state): the
    /// registry now holds every container lmgw is going to adopt, so what it
    /// does not hold is not lmgw's. Set by `lifecycle::boot` once adoption and
    /// the legacy sweep are done, whatever they found or failed at; a test
    /// clears it to stand in the boot window.
    pub fn set_boot_settled(&self, settled: bool) {
        self.boot_settled.store(settled, Ordering::Release);
        tracing::debug!("VRAM attribution: boot reconciliation settled = {settled}");
    }

    /// See [`Self::set_boot_settled`].
    pub fn boot_settled(&self) -> bool {
        self.boot_settled.load(Ordering::Acquire)
    }

    /// Replace the GPU probe. The seam tests use to state what the GPU looks
    /// like; `AppState` installs the real one ([`detect_probe`]) at startup.
    pub fn set_probe(&self, probe: Arc<dyn GpuProbe>) {
        *self.probe.write().unwrap() = probe;
    }

    /// Replace the process tree attribution walks — the tests' seam, like
    /// [`Self::set_probe`].
    pub fn set_proc_tree(&self, tree: Arc<dyn ProcTree>) {
        *self.procs.write().unwrap() = tree;
    }

    /// How many admissions have entered the queue since this scheduler was
    /// made. A counter, never reset: what lets a test tell "answered at once"
    /// from "queued, then answered" without timing it (review finding 9).
    pub fn waits_begun(&self) -> u64 {
        self.waits.load(Ordering::Relaxed)
    }

    /// Forget every memoized footprint — the model rows or the models dir moved.
    pub async fn forget_plans(&self) {
        self.plans.clear().await;
    }

    /// The installed probe — what a benchmark run's sampler reads (benchmark
    /// design §4.3), so it sees the same driver, or the same test fake, as
    /// admission does.
    pub fn probe(&self) -> Arc<dyn GpuProbe> {
        self.probe.read().unwrap().clone()
    }

    pub(super) async fn devices(&self) -> Result<Vec<GpuMemory>, String> {
        let probe = self.probe();
        tokio::task::spawn_blocking(move || probe.devices())
            .await
            .unwrap_or_else(|e| Err(format!("GPU probe thread failed: {e}")))
    }

    pub(super) fn queue_view(&self) -> Vec<WaiterView> {
        let live = self.live.lock().unwrap();
        live.queue
            .iter()
            .enumerate()
            .map(|(i, w)| WaiterView {
                position: i + 1,
                alias: w.alias.clone(),
                model: w.model.clone(),
                container: w.class,
                needs_bytes: w.needs,
                waiting_ms: w.since.elapsed().as_millis() as u64,
                stage: w.stage.clone(),
            })
            .collect()
    }

    pub(super) fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Put a waiter in the queue. The returned token is what takes it out
    /// again — see [`Queued`].
    pub(super) fn enqueue(&self, state: &SharedState, w: Waiter) -> Queued {
        let id = w.id;
        self.waits.fetch_add(1, Ordering::Relaxed);
        self.live.lock().unwrap().queue.push(w);
        Queued {
            state: state.clone(),
            id,
        }
    }

    fn dequeue(&self, id: u64) {
        self.live.lock().unwrap().queue.retain(|w| w.id != id);
    }

    pub(super) fn set_stage(&self, id: u64, stage: &str) {
        let mut live = self.live.lock().unwrap();
        if let Some(w) = live.queue.iter_mut().find(|w| w.id == id) {
            w.stage = stage.to_string();
        }
    }

    /// What a waiter waits for, once it knows better than when it queued — a
    /// climb planned again after its drain (ladder design §12 entry 55).
    pub(super) fn set_needs(&self, id: u64, needs: u64) {
        let mut live = self.live.lock().unwrap();
        if let Some(w) = live.queue.iter_mut().find(|w| w.id == id) {
            w.needs = needs;
        }
    }

    /// Claim `bytes` for a start that has been decided but not begun. Taken
    /// inside the gate; released by dropping the returned token.
    pub(super) fn reserve(&self, state: &SharedState, target: &Target, bytes: u64) -> Reserved {
        let id = self.next_id();
        self.live.lock().unwrap().reservations.push(Reservation {
            id,
            class: target.class,
            model_id: target.model_id.clone(),
            bytes,
        });
        Reserved {
            state: state.clone(),
            id,
        }
    }

    /// Starts admission has decided and whose containers are not in the
    /// registry yet, as `"{class}/{model_id}"` — what a benchmark's drain
    /// waits out besides the registry (benchmark design §3.2 step 3): such a
    /// start passed the lease before it was taken, and its container appears
    /// a moment later.
    pub fn pending_starts(&self) -> Vec<String> {
        self.live
            .lock()
            .unwrap()
            .reservations
            .iter()
            .map(|r| format!("{}/{}", r.class.as_str(), r.model_id))
            .collect()
    }

    /// Wait until no admission decision is in progress: take the gate once
    /// and let it go. A benchmark's drain calls this after taking the lease
    /// (benchmark design §3.2 step 3): every decision that holds the gate
    /// then finishes first — each asks the lease again after its last await
    /// and before its claim — so the drain never looks at the registry while
    /// a start it cannot see yet is being decided. Decisions that take the
    /// gate later see the lease themselves.
    pub async fn settle_decisions(&self) {
        drop(self.gate.lock().await);
    }

    fn unreserve(&self, id: u64) {
        self.live
            .lock()
            .unwrap()
            .reservations
            .retain(|r| r.id != id);
    }
}

/// A ledger reservation, released on drop (§4).
///
/// The lifetime that matters: created inside the gate the moment a decision
/// says the model fits, dropped the moment its `acquire` returns — success or
/// failure. Between those two points the gate is *not* held, the container is
/// uploading weights, and neither the driver's free figure nor the registry
/// yet accounts for the model in full. This is what the next decision measures
/// against instead.
pub(super) struct Reserved {
    state: SharedState,
    id: u64,
}

impl Drop for Reserved {
    fn drop(&mut self) {
        self.state.vram.unreserve(self.id);
    }
}

/// A queue entry, removed on drop.
///
/// A wait ends three ways: admitted, refused, or *cancelled* — the client hung
/// up, hyper dropped the handler future, and nothing written after the
/// `.await` in `arbitrate` ever runs. Observed on the live box: two entries
/// outlived their disconnected clients and sat in the dashboard's queue with
/// no request behind them. So the removal is a destructor, not a statement
/// after the await, and it broadcasts on the way out for the same reason the
/// enqueue does — the dashboard is told at both ends of the wait, whichever
/// end it is.
pub(super) struct Queued {
    state: SharedState,
    pub(super) id: u64,
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.state.vram.dequeue(self.id);
        broadcast(&self.state);
    }
}
