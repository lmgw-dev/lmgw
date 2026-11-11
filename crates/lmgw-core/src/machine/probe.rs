//! NVML in a separate process (machine-check design §1.1): lmgw's own
//! executable started as `lmgw --machine-probe nvml` loads NVML, prints one
//! JSON line and exits.
//!
//! Why a process of its own: a process that has loaded `libnvidia-ml` keeps
//! the old library mapped after a driver update, and that old library
//! matches the old kernel module, so an in-process NVML call (the ledger's)
//! answers as if nothing changed. A new process maps what is on disk now.
//! The check starts it only once the kernel module's and the libraries'
//! versions agree.

use std::ffi::OsString;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The flag that turns lmgw's executable into the probe.
pub const PROBE_FLAG: &str = "--machine-probe";

/// The probe this module answers.
pub const PROBE_NVML: &str = "nvml";

/// What the check starts: the running program's own image, which stays
/// there even when an update replaced `/usr/bin/lmgw` on disk.
pub const SELF_EXE: &str = "/proc/self/exe";

/// One card as NVML reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmlCard {
    pub index: u32,
    pub name: String,
    pub uuid: Option<String>,
    pub memory_total: Option<u64>,
    pub pci_bus_id: Option<String>,
}

/// The probe's answer: one JSON line on stdout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum NvmlAnswer {
    Ok {
        driver_version: Option<String>,
        /// `nvmlSystemGetCudaDriverVersion`, e.g. 13020 for 13.2.
        cuda_version: Option<i32>,
        cards: Vec<NvmlCard>,
    },
    Failed {
        /// `library_not_found`, `driver_not_loaded`, `version_mismatch`,
        /// `no_permission` or `other`.
        kind: String,
        error: String,
    },
}

/// What the arguments after the program's name ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    /// An ordinary start: anything that does not begin with [`PROBE_FLAG`].
    No,
    /// `--machine-probe nvml`, exactly.
    Nvml,
    /// [`PROBE_FLAG`] with anything else after it, or nothing: a usage
    /// error, exit 2, and never an ordinary start.
    Usage,
}

/// The probe's grammar: [`PROBE_FLAG`] first, then exactly [`PROBE_NVML`].
pub fn asked(args: &[OsString]) -> Asked {
    if args.first().and_then(|a| a.to_str()) != Some(PROBE_FLAG) {
        return Asked::No;
    }
    if args.len() == 2 && args[1].to_str() == Some(PROBE_NVML) {
        Asked::Nvml
    } else {
        Asked::Usage
    }
}

/// The probe mode, when the process was started as one: answer and return
/// the exit code, before anything else of the program starts. `None` for an
/// ordinary start. The tray app's and the headless runner's `main` call it
/// first, before an async runtime, a GUI toolkit or logging starts.
pub fn run_if_asked() -> Option<i32> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match asked(&args) {
        Asked::No => return None,
        Asked::Usage => {
            eprintln!("usage: lmgw {PROBE_FLAG} {PROBE_NVML}");
            return Some(2);
        }
        Asked::Nvml => {}
    }
    let answer = nvml_now();
    let ok = matches!(answer, NvmlAnswer::Ok { .. });
    match serde_json::to_string(&answer) {
        Ok(line) => println!("{line}"),
        Err(e) => {
            eprintln!("the probe's answer could not be written: {e}");
            return Some(3);
        }
    }
    Some(if ok { 0 } else { 1 })
}

/// NVML, loaded in this process now.
fn nvml_now() -> NvmlAnswer {
    use nvml_wrapper::error::NvmlError;
    let nvml = match nvml_wrapper::Nvml::init() {
        Ok(n) => n,
        Err(e) => {
            let kind = match &e {
                NvmlError::LibloadingError(_) | NvmlError::LibraryNotFound => "library_not_found",
                NvmlError::DriverNotLoaded => "driver_not_loaded",
                NvmlError::LibRmVersionMismatch => "version_mismatch",
                NvmlError::NoPermission => "no_permission",
                _ => "other",
            };
            return NvmlAnswer::Failed {
                kind: kind.into(),
                error: e.to_string(),
            };
        }
    };
    let count = match nvml.device_count() {
        Ok(c) => c,
        Err(e) => {
            return NvmlAnswer::Failed {
                kind: "other".into(),
                error: format!("nvmlDeviceGetCount: {e}"),
            }
        }
    };
    let mut cards = Vec::new();
    for index in 0..count {
        let dev = match nvml.device_by_index(index) {
            Ok(d) => d,
            Err(e) => {
                return NvmlAnswer::Failed {
                    kind: "other".into(),
                    error: format!("nvmlDeviceGetHandleByIndex({index}): {e}"),
                }
            }
        };
        cards.push(NvmlCard {
            index,
            name: dev.name().unwrap_or_else(|_| format!("GPU {index}")),
            uuid: dev.uuid().ok(),
            memory_total: dev.memory_info().ok().map(|m| m.total),
            pci_bus_id: dev.pci_info().ok().map(|p| p.bus_id),
        });
    }
    NvmlAnswer::Ok {
        driver_version: nvml.sys_driver_version().ok(),
        cuda_version: nvml.sys_cuda_driver_version().ok(),
        cards,
    }
}

/// The probe as the detail names it.
pub(crate) const PROBE_WORDS: &str = "the graphics card check (lmgw --machine-probe nvml)";

/// Start the probe through `facts` and read its answer, within `limit` (the
/// load timeout, named with its setting when it runs out).
pub(crate) fn ask(facts: &dyn lmgw_machine::Facts, limit: Duration) -> Result<NvmlAnswer, String> {
    // Windows has no `/proc`: the running program's own path (an update
    // there replaces the file only once lmgw has exited).
    #[cfg(windows)]
    let exe = std::env::current_exe()
        .map_err(|e| format!("{PROBE_WORDS}: lmgw's own executable is not known: {e}"))?;
    #[cfg(not(windows))]
    let exe = std::path::PathBuf::from(SELF_EXE);
    let out = facts
        .run_limited(&exe, &[PROBE_FLAG, PROBE_NVML], limit)
        .map_err(|e| super::gather::limited(PROBE_WORDS, &e, limit))?;
    let line = out
        .stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .ok_or_else(|| {
            format!(
                "the probe exited {} without an answer{}",
                out.status_text(),
                match out.stderr.trim() {
                    "" => String::new(),
                    e => format!(": {e}"),
                }
            )
        })?;
    serde_json::from_str(line).map_err(|e| format!("the probe's answer could not be read: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    /// Only `--machine-probe nvml` exactly is the probe; the flag with
    /// anything else is a usage error, never an ordinary start; anything
    /// without the flag first is an ordinary start.
    #[test]
    fn the_probe_grammar_is_the_flag_then_nvml_exactly() {
        assert_eq!(asked(&args(&["--machine-probe", "nvml"])), Asked::Nvml);
        for usage in [
            &["--machine-probe"][..],
            &["--machine-probe", "amd"],
            &["--machine-probe", "nvml", "extra"],
            &["--machine-probe", "NVML"],
            &["--machine-probe", ""],
        ] {
            assert_eq!(asked(&args(usage)), Asked::Usage, "{usage:?}");
        }
        for ordinary in [
            &[][..],
            &["127.0.0.1:8001"],
            &["nvml", "--machine-probe"],
            &["--machine-probe=nvml"],
        ] {
            assert_eq!(asked(&args(ordinary)), Asked::No, "{ordinary:?}");
        }
        // An argument that is not UTF-8 is no flag.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            assert_eq!(asked(&[OsString::from_vec(vec![0xff])]), Asked::No);
            assert_eq!(
                asked(&[
                    OsString::from(PROBE_FLAG),
                    OsString::from_vec(vec![b'n', 0xff])
                ]),
                Asked::Usage
            );
        }
    }

    /// A probe that runs out of the load timeout is named as the check and
    /// with its setting, not as `/proc/self/exe`.
    #[test]
    fn a_probe_that_hangs_names_the_check_and_the_load_timeout() {
        let f = lmgw_machine::fake::FakeFacts::new();
        // The program `ask` starts on this system: lmgw's own path on
        // Windows, which has no /proc.
        #[cfg(windows)]
        let exe = std::env::current_exe().unwrap();
        #[cfg(not(windows))]
        let exe = std::path::PathBuf::from(SELF_EXE);
        f.program(&exe, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "/proc/self/exe ran for 600.0 s without finishing and was stopped",
            ))
        });
        let e = ask(&f, Duration::from_secs(600)).unwrap_err();
        assert!(e.starts_with("the graphics card check"), "{e}");
        assert!(e.contains("600 s, the load timeout"), "{e}");
        assert!(
            e.contains("Settings → GPU → Admission → Load timeout"),
            "{e}"
        );
        assert!(!e.contains("/proc/self/exe ran"), "{e}");
    }
}
