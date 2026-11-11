//! Scheduler

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::runtime::registry::Origin;
use crate::runtime::Class;
use crate::state::SharedState;

#[cfg(not(windows))]
use super::amdgpu::AmdSysfsProbe;
#[cfg(not(windows))]
use super::attribution::ProcFs;
use super::attribution::{PidCache, ProcTree};
use super::nvml::{GpuMemory, GpuProbe, NvmlProbe};
use super::stage_watch::{self, StageWatch, WaitPhase};
use super::{background, broadcast, plan, residency, Target, WaiterView};

/// The telemetry source for this host, picked once at startup.
///
/// NVML first, because it is the only one of the two that can answer for a
/// *discrete* card whatever else is in the box — a Ryzen laptop with an NVIDIA
/// dGPU is a host where both probes would find something, and the dGPU is the
/// one llama.cpp will be running on. amdgpu's sysfs counters are the fallback,
/// and they are only installed when a card actually exposes them, so the last
/// case keeps NVML's own failure text: "NVML unavailable: …" is the reason the
/// owner needs to see, and inventing a second one would bury it.
///
/// On Windows NVML is the only source of device totals so far, and its
/// per-process figures are `[N/A]` under WDDM: the probe there is
/// [`super::wddm::WddmProbe`], NVML's totals with D3DKMT's per-process
/// bytes (Windows design §4.3).
pub fn detect_probe() -> Arc<dyn GpuProbe> {
    let nvml = NvmlProbe::detect();
    if nvml.is_available() {
        #[cfg(windows)]
        return Arc::new(super::wddm::WddmProbe::new(nvml));
        #[cfg(not(windows))]
        return Arc::new(nvml);
    }
    without_nvml(nvml)
}

#[cfg(not(windows))]
fn without_nvml(nvml: NvmlProbe) -> Arc<dyn GpuProbe> {
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

/// amdgpu's counters are Linux sysfs; Windows has no second source yet.
#[cfg(windows)]
fn without_nvml(nvml: NvmlProbe) -> Arc<dyn GpuProbe> {
    Arc::new(nvml)
}

/// The host's process tree for attribution's descendant walk: `/proc`, or a
/// Toolhelp snapshot on Windows.
fn default_proc_tree() -> Arc<dyn ProcTree> {
    #[cfg(windows)]
    return Arc::new(super::wddm::toolhelp::Toolhelp);
    #[cfg(not(windows))]
    return Arc::new(ProcFs::default());
}

pub(super) const MIB: u64 = 1024 * 1024;

/// How often the scheduler re-measures while it waits for room. A sampling
/// rate, not a bound on anything: nothing is skipped or capped because of it,
/// the loop simply looks again this often. A warm group's stage looks as
/// often whether it can still get room beside its siblings (`beside`).
pub(crate) const POLL: Duration = Duration::from_millis(250);

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
    /// `stage` typed (chat-media-tools design §5.3); `None` for a stage
    /// only words say (a climb's).
    pub(super) phase: Option<WaitPhase>,
    /// The caller that follows this wait's phases
    /// ([`stage_watch::follow`]).
    pub(super) watch: Option<StageWatch>,
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
    /// Starts waiting for their engine image's download, before admission
    /// (machine-check design §5, [`super::download`]): no place in `queue`.
    pub(super) downloads: Vec<Waiter>,
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
    /// Every decision that ever asked for the gate ([`Self::gate_asks`]).
    gate_asks: AtomicU64,
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
    /// Which audio containers have loaded their model (realtime design §9.4,
    /// [`residency`]): the ledger charges the others' load as pending.
    pub(super) residency: residency::Residency,
    /// The pooled devices' total as the last ledger read measured it, 0
    /// before any did ([`Self::known_total`]): what a guest's start judges
    /// `vram_too_large` by where a busy card would answer it before its own
    /// reading, without a device read of its own (per-request background
    /// design §11.14).
    pub(super) device_total: AtomicU64,
    /// How many devices the last ledger read listed, 0 before any did: what
    /// a partial image start counts the cards it would see by
    /// ([`super::partial::cards_seen`]).
    pub(super) device_count: std::sync::atomic::AtomicUsize,
    /// The engine build the next start of a class would run, where the
    /// launcher chooses one ([`Self::set_engine_build`]).
    engine_build: RwLock<EngineBuild>,
    /// The image containers read at rest for their row's idle residency
    /// ([`super::partial::floor`]), by generation: once each.
    pub(super) image_at_rest: Mutex<std::collections::HashSet<u64>>,
    /// The record of outside use auto headroom learns from
    /// ([`super::headroom::growth`]).
    pub(super) growth: Mutex<super::headroom::growth::Growth>,
    /// What each chat image's llama-server said about `--fit`
    /// ([`super::fit_vocab`]).
    pub(super) fit_vocab: super::fit_vocab::FitVocab,
}

/// What says which engine build the next start of a class would run
/// ([`VramScheduler::set_engine_build`]).
pub type EngineBuild = Arc<dyn Fn(crate::runtime::Class) -> Option<String> + Send + Sync>;

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
            gate_asks: AtomicU64::new(0),
            procs: RwLock::new(default_proc_tree()),
            pids: Arc::new(Mutex::new(PidCache::default())),
            boot_settled: AtomicBool::new(false),
            guest_turns: background::GuestTurns::default(),
            residency: residency::Residency::default(),
            device_total: AtomicU64::new(0),
            device_count: std::sync::atomic::AtomicUsize::new(0),
            engine_build: RwLock::new(Arc::new(|_| None)),
            image_at_rest: Mutex::new(Default::default()),
            growth: Mutex::new(Default::default()),
            fit_vocab: Default::default(),
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

    /// Install what says which engine build the next start of a class runs:
    /// the native launcher's choice (`Native::expected_build`, Windows), which
    /// the image row's learned figures are keyed by beside the engine image
    /// and run args ([`super::plan::image::engine_key`]). Without one (Podman,
    /// where the image names the build) it answers `None`.
    pub fn set_engine_build(&self, build: EngineBuild) {
        *self.engine_build.write().unwrap() = build;
    }

    /// The engine build the next start of `class` would run, where the
    /// launcher chooses one ([`Self::set_engine_build`]).
    pub fn engine_build(&self, class: crate::runtime::Class) -> Option<String> {
        let f = self.engine_build.read().unwrap().clone();
        f(class)
    }

    /// The engine key image row `m` resolves to now
    /// ([`super::plan::image::engine_key`]): its engine image and run args,
    /// else the class's, and the build the launcher would start.
    pub fn image_engine_key(
        &self,
        snap: &crate::config::Snapshot,
        m: &crate::config::ImageModel,
    ) -> String {
        let i = &snap.settings.image;
        super::plan::image::engine_key(
            m.image.as_deref().unwrap_or(&i.image),
            m.extra_run_args.as_ref().unwrap_or(&i.extra_run_args),
            self.engine_build(crate::runtime::Class::Image).as_deref(),
        )
    }

    /// The key a running image container reads its row's figures by: the
    /// engine it was started with, else (still starting) the row's now.
    pub(super) fn entry_engine_key(
        &self,
        snap: &crate::config::Snapshot,
        e: &crate::runtime::registry::RuntimeView,
        m: &crate::config::ImageModel,
    ) -> String {
        e.engine_key
            .clone()
            .unwrap_or_else(|| self.image_engine_key(snap, m))
    }

    /// How many admissions have entered the queue since this scheduler was
    /// made. A counter, never reset: what lets a test tell "answered at once"
    /// from "queued, then answered" without timing it (review finding 9).
    pub fn waits_begun(&self) -> u64 {
        self.waits.load(Ordering::Relaxed)
    }

    /// How many admission decisions have asked for the gate since this
    /// scheduler was made — a request's and a start's pre-flight, counted
    /// when they ask, before they hold it. A counter, never reset, like
    /// [`Self::waits_begun`]: what lets a test tell a decision is waiting
    /// behind the gate without timing it.
    pub fn gate_asks(&self) -> u64 {
        self.gate_asks.load(Ordering::Relaxed)
    }

    /// The gate, for a decision: counted ([`Self::gate_asks`]), then locked.
    pub(super) fn ask_gate(
        &self,
    ) -> impl std::future::Future<Output = tokio::sync::MutexGuard<'_, ()>> + '_ {
        self.gate_asks.fetch_add(1, Ordering::Relaxed);
        self.gate.lock()
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
                phase: w.phase.clone(),
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

    /// Set waiter `id`'s stage to words no phase types (a climb's, the look
    /// for lost containers, the moment before a victim is chosen): its
    /// phase is cleared with it, so the view never pairs these words with
    /// the phase before them.
    pub(super) fn set_stage(&self, id: u64, stage: &str) {
        let mut live = self.live.lock().unwrap();
        if let Some(w) = live.queue.iter_mut().find(|w| w.id == id) {
            w.stage = stage.to_string();
            w.phase = None;
        }
    }

    /// Set waiter `id`'s phase, and its words: `stage`, else the phase's own
    /// ([`stage_watch::words`]). The caller that follows the wait is told
    /// of a phase that changed since it was last told one
    /// ([`StageWatch`]).
    pub(super) fn set_phase(&self, id: u64, phase: WaitPhase, stage: Option<&str>) {
        let mut live = self.live.lock().unwrap();
        let Some(w) = live.queue.iter_mut().find(|w| w.id == id) else {
            return;
        };
        if let Some(text) = stage
            .map(str::to_string)
            .or_else(|| stage_watch::words(&phase))
        {
            w.stage = text;
        }
        if let Some(watch) = &w.watch {
            watch.send(&phase);
        }
        w.phase = Some(phase);
    }

    /// What a waiter waits for, once it knows better than when it queued — a
    /// climb planned again after its drain (ladder design §12 entry 55).
    pub(super) fn set_needs(&self, id: u64, needs: u64) {
        let mut live = self.live.lock().unwrap();
        if let Some(w) = live.queue.iter_mut().find(|w| w.id == id) {
            w.needs = needs;
        }
    }

    /// Claim what admission decided for a start that has not begun (`vram::partial`): the
    /// whole footprint (with a note, `Grant::Noted`), or a partial grant,
    /// which is logged with its warning here and staged for the registry
    /// entry the start creates
    /// (`registry::admission_grant`) — the entry carries it from its claim
    /// on, renders sd-server's `--max-vram` from it and is charged it.
    pub(super) fn reserve_as(
        &self,
        state: &SharedState,
        target: &Target,
        grant: super::partial::Grant,
    ) -> Reserved {
        let id = self.next_id();
        let (bytes, partial, note) = match grant {
            super::partial::Grant::Whole(b) => (b, None, None),
            super::partial::Grant::Noted(b, note) => (b, None, Some(note)),
            super::partial::Grant::Partial(g) => (g.bytes, Some(g), None),
        };
        self.live.lock().unwrap().reservations.push(Reservation {
            id,
            class: target.class,
            model_id: target.model_id.clone(),
            bytes,
        });
        if let Some(g) = partial.as_ref().filter(|g| !g.warning.is_empty()) {
            tracing::warn!(
                model = %target.model_id,
                "{} model '{}' starts with less than all of it on the GPU: {}",
                target.class.as_str(),
                target.model_id,
                g.warning
            );
        }
        if let Some(n) = &note {
            tracing::warn!(
                model = %target.model_id,
                "{} model '{}' starts with a warning: {n}",
                target.class.as_str(),
                target.model_id,
            );
        }
        let staged = (partial.is_some() || note.is_some()).then(|| {
            state.runtime().stage_grant(
                target.class,
                &target.model_id,
                id,
                crate::runtime::registry::Staged {
                    grant: partial,
                    note,
                },
            );
            target.clone()
        });
        Reserved {
            state: state.clone(),
            id,
            staged,
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
    /// The model whose registry entry this start's partial grant is staged
    /// for: dropped with the token when no entry took it.
    staged: Option<Target>,
}

impl Drop for Reserved {
    fn drop(&mut self) {
        self.state.vram.unreserve(self.id);
        if let Some(t) = self.staged.take() {
            self.state
                .runtime()
                .unstage_grant(t.class, &t.model_id, self.id);
        }
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
