//! The run args' resource limits a native start applies (Windows design §4.4,
//! WP6): `--memory`/`-m` and `--cpus`, which a container gets from its
//! cgroup, as limits of a job object of the engine's own on Windows.
//!
//! - **`--memory <n>[b|k|m|g]`** (binary units, as Podman reads them) is the
//!   job's `JOB_OBJECT_LIMIT_JOB_MEMORY`: the committed private memory of the
//!   engine and anything it starts. An allocation past it fails inside the
//!   engine (it reports out of memory and usually exits), where a cgroup
//!   would reclaim cache first and then kill. A model file the engine maps is
//!   file-backed, not committed, so it counts there as page cache counts in a
//!   cgroup: not toward the limit until it is copied.
//! - **`--cpus <x>`** is the job's CPU rate hard cap: `x` of this machine's
//!   logical processors, as a share of all of them. `x` at or above their
//!   number sets no cap (a container at that `--cpus` has none either).
//!
//! Every other limit (`--memory-swap`, `--memory-reservation`,
//! `--cpuset-cpus`, `--cpu-shares`, `--pids-limit`, …) is not applied yet;
//! it stays among the run args the start's log names as not applied. On
//! Linux the native launcher applies none of these yet ([`take`] takes
//! nothing there).

/// The limits a native start applies.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Limits {
    /// `--memory` in bytes.
    pub memory_bytes: Option<u64>,
    /// `--cpus`.
    pub cpus: Option<f64>,
}

impl Limits {
    pub fn is_empty(&self) -> bool {
        self.memory_bytes.is_none() && self.cpus.is_none()
    }

    /// `--memory 8.0 GiB --cpus 4`, for the start's log.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(b) = self.memory_bytes {
            parts.push(format!("--memory {}", crate::hf::fmt_bytes(b)));
        }
        if let Some(c) = self.cpus {
            parts.push(format!("--cpus {c}"));
        }
        parts.join(" ")
    }
}

/// The limits in `run_args` that apply here, and the run args without them
/// (what the rest of the native start reads). A value that does not parse
/// is left in the run args, so the start's log names it as not applied.
pub fn take(run_args: &[String]) -> (Limits, Vec<String>) {
    let mut limits = Limits::default();
    if !cfg!(windows) {
        return (limits, run_args.to_vec());
    }
    let mut rest = Vec::with_capacity(run_args.len());
    let mut i = 0;
    while i < run_args.len() {
        let a = &run_args[i];
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (a.as_str(), None),
        };
        let is_memory = matches!(flag, "--memory" | "-m");
        let is_cpus = flag == "--cpus";
        if !(is_memory || is_cpus) {
            rest.push(a.clone());
            i += 1;
            continue;
        }
        let (value, used) = match inline {
            Some(v) => (Some(v), 1),
            None => (run_args.get(i + 1).map(String::as_str), 2),
        };
        let parsed = match value {
            Some(v) if is_memory => parse_memory(v).map(|b| limits.memory_bytes = Some(b)),
            Some(v) => parse_cpus(v).map(|c| limits.cpus = Some(c)),
            None => None,
        };
        if parsed.is_none() {
            rest.extend(run_args[i..(i + used).min(run_args.len())].iter().cloned());
        }
        i += used;
    }
    (limits, rest)
}

/// Podman's `--memory`: a number with an optional `b`, `k`, `m` or `g`
/// (binary: `k` is 1024), case-insensitive; zero or less is no limit, and is
/// refused here as a value that limits nothing.
fn parse_memory(v: &str) -> Option<u64> {
    let v = v.trim().to_ascii_lowercase();
    let (num, mult) = match v.chars().last()? {
        'b' => (&v[..v.len() - 1], 1u64),
        'k' => (&v[..v.len() - 1], 1 << 10),
        'm' => (&v[..v.len() - 1], 1 << 20),
        'g' => (&v[..v.len() - 1], 1 << 30),
        c if c.is_ascii_digit() => (v.as_str(), 1),
        _ => return None,
    };
    let n: f64 = num.trim().parse().ok()?;
    (n > 0.0 && n.is_finite()).then_some((n * mult as f64) as u64)
}

fn parse_cpus(v: &str) -> Option<f64> {
    let c: f64 = v.trim().parse().ok()?;
    (c > 0.0 && c.is_finite()).then_some(c)
}

/// `cpus` of `logical` processors as the job's CPU rate: hundredths of a
/// percent of all of them, `None` when it is all of them or more.
pub fn cpu_rate(cpus: f64, logical: usize) -> Option<u32> {
    let logical = logical.max(1) as f64;
    if cpus >= logical {
        return None;
    }
    Some(((cpus / logical) * 10_000.0).round().clamp(1.0, 10_000.0) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn podmans_memory_values_read_in_binary_units() {
        assert_eq!(parse_memory("8g"), Some(8 << 30));
        assert_eq!(parse_memory("512M"), Some(512 << 20));
        assert_eq!(parse_memory("1.5g"), Some(3 << 29));
        assert_eq!(parse_memory("4096"), Some(4096));
        assert_eq!(parse_memory("0"), None);
        assert_eq!(parse_memory("lots"), None);
    }

    #[test]
    fn a_cpu_share_is_hundredths_of_a_percent_of_every_processor() {
        assert_eq!(cpu_rate(4.0, 16), Some(2500));
        assert_eq!(cpu_rate(0.5, 8), Some(625));
        assert_eq!(cpu_rate(16.0, 16), None);
        assert_eq!(cpu_rate(32.0, 16), None);
    }

    #[cfg(windows)]
    #[test]
    fn the_limits_are_taken_out_and_the_rest_left_in_order() {
        let (l, rest) = take(&v(&[
            "--memory",
            "8g",
            "--device",
            "nvidia.com/gpu=0",
            "--cpus=2.5",
            "--memory-swap",
            "9g",
            "--cpus",
            "many",
        ]));
        assert_eq!(l.memory_bytes, Some(8 << 30));
        assert_eq!(l.cpus, Some(2.5));
        assert_eq!(
            rest,
            v(&[
                "--device",
                "nvidia.com/gpu=0",
                "--memory-swap",
                "9g",
                "--cpus",
                "many"
            ])
        );
        assert_eq!(l.describe(), "--memory 8.0 GiB --cpus 2.5");
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_takes_nothing() {
        let args = v(&["--memory", "8g", "--cpus", "2"]);
        let (l, rest) = take(&args);
        assert!(l.is_empty());
        assert_eq!(rest, args);
    }
}
