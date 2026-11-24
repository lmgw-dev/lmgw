//! A tool's CPU time bound (the review of security round B): every tool
//! started through lmgw's reader gets `RLIMIT_CPU` (`--cpu-seconds`) of its
//! wall timeout times the cores it may use. No tool is pinned to fewer
//! cores than the gateway has, so a run that keeps to its timeout can
//! never reach the bound: the timeout ends it first. What the bound is for
//! is a process that left the run's process group (`setsid`), which the
//! group kill does not reach: it keeps the bound, inherited, and is ended
//! by the kernel at it instead of running on without one. Each process has
//! the bound of its own; one that starts others does not bound them
//! together.

use std::time::Duration;

/// The cores a tool may use: what this process may run on
/// (`available_parallelism`, which counts a CPU affinity mask and a
/// cgroup's CPU quota), 1 when that cannot be read.
pub fn cores() -> u64 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u64)
}

/// A run's CPU time bound: its timeout, in whole seconds rounded up,
/// times [`cores`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuBound {
    pub seconds: u64,
    pub timeout_seconds: u64,
    pub cores: u64,
}

impl CpuBound {
    /// The bound of a run whose timeout is `timeout`.
    pub fn of(timeout: Duration) -> Self {
        Self::with(timeout, cores())
    }

    fn with(timeout: Duration, cores: u64) -> Self {
        let timeout_seconds = timeout.as_secs() + u64::from(timeout.subsec_nanos() > 0);
        let timeout_seconds = timeout_seconds.max(1);
        Self {
            seconds: timeout_seconds.saturating_mul(cores.max(1)),
            timeout_seconds,
            cores: cores.max(1),
        }
    }

    /// What the run's error says when the bound ended `name`.
    pub fn said(&self, name: &str) -> String {
        format!(
            "{name} used its CPU time bound ({} s: its timeout of {} s × {} cores, \
             RLIMIT_CPU) and was stopped",
            self.seconds, self.timeout_seconds, self.cores
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bound_is_the_timeout_times_the_cores() {
        let b = CpuBound::with(Duration::from_secs(300), 16);
        assert_eq!((b.seconds, b.timeout_seconds, b.cores), (4800, 300, 16));
        assert_eq!(CpuBound::with(Duration::from_millis(1500), 2).seconds, 4);
        assert_eq!(CpuBound::with(Duration::ZERO, 0).seconds, 1);
        assert!(cores() >= 1);
        assert_eq!(
            b.said("ffmpeg"),
            "ffmpeg used its CPU time bound (4800 s: its timeout of 300 s × 16 cores, \
             RLIMIT_CPU) and was stopped"
        );
    }
}
