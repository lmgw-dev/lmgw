//! Per-process GPU memory on Windows, from the WDDM kernel's own statistics
//! (Windows design §4.3).
//!
//! Under WDDM, NVML lists a process with `[N/A]` for its memory (§2 point 5),
//! so the per-process attribution the outside-VRAM fallback needs
//! (candidate-aliases §4.7, [`super::attribution`]) has no source there.
//! What Windows itself publishes is the video memory manager's accounting:
//! `D3DKMTQueryStatistics` with `D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT`
//! answers, for one process and one memory segment of one adapter, the bytes
//! that process has committed there. Summed over an adapter's *local*
//! segments (those that are not an aperture into system memory) it is what
//! Task Manager shows as a process's "Dedicated GPU memory".
//!
//! **Which adapters count.** The ledger's devices are NVML's, so the bytes
//! counted are those on the adapters that are NVML's devices, matched by PCI
//! bus, device and function ([`counted`]). An NVML device without a matching
//! adapter makes the per-process answer an error: the share on that card
//! could not be read, and a low share is the wrong direction.
//!
//! **What a process answer means**, as [`super::GpuProbe::processes`]
//! defines it for a probe asked per PID (like amdgpu's fdinfo): a process
//! with no bytes committed on a counted adapter is not listed (it holds
//! nothing the driver can see); one that is gone is not listed; one of
//! lmgw's own that cannot be opened or read is an error, and a tombstone that
//! cannot be opened or read is skipped (its PID may be someone else's by
//! now). A process that has exited while lmgw still holds a handle to it (an
//! engine its supervisor has not reaped yet) is gone.
//!
//! The kernel calls live in [`d3dkmt`] (Windows only); this module's
//! decisions are portable, so they are tested on every platform through a
//! [`Kernel`] the tests state.

use std::collections::BTreeSet;

use super::nvml::ProcessMemory;

#[cfg(windows)]
pub mod d3dkmt;
#[cfg(windows)]
mod probe;
#[cfg(windows)]
pub use probe::WddmProbe;
#[cfg(windows)]
pub mod toolhelp;
pub mod tree;

/// An adapter's place on the PCI bus. WDDM reports no PCI domain, so none is
/// compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciAddress {
    pub bus: u32,
    pub device: u32,
    pub function: u32,
}

impl PciAddress {
    /// NVML's bus id, `DDDDDDDD:BB:DD.F` (hexadecimal), as
    /// `nvmlDeviceGetPciInfo` answers it: `00000000:01:00.0`.
    pub fn from_nvml_bus_id(id: &str) -> Option<Self> {
        let mut parts = id.trim().trim_end_matches('\0').rsplitn(3, ':');
        let dev_fn = parts.next()?;
        let bus = parts.next()?;
        let (device, function) = dev_fn.split_once('.')?;
        Some(Self {
            bus: u32::from_str_radix(bus, 16).ok()?,
            device: u32::from_str_radix(device, 16).ok()?,
            function: u32::from_str_radix(function, 16).ok()?,
        })
    }
}

impl std::fmt::Display for PciAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }
}

/// One memory segment of an adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub id: u32,
    /// An aperture segment maps system memory; a local one is the card's own.
    pub aperture: bool,
    pub commit_limit: u64,
    /// What every process together has committed in it.
    pub committed: u64,
}

/// One WDDM adapter, as `D3DKMTEnumAdapters2` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapter {
    /// The adapter's LUID, high part first.
    pub luid: u64,
    /// `None` for an adapter with no PCI address (a software or indirect one).
    pub pci: Option<PciAddress>,
    pub segments: Vec<Segment>,
}

impl Adapter {
    /// The segments that are the card's own memory.
    pub fn local(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter().filter(|s| !s.aperture)
    }
}

/// An adapter the kernel enumerated and that could not be described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedAdapter {
    pub luid: u64,
    /// Where it is, when that much was read before the failure.
    pub pci: Option<PciAddress>,
    /// The call that failed and its NTSTATUS.
    pub why: String,
}

/// Every adapter `D3DKMTEnumAdapters2` lists: those described, and those
/// that could not be, kept with why, so an NVML device whose adapter is
/// among the second is reported under that cause ([`counted`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Adapters {
    pub listed: Vec<Adapter>,
    pub failed: Vec<FailedAdapter>,
}

/// The adapters that are `devices` (NVML's, by PCI address), or why one
/// device has none: no adapter there, the one there could not be read, or
/// more than one there.
///
/// An adapter listed twice (the same LUID) counts once.
pub fn counted<'a>(
    adapters: &'a Adapters,
    devices: &[PciAddress],
) -> Result<Vec<&'a Adapter>, String> {
    let mut seen = BTreeSet::new();
    let listed: Vec<&Adapter> = adapters
        .listed
        .iter()
        .filter(|a| seen.insert(a.luid))
        .collect();
    let failed: Vec<&FailedAdapter> = adapters
        .failed
        .iter()
        .filter(|f| seen.insert(f.luid))
        .collect();
    let mut out = Vec::new();
    for d in devices {
        let here: Vec<&Adapter> = listed
            .iter()
            .copied()
            .filter(|a| a.pci == Some(*d) && a.local().next().is_some())
            .collect();
        let failed_here: Vec<&FailedAdapter> = failed
            .iter()
            .copied()
            .filter(|f| f.pci == Some(*d))
            .collect();
        if here.len() + failed_here.len() > 1 {
            return Err(format!(
                "{} WDDM adapters are at PCI {d}, where NVML has one device ({}): which one \
                 holds the card's memory is ambiguous",
                here.len() + failed_here.len(),
                here.iter()
                    .map(|a| format!("LUID {:#018x}", a.luid))
                    .chain(
                        failed_here
                            .iter()
                            .map(|f| format!("LUID {:#018x}, not readable", f.luid))
                    )
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        if let Some(f) = failed_here.first() {
            return Err(format!(
                "the WDDM adapter at PCI {d}, where NVML has a device, could not be read: {}",
                f.why
            ));
        }
        let Some(a) = here.first() else {
            let mut why = format!(
                "no WDDM adapter with local memory is at PCI {d}, where NVML has a device \
                 ({} adapters listed: {})",
                listed.len(),
                listed
                    .iter()
                    .map(|a| match a.pci {
                        Some(p) => p.to_string(),
                        None => "no PCI address".into(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let unplaced: Vec<&&FailedAdapter> =
                failed.iter().filter(|f| f.pci.is_none()).collect();
            if !unplaced.is_empty() {
                why.push_str(&format!(
                    "; {} more could not be read before their address was, one of them may be \
                     it: {}",
                    unplaced.len(),
                    unplaced
                        .iter()
                        .map(|f| format!("LUID {:#018x}: {}", f.luid, f.why))
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
            }
            return Err(why);
        };
        out.push(*a);
    }
    Ok(out)
}

/// What reading one process's committed bytes answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// No process has this PID.
    Gone,
    /// The process exists and could not be read: why.
    Failed(String),
    /// The bytes committed over the counted adapters' local segments.
    Bytes(u64),
}

/// The operating system's side of [`processes`]: the real one is
/// [`d3dkmt`]'s, the tests state one.
pub trait Kernel {
    fn adapters(&self) -> Result<Adapters, String>;
    /// `pid`'s committed bytes over these adapters' local segments; once the
    /// process is open, by [`read_opened`].
    fn local_committed(&self, pid: u32, adapters: &[&Adapter]) -> Read;
}

/// A process opened for the statistics queries: the real one is
/// [`d3dkmt::Process`], the tests state one.
pub trait Opened {
    /// Its committed bytes over `adapters`' local segments.
    fn committed(&self, adapters: &[&Adapter]) -> Result<u64, String>;
    /// Whether it has exited while this handle keeps its object, and its
    /// PID, alive.
    fn exited(&self) -> bool;
}

/// What an opened process's reading answers: its bytes, or, when a query
/// fails, gone if the process has exited by then (an engine its supervisor
/// has not reaped yet: the native one drains the log readers before it
/// closes its handle), and the failure otherwise.
pub fn read_opened(p: &dyn Opened, adapters: &[&Adapter]) -> Read {
    match p.committed(adapters) {
        Ok(b) => Read::Bytes(b),
        Err(_) if p.exited() => Read::Gone,
        Err(e) => Read::Failed(e),
    }
}

/// Per-process GPU memory on `devices` for the PIDs asked about, by the
/// rules in the module docs.
pub fn processes(
    kernel: &dyn Kernel,
    devices: &[PciAddress],
    own: &[u32],
    retired: &[u32],
) -> Result<Vec<ProcessMemory>, String> {
    let adapters = kernel.adapters()?;
    let counted = counted(&adapters, devices)?;
    let own_set: BTreeSet<u32> = own.iter().copied().collect();
    let asked: BTreeSet<u32> = own_set.iter().chain(retired).copied().collect();
    let mut out = Vec::new();
    for pid in asked {
        match kernel.local_committed(pid, &counted) {
            Read::Gone | Read::Bytes(0) => {}
            Read::Bytes(b) => out.push(ProcessMemory {
                pid,
                bytes: Some(b),
            }),
            Read::Failed(why) if own_set.contains(&pid) => {
                return Err(format!("process {pid}'s GPU memory: {why}"))
            }
            // A tombstone's PID may be another user's process by now.
            Read::Failed(_) => {}
        }
    }
    Ok(out)
}
