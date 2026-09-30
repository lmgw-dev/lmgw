//! amdgpu's own memory counters, read out of sysfs — the AMD half of the
//! telemetry [`GpuProbe`] (the NVIDIA half is [`super::nvml`]).
//!
//! There is no NVML for AMD, and ROCm's SMI library is a separate install that
//! a Vulkan-only box (the common case on a Ryzen laptop) does not have. What
//! *is* always there, on any kernel that bound `amdgpu`, is the driver's own
//! accounting under `/sys/class/drm/card<N>/device/`:
//!
//! * `mem_info_vram_total` / `mem_info_vram_used` — the VRAM heap. On a
//!   discrete card that is the card's memory. On an APU it is the BIOS
//!   carve-out (`UMA_SPECIFIED`), which is often a few hundred MiB and is
//!   **not** the ceiling a model actually has.
//! * `mem_info_gtt_total` / `mem_info_gtt_used` — host pages the GPU may map.
//!   On an APU this is where everything past the carve-out comes from, so
//!   ignoring it would understate an iGPU's capacity by an order of magnitude.
//! * `mem_info_vram_type` — `DDR5`/`LPDDR5`/… when the "VRAM" is system
//!   memory (an APU), `GDDR6`/`HBM`/… when it is a real card. This is the
//!   whole APU test; there is no `is_apu` flag to read.
//!
//! **What an APU's capacity is taken to be.** `total` = carve-out + GTT limit,
//! `used` = what the driver reports in both pools, and `free` is the carve-out's
//! free bytes plus *at most* `MemAvailable` of the free GTT — because GTT is
//! host RAM, and the kernel's GTT limit says nothing about whether the machine
//! can spare it right now. That last clamp is the difference between "the
//! driver would let you map 8 GiB" and "the host has 8 GiB to give". Where
//! that arithmetic is not wanted, `vram.budget_mb` is a declared number and
//! overrides all of it (see [`super`]'s ledger).
//!
//! All files are world-readable, so this needs no group membership, no device
//! node and no root — unlike the ROCm stack it stands in for.
//!
//! **Per process** (candidate-aliases design §4.7) there is no driver-wide
//! list like NVML's. What amdgpu does publish is DRM's per-client accounting
//! in `/proc/<pid>/fdinfo/<fd>`, readable for any process running as the same
//! user — which a rootless podman container's processes are. So this probe
//! answers per PID, for exactly the PIDs lmgw asks about (its own containers'
//! process trees): every fd whose fdinfo says `drm-driver: amdgpu`, one count
//! per DRM client (`drm-client-id`, per device — a dup'd or inherited fd is
//! the same client), matched to a probed card by `drm-pdev`. Two generations
//! of keys exist: amdgpu's own `drm-memory-vram`/`drm-memory-gtt` and the
//! DRM-common `drm-resident-vram`/`drm-resident-gtt`; newer kernels print
//! both, and resident is preferred. GTT counts exactly where the card's
//! capacity counts it — on an APU — so a share and the capacity it is a share
//! of always use the same definition.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::nvml::{GpuMemory, GpuProbe, ProcessMemory};
use crate::hf::fmt_bytes;

/// Where the kernel publishes one directory per DRM device.
const DRM_ROOT: &str = "/sys/class/drm";

/// The kernel's own estimate of what can still be allocated without swapping —
/// the bound on how much free GTT an APU may actually be handed.
const MEMINFO: &str = "/proc/meminfo";

/// Where the per-process fdinfo lives.
const PROC_ROOT: &str = "/proc";

/// amdgpu memory telemetry from sysfs.
pub struct AmdSysfsProbe {
    root: PathBuf,
    meminfo: PathBuf,
    /// `/proc`, for the per-process fdinfo; injectable so the tests can state
    /// what a process's fds look like.
    proc_root: PathBuf,
}

impl AmdSysfsProbe {
    /// `Some` only when at least one DRM device exposes amdgpu's memory
    /// counters. A host with no AMD GPU — or with one whose driver is not
    /// `amdgpu` — gets `None`, so [`super::detect_probe`] can fall through to
    /// the honest "no telemetry" state instead of installing a probe that
    /// would report an empty device list forever.
    pub fn detect() -> Option<Self> {
        Self::at(DRM_ROOT, MEMINFO)
    }

    /// [`Self::detect`] against an injected tree. The tests state what sysfs
    /// looks like; nothing else has a reason to call this.
    pub fn at(root: impl Into<PathBuf>, meminfo: impl Into<PathBuf>) -> Option<Self> {
        let probe = Self {
            root: root.into(),
            meminfo: meminfo.into(),
            proc_root: PathBuf::from(PROC_ROOT),
        };
        match probe.cards() {
            Ok(cards) if !cards.is_empty() => Some(probe),
            _ => None,
        }
    }

    /// Every `card<N>/device` directory that carries amdgpu's memory counters,
    /// ordered by card number.
    ///
    /// The `mem_info_vram_total` test is what makes this amdgpu-specific:
    /// `/sys/class/drm` also holds every other driver's cards, the connector
    /// directories (`card1-DP-1` — excluded by the all-digits suffix parse)
    /// and the render nodes.
    fn cards(&self) -> Result<Vec<(u32, PathBuf)>, String> {
        let entries =
            std::fs::read_dir(&self.root).map_err(|e| format!("{}: {e}", self.root.display()))?;
        let mut cards: Vec<(u32, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(index) = name
                .strip_prefix("card")
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let dir = entry.path().join("device");
            if dir.join("mem_info_vram_total").is_file() {
                cards.push((index, dir));
            }
        }
        cards.sort_by_key(|(index, _)| *index);
        Ok(cards)
    }

    /// [`Self::at`]'s probe, reading per-process fdinfo from `root` instead
    /// of `/proc`. For the tests.
    pub fn with_proc_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.proc_root = root.into();
        self
    }

    fn mem_available(&self) -> Option<u64> {
        let text = std::fs::read_to_string(&self.meminfo).ok()?;
        let line = text.lines().find(|l| l.starts_with("MemAvailable:"))?;
        // `MemAvailable:   15845960 kB` — the kernel prints kB, always.
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        kb.checked_mul(1024)
    }
}

fn read_u64(path: PathBuf) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_str(path: PathBuf) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?.trim().to_string();
    // `product_name` is present but empty (or a literal `0`) on most consumer
    // parts; both mean "the driver has no name for this", not a name.
    (!s.is_empty() && s != "0").then_some(s)
}

/// True when the "VRAM" a card reports is really system memory — i.e. it is an
/// APU. The strings are the kernel's own (`amdgpu_vram_names`): `DDR2`…`DDR5`
/// and `LPDDR4`/`LPDDR5` are system memory; `GDDR*`/`HBM` are a discrete card.
fn is_system_memory(vram_type: &str) -> bool {
    let t = vram_type.trim().to_ascii_uppercase();
    t.starts_with("LPDDR") || (t.starts_with("DDR") && !t.starts_with("GDDR"))
}

impl GpuProbe for AmdSysfsProbe {
    fn devices(&self) -> Result<Vec<GpuMemory>, String> {
        let cards = self.cards()?;
        if cards.is_empty() {
            return Err(format!(
                "no amdgpu memory counters under {} (the card was there at startup)",
                self.root.display()
            ));
        }
        // Read once for the whole sweep: every APU on the box is bounded by the
        // same host memory, and re-reading per card would let two devices be
        // told they can each have all of it.
        let available = self.mem_available();
        let mut out = Vec::with_capacity(cards.len());
        for (position, (card, dir)) in cards.iter().enumerate() {
            let vram_total = read_u64(dir.join("mem_info_vram_total"))
                .ok_or_else(|| format!("{}/mem_info_vram_total: unreadable", dir.display()))?;
            let vram_used = read_u64(dir.join("mem_info_vram_used")).unwrap_or(0);
            let vram_free = vram_total.saturating_sub(vram_used);
            let vram_type = read_str(dir.join("mem_info_vram_type")).unwrap_or_default();

            let (total, used, free, pool) = if is_system_memory(&vram_type) {
                let gtt_total = read_u64(dir.join("mem_info_gtt_total")).unwrap_or(0);
                let gtt_used = read_u64(dir.join("mem_info_gtt_used")).unwrap_or(0);
                let gtt_free = gtt_total.saturating_sub(gtt_used);
                // The clamp the module docs are about: free GTT is only real
                // if the host still has the pages.
                let usable_gtt = match available {
                    Some(a) => gtt_free.min(a),
                    None => gtt_free,
                };
                (
                    vram_total.saturating_add(gtt_total),
                    vram_used.saturating_add(gtt_used),
                    vram_free.saturating_add(usable_gtt),
                    format!(
                        "APU — {} {vram_type} carve-out + {} GTT",
                        fmt_bytes(vram_total),
                        fmt_bytes(gtt_total)
                    ),
                )
            } else {
                (vram_total, vram_used, vram_free, vram_type.clone())
            };

            let model = read_str(dir.join("product_name")).unwrap_or_else(|| {
                format!(
                    "AMD GPU {}",
                    read_str(dir.join("device")).unwrap_or_else(|| "(unknown id)".into())
                )
            });
            let name = if pool.is_empty() {
                format!("{model} (card{card})")
            } else {
                format!("{model} (card{card}, {pool})")
            };

            out.push(GpuMemory {
                // Enumeration order, not the card number: the index is a
                // position in this list, and `card1` being the only card on a
                // box is normal (card0 is often a display-only device).
                index: position as u32,
                name,
                total_bytes: total,
                used_bytes: used,
                free_bytes: free,
            });
        }
        Ok(out)
    }

    fn source(&self) -> String {
        format!("amdgpu sysfs ({})", self.root.display())
    }

    fn process_support(&self) -> Result<(), String> {
        Ok(())
    }

    fn processes(&self, own: &[u32], retired: &[u32]) -> Result<Vec<ProcessMemory>, String> {
        // Which PCI device each probed card is, and whether its capacity
        // counts GTT (the APU case in `devices`). A card whose address cannot
        // be read is left out: its memory then cannot be attributed, which
        // reads as "not attributed" — the conservative direction.
        let mut counted: HashMap<String, bool> = HashMap::new();
        for (_, dir) in self.cards()? {
            let Some(pdev) = pci_address(&dir) else {
                continue;
            };
            let apu =
                read_str(dir.join("mem_info_vram_type")).is_some_and(|t| is_system_memory(&t));
            counted.insert(pdev, apu);
        }
        let mut clients_seen: HashSet<(String, u64)> = HashSet::new();
        let mut out = Vec::new();
        // lmgw's own processes first, so a client a tombstone shares with one
        // of them (an inherited fd) is counted for the live container.
        let asked = own.iter().map(|&p| (p, true)).chain(
            retired
                .iter()
                .filter(|p| !own.contains(p))
                .map(|&p| (p, false)),
        );
        for (pid, is_own) in asked {
            let clients = match drm_clients(&self.proc_root, pid) {
                Ok(c) => c,
                // Gone since it was named: it holds nothing.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                // A tombstone's PID can have been reused by a process this
                // user may not read (EACCES): whatever it holds is not lmgw's,
                // and failing here would fail every pass for as long as that
                // process lives.
                Err(_) if !is_own => continue,
                Err(e) => return Err(format!("{}/{pid}/fdinfo: {e}", self.proc_root.display())),
            };
            if let Some(bytes) = attribute_clients(&clients, &counted, &mut clients_seen) {
                out.push(ProcessMemory { pid, bytes });
            }
        }
        Ok(out)
    }
}

/// One DRM client an fd belongs to, as its fdinfo describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DrmClient {
    /// `drm-pdev`: the PCI address of the device, e.g. `0000:03:00.0`.
    pub pdev: String,
    /// `drm-client-id`: one per open of the device, shared by every fd that
    /// was dup'd or inherited from it.
    pub client_id: u64,
    /// Bytes resident in VRAM, `None` when the fdinfo carries no VRAM key.
    pub vram: Option<u64>,
    /// Bytes resident in GTT (host pages the GPU maps).
    pub gtt: Option<u64>,
}

/// Parse one `/proc/<pid>/fdinfo/<fd>`. `None` for anything that is not an
/// amdgpu DRM client — a socket, a file, another driver's render node.
pub(crate) fn parse_fdinfo(text: &str) -> Option<DrmClient> {
    let mut driver = None;
    let mut pdev = None;
    let mut client_id = None;
    let (mut legacy_vram, mut legacy_gtt) = (None, None);
    let (mut resident_vram, mut resident_gtt) = (None, None);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "drm-driver" => driver = Some(value),
            "drm-pdev" => pdev = Some(value.to_string()),
            "drm-client-id" => client_id = value.parse::<u64>().ok(),
            "drm-memory-vram" => legacy_vram = parse_size(value),
            "drm-memory-gtt" => legacy_gtt = parse_size(value),
            "drm-resident-vram" => resident_vram = parse_size(value),
            "drm-resident-gtt" => resident_gtt = parse_size(value),
            _ => {}
        }
    }
    if driver != Some("amdgpu") {
        return None;
    }
    Some(DrmClient {
        pdev: pdev?,
        client_id: client_id?,
        vram: resident_vram.or(legacy_vram),
        gtt: resident_gtt.or(legacy_gtt),
    })
}

/// A DRM fdinfo size: a number, optionally followed by a unit. The kernel
/// prints the largest of bytes, `KiB` and `MiB` that divides the value evenly
/// (`drm_fdinfo_print_size`); amdgpu's own legacy keys are always `KiB`.
/// `GiB` is accepted for a kernel that grows one.
fn parse_size(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let n: u64 = parts.next()?.parse().ok()?;
    let unit: u64 = match parts.next() {
        None | Some("B") => 1,
        Some("KiB") | Some("kB") => 1 << 10,
        Some("MiB") => 1 << 20,
        Some("GiB") => 1 << 30,
        Some(_) => return None,
    };
    n.checked_mul(unit)
}

/// Every amdgpu DRM client one process holds an fd to. `NotFound` when the
/// process is gone.
fn drm_clients(proc_root: &Path, pid: u32) -> std::io::Result<Vec<DrmClient>> {
    let dir = proc_root.join(pid.to_string()).join("fdinfo");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)?.flatten() {
        // An fd closed between the listing and the read is simply not there.
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Some(c) = parse_fdinfo(&text) {
            out.push(c);
        }
    }
    Ok(out)
}

/// One process's memory on the probed cards, from its DRM clients.
///
/// `None` = the process holds no client on any probed card, i.e. the driver
/// lists nothing for it. `Some(None)` = it does, but a client carries no
/// memory keys, so how much it holds is unknown. A client already counted for
/// another process (an inherited fd) adds nothing a second time; `seen` is
/// shared across the PIDs of one answer for that reason.
pub(crate) fn attribute_clients(
    clients: &[DrmClient],
    counted: &HashMap<String, bool>,
    seen: &mut HashSet<(String, u64)>,
) -> Option<Option<u64>> {
    let mut listed = false;
    let mut total: Option<u64> = Some(0);
    for c in clients {
        let Some(&counts_gtt) = counted.get(&c.pdev) else {
            continue;
        };
        listed = true;
        if !seen.insert((c.pdev.clone(), c.client_id)) {
            continue;
        }
        let bytes = match (c.vram, counts_gtt) {
            (Some(v), false) => Some(v),
            (Some(v), true) => c.gtt.map(|g| v.saturating_add(g)),
            (None, _) => None,
        };
        total = match (total, bytes) {
            (Some(t), Some(b)) => Some(t.saturating_add(b)),
            _ => None,
        };
    }
    listed.then_some(total)
}

/// The PCI address of a card's `device` directory: `PCI_SLOT_NAME` from its
/// `uevent`, else the name the `device` symlink resolves to (sysfs links it
/// to `…/0000:03:00.0`).
fn pci_address(device_dir: &Path) -> Option<String> {
    if let Ok(text) = std::fs::read_to_string(device_dir.join("uevent")) {
        if let Some(slot) = text
            .lines()
            .find_map(|l| l.strip_prefix("PCI_SLOT_NAME="))
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(slot.to_string());
        }
    }
    let resolved = std::fs::canonicalize(device_dir).ok()?;
    let name = resolved.file_name()?.to_string_lossy().into_owned();
    // `domain:bus:device.function`; anything else is a non-PCI device (or a
    // test tree), and matching `drm-pdev` against it would be a guess.
    (name.matches(':').count() == 2 && name.contains('.')).then_some(name)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// Build a fake `/sys/class/drm` tree: `(card name, [(file, contents)])`.
    fn sysfs(cards: &[(&str, &[(&str, &str)])]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (card, files) in cards {
            let device = dir.path().join(card).join("device");
            std::fs::create_dir_all(&device).unwrap();
            for (name, contents) in *files {
                std::fs::write(device.join(name), contents).unwrap();
            }
        }
        dir
    }

    fn meminfo(dir: &Path, available_kb: u64) -> PathBuf {
        let path = dir.join("meminfo");
        std::fs::write(
            &path,
            format!("MemTotal:       32000000 kB\nMemAvailable:   {available_kb} kB\n"),
        )
        .unwrap();
        path
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn discrete_card_reports_vram_only() {
        let tree = sysfs(&[(
            "card1",
            &[
                ("mem_info_vram_total", "17163091968"),
                ("mem_info_vram_used", "1073741824"),
                ("mem_info_vram_type", "GDDR6"),
                // Present, and deliberately ignored: a discrete card's GTT is
                // not memory a model's weights can live in.
                ("mem_info_gtt_total", "8589934592"),
                ("mem_info_gtt_used", "0"),
            ],
        )]);
        let mem = meminfo(tree.path(), 16_000_000);
        let probe = AmdSysfsProbe::at(tree.path(), mem).expect("card detected");
        let devices = probe.devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].total_bytes, 17_163_091_968);
        assert_eq!(devices[0].used_bytes, 1_073_741_824);
        assert_eq!(devices[0].free_bytes, 17_163_091_968 - 1_073_741_824);
        assert!(devices[0].name.contains("GDDR6"), "{}", devices[0].name);
        assert!(!devices[0].name.contains("APU"), "{}", devices[0].name);
    }

    #[test]
    fn apu_adds_gtt_to_the_carve_out() {
        let tree = sysfs(&[(
            "card0",
            &[
                // The 512 MiB BIOS carve-out a Ryzen laptop ships with.
                ("mem_info_vram_total", "536870912"),
                ("mem_info_vram_used", "134217728"),
                ("mem_info_vram_type", "LPDDR5"),
                ("mem_info_gtt_total", "16106127360"), // 15 GiB
                ("mem_info_gtt_used", "1073741824"),   // 1 GiB
            ],
        )]);
        // Plenty available, so the clamp is not what decides the answer.
        let mem = meminfo(tree.path(), 20 * 1024 * 1024);
        let probe = AmdSysfsProbe::at(tree.path(), mem).unwrap();
        let d = &probe.devices().unwrap()[0];
        assert_eq!(d.total_bytes, 536_870_912 + 16_106_127_360);
        assert_eq!(d.used_bytes, 134_217_728 + 1_073_741_824);
        assert_eq!(
            d.free_bytes,
            (536_870_912 - 134_217_728) + (16_106_127_360 - 1_073_741_824)
        );
        assert!(d.name.contains("APU"), "{}", d.name);
    }

    #[test]
    fn apu_free_is_clamped_by_host_memory() {
        let tree = sysfs(&[(
            "card0",
            &[
                ("mem_info_vram_total", "536870912"),
                ("mem_info_vram_used", "0"),
                ("mem_info_vram_type", "DDR5"),
                ("mem_info_gtt_total", "16106127360"),
                ("mem_info_gtt_used", "0"),
            ],
        )]);
        // 2 GiB available: the driver would map 15 GiB of GTT, the host cannot
        // spare it, and the smaller number is what admission must plan against.
        let mem = meminfo(tree.path(), 2 * 1024 * 1024);
        let probe = AmdSysfsProbe::at(tree.path(), mem).unwrap();
        let d = &probe.devices().unwrap()[0];
        assert_eq!(d.free_bytes, 536_870_912 + 2 * GIB);
        // The ceiling is still the driver's, only the free figure is clamped.
        assert_eq!(d.total_bytes, 536_870_912 + 16_106_127_360);
    }

    #[test]
    fn connectors_and_foreign_drivers_are_not_cards() {
        let tree = sysfs(&[
            // An NVIDIA card: same directory shape, no amdgpu counters.
            ("card1", &[("vendor", "0x10de")]),
            ("card1-DP-1", &[("status", "connected")]),
        ]);
        let mem = meminfo(tree.path(), 8_000_000);
        assert!(AmdSysfsProbe::at(tree.path(), mem).is_none());
    }

    #[test]
    fn missing_gtt_files_degrade_to_the_carve_out() {
        let tree = sysfs(&[(
            "card0",
            &[
                ("mem_info_vram_total", "536870912"),
                ("mem_info_vram_used", "0"),
                ("mem_info_vram_type", "DDR4"),
            ],
        )]);
        let mem = meminfo(tree.path(), 8_000_000);
        let probe = AmdSysfsProbe::at(tree.path(), mem).unwrap();
        let d = &probe.devices().unwrap()[0];
        assert_eq!(d.total_bytes, 536_870_912);
        assert_eq!(d.free_bytes, 536_870_912);
    }

    #[test]
    fn cards_are_ordered_by_card_number() {
        let vram = |n: &str| -> Vec<(&str, String)> {
            vec![
                ("mem_info_vram_total", n.to_string()),
                ("mem_info_vram_type", "GDDR6".to_string()),
            ]
        };
        let dir = tempfile::tempdir().unwrap();
        for (card, total) in [("card10", "10"), ("card2", "2")] {
            let device = dir.path().join(card).join("device");
            std::fs::create_dir_all(&device).unwrap();
            for (name, contents) in vram(total) {
                std::fs::write(device.join(name), contents).unwrap();
            }
        }
        let mem = meminfo(dir.path(), 8_000_000);
        let probe = AmdSysfsProbe::at(dir.path(), mem).unwrap();
        let devices = probe.devices().unwrap();
        assert_eq!(devices[0].total_bytes, 2);
        assert_eq!(devices[0].index, 0);
        assert!(devices[0].name.contains("card2"), "{}", devices[0].name);
        assert_eq!(devices[1].total_bytes, 10);
        assert_eq!(devices[1].index, 1);
    }

    /// A real amdgpu fdinfo from a 6.x kernel carries both generations of
    /// memory keys: resident wins, and `KiB`/`MiB` are both units.
    #[test]
    fn fdinfo_prefers_resident_keys_and_reads_units() {
        let text = "pos:\t0\nflags:\t02100002\nmnt_id:\t26\nino:\t1073\n\
                    drm-driver:\tamdgpu\ndrm-client-id:\t41\ndrm-pdev:\t0000:03:00.0\n\
                    pasid:\t32781\ndrm-memory-vram:\t4196352 KiB\ndrm-memory-gtt:\t2048 KiB\n\
                    drm-memory-cpu:\t0 KiB\ndrm-total-vram:\t4100 MiB\n\
                    drm-resident-vram:\t4098 MiB\ndrm-resident-gtt:\t2 MiB\n\
                    drm-engine-gfx:\t1234 ns\n";
        let c = parse_fdinfo(text).expect("an amdgpu client");
        assert_eq!(c.pdev, "0000:03:00.0");
        assert_eq!(c.client_id, 41);
        assert_eq!(c.vram, Some(4098 * 1024 * 1024), "resident, not legacy");
        assert_eq!(c.gtt, Some(2 * 1024 * 1024));
    }

    /// An older kernel prints only amdgpu's own keys, always in KiB.
    #[test]
    fn fdinfo_reads_the_legacy_keys_alone() {
        let text = "drm-driver:\tamdgpu\ndrm-pdev:\t0000:c1:00.0\ndrm-client-id:\t7\n\
                    drm-memory-vram:\t1024 KiB\ndrm-memory-gtt:\t512 KiB\n";
        let c = parse_fdinfo(text).unwrap();
        assert_eq!(c.vram, Some(1024 * 1024));
        assert_eq!(c.gtt, Some(512 * 1024));
        // A bare number is bytes (the kernel omits the unit when the value is
        // not a whole KiB).
        let c = parse_fdinfo(
            "drm-driver:\tamdgpu\ndrm-pdev:\tp\ndrm-client-id:\t1\ndrm-resident-vram:\t4095\n",
        )
        .unwrap();
        assert_eq!(c.vram, Some(4095));
    }

    /// Sockets, files and other drivers' render nodes are not amdgpu clients.
    #[test]
    fn fdinfo_ignores_everything_that_is_not_an_amdgpu_client() {
        assert_eq!(parse_fdinfo("pos:\t0\nflags:\t02\nmnt_id:\t15\n"), None);
        assert_eq!(
            parse_fdinfo(
                "drm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\ndrm-client-id:\t3\n\
                 drm-resident-system0:\t10 MiB\n"
            ),
            None
        );
        // No client id or device: nothing to dedupe or map it by.
        assert_eq!(
            parse_fdinfo("drm-driver:\tamdgpu\ndrm-memory-vram:\t1 KiB\n"),
            None
        );
    }

    fn client(pdev: &str, id: u64, vram: Option<u64>, gtt: Option<u64>) -> DrmClient {
        DrmClient {
            pdev: pdev.into(),
            client_id: id,
            vram,
            gtt,
        }
    }

    /// One client, however many fds point at it, and wherever it was
    /// inherited to; GTT only on a card whose capacity counts it.
    #[test]
    fn clients_dedupe_by_id_per_device_and_count_gtt_only_on_an_apu() {
        let counted: HashMap<String, bool> =
            [("dgpu".to_string(), false), ("apu".to_string(), true)].into();
        let mut seen = HashSet::new();
        let a = [
            client("dgpu", 1, Some(100), Some(7)),
            // The same client through a dup'd fd.
            client("dgpu", 1, Some(100), Some(7)),
            client("apu", 1, Some(10), Some(5)),
            // A device the probe does not count: ignored, never a guess.
            client("elsewhere", 9, Some(1000), None),
        ];
        assert_eq!(
            attribute_clients(&a, &counted, &mut seen),
            Some(Some(100 + 10 + 5)),
            "discrete: VRAM only; APU: VRAM + GTT; one count per client"
        );
        // A child that inherited the discrete card's fd adds nothing, but is
        // still listed.
        assert_eq!(
            attribute_clients(&[client("dgpu", 1, Some(100), None)], &counted, &mut seen),
            Some(Some(0))
        );
        // Only on uncounted devices: not listed at all.
        assert_eq!(
            attribute_clients(
                &[client("elsewhere", 2, Some(1), None)],
                &counted,
                &mut seen
            ),
            None
        );
        // A client with no memory keys: listed, amount unknown.
        assert_eq!(
            attribute_clients(&[client("dgpu", 3, None, None)], &counted, &mut seen),
            Some(None)
        );
    }

    /// The whole probe against a fake sysfs + `/proc`: `drm-pdev` maps to the
    /// card through its uevent, and the answer is exactly the PIDs asked about.
    #[test]
    fn processes_reads_the_asked_pids_fdinfo_against_the_probed_cards() {
        let tree = sysfs(&[(
            "card1",
            &[
                ("mem_info_vram_total", "17163091968"),
                ("mem_info_vram_used", "1073741824"),
                ("mem_info_vram_type", "GDDR6"),
                ("uevent", "DRIVER=amdgpu\nPCI_SLOT_NAME=0000:03:00.0\n"),
            ],
        )]);
        let proc_dir = tree.path().join("proc");
        let fd = |pid: u32, fd: u32, text: &str| {
            let dir = proc_dir.join(pid.to_string()).join("fdinfo");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(fd.to_string()), text).unwrap();
        };
        let drm = |id: u64, vram_kib: u64| {
            format!(
                "drm-driver:\tamdgpu\ndrm-pdev:\t0000:03:00.0\ndrm-client-id:\t{id}\n\
                 drm-memory-vram:\t{vram_kib} KiB\ndrm-memory-gtt:\t64 KiB\n"
            )
        };
        fd(100, 0, "pos:\t0\nflags:\t0\n");
        fd(100, 5, &drm(11, 2048));
        fd(100, 6, &drm(11, 2048));
        fd(200, 4, &drm(12, 1024));
        // A process nobody asked about: never read.
        fd(300, 4, &drm(13, 999));
        // A process with no GPU fd at all.
        fd(400, 0, "pos:\t0\n");

        let mem = meminfo(tree.path(), 16_000_000);
        let probe = AmdSysfsProbe::at(tree.path(), mem)
            .unwrap()
            .with_proc_root(&proc_dir);
        assert!(probe.process_support().is_ok());
        let got = probe.processes(&[100, 200, 400, 500], &[]).unwrap();
        assert_eq!(
            got,
            vec![
                ProcessMemory {
                    pid: 100,
                    bytes: Some(2048 * 1024)
                },
                ProcessMemory {
                    pid: 200,
                    bytes: Some(1024 * 1024)
                },
            ],
            "discrete card: GTT not counted; 400 has no GPU fd; 500 is gone"
        );

        // A PID whose fdinfo cannot be read for any reason but "gone" (here
        // not a directory; on a live box EACCES, a tombstone's PID reused by
        // another user's process): not listed when it is a tombstone, and an
        // error when it is one of lmgw's live processes.
        std::fs::create_dir_all(proc_dir.join("600")).unwrap();
        std::fs::write(proc_dir.join("600").join("fdinfo"), "not a directory").unwrap();
        let got = probe.processes(&[200], &[600, 500, 100]).unwrap();
        assert_eq!(
            got,
            vec![
                ProcessMemory {
                    pid: 200,
                    bytes: Some(1024 * 1024)
                },
                ProcessMemory {
                    pid: 100,
                    bytes: Some(2048 * 1024)
                },
            ],
            "the unreadable tombstone is simply not listed"
        );
        let err = probe.processes(&[200, 600], &[]).unwrap_err();
        assert!(err.contains("600"), "{err}");
    }

    #[test]
    fn vram_type_decides_apu_not_size() {
        assert!(is_system_memory("DDR5"));
        assert!(is_system_memory("LPDDR5"));
        assert!(is_system_memory("ddr4"));
        assert!(!is_system_memory("GDDR6"));
        assert!(!is_system_memory("HBM"));
        assert!(!is_system_memory("unknown"));
        assert!(!is_system_memory(""));
    }
}
