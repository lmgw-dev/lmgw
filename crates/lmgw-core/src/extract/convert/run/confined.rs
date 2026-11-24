//! A tool run confined by lmgw's reader executable (document-conversion
//! design §5.1, §14 WP7 review round): the runner starts
//! `lmgw-doc-reader run [rules] -- <tool> <args…>` instead of the tool
//! ([`lmgw_doc_reader::sandbox`]), and the reader applies the rules, the
//! memory limit and the file-size limit to itself and `exec`s the tool in
//! its place. So the tool keeps the process the runner started: its own
//! process group, the group kill, the slot's lease until it is reaped.
//!
//! No hook runs between the gateway's fork and exec for such a run (the
//! runner spawns the reader as it spawns the reader for a document), so the
//! gateway is not forked for it; every tool can move behind the helper this
//! way, and LibreOffice, pandoc and ffmpeg have.
//!
//! How the run ended is read off the reader's exit status
//! ([`lmgw_doc_reader::sandbox::EXIT_UNCONFINED`]: the rules could not be
//! applied and the tool never started) and the tool's (`SIGXFSZ`: a file
//! written past [`Confined::file_size`]; `SIGXCPU`: its CPU time bound,
//! `run::cpu`).

use std::ffi::OsStr;
use std::path::Path;

use lmgw_doc_reader::sandbox;

use super::RunError;

/// A confined tool's `PATH`: nothing else of the gateway's environment is
/// passed on; the tool's caller adds what it needs (`Tool::env`).
pub const PATH: &str = "/usr/bin:/bin";

/// A confined run: the reader that confines, and the rules.
#[derive(Debug, Clone, Copy)]
pub struct Confined<'a> {
    /// lmgw's reader executable.
    pub helper: &'a Path,
    /// What the tool may reach; its `address_space` is the run's
    /// `Bounds::memory`, whatever is set here.
    pub rules: &'a sandbox::Confine,
}

impl Confined<'_> {
    /// The reader's arguments for `bin` with `args`, within `memory` and
    /// `cpu_seconds` (`run::cpu`).
    // Started through the reader on unix only (`run::command`).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(super) fn argv(
        &self,
        bin: &Path,
        args: &[&str],
        (memory, cpu_seconds): (u64, u64),
        token: &str,
    ) -> Vec<std::ffi::OsString> {
        let rules = sandbox::Confine {
            address_space: Some(memory),
            cpu_seconds: Some(cpu_seconds),
            status_token: Some(token.to_string()),
            // A tool that runs only confined is never started without its
            // rules, whatever they say (review N4).
            limits_only: false,
            ..self.rules.clone()
        };
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        rules.args(bin.as_os_str(), &args)
    }

    /// The file-size limit, when one is set.
    pub fn file_size(&self) -> Option<u64> {
        self.rules.file_size
    }
}

/// What a run the reader could not confine or limit says (S4): the tool
/// by its file name, no path.
pub fn not_started(name: &str) -> String {
    let tool = Path::new(name)
        .file_name()
        .map_or_else(|| "the tool".into(), |n| n.to_string_lossy());
    format!(
        "lmgw's document reader could not confine {tool} or set its limits, so {tool} was \
         not started (tool_not_started); the gateway's log says why"
    )
}

/// What the exit status of a run started through lmgw's reader says
/// ([`super::start`]; a confined one or one with the limits alone), before
/// its stderr is read as any tool's: `None` when it is the tool's own. The
/// reader's own failures are told by its exit status and its stderr line
/// together ([`super::start::by_reader`], with the run's `token`).
pub(super) fn ended(
    status: &std::process::ExitStatus,
    stderr: &str,
    name: &str,
    helper: &Path,
    (file_size, cpu): (Option<u64>, &super::cpu::CpuBound),
    token: &str,
) -> Option<RunError> {
    #[cfg(not(unix))]
    let _ = (file_size, cpu);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        // The CPU time bound (`run::cpu`): `SIGXCPU`, or the `SIGKILL` a
        // second after it for a process that ignored that.
        if status.signal() == Some(libc::SIGXCPU) {
            return Some(RunError::Failed(cpu.said(name)));
        }
        if status.signal() == Some(libc::SIGXFSZ) {
            return Some(match file_size {
                Some(limit) => RunError::FileTooLarge(limit),
                // No limit of its run: the one the gateway inherited.
                None => RunError::Failed(format!(
                    "{name} wrote a file past the file-size limit lmgw inherited \
                     (RLIMIT_FSIZE, {status})"
                )),
            });
        }
    }
    if !super::start::by_reader(stderr, token) {
        return None;
    }
    match status.code() {
        Some(sandbox::EXIT_NOT_FOUND) => Some(RunError::Missing),
        // Worded as a start the gateway itself failed was (`could not run`).
        Some(sandbox::EXIT_NOT_STARTED) => Some(RunError::Failed(format!(
            "could not run {name}: {}",
            super::start::start_reason(stderr).unwrap_or("it could not be started")
        ))),
        Some(sandbox::EXIT_UNCONFINED) => {
            // The reader's line names paths (the tool's, a rule's): the
            // log's, never the caller's.
            tracing::warn!(
                "lmgw's document reader ({}) did not start {name} ({status}): {}",
                helper.display(),
                super::start::reader_line(stderr)
            );
            Some(RunError::NotStarted(not_started(name)))
        }
        _ => None,
    }
}
