//! Facts about the machine lmgw runs on, read once per process.
//!
//! So far one: its CPUs, which an audio row switched to the CPU uses as its
//! thread count when it names none ([`crate::runtime::audio::engine_settings`]).
//! Read once because they do not change under a running gateway, and every
//! render of a CPU row's `server.json` asks.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;

use serde::Serialize;

/// Where [`HostCpu::physical_cores`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoreSource {
    /// Counted from the kernel's CPU topology: one core per distinct
    /// `core_cpus_list` among the online CPUs.
    Topology,
    /// The topology could not be read, so the logical CPU count stands in.
    Logical,
}

impl CoreSource {
    /// What a surface calls the figure ("16 · physical cores").
    pub fn describe(self) -> &'static str {
        match self {
            Self::Topology => "physical cores",
            Self::Logical => "logical CPUs, topology unreadable",
        }
    }
}

/// This machine's CPUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct HostCpu {
    /// Physical cores (16 on a 16-core, 32-thread CPU), or the logical
    /// count when the topology is unreadable ([`Self::source`]). Why cores
    /// and not threads: audio.cpp's CPU kernels gained nothing from the SMT
    /// siblings in the CPU ASR eval (8 threads about 110 ms per check, 16
    /// about 120 ms), and more threads than cores only add contention.
    pub physical_cores: usize,
    /// Online logical CPUs, SMT siblings included.
    pub logical_cpus: usize,
    pub source: CoreSource,
}

/// The host's CPUs, read from sysfs on first use.
pub fn cpu() -> HostCpu {
    static CPU: OnceLock<HostCpu> = OnceLock::new();
    *CPU.get_or_init(|| read_cpu(Path::new("/sys/devices/system/cpu")))
}

/// [`cpu`] for the sysfs CPU directory at `root`
/// (`/sys/devices/system/cpu`): the online list, and each online CPU's
/// `topology/core_cpus_list` — the CPUs that share its core, so one distinct
/// list is one physical core. Anything unreadable falls back to the logical
/// count the standard library reports.
pub(crate) fn read_cpu(root: &Path) -> HostCpu {
    match topology(root) {
        Some((cores, logical)) => HostCpu {
            physical_cores: cores,
            logical_cpus: logical,
            source: CoreSource::Topology,
        },
        None => {
            let logical = std::thread::available_parallelism().map_or(1, |n| n.get());
            HostCpu {
                physical_cores: logical,
                logical_cpus: logical,
                source: CoreSource::Logical,
            }
        }
    }
}

/// `(physical cores, online logical CPUs)`, or `None` when any part of the
/// topology cannot be read.
fn topology(root: &Path) -> Option<(usize, usize)> {
    let online = std::fs::read_to_string(root.join("online")).ok()?;
    let ranges = crate::backends::run::cpus::ranges(&online)?;
    let mut logical = 0usize;
    let mut cores = BTreeSet::new();
    for (lo, hi) in ranges {
        for n in lo..=hi {
            logical += 1;
            let list = root.join(format!("cpu{n}/topology/core_cpus_list"));
            cores.insert(std::fs::read_to_string(list).ok()?.trim().to_string());
        }
    }
    (logical > 0).then_some((cores.len(), logical))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sysfs CPU directory with `online` and each CPU's core list.
    fn tree(online: &str, cores: &[(u32, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("online"), format!("{online}\n")).unwrap();
        for (n, list) in cores {
            let topo = dir.path().join(format!("cpu{n}/topology"));
            std::fs::create_dir_all(&topo).unwrap();
            std::fs::write(topo.join("core_cpus_list"), format!("{list}\n")).unwrap();
        }
        dir
    }

    #[test]
    fn the_cores_are_the_distinct_core_lists_of_the_online_cpus() {
        // Four cores with two threads each, numbered the way a Ryzen does.
        let t = tree(
            "0-7",
            &[
                (0, "0,4"),
                (1, "1,5"),
                (2, "2,6"),
                (3, "3,7"),
                (4, "0,4"),
                (5, "1,5"),
                (6, "2,6"),
                (7, "3,7"),
            ],
        );
        let cpu = read_cpu(t.path());
        assert_eq!(
            cpu,
            HostCpu {
                physical_cores: 4,
                logical_cpus: 8,
                source: CoreSource::Topology,
            }
        );
        assert_eq!(cpu.source.describe(), "physical cores");
        // An offline CPU is not counted, even with its topology present.
        let t = tree("0-1,3", &[(0, "0"), (1, "1"), (2, "2"), (3, "3")]);
        assert_eq!(read_cpu(t.path()).physical_cores, 3);
    }

    #[test]
    fn an_unreadable_topology_falls_back_to_the_logical_cpus() {
        let t = tree("0-3", &[(0, "0,2"), (1, "1,3")]);
        let cpu = read_cpu(t.path());
        assert_eq!(cpu.source, CoreSource::Logical);
        assert_eq!(cpu.physical_cores, cpu.logical_cpus);
        assert!(cpu.physical_cores >= 1);
        assert_eq!(cpu.source.describe(), "logical CPUs, topology unreadable");
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(read_cpu(empty.path()).source, CoreSource::Logical);
    }
}
