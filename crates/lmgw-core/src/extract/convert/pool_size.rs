//! What the tool pool is sized from (document-conversion design §7.7, D19):
//! the logical cores lmgw may use, and the memory it may use, each read from
//! the machine.
//!
//! - Cores: [`std::thread::available_parallelism`], which respects a cgroup
//!   CPU quota (the server image's) and the process's affinity mask.
//! - Memory on Linux: the smallest cgroup v2 `memory.max` on the way from
//!   lmgw's own cgroup up to the root, when it is below the physical memory
//!   (the server image's limit, or a systemd unit's `MemoryMax=`), else the
//!   physical memory (`MemTotal` in `/proc/meminfo`).
//! - Memory elsewhere: not read yet, so the pool is sized from the cores
//!   alone and the page says so.

use std::path::Path;

use lmgw_api_types::documents::MemorySource;

/// The memory lmgw may use, and where that figure came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    pub bytes: u64,
    pub source: MemorySource,
}

/// The machine's facts the pool is sized from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Machine {
    pub cores: u32,
    /// `None` where it is not read yet (module doc).
    pub memory: Option<Memory>,
}

/// This machine, read now.
pub fn read() -> Machine {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    Machine {
        cores: u32::try_from(cores).unwrap_or(u32::MAX),
        memory: memory(),
    }
}

#[cfg(target_os = "linux")]
fn memory() -> Option<Memory> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let own = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    memory_at(&meminfo, &own, Path::new("/sys/fs/cgroup"))
}

#[cfg(not(target_os = "linux"))]
fn memory() -> Option<Memory> {
    None
}

/// [`memory`] from `/proc/meminfo`'s text, `/proc/self/cgroup`'s text and
/// the cgroup v2 mount at `root`.
// Read on Linux only so far; the tests read it everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn memory_at(meminfo: &str, own_cgroup: &str, root: &Path) -> Option<Memory> {
    let physical = mem_total(meminfo)?;
    let limit = cgroup_limit(own_cgroup, root);
    Some(match limit {
        Some(l) if l < physical => Memory {
            bytes: l,
            source: MemorySource::Cgroup,
        },
        _ => Memory {
            bytes: physical,
            source: MemorySource::Physical,
        },
    })
}

/// `MemTotal:  131072000 kB` in bytes (the kernel prints kB, always).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn mem_total(meminfo: &str) -> Option<u64> {
    let kb: u64 = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    kb.checked_mul(1024)
}

/// The smallest `memory.max` from the cgroup `own_cgroup` names (its
/// `0::/path` line, cgroup v2) up to `root`; `None` when every one is `max`
/// or none can be read.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn cgroup_limit(own_cgroup: &str, root: &Path) -> Option<u64> {
    let rel = own_cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))?
        .trim()
        .trim_start_matches('/');
    let mut dir = root.join(rel);
    let mut smallest: Option<u64> = None;
    loop {
        if let Ok(text) = std::fs::read_to_string(dir.join("memory.max")) {
            if let Ok(n) = text.trim().parse::<u64>() {
                smallest = Some(smallest.map_or(n, |s| s.min(n)));
            }
        }
        if dir == root || !dir.pop() || !dir.starts_with(root) {
            break;
        }
    }
    smallest
}

/// The pool's size from `machine` (design §7.7): its cores, at least 1.
/// The memory bounds the steps by their charges (`pool::charge`), not the
/// size (§14 performance round).
pub fn derive(machine: &Machine) -> u32 {
    machine.cores.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn meminfo(gib: u64) -> String {
        format!(
            "MemTotal:       {} kB\nMemFree:        1000 kB\n",
            gib * GIB / 1024
        )
    }

    #[test]
    fn the_size_is_the_cores() {
        let m = |cores, gib| Machine {
            cores,
            memory: Some(Memory {
                bytes: gib * GIB,
                source: MemorySource::Physical,
            }),
        };
        assert_eq!(derive(&m(32, 128)), 32);
        assert_eq!(derive(&m(32, 16)), 32);
        assert_eq!(derive(&m(0, 2)), 1);
        let unread = Machine {
            cores: 6,
            memory: None,
        };
        assert_eq!(derive(&unread), 6);
    }

    #[test]
    fn a_cgroup_limit_below_the_ram_is_the_memory() {
        let root = tempfile::tempdir().unwrap();
        let own = root.path().join("user.slice/lmgw.service");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(root.path().join("user.slice/memory.max"), "max\n").unwrap();
        std::fs::write(own.join("memory.max"), format!("{}\n", 8 * GIB)).unwrap();
        let m = memory_at(&meminfo(64), "0::/user.slice/lmgw.service\n", root.path()).unwrap();
        assert_eq!(m.bytes, 8 * GIB);
        assert_eq!(m.source, MemorySource::Cgroup);
    }

    #[test]
    fn no_limit_or_one_above_the_ram_is_the_physical_memory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("memory.max"), "max\n").unwrap();
        let m = memory_at(&meminfo(64), "0::/\n", root.path()).unwrap();
        assert_eq!((m.bytes, m.source), (64 * GIB, MemorySource::Physical));
        std::fs::write(root.path().join("memory.max"), format!("{}\n", 512 * GIB)).unwrap();
        let m = memory_at(&meminfo(64), "0::/\n", root.path()).unwrap();
        assert_eq!(m.source, MemorySource::Physical);
        // cgroup v1 (no `0::` line): the physical memory.
        let m = memory_at(&meminfo(64), "12:memory:/x\n", root.path()).unwrap();
        assert_eq!(m.source, MemorySource::Physical);
        assert_eq!(memory_at("nothing here", "0::/\n", root.path()), None);
    }
}
