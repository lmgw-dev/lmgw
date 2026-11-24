//! One external tool run (document-conversion design §5.1): the runner the
//! PDF reader (`extract/pdf.rs`) and the conversion share, moved out of
//! `pdf.rs` so every tool runs the same way.
//!
//! A run:
//! - takes a slot from the gateway-wide tool pool first ([`super::pool`],
//!   design §7.7, D19), and holds it until the tool has ended;
//! - starts the tool as a system tool ([`crate::child::system_tool_tokio`]:
//!   no inherited descriptor beyond stdio, no `LD_LIBRARY_PATH`, no console
//!   window on Windows), in a process group of its own
//!   ([`crate::child::group`]), so a timeout or a dropped caller stops the
//!   tool and everything it started;
//! - writes the input to the tool's stdin from a task of its own while its
//!   stdout is read, so an input larger than the pipe buffer cannot
//!   deadlock against the output;
//! - reads stdout as it arrives, against [`Bounds::output`], and stops the
//!   tool when it passes it;
//! - limits the tool's address space to [`Bounds::memory`] (`RLIMIT_AS`);
//! - starts a foreign tool through lmgw's reader executable, which closes
//!   the descriptors it inherited, sets the limits, applies the tool's
//!   Landlock rules where it has some ([`Tool::rules`], [`Tool::confine`])
//!   and `exec`s the tool in its place ([`start`], [`confined`]): no hook
//!   between fork and exec, so the gateway is never forked for a tool
//!   (`posix_spawn`); lmgw's reader itself sets its limits on itself
//!   ([`Tool::limits_itself`]);
//! - drains stderr to the end (`stderr.rs`), keeping its first 64 KiB for
//!   the message, of which [`STDERR_LINES_SHOWN`] lines are shown (none of
//!   it for a [`Tool::quiet`] one, whose stderr may quote the document),
//!   and reading all of it for the out-of-memory words and the caller's
//!   [`Tool::watch`];
//! - keeps its slot until the tool is gone: a run that is stopped (its
//!   timeout, its output bound, a caller that went away) is killed with its
//!   group at once and then waited for on a task of its own, which holds a
//!   lease of the slot ([`pool::Lease`]) until the child has been reaped.
//!   The pool therefore never counts a slot free while its tool still has
//!   memory.
//!
//! The input is bytes written to the tool's stdin, or a file that is its
//! stdin ([`Input::File`]: a tool that reads one page of a large file by
//! position is not sent the whole file once per page).
//!
//! How a run ended is a [`RunError`]; each caller words it for its tool (a
//! missing `pdftotext` names poppler-utils).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use bytes::Bytes;

use super::pool;

pub mod confined;
pub mod cpu;
mod held;
pub mod scrub;
pub mod start;
mod stderr;
mod stub;
pub use confined::Confined;
pub use held::{poppler_held, tesseract_held};

/// How many lines of a failed run's stderr go into the error message (the rest
/// is counted as `(… N more lines)`, or `(… more)` when stderr was longer than
/// the 64 KiB kept and the count is unknown). Display only: the run's outcome never
/// depends on it, and stderr is always drained.
pub const STDERR_LINES_SHOWN: usize = 20;

pub use stderr::scan::{Said, Watch, Watcher, LINE_KEEP_BYTES};
pub use stderr::{first_lines, first_lines_in, shown_name, STDERR_DRAIN_TIMEOUT};

/// What one run may take.
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    /// How long the tool may run once it has its slot; the caller's error
    /// names the constant this came from.
    pub timeout: Duration,
    /// The most stdout may carry, in bytes.
    pub output: u64,
    /// The tool's address space (`RLIMIT_AS`), in bytes.
    pub memory: u64,
}

/// How a run ended, before it is worded for its tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The binary is not there (`ENOENT`).
    Missing,
    /// It could not be started, or it failed: the message, with stderr's
    /// first lines and the exit status.
    Failed(String),
    /// It passed [`Bounds::timeout`] and was stopped.
    Timeout,
    /// Its stdout passed [`Bounds::output`] and it was stopped.
    TooLarge,
    /// It died of [`Bounds::memory`].
    OutOfMemory,
    /// A [`Tool::confine`]d tool wrote a file past its file-size limit
    /// (`RLIMIT_FSIZE`, `SIGXFSZ`): the limit, in bytes.
    FileTooLarge(u64),
    /// lmgw's reader, which starts every foreign tool, is not there
    /// ([`start::no_reader`] words it): the tool was not started.
    NoReader,
    /// lmgw's reader could not confine the tool or set its limits, so it
    /// was not started (performance round review S4): lmgw's own failure,
    /// not the file's. The message names the tool by its file name only;
    /// the reader's line with its paths goes to the log.
    NotStarted(String),
}

/// What a tool reads on stdin.
#[derive(Debug)]
pub enum Input {
    /// Bytes, written to a pipe from a task of their own.
    Bytes(Bytes),
    /// A file that is the tool's stdin itself (an open file description of
    /// the run's own, so runs side by side do not share a position).
    File(std::fs::File),
}

/// A tool run's command.
#[derive(Debug, Clone, Copy)]
pub struct Tool<'a> {
    pub bin: &'a Path,
    pub args: &'a [&'a str],
    /// Environment variables set for it (tesseract's `OMP_THREAD_LIMIT`).
    pub env: &'a [(&'a str, &'a str)],
    /// Its working directory, when it needs a private one.
    pub dir: Option<&'a Path>,
    /// Keep nothing of its stderr: a failure's message is its exit status
    /// alone. For lmgw's own reader, whose stderr may quote the document.
    pub quiet: bool,
    /// The tool sets [`Bounds::memory`] on itself and closes its inherited
    /// descriptors before it does anything else: lmgw's own reader, which
    /// takes the limit after this flag as its first arguments. Such a tool
    /// is started without the hooks between fork and exec that set these
    /// for a foreign tool, and so without a fork at all (`posix_spawn`): a
    /// fork copies the gateway's page tables, which cost 50 ms a start at
    /// 0.8 GiB of gateway memory (measured 2026-11-19), per file.
    pub limits_itself: Option<&'a str>,
    /// Kept alive until the tool has been reaped, however its run ends: a
    /// private directory the tool writes into (LibreOffice's profile and
    /// output, `libreoffice.rs`), deleted when the last holder lets go, so
    /// it is never removed under a tool that is still being killed.
    pub keep: Option<&'a Keep>,
    /// Start it confined by lmgw's reader executable ([`confined`]): the
    /// rules it may reach the filesystem and network by, and the file-size
    /// limit. [`Bounds::memory`] goes to the reader with them. A tool that
    /// runs only confined (LibreOffice) or whose caller asked whether the
    /// confinement works (pandoc, ffmpeg).
    pub confine: Option<Confined<'a>>,
    /// The rules a tool runs under where the confinement works here, and
    /// with the limits alone where it does not (poppler, tesseract;
    /// `confine.rs`): asked of the reader once per process.
    pub rules: Option<&'a lmgw_doc_reader::sandbox::Confine>,
    /// Handed every line of its stderr as it arrives (a run that succeeds
    /// may still say something the caller acts on: poppler drawing a JPEG
    /// 2000 image blank, `pdf::wide::jpx`); the caller reads it after.
    pub watch: Option<&'a Watcher>,
}

/// What [`Tool::keep`] holds.
pub type Keep = std::sync::Arc<dyn std::any::Any + Send + Sync>;

impl<'a> Tool<'a> {
    pub fn new(bin: &'a Path, args: &'a [&'a str]) -> Self {
        Self {
            bin,
            args,
            env: &[],
            dir: None,
            quiet: false,
            limits_itself: None,
            keep: None,
            confine: None,
            rules: None,
            watch: None,
        }
    }
}

/// `bin args…` with `input` on stdin, stdout back undecoded, within
/// `bounds`; the slot from the tool pool is taken before the tool starts and
/// waiting for it does not count against the timeout.
pub async fn piped(
    bin: &Path,
    args: &[&str],
    input: Bytes,
    bounds: Bounds,
) -> Result<Vec<u8>, RunError> {
    piped_on(pool::global(), bin, args, input, bounds).await
}

/// `tool` with `input` on a slot the caller holds; the slot stays in use
/// until the tool is gone, whatever becomes of the caller (module doc).
pub async fn run_held(
    slot: &pool::Slot,
    tool: Tool<'_>,
    input: Input,
    bounds: Bounds,
) -> Result<Vec<u8>, RunError> {
    run_leased(slot.lease(), tool, input, bounds).await
}

/// [`piped`] with its slot from `pool` (a test's own).
async fn piped_on(
    pool: &pool::ToolPool,
    bin: &Path,
    args: &[&str],
    input: Bytes,
    bounds: Bounds,
) -> Result<Vec<u8>, RunError> {
    let slot = pool.acquire().await;
    run_held(&slot, Tool::new(bin, args), Input::Bytes(input), bounds).await
}

/// A started tool, until it has ended by itself. Dropped before that (the
/// run's timeout, its output bound, a caller that went away), the tool and
/// its group are killed at once and the child is waited for on a task of
/// its own, which keeps the slot's lease until the child has been reaped.
struct Running {
    child: Option<tokio::process::Child>,
    group: Option<crate::child::group::KillGroup>,
    lease: Option<pool::Lease>,
    /// [`Tool::keep`], let go of once the child has been reaped.
    keep: Option<Keep>,
}

impl Running {
    /// The tool ended by itself and was reaped: nothing to stop, and its
    /// group id may be reused from here on.
    fn ended(mut self) {
        self.child = None;
        if let Some(g) = self.group.take() {
            g.disarm();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // The group first (dropping it sends the signal), then the child
        // itself, in case it left its group.
        drop(self.group.take());
        let _ = child.start_kill();
        let lease = self.lease.take();
        let keep = self.keep.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = child.wait().await;
                drop(keep);
                drop(lease);
            });
        }
        // Without a runtime (it is shutting down) `kill_on_drop` has the
        // child, and the process is going away with its pool.
    }
}

/// [`run_held`] once the slot's lease is taken.
async fn run_leased(
    lease: pool::Lease,
    tool: Tool<'_>,
    input: Input,
    bounds: Bounds,
) -> Result<Vec<u8>, RunError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // The tool by its file name in what the caller is told (security round
    // B, L1); its path, its directory and its stderr's paths go to the log.
    let name = shown_name(tool.bin);
    let stdin_file = matches!(input, Input::File(_));
    let (mut cmd, via) = command(&tool, bounds, stdin_file).await?;
    crate::child::group::own_group(&mut cmd);
    if let Some(d) = tool.dir {
        cmd.current_dir(d);
    }
    cmd.envs(tool.env.iter().copied());
    let (stdin, input) = match input {
        Input::Bytes(b) => (Stdio::piped(), Some(b)),
        Input::File(f) => (Stdio::from(f), None),
    };
    let child = cmd
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A timeout, or a caller that is dropped mid-file, must not leave the
        // tool behind: `Running` stops it and its group and waits for it.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match (&via, e.kind()) {
            (Via::Reader { .. }, std::io::ErrorKind::NotFound) => RunError::NoReader,
            (Via::Direct, std::io::ErrorKind::NotFound) => RunError::Missing,
            _ => RunError::Failed(format!("could not run {name}: {e}")),
        })?;
    let group = crate::child::group::KillGroup::of(&child);
    let mut running = Running {
        child: Some(child),
        group: Some(group),
        lease: Some(lease),
        keep: tool.keep.cloned(),
    };
    let pipe = |what: &str| RunError::Failed(format!("{name}'s {what} was not piped"));
    let child = running.child.as_mut().ok_or_else(|| pipe("process"))?;
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take().ok_or_else(|| pipe("stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| pipe("stderr"))?;
    // A write that fails because the tool gave up early is not the error
    // worth reporting — its exit status and stderr are.
    let writer = match (stdin, input) {
        (Some(mut stdin), Some(input)) => Some(tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
            let _ = stdin.shutdown().await;
        })),
        (None, Some(_)) => return Err(pipe("stdin")),
        _ => None,
    };
    let err_reader = stderr::read(stderr, tool.watch.cloned());
    let work = async {
        let mut out = Vec::new();
        let mut chunk = vec![0u8; 64 << 10];
        loop {
            let n = stdout
                .read(&mut chunk)
                .await
                .map_err(|e| RunError::Failed(format!("{name}: {e}")))?;
            if n == 0 {
                break;
            }
            if (out.len() + n) as u64 > bounds.output {
                return Err(RunError::TooLarge);
            }
            out.extend_from_slice(&chunk[..n]);
        }
        // Its group goes with it, also when it ended by itself (L3).
        let status = crate::child::group::end_group(child)
            .await
            .map_err(|e| RunError::Failed(format!("{name}: {e}")))?;
        Ok((out, status))
    };
    let stop = |e: RunError| {
        if let Some(w) = &writer {
            w.abort();
        }
        err_reader.abort();
        e
    };
    // Returning early drops `running`, which stops the tool (its doc).
    let (out, status) = match tokio::time::timeout(bounds.timeout, work).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(stop(e)),
        Err(_) => return Err(stop(RunError::Timeout)),
    };
    running.ended();
    if let Some(w) = writer {
        let _ = w.await;
    }
    // A tool can report a failed allocation on stderr and still exit 0 with
    // a stub (poppler's 1x1 image for a page that did not fit), so stderr is
    // read for a success too; the pipe closes when the group is gone, and a
    // process that left the group and holds it is not waited for past
    // STDERR_DRAIN_TIMEOUT.
    let stderr::Drained {
        kept: stderr,
        said,
        cut,
        held,
    } = stderr::drain(err_reader, tool.bin, &status).await;
    let stderr = String::from_utf8_lossy(&stderr);
    if let Via::Reader {
        file_size,
        cpu,
        helper,
        token,
    } = &via
    {
        if let Some(e) = confined::ended(&status, &stderr, &name, helper, (*file_size, cpu), token)
        {
            return Err(match e {
                RunError::Failed(m) if !start::by_reader(&stderr, token) => {
                    RunError::Failed(format!("{m}: {}", first_lines_in(&stderr, !cut, tool.dir)))
                }
                e => e,
            });
        }
    }
    if said_out_of_memory(&status, said) || (said.bogus_size && stub::is_stub(&out)) {
        return Err(RunError::OutOfMemory);
    }
    if !status.success() {
        // The detail the caller's message leaves out: where the tool is and
        // where it ran (its stderr is the caller's to read, paths cut).
        tracing::info!(
            "document tool {} failed ({status}){}",
            tool.bin.display(),
            tool.dir
                .map(|d| format!(" in {}", d.display()))
                .unwrap_or_default()
        );
        return Err(RunError::Failed(if tool.quiet {
            status.to_string()
        } else if held {
            stderr::held(&stderr, &status, tool.dir)
        } else {
            format!("{} ({status})", first_lines_in(&stderr, !cut, tool.dir))
        }));
    }
    Ok(out)
}

/// How a tool was started: by lmgw's reader (`run`, which reports a tool
/// it could not start by its exit status), or as itself (off unix, every
/// tool yet).
#[cfg_attr(not(unix), allow(dead_code))]
enum Via {
    Reader {
        helper: std::path::PathBuf,
        /// The file-size limit it was given.
        file_size: Option<u64>,
        /// Its CPU time bound (`cpu`).
        cpu: cpu::CpuBound,
        /// The run's word in the reader's own lines ([`start::token`]).
        token: String,
    },
    Direct,
}

/// `tool`'s command within `bounds` (module doc): no hook between fork and
/// exec on unix.
#[cfg(unix)]
async fn command(
    tool: &Tool<'_>,
    bounds: Bounds,
    stdin_file: bool,
) -> Result<(tokio::process::Command, Via), RunError> {
    let token = start::token();
    let cpu = cpu::CpuBound::of(bounds.timeout);
    if let Some(c) = tool.confine {
        let mut cmd = tokio::process::Command::new(c.helper);
        confined_start(&mut cmd)?;
        // Nothing of the gateway's environment: the system's programs on
        // `PATH`, and what the tool's caller sets.
        cmd.env_clear().env("PATH", confined::PATH);
        cmd.args(c.argv(tool.bin, tool.args, (bounds.memory, cpu.seconds), &token));
        let via = Via::Reader {
            helper: c.helper.to_path_buf(),
            file_size: c.file_size(),
            cpu,
            token,
        };
        return Ok((cmd, via));
    }
    if let Some(flag) = tool.limits_itself {
        let mut cmd = tokio::process::Command::new(tool.bin);
        // lmgw's reader needs nothing of the gateway's environment.
        cmd.env_clear();
        cmd.arg(flag).arg(bounds.memory.to_string());
        cmd.args(tool.args);
        return Ok((cmd, Via::Direct));
    }
    let helper = start::starter().ok_or(RunError::NoReader)?;
    let bin = start::absolute(tool.bin).ok_or(RunError::Missing)?;
    let rules = match tool.rules {
        Some(r) if super::confine::available(&helper).await.is_ok() => Some(r),
        _ => None,
    };
    let mut cmd = tokio::process::Command::new(&helper);
    // Nothing of the gateway's environment (its keys may be in it): the
    // system's programs on `PATH`, what the caller sets (`Tool::env`), and
    // the few variables a tool reads its own data by (`start::PASSED_ON`).
    cmd.env_clear().env("PATH", confined::PATH);
    for (k, v) in start::passed_on() {
        cmd.env(k, v);
    }
    cmd.args(start::argv(
        rules,
        &bin,
        tool.args,
        (bounds.memory, cpu.seconds),
        stdin_file,
        &token,
    ));
    let via = Via::Reader {
        helper,
        file_size: rules.and_then(|r| r.file_size),
        cpu,
        token,
    };
    Ok((cmd, via))
}

/// Off unix a tool is started as itself, and refused while its memory
/// cannot be limited ([`limit_address_space`]).
#[cfg(not(unix))]
async fn command(
    tool: &Tool<'_>,
    bounds: Bounds,
    _stdin_file: bool,
) -> Result<(tokio::process::Command, Via), RunError> {
    let mut cmd = tokio::process::Command::new(tool.confine.map_or(tool.bin, |c| c.helper));
    if tool.confine.is_some() {
        confined_start(&mut cmd)?;
    }
    crate::child::system_tool_tokio(&mut cmd);
    limit_address_space(&mut cmd, bounds.memory)?;
    cmd.args(tool.args);
    Ok((cmd, Via::Direct))
}

/// A confined tool's start: the loader path every tool is started
/// without.
#[cfg(target_os = "linux")]
fn confined_start(cmd: &mut tokio::process::Command) -> Result<(), RunError> {
    cmd.env_remove("LD_LIBRARY_PATH");
    Ok(())
}

/// Programs are confined on Linux only yet; no confined tool runs elsewhere.
#[cfg(not(target_os = "linux"))]
fn confined_start(_cmd: &mut tokio::process::Command) -> Result<(), RunError> {
    Err(RunError::Failed(
        "programs are confined on Linux only yet (Landlock)".to_string(),
    ))
}

/// No tool is run without its memory limit, and a job object's limit is not
/// set yet on Windows (Windows design §4.4, WP6).
#[cfg(not(unix))]
fn limit_address_space(
    _cmd: &mut tokio::process::Command,
    _mem_limit: u64,
) -> Result<(), RunError> {
    Err(RunError::Failed(
        "the memory limit for document tools is not supported on Windows yet".to_string(),
    ))
}

/// Did the child die of the address-space limit? A failed allocation shows
/// as the tool's own out-of-memory message, a C++ `bad_alloc`, or the abort
/// or segfault that follows one.
pub(crate) fn ran_out_of_memory(status: &std::process::ExitStatus, stderr: &str) -> bool {
    said_out_of_memory(status, Said::of(stderr.as_bytes()))
}

/// [`ran_out_of_memory`] from what all of a streamed stderr said
/// (`stderr::scan`).
fn said_out_of_memory(status: &std::process::ExitStatus, said: Said) -> bool {
    said.out_of_memory || (died_of_abort_or_segv(status) && said.memory)
}

#[cfg(unix)]
fn died_of_abort_or_segv(status: &std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    matches!(status.signal(), Some(libc::SIGABRT | libc::SIGSEGV))
}

/// Windows has no signals; no tool runs there yet ([`limit_address_space`]).
#[cfg(not(unix))]
fn died_of_abort_or_segv(_status: &std::process::ExitStatus) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests;
#[cfg(all(test, target_os = "linux"))]
mod tests_end;
#[cfg(all(test, target_os = "linux"))]
mod tests_start;
