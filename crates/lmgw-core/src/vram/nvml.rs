//! GPU memory telemetry, behind a trait (quickdoc §9b).
//!
//! The llama.cpp router exposes no VRAM figures at all, so the only measured
//! truth available is the driver's own. NVML is loaded **at runtime** — a host
//! with no NVIDIA driver (a CI runner, this crate's own test suite, a laptop)
//! gets an `Err` from [`GpuProbe::devices`], which the ledger reports as a
//! degraded state rather than treating as a crash or as "0 bytes free". On an
//! AMD host [`crate::vram::detect_probe`] falls through to
//! [`crate::vram::amdgpu`] before it settles for that degraded state.
//!
//! The trait exists for exactly one reason: tests must be able to state what
//! the GPU looks like. Everything else in this module talks to real hardware.

use serde::Serialize;

/// One device's memory, as the driver reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GpuMemory {
    pub index: u32,
    pub name: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

/// Where the ledger's measured numbers come from.
///
/// Implementations are synchronous and may block for a millisecond or two;
/// callers run them on a blocking thread.
pub trait GpuProbe: Send + Sync + 'static {
    /// Every visible device, ordered by driver index. `Err` means no telemetry
    /// is available *at all* and carries the reason verbatim — it is shown to
    /// the owner, so it has to say what failed.
    fn devices(&self) -> Result<Vec<GpuMemory>, String>;

    /// Short label for the status surface, e.g. `NVML (driver 580.82.07)`.
    fn source(&self) -> String;

    /// Whether this probe can attribute GPU memory to processes at all
    /// (candidate-aliases design §4.7) — asked before lmgw spends a `podman
    /// inspect` on learning its containers' PIDs. `Err` carries the reason,
    /// verbatim, for the status surface. The default is "cannot": a probe
    /// that knows only device totals must say so rather than answer an empty
    /// process list, which would read as "no lmgw model holds anything".
    fn process_support(&self) -> Result<(), String> {
        Err(format!(
            "{} reports no per-process GPU memory",
            self.source()
        ))
    }

    /// GPU memory per process, summed over every device this probe counts.
    ///
    /// `own` are the processes of lmgw's registered containers, `retired`
    /// those of containers that just left the registry (tombstones,
    /// [`super::attribution`]). A driver that lists every process (NVML)
    /// answers with all of them and may ignore both: anything not asked about
    /// is outside use by definition. One that can only be asked per process
    /// (amdgpu's DRM `fdinfo`) answers for exactly these. A PID in `own` it
    /// cannot read is an error — lmgw's share would be a guess; one in
    /// `retired` it cannot read is simply not listed, since a tombstone's PID
    /// may belong to somebody else's process by now. A PID absent from the
    /// answer holds nothing the driver can see. `Err` = no process list right
    /// now.
    fn processes(&self, own: &[u32], retired: &[u32]) -> Result<Vec<ProcessMemory>, String> {
        let _ = (own, retired);
        Err(format!(
            "{} reports no per-process GPU memory",
            self.source()
        ))
    }

    /// Power, energy, temperature and clock-event reasons per device, ordered
    /// by driver index (benchmark design §4.3) — what a benchmark run's
    /// sampler reads every 100 ms. `Err` = none of it is available; a device
    /// that answers some readings and not others has `None` in exactly those
    /// fields, so a missing energy counter falls back to integrating power
    /// rather than losing the energy figure altogether.
    ///
    /// The default is "cannot": the amdgpu probe and the test fakes that
    /// predate the benchmark keep compiling, and a run on them says it has no
    /// energy figures instead of reporting zero joules.
    fn power(&self) -> Result<Vec<GpuPower>, String> {
        Err(format!(
            "{} reports no power or energy readings",
            self.source()
        ))
    }

    /// The driver version, for a benchmark run's GPU identity (§6). `None`
    /// when the probe cannot say.
    fn driver_version(&self) -> Option<String> {
        None
    }
}

/// One device's power and thermal state (benchmark design §4.3). Each reading
/// is optional on its own, because drivers support them separately (NVML's
/// energy counter needs Volta or newer; power readings need power
/// management).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct GpuPower {
    pub index: u32,
    /// Current draw of the board, milliwatts.
    pub power_mw: Option<u32>,
    /// Total energy since the driver loaded, millijoules — a monotonic counter
    /// (`nvmlDeviceGetTotalEnergyConsumption`).
    pub energy_mj: Option<u64>,
    /// GPU die temperature, °C.
    pub temperature_c: Option<u32>,
    /// NVML's clock-event ("throttle") reasons bitmask
    /// (`lmgw_api_types::bench::CLOCK_EVENT_REASONS`).
    pub clock_events: Option<u64>,
    /// The enforced power limit, milliwatts.
    pub power_limit_mw: Option<u32>,
}

/// What one process holds on the GPU, summed over the devices a probe counts
/// (candidate-aliases design §4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessMemory {
    pub pid: u32,
    /// `None` when the driver lists the process but cannot say how much it
    /// holds — NVML's `NVML_VALUE_NOT_AVAILABLE`, an fdinfo without memory
    /// keys. Never read as zero: for an lmgw process that would move its
    /// memory into "outside use".
    pub bytes: Option<u64>,
}

/// A probe that reports no telemetry at all, with a fixed reason.
///
/// [`crate::state::AppState::init_for_tests`] installs one so the suite can
/// never read — or be made to depend on — a real driver, and so the
/// no-telemetry path is what every unrelated test exercises by default.
pub struct NoTelemetry(pub String);

impl GpuProbe for NoTelemetry {
    fn devices(&self) -> Result<Vec<GpuMemory>, String> {
        Err(self.0.clone())
    }

    fn source(&self) -> String {
        format!("no GPU telemetry: {}", self.0)
    }
}

/// The real thing: `libnvidia-ml.so`, dlopened once at startup.
pub struct NvmlProbe {
    /// `Err` is a permanent condition for this process — the library is either
    /// there at init or it is not — so the reason is kept and reported rather
    /// than re-attempted on every request.
    nvml: Result<nvml_wrapper::Nvml, String>,
}

impl NvmlProbe {
    pub fn detect() -> Self {
        let nvml = nvml_wrapper::Nvml::init().map_err(|e| e.to_string());
        match &nvml {
            Ok(_) => tracing::info!("NVML loaded — VRAM admission control has device telemetry"),
            Err(e) => tracing::info!(
                "NVML unavailable ({e}) — trying amdgpu's sysfs counters next \
                 (see vram::detect_probe)"
            ),
        }
        Self { nvml }
    }

    /// Whether the library loaded at all. What [`crate::vram::detect_probe`]
    /// branches on before it reaches for the AMD probe — asked of the handle
    /// rather than of `devices()` so the decision costs no driver call.
    pub fn is_available(&self) -> bool {
        self.nvml.is_ok()
    }
}

impl GpuProbe for NvmlProbe {
    fn devices(&self) -> Result<Vec<GpuMemory>, String> {
        let nvml = self.nvml.as_ref().map_err(Clone::clone)?;
        let count = nvml
            .device_count()
            .map_err(|e| format!("nvmlDeviceGetCount: {e}"))?;
        let mut out = Vec::with_capacity(count as usize);
        for index in 0..count {
            let dev = nvml
                .device_by_index(index)
                .map_err(|e| format!("nvmlDeviceGetHandleByIndex({index}): {e}"))?;
            let mem = dev
                .memory_info()
                .map_err(|e| format!("nvmlDeviceGetMemoryInfo({index}): {e}"))?;
            out.push(GpuMemory {
                index,
                name: dev.name().unwrap_or_else(|_| format!("GPU {index}")),
                total_bytes: mem.total,
                used_bytes: mem.used,
                free_bytes: mem.free,
            });
        }
        Ok(out)
    }

    fn source(&self) -> String {
        match &self.nvml {
            Ok(n) => match n.sys_driver_version() {
                Ok(v) => format!("NVML (driver {v})"),
                Err(_) => "NVML".into(),
            },
            Err(e) => format!("NVML unavailable: {e}"),
        }
    }

    fn process_support(&self) -> Result<(), String> {
        self.nvml.as_ref().map(|_| ()).map_err(Clone::clone)
    }

    /// Measured 2026-09-29 (RTX 4090, driver 615.71.09): every reading
    /// answers — see `live_nvml_power_readings` below for the check.
    fn power(&self) -> Result<Vec<GpuPower>, String> {
        use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
        let nvml = self.nvml.as_ref().map_err(Clone::clone)?;
        let count = nvml
            .device_count()
            .map_err(|e| format!("nvmlDeviceGetCount: {e}"))?;
        let mut out = Vec::with_capacity(count as usize);
        for index in 0..count {
            let dev = nvml
                .device_by_index(index)
                .map_err(|e| format!("nvmlDeviceGetHandleByIndex({index}): {e}"))?;
            out.push(GpuPower {
                index,
                power_mw: dev.power_usage().ok(),
                energy_mj: dev.total_energy_consumption().ok(),
                temperature_c: dev.temperature(TemperatureSensor::Gpu).ok(),
                clock_events: dev.current_throttle_reasons().ok().map(|r| r.bits()),
                power_limit_mw: dev.enforced_power_limit().ok(),
            });
        }
        Ok(out)
    }

    fn driver_version(&self) -> Option<String> {
        self.nvml.as_ref().ok()?.sys_driver_version().ok()
    }

    /// NVML's running-process lists, compute and graphics, per device.
    ///
    /// Measured 2026-09-27 (RTX 4090, driver 615.71.09, rootless podman): a
    /// llama-server container's host PID — what `podman inspect` reports as
    /// `State.Pid` — is listed as a compute process with its own allocation,
    /// within a second of its first answer. The desktop's processes (kwin,
    /// an editor) are listed too; they are the outside use §4.7 is about.
    ///
    /// The PIDs asked about are not needed: NVML answers for the whole device.
    fn processes(&self, _own: &[u32], _retired: &[u32]) -> Result<Vec<ProcessMemory>, String> {
        let nvml = self.nvml.as_ref().map_err(Clone::clone)?;
        let count = nvml
            .device_count()
            .map_err(|e| format!("nvmlDeviceGetCount: {e}"))?;
        let mut devices = Vec::with_capacity(count as usize);
        for index in 0..count {
            let dev = nvml
                .device_by_index(index)
                .map_err(|e| format!("nvmlDeviceGetHandleByIndex({index}): {e}"))?;
            let compute = dev
                .running_compute_processes()
                .map_err(|e| format!("nvmlDeviceGetComputeRunningProcesses({index}): {e}"))?;
            // A device without a graphics engine (a headless datacenter part)
            // answers NotSupported; that is an empty list, not a failure —
            // llama.cpp's CUDA contexts are compute processes anyway.
            let graphics = match dev.running_graphics_processes() {
                Ok(g) => g,
                Err(nvml_wrapper::error::NvmlError::NotSupported) => Vec::new(),
                Err(e) => {
                    return Err(format!(
                        "nvmlDeviceGetGraphicsRunningProcesses({index}): {e}"
                    ))
                }
            };
            devices.push(merge_device(&listed(&compute), &listed(&graphics)));
        }
        Ok(sum_devices(&devices))
    }
}

fn listed(infos: &[nvml_wrapper::struct_wrappers::device::ProcessInfo]) -> Vec<ProcessMemory> {
    use nvml_wrapper::enums::device::UsedGpuMemory;
    infos
        .iter()
        .map(|p| ProcessMemory {
            pid: p.pid,
            bytes: match p.used_gpu_memory {
                UsedGpuMemory::Used(b) => Some(b),
                UsedGpuMemory::Unavailable => None,
            },
        })
        .collect()
}

/// One device's two lists as one: per PID, the larger of the compute and the
/// graphics figure.
///
/// A process with both a compute and a graphics context is listed in both,
/// with the same allocation — adding them would count it twice. Within one
/// list a PID appears more than once only under MIG (once per GPU instance,
/// each with its own memory), so repeats *inside* a list are added. A figure
/// the driver could not give (`None`) loses to one it could; only a PID with
/// no figure anywhere stays `None`.
pub(crate) fn merge_device(
    compute: &[ProcessMemory],
    graphics: &[ProcessMemory],
) -> Vec<ProcessMemory> {
    fn per_pid(list: &[ProcessMemory]) -> std::collections::BTreeMap<u32, Option<u64>> {
        let mut out: std::collections::BTreeMap<u32, Option<u64>> = Default::default();
        for p in list {
            out.entry(p.pid)
                .and_modify(|b| {
                    *b = match (*b, p.bytes) {
                        (Some(a), Some(c)) => Some(a.saturating_add(c)),
                        // One MIG instance the driver cannot size makes the
                        // whole figure unknown.
                        _ => None,
                    }
                })
                .or_insert(p.bytes);
        }
        out
    }
    let mut merged = per_pid(compute);
    for (pid, g) in per_pid(graphics) {
        merged
            .entry(pid)
            .and_modify(|c| {
                *c = match (*c, g) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (Some(a), None) | (None, Some(a)) => Some(a),
                    (None, None) => None,
                }
            })
            .or_insert(g);
    }
    merged
        .into_iter()
        .map(|(pid, bytes)| ProcessMemory { pid, bytes })
        .collect()
}

/// Every device's list as one pooled list, the way the ledger pools the
/// devices themselves: a process on two cards holds the sum. One device that
/// cannot size it makes its total unknown.
pub(crate) fn sum_devices(devices: &[Vec<ProcessMemory>]) -> Vec<ProcessMemory> {
    let mut out: std::collections::BTreeMap<u32, Option<u64>> = Default::default();
    for p in devices.iter().flatten() {
        out.entry(p.pid)
            .and_modify(|b| {
                *b = match (*b, p.bytes) {
                    (Some(a), Some(c)) => Some(a.saturating_add(c)),
                    _ => None,
                }
            })
            .or_insert(p.bytes);
    }
    out.into_iter()
        .map(|(pid, bytes)| ProcessMemory { pid, bytes })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The benchmark's GPU readings on real hardware (benchmark design §2.3,
    /// WP1): power, the energy counter, temperature and the clock-event
    /// reasons all answer, the counter moves, and the numbers are in a sane
    /// range. Needs an NVIDIA card, hence ignored:
    /// `cargo test -p lmgw-core --lib live_nvml_power_readings -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs an NVIDIA GPU and driver"]
    fn live_nvml_power_readings() {
        let probe = NvmlProbe::detect();
        assert!(probe.is_available(), "NVML did not load");
        let first = probe.power().expect("power readings");
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let second = probe.power().expect("power readings");
        println!("driver {:?}", probe.driver_version());
        for (a, b) in first.iter().zip(&second) {
            println!("{a:?}\n{b:?}");
            let mw = a.power_mw.expect("power_usage");
            assert!((1_000..700_000).contains(&mw), "power {mw} mW");
            let t = a.temperature_c.expect("temperature");
            assert!((5..110).contains(&t), "temperature {t} C");
            assert!(a.clock_events.is_some(), "clock-event reasons");
            let limit = a.power_limit_mw.expect("enforced power limit");
            assert!(limit >= mw / 2, "limit {limit} mW");
            let (e0, e1) = (a.energy_mj.expect("energy"), b.energy_mj.expect("energy"));
            let joules = (e1 - e0) as f64 / 1000.0;
            println!(
                "energy over ~1 s: {joules:.2} J (power read {:.2} W)",
                mw as f64 / 1000.0
            );
            assert!(e1 > e0, "the energy counter did not move");
            assert!(joules < 700.0, "{joules} J in one second");
        }
        // How often the counter moves: read it every 5 ms for a second.
        let mut changes = 0;
        let mut last = probe.power().unwrap()[0].energy_mj;
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(5));
            let now = probe.power().unwrap()[0].energy_mj;
            changes += usize::from(now != last);
            last = now;
        }
        println!("the energy counter changed {changes} times in ~1 s of 5 ms reads");
        assert!(
            changes >= 5,
            "a counter this coarse cannot measure a 100 ms window"
        );
    }

    fn pm(pid: u32, bytes: Option<u64>) -> ProcessMemory {
        ProcessMemory { pid, bytes }
    }

    /// A process with a compute and a graphics context is listed twice with
    /// one allocation: the merge takes it once, at the larger figure.
    #[test]
    fn compute_and_graphics_merge_by_max_per_pid() {
        let merged = merge_device(
            &[pm(10, Some(1070)), pm(20, Some(300))],
            &[pm(20, Some(300)), pm(30, Some(90))],
        );
        assert_eq!(
            merged,
            vec![pm(10, Some(1070)), pm(20, Some(300)), pm(30, Some(90))]
        );
        // Not always equal: the larger one wins, the two are never added.
        let merged = merge_device(&[pm(20, Some(250))], &[pm(20, Some(300))]);
        assert_eq!(merged, vec![pm(20, Some(300))]);
    }

    /// `Unavailable` is unknown, never zero — but a figure from the other
    /// list is the same allocation and answers for it.
    #[test]
    fn an_unavailable_figure_stays_unknown_unless_the_other_list_has_one() {
        assert_eq!(
            merge_device(&[pm(10, None)], &[]),
            vec![pm(10, None)],
            "an lmgw process the driver cannot size must not read as 0 bytes"
        );
        assert_eq!(
            merge_device(&[pm(10, None)], &[pm(10, Some(64))]),
            vec![pm(10, Some(64))]
        );
        assert_eq!(
            merge_device(&[pm(10, None)], &[pm(10, None)]),
            vec![pm(10, None)]
        );
    }

    /// MIG lists one process once per GPU instance, each with its own memory:
    /// repeats inside one list add up.
    #[test]
    fn repeats_inside_one_list_add_up() {
        assert_eq!(
            merge_device(&[pm(10, Some(100)), pm(10, Some(50))], &[]),
            vec![pm(10, Some(150))]
        );
        assert_eq!(
            merge_device(&[pm(10, Some(100)), pm(10, None)], &[]),
            vec![pm(10, None)]
        );
    }

    /// Pooled like the ledger: a process on two cards holds the sum, and one
    /// card that cannot size it makes the total unknown.
    #[test]
    fn devices_pool_per_pid() {
        let pooled = sum_devices(&[
            vec![pm(10, Some(100)), pm(20, Some(5))],
            vec![pm(10, Some(40)), pm(30, None)],
        ]);
        assert_eq!(
            pooled,
            vec![pm(10, Some(140)), pm(20, Some(5)), pm(30, None)]
        );
        assert_eq!(
            sum_devices(&[vec![pm(10, Some(1))], vec![pm(10, None)]]),
            vec![pm(10, None)]
        );
    }
}
