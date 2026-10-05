//! The ledger

use std::collections::HashSet;

use crate::config::Snapshot;
use crate::runtime::registry::{RuntimeState, RuntimeView};
use crate::runtime::Class;
use crate::state::SharedState;

use super::attribution::{self, Container, Fill, Member, Reading};
use super::nvml::{GpuMemory, ProcessMemory};
use super::on_cpu::on_gpu;
use super::residency;
use super::scheduler::{Reservation, CONTROL_TIMEOUT, MIB};
use super::view::{peak_note, ResidentView};
use super::{broadcast, VramScheduler};
use crate::bench::OnCard;

pub(super) struct Capacity {
    pub(super) total: u64,
    pub(super) free: u64,
    /// True when `free` came from the driver rather than from the estimate.
    pub(super) measured: bool,
}

pub(super) struct Ledger {
    pub(super) devices: Vec<GpuMemory>,
    pub(super) telemetry_error: Option<String>,
    pub(super) residents: Vec<ResidentView>,
    /// The registry rows the residents were built from — carries the host port
    /// and the raw `last_used`, which eviction needs and the published view
    /// deliberately does not grow a field for.
    pub(super) entries: Vec<RuntimeView>,
    pub(super) estimated_resident: u64,
    pub(super) capacity: Option<Capacity>,
    pub(super) inactive_reason: Option<String>,
}

impl VramScheduler {
    /// Assemble the ledger: measure, then account, then reconcile.
    pub(super) async fn ledger(&self, state: &SharedState, snap: &Snapshot) -> Ledger {
        let s = &snap.settings;
        let (devices, telemetry_error) = match self.devices().await {
            Ok(d) => (d, None),
            Err(e) => (vec![], Some(e)),
        };

        // Residency is the registry, not a question asked over HTTP: an entry
        // exists exactly while lmgw believes a container for that model is up,
        // and `starting` counts because the weights are already going onto the
        // card. A container on the CPU is no part of it (`on_cpu`): this one
        // filter keeps it out of the charge, the pending loads, eviction, the
        // holders a refusal names and the hold's draining list.
        let entries: Vec<RuntimeView> = state
            .runtime()
            .list()
            .into_iter()
            .filter(|e| matches!(e.state, RuntimeState::Starting | RuntimeState::Ready))
            .filter(on_gpu)
            .collect();
        let reservations = self.live.lock().unwrap().reservations.clone();

        let mut residents: Vec<ResidentView> = Vec::new();
        // What is committed but cannot be in the driver's `used` figure yet: a
        // container still uploading weights, and a reservation whose container
        // has not been created at all. Subtracted from the measured free below
        // — conservatively, since a half-loaded model is counted whole.
        let mut unmeasured: u64 = 0;
        // The other thing the driver's `used` cannot show: what a resident
        // image pipeline will allocate the moment it is asked to draw
        // (image-generation §9). Measured per row by [`peak`] and charged here
        // for exactly as long as the pipeline is on the card, because a
        // generation is one request away the whole time. Its sibling above is
        // memory that is *arriving*; this is memory that will be *taken back*.
        let mut reserved_peaks: u64 = 0;
        // And the third (realtime design §9.4): what a ready audio container
        // will load on its first request — audio.cpp loads lazily, and its
        // readiness route answers before the weights are on the card. Like
        // the peaks, charged only where the driver is the measurement: in the
        // budget-only branch `estimated_resident` already counts the whole
        // expected residency.
        let mut pending_loads: u64 = 0;
        // Pruned against every live container, the CPU's too: what a CPU
        // container has answered is all its residency knows (`audio_model_
        // loaded`), and forgetting it here made it read as never loaded — a
        // reload sent at each warm, and a "cold" stage on every turn.
        let live: Vec<u64> = state
            .runtime()
            .list()
            .into_iter()
            .filter(|e| matches!(e.state, RuntimeState::Starting | RuntimeState::Ready))
            .map(|e| e.generation)
            .collect();
        let gens = self.residency.view(&live);
        let now = std::time::Instant::now();
        let cannot_learn = entries
            .iter()
            .any(|e| e.class == Class::Audio)
            .then(|| self.probe().process_support().err())
            .flatten();
        for e in &entries {
            // The rung the container runs, `starting` included: a climb's new
            // rung is charged from its claim on (ladder design §5).
            let fp = self.footprint_of(snap, e).await;
            let bytes = fp.as_ref().map(|f| f.total_bytes).unwrap_or(0);
            if e.state == RuntimeState::Starting {
                unmeasured = unmeasured.saturating_add(bytes);
            }
            // Charged from `ready` on, in flight or not. While a generation is
            // actually running the driver's `used` already contains some or
            // all of it, so for those seconds the charge is doubled — left
            // that way deliberately: the alternative is subtracting a figure
            // nobody measured from a figure that is still moving, and the
            // window it spans is the one where being wrong is expensive.
            let peak = match (e.class, e.state) {
                (Class::Image, RuntimeState::Ready) => snap
                    .image_models
                    .iter()
                    .find(|m| m.model_id == e.model_id)
                    .and_then(|m| m.peak_extra_bytes),
                _ => None,
            };
            if let Some(p) = peak {
                reserved_peaks = reserved_peaks.saturating_add(p);
            }
            let audio = match (e.class, e.state, fp.as_ref()) {
                (Class::Audio, RuntimeState::Ready, Some(fp)) => snap
                    .audio_models
                    .iter()
                    .find(|m| m.model_id == e.model_id)
                    .map(|row| (row, fp)),
                _ => None,
            };
            let gen = gens.get(&e.generation).cloned().unwrap_or_default();
            let pending = match audio {
                Some((row, fp)) => {
                    // Only an eager row nothing was read of needs its
                    // weights file (memoized after the first ledger).
                    let weights = if residency::is_eager(row, &s.audio) {
                        self.plans.audio_weights(&s.audio.models_dir, row).await
                    } else {
                        0
                    };
                    Some(residency::pending(
                        row,
                        &s.audio,
                        fp.total_bytes,
                        weights,
                        &gen,
                        now,
                    ))
                }
                None => None,
            };
            let pending_bytes = pending.as_ref().map(|p| p.bytes);
            pending_loads = pending_loads.saturating_add(pending_bytes.unwrap_or(0));
            let note = match (e.class, e.state, audio, &pending) {
                (Class::Image, RuntimeState::Ready, _, _) => {
                    Some(peak_note(fp.as_ref().and_then(|f| f.note.as_deref()), peak))
                }
                (_, _, Some((row, fp)), Some(pending)) => Some(residency::resident_note(
                    row,
                    &s.audio,
                    &residency::NoteFacts {
                        on_disk: fp.weights_bytes,
                        pending,
                        cannot_learn: cannot_learn.as_deref(),
                        sampling: self.sampling(row, &s.audio),
                        gen: &gen,
                        runs_previous: e.resident_key.as_deref().is_some_and(|k| {
                            k != residency::resident_key(row, &s.audio)
                                && row.residency.as_ref().is_some_and(|r| r.key == k)
                        }),
                    },
                )),
                _ => fp.as_ref().and_then(|f| f.note.clone()),
            };
            residents.push(ResidentView {
                container: e.class,
                model: e.model_id.clone(),
                state: e.state.as_str().to_string(),
                estimated_bytes: bytes,
                in_flight: e.in_flight as usize,
                idle_seconds: Some(e.last_used_age_seconds),
                peak_extra_bytes: peak,
                pending_bytes: pending_bytes.filter(|p| *p > 0),
                note,
            });
        }
        // A reservation is only charged while nothing in the registry answers
        // for its model — otherwise the same model would be counted twice for
        // the whole length of its own start.
        for r in &reservations {
            if entries
                .iter()
                .any(|e| e.class == r.class && e.model_id == r.model_id)
            {
                continue;
            }
            unmeasured = unmeasured.saturating_add(r.bytes);
            residents.push(ResidentView {
                container: r.class,
                model: r.model_id.clone(),
                state: "reserved".into(),
                estimated_bytes: r.bytes,
                in_flight: 0,
                idle_seconds: None,
                // A reservation is a start that has not happened; its
                // pipeline cannot be asked to draw until it is `ready`, and
                // the reservation itself already charges the idle figure.
                peak_extra_bytes: None,
                pending_bytes: None,
                note: None,
            });
        }

        let estimated_resident: u64 = residents.iter().map(|r| r.estimated_bytes).sum();
        let budget = s.vram.budget_mb.saturating_mul(MIB);

        // Capacity, in the order of decreasing authority: a declared budget is
        // an explicit instruction, the driver's total is a measurement, and
        // absent both there is nothing to admit against.
        let (capacity, inactive_reason) = match (budget, devices.is_empty()) {
            (0, true) => (
                None,
                Some(format!(
                    "no GPU telemetry ({}) and no vram.budget_mb declared — admission control \
                     has nothing to measure against and is forwarding every request unchanged",
                    telemetry_error.as_deref().unwrap_or("no devices")
                )),
            ),
            (0, false) => {
                let (total, free) = pooled(0, &devices);
                (
                    Some(Capacity {
                        total,
                        free: free
                            .saturating_sub(unmeasured)
                            .saturating_sub(reserved_peaks)
                            .saturating_sub(pending_loads),
                        measured: true,
                    }),
                    None,
                )
            }
            (b, true) => (
                Some(Capacity {
                    total: b,
                    // Everything committed is already in `estimated_resident`
                    // here, reservations included — no second subtraction.
                    // The learned peaks are the exception, and have to be:
                    // `estimated_resident` is what the ledger believes is *on
                    // the card*, the figure a surface compares against the
                    // driver's `used`, and a transient nobody has allocated
                    // yet does not belong in it.
                    free: b
                        .saturating_sub(estimated_resident)
                        .saturating_sub(reserved_peaks),
                    measured: false,
                }),
                None,
            ),
            (b, false) => {
                let (_, raw_free) = pooled(b, &devices);
                (
                    Some(Capacity {
                        total: b,
                        free: raw_free
                            .saturating_sub(unmeasured)
                            .saturating_sub(reserved_peaks)
                            .saturating_sub(pending_loads),
                        measured: true,
                    }),
                    None,
                )
            }
        };

        Ledger {
            devices,
            telemetry_error,
            residents,
            entries,
            estimated_resident,
            capacity,
            inactive_reason,
        }
    }

    /// §4.7's measurement over the pooled devices: lmgw's own share, measured
    /// per process, the used memory outside it, and the capacity and free
    /// memory from the same device read. `Err` is why the share cannot be
    /// measured right now — the trigger is then unavailable and admission
    /// behaves exactly as it always has.
    ///
    /// Everything that could make the share a low guess is a refusal here: a
    /// container still starting, a reservation for one that has not entered
    /// the registry, a container none of whose processes the driver lists,
    /// a figure the driver cannot give ([`attribution`]'s module docs), and
    /// lmgw's containers changing while it reads.
    ///
    /// **In this order** (review finding 2), because free memory and lmgw's
    /// share have to describe the same moment — something of lmgw's that
    /// shrank between a device read and a process read would be in neither,
    /// and read as outside use:
    /// 1. the registry, and every PID it needs — the only part that may wait
    ///    (a `podman inspect`);
    /// 2. on one blocking thread, back to back: the processes under each
    ///    init, the driver's process list, the devices, the process list
    ///    again — per PID the larger of the two figures, so a process that
    ///    shrank or left around the device read counts at what the device
    ///    read may have seen;
    /// 3. the registry again: any container that appeared, left or changed
    ///    state (or a start reserved) since step 1 is a change the reads may
    ///    have seen half of, and the pass is unavailable.
    pub(super) async fn measure(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        l: &Ledger,
        fill: Fill,
    ) -> Result<Measured, String> {
        if let Some(e) = &l.telemetry_error {
            return Err(format!("no GPU telemetry ({e})"));
        }
        if l.devices.is_empty() {
            return Err("the GPU probe reports no device".into());
        }
        let probe = self.probe();
        probe
            .process_support()
            .map_err(|e| format!("no per-process GPU memory: {e}"))?;

        // 1. lmgw's containers, and their PIDs — a benchmark run's too. Those
        // on the GPU only: a CPU container has no GPU process, and as a member
        // it would make every pass refuse ("running without the GPU").
        let entries = gpu_entries(state);
        let reservations = self.live.lock().unwrap().reservations.clone();
        if let Some(why) = unmeasured_start(&entries, &reservations) {
            return Err(why);
        }
        let bench = bench_container(state)?;
        let before = Roster::of(&entries, &reservations).and_bench(bench_on_card(state));

        let mut current: Vec<Container> = entries.iter().map(container_of).collect();
        current.extend(bench);
        let plan = self.pids.lock().unwrap().plan(&current, fill);
        if let Some(done) = plan.done {
            // One `podman inspect` for every container not known yet, in a
            // task of its own so the answer is recorded even if the caller
            // goes away (a view whose client hung up).
            self.read_pids(state, plan.inspect, done, fill == Fill::View, false);
        }
        if let Some(why) = plan.blocked {
            return Err(why);
        }
        // A verdict waits for its own read and joins any already in flight
        // (review finding 4). Each read is bounded by `CONTROL_TIMEOUT` in its
        // own task; this bound is only the net under that one.
        if !plan.wait.is_empty() {
            let reads = futures::future::join_all(plan.wait.into_iter().map(|r| r.done()));
            if tokio::time::timeout(CONTROL_TIMEOUT, reads).await.is_err() {
                return Err(format!(
                    "a container's PID was still being read after {}s — podman inspect is \
                     that slow to answer (lmgw's timeout for control calls, \
                     vram::CONTROL_TIMEOUT)",
                    CONTROL_TIMEOUT.as_secs()
                ));
            }
        }
        let roots: Vec<(Container, u32)> = {
            let cache = self.pids.lock().unwrap();
            current
                .into_iter()
                .map(|c| cache.root(c.generation).map(|r| (c, r)))
                .collect::<Result<_, _>>()?
        };

        // 2. The processes under each init, then the driver's process list,
        // its devices and its process list again — all blocking reads, on one
        // thread, with nothing in between that could wait.
        let tree = self.procs.read().unwrap().clone();
        let tombstones = self.pids.lock().unwrap().tombstone_pids();
        let read = tokio::task::spawn_blocking(move || {
            let members: Vec<Member> = roots
                .into_iter()
                .map(|(c, root)| Member {
                    generation: c.generation,
                    label: c.label,
                    pids: std::iter::once(root)
                        .chain(tree.descendants(root))
                        .collect(),
                })
                .collect();
            let own: Vec<u32> = members
                .iter()
                .flat_map(|m| m.pids.iter().copied())
                .collect();
            let list = || {
                probe
                    .processes(&own, &tombstones)
                    .map_err(|e| format!("no per-process GPU memory right now: {e}"))
            };
            let first = list()?;
            let devices = probe
                .devices()
                .map_err(|e| format!("no GPU telemetry ({e})"))?;
            let second = list()?;
            let asked: HashSet<u32> = own
                .iter()
                .copied()
                .chain(tombstones.iter().copied())
                .collect();
            Ok::<_, String>((members, larger_per_pid(&first, &second), devices, asked))
        })
        .await
        .map_err(|e| format!("GPU probe thread failed: {e}"))?;
        let (members, listed, devices, asked) = read?;
        if devices.is_empty() {
            return Err("the GPU probe reports no device".into());
        }

        // 3. The registry (and the benchmark's container) again.
        let after = Roster::of(&gpu_entries(state), &self.live.lock().unwrap().reservations)
            .and_bench(bench_on_card(state));
        let mut cache = self.pids.lock().unwrap();
        cache.remember(&members);
        if after != before {
            return Err(
                "lmgw's containers changed while measuring — the next verdict measures again"
                    .into(),
            );
        }
        let own = attribution::members_share(&members, &listed)?;
        let stopped = cache.settle(&listed, &asked)?;
        drop(cache);
        let lmgw = own.saturating_add(stopped);
        let used: u64 = devices.iter().map(|d| d.used_bytes).sum();
        let budget = snap.settings.vram.budget_mb.saturating_mul(MIB);
        let (total, raw_free) = pooled(budget, &devices);
        Ok(Measured {
            shares: Shares {
                lmgw,
                outside: used.saturating_sub(lmgw),
            },
            total,
            raw_free,
        })
    }

    /// Read the host PID of every registered container that has none cached
    /// yet — one batched `podman inspect` in a background task, never waited
    /// on (candidate-aliases design §4.7).
    ///
    /// Called when a container becomes `ready` (a start through admission or
    /// the operator surface, a warm start) and after boot adopts what podman
    /// still runs. Without it a container's PID was only learned by the first
    /// verdict or status view that saw it, so one evicted before either had
    /// run left no tombstone, and its process — still on the card for a
    /// moment after `podman wait` — read as outside use. The processes under
    /// the init are read too, so an image container's tombstone holds its
    /// sd-server and not only catatonit.
    ///
    /// Only while the trigger is armed and the probe can list processes:
    /// otherwise no pass would read the PID, and nothing is spawned (§7 item
    /// 14). A `starting` entry is left alone — its PID is not meaningful yet,
    /// and a start in flight makes every pass unavailable anyway. The one
    /// part that is not gated is an audio container's reading at rest
    /// ([`residency`], WP7 review M4), which reads its PID for itself.
    pub fn cache_pids(&self, state: &SharedState) {
        // What a ready audio container holds at rest is read whatever the
        // trigger's switches say ([`residency`]'s at-rest reading).
        self.read_audio_at_rest(state);
        if trigger_off(&state.snapshot()).is_some() || self.probe().process_support().is_err() {
            return;
        }
        // The status view's rule: ask only for a generation never asked
        // about; one already being read, or whose read failed, is left to
        // the next verdict.
        self.pid_plan(state, Fill::View);
    }

    /// [`Self::cache_pids`] without its gate: plan the PIDs of every
    /// registered container that is not `starting` (and the benchmark's),
    /// spawn the batched `podman inspect` the plan asks for — reading the
    /// processes under each init too — and hand back the reads `fill` waits
    /// for (none for [`Fill::View`]).
    ///
    /// Every container goes into the plan, never only the one a caller is
    /// after: the cache retires whatever a plan does not name into
    /// tombstones. The audio residency reading is the caller that needs no
    /// gate ([`residency`]): it learns a figure about a row, whatever the
    /// outside-VRAM trigger's switches say.
    pub(super) fn pid_plan(&self, state: &SharedState, fill: Fill) -> Vec<Reading> {
        let mut current: Vec<Container> = state
            .runtime()
            .list()
            .iter()
            .filter(|e| e.state != RuntimeState::Starting && on_gpu(e))
            .map(container_of)
            .collect();
        current.extend(bench_container(state).ok().flatten());
        let plan = self.pids.lock().unwrap().plan(&current, fill);
        if let Some(done) = plan.done {
            self.read_pids(state, plan.inspect, done, false, true);
        }
        plan.wait
    }

    /// Run one batched `podman inspect` for `asked` in a task of its own, and
    /// record the answer before `done` is dropped (which is what every pass
    /// waiting on this read waits for).
    ///
    /// - Bounded by [`CONTROL_TIMEOUT`] (review finding 4): `podman inspect`
    ///   takes the container's lock, so it can queue behind a stop's grace
    ///   period or a pull's storage lock. A read that times out is recorded
    ///   as failed with that reason — the verdict is unavailable, today's
    ///   path — and the child is killed with the dropped future.
    /// - Recorded against the registry as it is when podman answered (review
    ///   finding 5, [`PidCache::record`](crate::vram::attribution::PidCache::record)).
    /// - `announce`: push a fresh `vram` frame once recorded (the view's
    ///   frame said "being read"). `walk`: also read the processes under
    ///   each init, so a tombstone made before any pass holds them
    ///   ([`Self::cache_pids`]).
    fn read_pids(
        &self,
        state: &SharedState,
        asked: Vec<Container>,
        done: attribution::ReadDone,
        announce: bool,
        walk: bool,
    ) {
        let registry = state.runtime();
        let cache = self.pids.clone();
        let tree = self.procs.read().unwrap().clone();
        let st = state.clone();
        tokio::spawn(async move {
            let names: Vec<String> = asked.iter().map(|c| c.name.clone()).collect();
            let answer =
                match tokio::time::timeout(CONTROL_TIMEOUT, registry.host_pids(&names)).await {
                    Ok(answer) => answer,
                    Err(_) => Err(format!(
                        "podman inspect did not answer within {}s (lmgw's timeout for control \
                     calls, vram::CONTROL_TIMEOUT)",
                        CONTROL_TIMEOUT.as_secs()
                    )),
                };
            let mut live: HashSet<u64> = registry
                .list()
                .iter()
                .filter(|e| on_gpu(e))
                .map(|e| e.generation)
                .collect();
            if let OnCard::Ready { generation } = bench_on_card(&st) {
                live.insert(generation);
            }
            let roots: Vec<(Container, u32)> = {
                let mut cache = cache.lock().unwrap();
                cache.record(&asked, &answer, &live);
                if walk {
                    asked
                        .iter()
                        .filter_map(|c| cache.root(c.generation).ok().map(|r| (c.clone(), r)))
                        .collect()
                } else {
                    Vec::new()
                }
            };
            drop(done);
            if announce {
                broadcast(&st);
            }
            if roots.is_empty() {
                return;
            }
            let members = tokio::task::spawn_blocking(move || {
                roots
                    .into_iter()
                    .map(|(c, root)| Member {
                        generation: c.generation,
                        label: c.label,
                        pids: std::iter::once(root)
                            .chain(tree.descendants(root))
                            .collect(),
                    })
                    .collect::<Vec<_>>()
            })
            .await;
            if let Ok(members) = members {
                cache.lock().unwrap().remember(&members);
            }
        });
    }
}

/// Where the benchmark run's container is ([`OnCard::Off`] with no run).
fn bench_on_card(state: &SharedState) -> OnCard {
    state.bench.current().map(|c| c.on_card).unwrap_or_default()
}

/// The benchmark run's container as the PID cache keys it, once it is loaded
/// (benchmark design §3.4): its processes are lmgw's share like a model
/// container's, and a tombstone once it is removed. `Err` while it loads —
/// its memory is still growing, as a `starting` entry's is
/// ([`unmeasured_start`]).
fn bench_container(state: &SharedState) -> Result<Option<Container>, String> {
    let Some(run) = state.bench.current() else {
        return Ok(None);
    };
    match (run.on_card, run.container) {
        (OnCard::Loading, _) => Err(format!(
            "benchmark run {} is loading chat/{} — its memory is not measured yet",
            run.run_id, run.model_id
        )),
        (OnCard::Ready { generation }, Some(name)) => Ok(Some(Container {
            generation,
            label: format!("chat/{} (benchmark run {})", run.model_id, run.run_id),
            name,
        })),
        _ => Ok(None),
    }
}

/// The registry's containers on the GPU (`on_cpu::on_gpu`) — what the
/// per-process attribution measures.
fn gpu_entries(state: &SharedState) -> Vec<RuntimeView> {
    state.runtime().list().into_iter().filter(on_gpu).collect()
}

/// A registry entry as the PID cache keys it.
fn container_of(e: &RuntimeView) -> Container {
    Container {
        generation: e.generation,
        label: format!("{}/{}", e.class.as_str(), e.model_id),
        name: e.container_name.clone(),
    }
}

/// lmgw's measured share of the pooled devices, and the used memory outside
/// it (§4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shares {
    pub lmgw: u64,
    pub outside: u64,
}

/// One measurement pass ([`VramScheduler::measure`]): the shares, and the
/// capacity and free memory of the same device read, by the ledger's rule
/// ([`pooled`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Measured {
    pub(super) shares: Shares,
    pub(super) total: u64,
    pub(super) raw_free: u64,
}

/// The pooled capacity and measured free memory of `devices`, by the
/// ledger's rule: a declared budget is the capacity and what "free" is
/// measured against, otherwise the devices' own figures. One definition for
/// the ledger's `raw_free` and the verdict's, so the two cannot drift.
pub(super) fn pooled(budget: u64, devices: &[GpuMemory]) -> (u64, u64) {
    if budget == 0 {
        (
            devices.iter().map(|d| d.total_bytes).sum(),
            devices.iter().map(|d| d.free_bytes).sum(),
        )
    } else {
        let used: u64 = devices.iter().map(|d| d.used_bytes).sum();
        (budget, budget.saturating_sub(used))
    }
}

/// Two readings of the driver's process list as one, per PID the larger
/// figure (review finding 2): the device read between them may have seen
/// either, and the larger one is the direction that never reads lmgw's
/// memory as outside use. A PID only one reading lists keeps that reading's
/// figure. A figure one reading could not give makes the PID's unknown —
/// never the other reading's, which may be the smaller.
pub(super) fn larger_per_pid(
    first: &[ProcessMemory],
    second: &[ProcessMemory],
) -> Vec<ProcessMemory> {
    let mut per: std::collections::BTreeMap<u32, Option<u64>> = Default::default();
    for p in first.iter().chain(second) {
        per.entry(p.pid)
            .and_modify(|b| {
                *b = match (*b, p.bytes) {
                    (Some(a), Some(c)) => Some(a.max(c)),
                    _ => None,
                }
            })
            .or_insert(p.bytes);
    }
    per.into_iter()
        .map(|(pid, bytes)| ProcessMemory { pid, bytes })
        .collect()
}

/// Which lmgw containers exist and in what state, and which starts are
/// reserved: what a measurement compares before and after its reads (review
/// finding 2). The in-flight count and the idle age are left out — they
/// change nothing on the card.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Roster {
    entries: Vec<(u64, RuntimeState)>,
    reservations: Vec<u64>,
    /// The benchmark run's container, which is no registry entry.
    bench: OnCard,
}

impl Roster {
    pub(super) fn of(entries: &[RuntimeView], reservations: &[Reservation]) -> Self {
        let mut e: Vec<(u64, RuntimeState)> =
            entries.iter().map(|e| (e.generation, e.state)).collect();
        e.sort_by_key(|(g, _)| *g);
        let mut r: Vec<u64> = reservations.iter().map(|r| r.id).collect();
        r.sort_unstable();
        Self {
            entries: e,
            reservations: r,
            bench: OnCard::Off,
        }
    }

    /// With the benchmark run's container.
    pub(super) fn and_bench(mut self, bench: OnCard) -> Self {
        self.bench = bench;
        self
    }
}

/// Why the outside-VRAM trigger is switched off by configuration, if it is —
/// the checks that cost nothing, so a request (or a frame) behind any of them
/// probes nothing and spawns nothing (candidate-aliases design §7 item 14).
pub(super) fn trigger_off(snap: &Snapshot) -> Option<String> {
    let s = &snap.settings;
    if !s.vram.fallback_on_external {
        return Some(
            "vram.fallback_on_external is off — only the GPU hold sends a local request to \
             its fallback"
                .into(),
        );
    }
    if !s.vram.enabled {
        return Some(
            "vram.enabled is off — admission does not arbitrate, so there is no shortfall to \
             measure"
                .into(),
        );
    }
    if s.hold.active {
        return Some(
            "the GPU hold is on — every local request already goes to its fallback or is \
             refused"
                .into(),
        );
    }
    // A benchmark run's lease does not switch it off (benchmark design
    // §3.4): the run's container is attributed as lmgw's
    // ([`bench_container`]), so outside use stays measured during a run.
    None
}

impl VramScheduler {
    /// Why the verdict must wait for boot, while it must (see
    /// [`Self::set_boot_settled`]).
    pub(super) fn boot_unsettled(&self) -> Option<String> {
        (!self.boot_settled()).then(|| {
            "boot reconciliation still running — containers a previous lmgw left on the card \
             are not adopted yet, so their memory would read as outside use"
                .to_string()
        })
    }
}

/// A start whose memory the driver cannot show yet, if there is one: a
/// registry entry still `starting`, or a reservation (a start admitted but
/// not in the registry yet). Either would make lmgw's measured share a low
/// guess, so either makes the trigger unavailable (§4.7).
pub(super) fn unmeasured_start(
    entries: &[RuntimeView],
    reservations: &[Reservation],
) -> Option<String> {
    if let Some(e) = entries.iter().find(|e| e.state == RuntimeState::Starting) {
        return Some(format!(
            "{}/{} is starting — its memory is not measured yet",
            e.class.as_str(),
            e.model_id
        ));
    }
    reservations
        .iter()
        .find(|r| {
            !entries
                .iter()
                .any(|e| e.class == r.class && e.model_id == r.model_id)
        })
        .map(|r| {
            format!(
                "{}/{} has just been admitted and its container is not measured yet",
                r.class.as_str(),
                r.model_id
            )
        })
}
