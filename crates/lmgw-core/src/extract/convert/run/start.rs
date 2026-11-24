//! How a foreign tool is started (document-conversion design §5.1, §14
//! performance round): never with a fork of the gateway.
//!
//! A hook between fork and exec (`pre_exec`: the memory limit, the
//! descriptors closed) makes std fork the gateway, and a fork copies the
//! gateway's page tables: measured 2026-11-19, 30 to 60 ms a start at 0.7 to
//! 0.8 GiB of gateway memory, on the runtime thread that polls the
//! conversion, so 50 small PDFs took 5.4 to 8.4 s instead of 0.3 s. So every
//! foreign tool of the conversion path (poppler's five, tesseract, pandoc,
//! ffmpeg, LibreOffice) is started as
//! `lmgw-doc-reader run [rules] --address-space N -- /abs/tool args…`
//! ([`lmgw_doc_reader::sandbox`]) with no hook at all, which std starts with
//! `posix_spawn` (`clone(CLONE_VM | CLONE_VFORK)`). The reader closes the
//! descriptors it inherited, sets the limits, applies the tool's Landlock
//! ruleset where there is one and Landlock works, and `exec`s the tool in
//! its own place: the tool keeps the process the runner started, its
//! process group, the group kill and the slot's lease until it is reaped.
//!
//! **Without lmgw's reader no tool starts** ([`super::RunError::NoReader`]):
//! there is no second way in that sets the limits. The capability table
//! needs the reader for every handler with a tool (`caps.rs`), so Settings ›
//! Documents and the route name that one state; the reader is installed
//! with lmgw (RPM, AppImage, server image) and built by cargo beside the
//! tests and dev instances.
//!
//! The tool's name is made absolute here (the reader `exec`s an absolute
//! path only): a bare name is looked up on the gateway's `PATH` as the
//! spawn would have, and one found nowhere is [`super::RunError::Missing`]
//! without a start.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use lmgw_doc_reader::sandbox;

/// The variables of the gateway's environment a foreign tool still gets
/// (its environment is otherwise cleared, as a confined tool's is): where
/// tesseract's language data is, when the owner set it
/// (`TESSDATA_PREFIX`; the probe ran with it too).
pub const PASSED_ON: &[&str] = &["TESSDATA_PREFIX"];

/// [`PASSED_ON`] as the gateway has them.
pub fn passed_on() -> Vec<(&'static str, std::ffi::OsString)> {
    PASSED_ON
        .iter()
        .filter_map(|k| std::env::var_os(k).map(|v| (*k, v)))
        .collect()
}

/// What a run whose tool cannot start because lmgw's reader is missing
/// says.
pub fn no_reader() -> String {
    format!(
        "lmgw's document reader ({}), which starts every document tool, was not found beside \
         lmgw; it is installed with lmgw, so install lmgw again",
        super::super::reader::READER_BIN
    )
}

/// The reader once found (it does not move while lmgw runs; a missing one
/// is looked for again at the next start, which is a few `stat`s).
static FOUND: Mutex<Option<PathBuf>> = Mutex::new(None);

/// lmgw's reader beside this executable (`reader::own_dirs`, then the
/// row's install directories), the same place the capability table finds
/// it.
pub fn starter() -> Option<PathBuf> {
    let mut found = FOUND.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = found.as_ref() {
        return Some(p.clone());
    }
    let row = super::super::caps::tool(super::super::caps::ToolId::Reader);
    let own = super::super::reader::own_dirs();
    let at = super::super::probe::lookup(row.probe.binaries[0], &own, row.probe.install_dirs)?;
    *found = Some(at.path.clone());
    Some(at.path)
}

/// `bin` as an absolute path: as it is when it has a directory, else the
/// first executable of that name on `PATH`.
pub fn absolute(bin: &Path) -> Option<PathBuf> {
    if bin.components().count() > 1 || bin.is_absolute() {
        return Some(std::path::absolute(bin).unwrap_or_else(|_| bin.to_path_buf()));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(bin))
        .find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// The reader's arguments that start `bin` with `args`: under `rules` when
/// given (their address space replaced by `memory`; with a read right on
/// stdin's own file when `stdin_file`, `--read-stdin`), else with the
/// limits alone; `cpu_seconds` its CPU time bound (`run::cpu`); the run's
/// `token` in the reader's own lines (N1).
pub fn argv(
    rules: Option<&sandbox::Confine>,
    bin: &Path,
    args: &[&str],
    (memory, cpu_seconds): (u64, u64),
    stdin_file: bool,
    token: &str,
) -> Vec<std::ffi::OsString> {
    let rules = match rules {
        Some(r) => sandbox::Confine {
            address_space: Some(memory),
            cpu_seconds: Some(cpu_seconds),
            read_stdin: stdin_file,
            status_token: Some(token.to_string()),
            ..r.clone()
        },
        None => sandbox::Confine {
            address_space: Some(memory),
            cpu_seconds: Some(cpu_seconds),
            limits_only: true,
            status_token: Some(token.to_string()),
            ..sandbox::Confine::default()
        },
    };
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    rules.args(bin.as_os_str(), &args)
}

/// A word of its own for one run started through the reader (N1): the
/// reader's own lines carry it, a tool's stderr cannot (the tool never
/// sees it).
pub fn token() -> String {
    hex::encode(rand::random::<[u8; 8]>())
}

/// Whether the reader's own failure is what ended a run started through
/// it with `token`: its stderr starts with the reader's line for that run
/// (a tool that happens to exit 125 to 127, or writes the reader's words,
/// says something else).
pub fn by_reader(stderr: &str, token: &str) -> bool {
    stderr.starts_with(&sandbox::status_prefix(Some(token)))
}

/// The reader's first line without its prefix (any token).
pub fn reader_line(stderr: &str) -> &str {
    let first = stderr.lines().next().unwrap_or("");
    match first.strip_prefix("lmgw-doc-reader run") {
        Some(rest) => rest.split_once(": ").map_or(rest, |(_, why)| why).trim(),
        None => first.trim(),
    }
}

/// The reason in the reader's "… could not be started: <reason>" line.
pub fn start_reason(stderr: &str) -> Option<&str> {
    reader_line(stderr)
        .split_once(" could not be started: ")
        .map(|(_, why)| why.trim())
}
