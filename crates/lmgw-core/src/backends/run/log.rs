//! A run's log file (container-builds §5 step 5):
//! `<builds_dir>/logs/<instance>-<run-id>.log`
//! ([`log_path`](crate::backends::paths::log_path)),
//! every line of every phase — git, the edits, `podman build`, the probes —
//! appended as it happens, never truncated once its run has started, and kept
//! after the run. The Backends page tails it through [`read_log`] by byte
//! offset.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::state::SharedState;
use crate::store;
use lmgw_api_types::builds::RunLogChunk;

/// How much of the log one [`read_log`] call returns at most (1 MiB). A
/// transport chunk, not a cap on the log: the reply's `next_offset` says where
/// the next poll continues, and `done` stays `false` until the reader has
/// reached the end of a finished run's file — so a caller that keeps polling
/// gets every byte. A multi-hour build writes tens of megabytes; one reply of
/// that size would stall the page that asked for it. Callers that want the
/// end of a log rather than its start use [`read_log_tail`], which has no
/// chunk limit (it returns exactly the lines asked for).
pub const LOG_CHUNK_BYTES: u64 = 1024 * 1024;

/// An open run log. Lines are written unbuffered, one `write` each, so a
/// [`read_log`] poll sees a line the moment the executor has logged it.
pub(crate) struct RunLog {
    path: PathBuf,
    file: Mutex<Option<std::fs::File>>,
    /// The last line written — the job detail's `last_line`.
    last: Mutex<String>,
}

impl RunLog {
    /// Start a **new** run's log at `path` — created, never opened: a file
    /// of that name is not this run's, and neither emptying it (it may be
    /// another instance's live log) nor appending to it (run 2's log opened
    /// with another data dir's run 2 above its own lines, found in the
    /// Backends UI live pass, 2026-09-26) is right. The name carries the
    /// instance id, so a clash takes a reset database or a copied one; the
    /// log then goes to the first free `<name>-2.log`, `-3`, … and the path
    /// the run records ([`Self::path`]) is where it went.
    pub fn create(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        }
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut candidate = path.to_path_buf();
        for n in 2u64.. {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(_) => return Self::open(&candidate),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    candidate = path.with_file_name(format!("{stem}-{n}.log"));
                }
                Err(e) => {
                    return Err(format!(
                        "could not create the run log {}: {e}",
                        candidate.display()
                    ))
                }
            }
        }
        unreachable!("an unbounded sequence of names has a free one")
    }

    /// Where this log is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open `path` for appending, creating it and its directory.
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("could not open the run log {}: {e}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(Some(file)),
            last: Mutex::new(String::new()),
        })
    }

    /// Append `text` and a newline. A write that fails is reported once and
    /// the log goes quiet — the run itself carries on and fails (or not) on
    /// its own merits; the disk that filled up is the build's problem too.
    pub fn line(&self, text: &str) {
        let text = text.trim_end_matches(['\n', '\r']);
        {
            let mut guard = self.file.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(f) = guard.as_mut() {
                let mut buf = Vec::with_capacity(text.len() + 1);
                buf.extend_from_slice(text.as_bytes());
                buf.push(b'\n');
                if let Err(e) = f.write_all(&buf) {
                    tracing::warn!(
                        "writing the build log {} failed, the rest of this run is not logged: {e}",
                        self.path.display()
                    );
                    *guard = None;
                }
            }
        }
        if !text.trim().is_empty() {
            *self.last.lock().unwrap_or_else(|p| p.into_inner()) = text.to_string();
        }
    }

    /// Every line of `text` (probe output, a multi-line report).
    pub fn lines(&self, text: &str) {
        for l in text.lines() {
            self.line(l);
        }
    }

    /// A section header: `==> 2026-09-26T12:00:00Z build`.
    pub fn header(&self, title: &str) {
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        self.line(&format!("==> {now} {title}"));
    }

    pub fn last_line(&self) -> String {
        self.last.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// The part of run `run_id`'s log from byte `offset` on — at most
/// [`LOG_CHUNK_BYTES`] of it, ending on a character boundary — with the
/// offset to continue from. `done` is set only when the run has ended **and**
/// the reply reaches the end of the file, which is when polling can stop.
///
/// An offset past the end is taken as the end (nothing new); a log that does
/// not exist yet reads as empty.
pub async fn read_log(
    state: &SharedState,
    run_id: i64,
    offset: u64,
) -> Result<RunLogChunk, String> {
    let run = store::get_build_run(&state.db, run_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no build run with id {run_id}"))?;
    // Read the status before the file: the executor writes its last line
    // before it finishes the run, so a terminal status means the file is
    // complete. (The other order could report `done` over a missing tail.)
    let terminal = run.status.is_terminal();
    let Some(path) = run.log_path.clone() else {
        return Ok(RunLogChunk {
            text: String::new(),
            next_offset: offset,
            done: terminal,
        });
    };
    let (text, next_offset, len) = tokio::task::spawn_blocking(move || read_chunk(&path, offset))
        .await
        .map_err(|e| format!("reading the log of run {run_id}: {e}"))??;
    Ok(RunLogChunk {
        text,
        next_offset,
        done: terminal && next_offset >= len,
    })
}

/// The last `lines` lines of run `run_id`'s log, with the offset to continue
/// from with [`read_log`] (the end of the file as it was read) — what an agent
/// or a CLI wants from a long build ("how did it end?") without paging through
/// megabytes from the start. The text ends with the file's last newline, if it
/// has one; `lines = 0` returns no text but still the offset. `done` is set
/// when the run has ended (the tail then is the end of the log).
///
/// There is no size cap: the reply is exactly the lines asked for, however
/// long they are. A log that does not exist yet reads as empty.
pub async fn read_log_tail(
    state: &SharedState,
    run_id: i64,
    lines: usize,
) -> Result<RunLogChunk, String> {
    let run = store::get_build_run(&state.db, run_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no build run with id {run_id}"))?;
    // Status before file, as in `read_log`.
    let terminal = run.status.is_terminal();
    let Some(path) = run.log_path.clone() else {
        return Ok(RunLogChunk {
            text: String::new(),
            next_offset: 0,
            done: terminal,
        });
    };
    let (text, len) = tokio::task::spawn_blocking(move || read_tail(Path::new(&path), lines))
        .await
        .map_err(|e| format!("reading the log of run {run_id}: {e}"))??;
    Ok(RunLogChunk {
        text,
        next_offset: len,
        done: terminal,
    })
}

/// `(the last `lines` lines, file length)`, reading backwards in blocks so a
/// tail of a huge log costs what the tail costs.
fn read_tail(path: &Path, lines: usize) -> Result<(String, u64), String> {
    const BLOCK: u64 = 64 * 1024;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((String::new(), 0)),
        Err(e) => {
            return Err(format!(
                "could not open the run log {}: {e}",
                path.display()
            ))
        }
    };
    let len = f
        .metadata()
        .map_err(|e| format!("could not stat the run log {}: {e}", path.display()))?
        .len();
    if lines == 0 || len == 0 {
        return Ok((String::new(), len));
    }
    // Collect blocks from the end until the buffer holds `lines` line breaks
    // before its final line (a trailing newline ends the last line, it does
    // not start a new one).
    let mut buf: Vec<u8> = Vec::new();
    let mut pos = len;
    let start = loop {
        if pos == 0 {
            break 0;
        }
        let from = pos.saturating_sub(BLOCK);
        let mut block = vec![0u8; (pos - from) as usize];
        f.seek(SeekFrom::Start(from))
            .map_err(|e| format!("could not seek in the run log {}: {e}", path.display()))?;
        f.read_exact(&mut block)
            .map_err(|e| format!("could not read the run log {}: {e}", path.display()))?;
        block.extend_from_slice(&buf);
        buf = block;
        pos = from;
        let body = buf.strip_suffix(b"\n").unwrap_or(&buf);
        // The `lines`-th newline from the end of `body` starts the tail.
        if let Some((i, _)) = body
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, b)| **b == b'\n')
            .nth(lines - 1)
        {
            break i + 1;
        }
    };
    // `start` indexes `buf` once the loop found enough lines, or is 0 (the
    // whole file) when it ran out of file first.
    let text = String::from_utf8_lossy(&buf[start..]).into_owned();
    Ok((text, len))
}

/// `(text, next_offset, file length)`.
fn read_chunk(path: &str, offset: u64) -> Result<(String, u64, u64), String> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((String::new(), 0, 0));
        }
        Err(e) => return Err(format!("could not open the run log {path}: {e}")),
    };
    let len = f
        .metadata()
        .map_err(|e| format!("could not stat the run log {path}: {e}"))?
        .len();
    let start = offset.min(len);
    f.seek(SeekFrom::Start(start))
        .map_err(|e| format!("could not seek in the run log {path}: {e}"))?;
    let mut buf = Vec::new();
    f.take(LOG_CHUNK_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| format!("could not read the run log {path}: {e}"))?;
    // Never split a UTF-8 sequence across two replies: stop before an
    // incomplete one at the end, unless it is the end of the file (then it is
    // just a broken byte and is shown as such).
    let at_eof = start + buf.len() as u64 >= len;
    let used = match std::str::from_utf8(&buf) {
        Ok(_) => buf.len(),
        Err(e) if e.error_len().is_none() && !at_eof && e.valid_up_to() > 0 => e.valid_up_to(),
        Err(_) => buf.len(),
    };
    let text = String::from_utf8_lossy(&buf[..used]).into_owned();
    Ok((text, start + used as u64, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_is_the_last_lines_however_the_blocks_fall() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.log");
        // Lines long enough that the tail spans several read blocks.
        let long: Vec<String> = (0..200)
            .map(|i| format!("{i:04} {}", "x".repeat(2000)))
            .collect();
        std::fs::write(&path, long.join("\n") + "\n").unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let (text, end) = read_tail(&path, 3).unwrap();
        assert_eq!(end, len);
        assert_eq!(text, long[197..].join("\n") + "\n");
        // More lines than the file has: all of it.
        let (all, _) = read_tail(&path, 10_000).unwrap();
        assert_eq!(all.lines().count(), 200);
        // Zero lines: no text, the offset still.
        assert_eq!(read_tail(&path, 0).unwrap(), (String::new(), len));
        // No trailing newline: the unterminated last line counts as one.
        std::fs::write(&path, "a\nb\nc").unwrap();
        assert_eq!(read_tail(&path, 2).unwrap().0, "b\nc");
        assert_eq!(read_tail(&path, 1).unwrap().0, "c");
        // A missing file is an empty log.
        assert_eq!(
            read_tail(&dir.path().join("nope.log"), 5).unwrap(),
            (String::new(), 0)
        );
    }

    #[test]
    fn a_new_run_never_empties_or_appends_to_a_log_of_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("ab12cd34-2.log");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let theirs = "==> run 2 of another data dir's build\n";
        std::fs::write(&path, theirs).unwrap();
        let log = RunLog::create(&path).unwrap();
        log.line("run 2 of this one");
        let mine = dir.path().join("logs").join("ab12cd34-2-2.log");
        assert_eq!(log.path(), mine);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs, "untouched");
        // The executor reopens it for appending: nothing it wrote is lost.
        RunLog::open(log.path()).unwrap().line("next line");
        assert_eq!(
            std::fs::read_to_string(&mine).unwrap(),
            "run 2 of this one\nnext line\n"
        );
        // A free name is taken as it is.
        let fresh = dir.path().join("logs").join("ab12cd34-3.log");
        assert_eq!(RunLog::create(&fresh).unwrap().path(), fresh);
    }
}
