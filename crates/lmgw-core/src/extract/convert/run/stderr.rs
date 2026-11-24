//! A run's stderr (`run.rs`'s module doc): read as it arrives, its first
//! [`STDERR_KEEP_BYTES`] kept for the message, and all of it matched for
//! the out-of-memory words and handed line by line to the run's watch
//! ([`scan`]); once the tool has ended,
//! read on for at most [`STDERR_DRAIN_TIMEOUT`] (security round B, L3);
//! and shown to the caller as its first [`super::STDERR_LINES_SHOWN`] lines
//! with their paths cut ([`first_lines`], L1).

use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;

use super::{scrub, STDERR_LINES_SHOWN};

pub mod scan;
use scan::{Lines, Said, Scan, Watcher};

/// How much of a run's stderr is kept to be shown: 64 KiB. Display only:
/// what it says of memory and JPEG 2000 images is read over all of it
/// ([`scan`]).
const STDERR_KEEP_BYTES: usize = 64 << 10;

/// How long a run's stderr is read on after the tool ended: its group is
/// killed then, so only a process that left the group can still hold it
/// open; past this the run ends with what was read, and its message says
/// so.
pub const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// What was kept of stderr, whether more came (`cut`), and what all of it
/// said so far.
#[derive(Default)]
struct Kept {
    bytes: Vec<u8>,
    cut: bool,
    scan: Scan,
}

/// A run's stderr being read: the task, and what it has kept so far (so a
/// read given up at [`STDERR_DRAIN_TIMEOUT`] keeps what came before).
pub(super) struct Reading {
    task: JoinHandle<()>,
    kept: std::sync::Arc<std::sync::Mutex<Kept>>,
}

impl Reading {
    /// Stop reading (the run was stopped).
    pub(super) fn abort(&self) {
        self.task.abort();
    }

    fn taken(&self) -> Kept {
        std::mem::take(&mut *self.kept.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// Read `stderr` to its end on a task of its own, each line handed to
/// `watch` as it arrives.
pub(super) fn read(mut stderr: tokio::process::ChildStderr, watch: Option<Watcher>) -> Reading {
    let kept = std::sync::Arc::new(std::sync::Mutex::new(Kept::default()));
    let into = kept.clone();
    let task = tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        let mut lines = Lines::default();
        while let Ok(n) = stderr.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            {
                let mut g = into.lock().unwrap_or_else(|e| e.into_inner());
                g.scan.feed(&chunk[..n]);
                // A chunk that crossed the bound is kept whole; only what
                // came after it is not kept.
                if g.bytes.len() < STDERR_KEEP_BYTES {
                    g.bytes.extend_from_slice(&chunk[..n]);
                } else {
                    g.cut = true;
                }
            }
            if let Some(w) = &watch {
                lines.feed(&chunk[..n], w);
            }
        }
        if let Some(w) = &watch {
            lines.end(w);
        }
    });
    Reading { task, kept }
}

/// What [`drain`] got.
pub(super) struct Drained {
    pub kept: Vec<u8>,
    /// What all of it said of memory.
    pub said: Said,
    /// More came than was kept, or the read was given up.
    pub cut: bool,
    /// It was still open [`STDERR_DRAIN_TIMEOUT`] after the tool ended.
    pub held: bool,
}

/// The rest of stderr once the tool at `bin` ended with `status`, read for
/// at most [`STDERR_DRAIN_TIMEOUT`]; a stderr held open past it is said in
/// the log, and what was read before is kept.
pub(super) async fn drain(
    mut reader: Reading,
    bin: &Path,
    status: &std::process::ExitStatus,
) -> Drained {
    match tokio::time::timeout(STDERR_DRAIN_TIMEOUT, &mut reader.task).await {
        Ok(_) => {
            let k = reader.taken();
            Drained {
                kept: k.bytes,
                said: k.scan.said,
                cut: k.cut,
                held: false,
            }
        }
        Err(_) => {
            reader.abort();
            tracing::info!(
                "document tool {} ended ({status}) and its stderr was still open \
                 STDERR_DRAIN_TIMEOUT ({} s) later; it was not read further",
                bin.display(),
                STDERR_DRAIN_TIMEOUT.as_secs()
            );
            let k = reader.taken();
            Drained {
                kept: k.bytes,
                said: k.scan.said,
                cut: true,
                held: true,
            }
        }
    }
}

/// A failed run's message when its stderr was still open
/// [`STDERR_DRAIN_TIMEOUT`] after it ended: what it wrote before is kept
/// (the review of round B, N2).
pub(super) fn held(stderr: &str, status: &std::process::ExitStatus, dir: Option<&Path>) -> String {
    let said = if stderr.trim().is_empty() {
        String::new()
    } else {
        format!("{} ", first_lines_in(stderr, false, dir))
    };
    format!(
        "{said}({status}; its stderr was still open STDERR_DRAIN_TIMEOUT, {} s, after it ended)",
        STDERR_DRAIN_TIMEOUT.as_secs()
    )
}

/// A tool as the caller's messages name it: its file name, never its
/// path (L1).
pub fn shown_name(bin: &Path) -> String {
    bin.file_name().map_or_else(
        || "the tool".to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// The first [`STDERR_LINES_SHOWN`] lines of `text`, trimmed, with `(… N more
/// lines)` when there were more — or `(… more)` when `complete` is false: the
/// text is only what was kept of a longer stderr, so a count would be wrong.
///
/// Paths are cut to their file names and heap addresses dropped
/// ([`scrub::scrub`], L1): the lines reach the caller.
pub fn first_lines(text: &str, complete: bool) -> String {
    first_lines_in(text, complete, None)
}

/// [`first_lines`] of a run in `dir`, whose paths are cut word for word
/// ([`scrub::scrub_in`]).
pub fn first_lines_in(text: &str, complete: bool, dir: Option<&Path>) -> String {
    let scrubbed = scrub::scrub_in(text, dir);
    let text = scrubbed.trim();
    let total = text.lines().count();
    let mut shown: Vec<&str> = text.lines().take(STDERR_LINES_SHOWN).collect();
    let more = if complete {
        format!(
            "(… {} more lines)",
            total.saturating_sub(STDERR_LINES_SHOWN)
        )
    } else {
        "(… more)".to_string()
    };
    if total > STDERR_LINES_SHOWN || !complete {
        shown.push(&more);
    }
    shown.join("\n")
}
