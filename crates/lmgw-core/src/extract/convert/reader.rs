//! lmgw's document reader, run as a child (document-conversion design §5.2,
//! §14 WP4 review round): every format whose bytes go to a third-party
//! parser (HTML through htmd and html5ever, EPUB, e-mail through
//! mail-parser, legacy `.xls` through calamine, pictures and TIFF pages
//! through the `image` and `tiff` decoders) is parsed by the
//! `lmgw-doc-reader` executable, never in this process. The gateway links
//! none of those parsers; from this module it sees a bounded frame on a pipe
//! (`lmgw_doc_reader::wire`): markdown, cells, a page's grey pixels, an
//! e-mail's attachments as bytes.
//!
//! **Why.** These parsers cannot be stopped once they run on a thread, and
//! Rust ends the whole process on a failed allocation, so a small hostile
//! file could hang a pool slot for good or take the gateway down with the
//! owner's keys in it. A guard per parser is one hole behind the next file.
//! As a child the reader runs like poppler does, through [`run`]: its own
//! process group, no inherited descriptor, a slot of the tool pool held
//! until it has been reaped, and three bounds, each named in its failure.
//! Being lmgw's own program, it sets the memory limit on itself and closes
//! what it inherited as the first thing it does, before it reads the file
//! (`Tool::limits_itself`), which lets the gateway start it without a
//! fork:
//!
//! | bound | value | the file's error |
//! |---|---|---|
//! | [`READER_TIMEOUT`] | `PDF_TIMEOUT` (300 s) | `unreadable`, naming it |
//! | [`READER_MEMORY_LIMIT_BYTES`] | 4 GiB | `unreadable`, naming it |
//! | output | `MAX_UNCOMPRESSED_BYTES`, or a page's `MAX_OCR_PIXELS` | `output_too_large` |
//!
//! The reader gets no environment beyond what every tool gets, opens no
//! file and no socket (it links no network client), and nothing it writes
//! to stderr is shown or logged: it may quote the document.
//!
//! **Where it is.** Beside lmgw ([`own_dirs`]): next to the executable, in
//! `../libexec/lmgw` (the RPM), or one directory up in a cargo target
//! directory. It is a row of the capability table (`ToolId::Reader`), found
//! and shown like poppler; a reader that is not there is those formats'
//! `tool_missing`, never a read in this process.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use lmgw_api_types::documents::convert::ErrorCode;
use lmgw_doc_reader::wire::{self, Answer, Failure, FrameError, Op};

use super::caps::ToolId;
use super::pool::{self, Slot};
use super::run::{self, Bounds, Input, RunError, Tool};
use super::tools::{Fact, ToolFacts};
use crate::extract::office::{OfficeError, MAX_UNCOMPRESSED_BYTES};
use crate::extract::pdf;

/// The reader's file name.
pub const READER_BIN: &str = "lmgw-doc-reader";

/// How long the reader may take for one file (or one page of a picture)
/// once it has its slot: [`pdf::PDF_TIMEOUT`], 300 s, the bound poppler has
/// for a PDF. Measured 2026-11-19 (release build, htmd 0.6.0): a 10 MiB
/// HTML page of headings, lists and tables takes 0.49 s as a child, 0.43 s
/// of it the conversion; a 2 KiB page 1.4 ms, 0.9 ms of it the process's
/// start and exit. A file still going after five minutes is hostile (nested
/// markup that costs the parser time growing with its depth) or broken, and
/// its error names this constant.
pub const READER_TIMEOUT: Duration = pdf::PDF_TIMEOUT;

/// The reader's address space (`RLIMIT_AS`): 4 GiB (poppler's limit when it
/// was set; poppler's was lowered from measured peaks on 2026-11-20, the
/// reader's was not measured then and keeps its figure), which the pool
/// charges a reader run with its answer
/// (`pool::charge::READER_RUN`). A page tree is some ten to twenty times its HTML, so a page at the default
/// `max_body_mb` (64 MiB) fits; a file that asks for more (a shared string
/// copied into a million cells, markup that rebuilds hundreds of elements
/// per paragraph) fails alone, and its error names this constant.
pub const READER_MEMORY_LIMIT_BYTES: u64 = 4 << 30;

/// Where the reader is looked for, in order, as a `PATH`-style list:
/// beside this executable, in `../libexec/lmgw` from it (the RPM puts lmgw
/// in `/usr/bin` and the reader in `/usr/libexec/lmgw`), and, when this
/// executable is in a cargo target directory's `deps` or `examples`, one
/// directory up (tests and `cargo run --example`).
pub fn own_dirs() -> OsString {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
    else {
        return OsString::new();
    };
    let mut dirs = vec![dir.clone()];
    if let Some(up) = dir.parent() {
        dirs.push(up.join("libexec").join("lmgw"));
        if dir
            .file_name()
            .is_some_and(|n| n == "deps" || n == "examples")
        {
            dirs.push(up.to_path_buf());
        }
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

/// The reader the snapshot found.
pub fn path(facts: &ToolFacts) -> Option<PathBuf> {
    match facts.fact(ToolId::Reader) {
        Some(Fact::Found(f)) => Some(f.located.path.clone()),
        _ => None,
    }
}

/// How a read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The reader is not there.
    Missing,
    /// It passed [`READER_TIMEOUT`] and was stopped.
    Timeout,
    /// It ran out of [`READER_MEMORY_LIMIT_BYTES`].
    OutOfMemory,
    /// Its answer passed the run's output bound.
    TooLarge,
    /// It ended without an answer: its exit status, nothing of its stderr.
    Stopped(String),
    /// It answers in another version's wire form.
    OtherVersion(String),
    /// A failure it named.
    Named(Failure),
}

impl ReadError {
    /// The file's error: its code and message.
    pub fn failure(&self) -> (ErrorCode, String) {
        match self {
            Self::Missing => (
                ErrorCode::ToolMissing,
                format!(
                    "lmgw's document reader ({READER_BIN}) was not found beside lmgw; it is \
                     installed with lmgw, so install lmgw again"
                ),
            ),
            Self::OtherVersion(v) => (
                ErrorCode::ToolMissing,
                format!(
                    "lmgw's document reader answers in another version's form ({v}; this lmgw \
                     reads {}): lmgw was updated while it runs, so restart lmgw",
                    wire::WIRE_VERSION
                ),
            ),
            Self::Timeout => (
                ErrorCode::Unreadable,
                format!(
                    "lmgw's reader did not finish this file within READER_TIMEOUT ({} s) and \
                     was stopped; a file that takes this long is usually malformed",
                    READER_TIMEOUT.as_secs()
                ),
            ),
            Self::OutOfMemory => (
                ErrorCode::Unreadable,
                format!(
                    "lmgw's reader ran out of its memory limit on this file, \
                     READER_MEMORY_LIMIT_BYTES ({}), and was stopped; the file asks for far \
                     more memory than a document of its size needs",
                    crate::hf::fmt_bytes(READER_MEMORY_LIMIT_BYTES)
                ),
            ),
            Self::TooLarge => (
                ErrorCode::OutputTooLarge,
                format!(
                    "what lmgw's reader made of this file passed MAX_UNCOMPRESSED_BYTES ({}) \
                     and the reader was stopped",
                    crate::hf::fmt_bytes(MAX_UNCOMPRESSED_BYTES)
                ),
            ),
            Self::Stopped(status) => (
                ErrorCode::Unreadable,
                format!(
                    "lmgw's reader stopped unexpectedly on this file ({status}); the file was \
                     skipped and the others were converted"
                ),
            ),
            Self::Named(Failure::Unreadable(why)) => (ErrorCode::Unreadable, why.clone()),
            Self::Named(Failure::OverBudget { what }) => (
                ErrorCode::Unreadable,
                OfficeError::TooLarge { what: what.clone() }.to_string(),
            ),
            Self::Named(Failure::TooManyPixels { width, height }) => (
                ErrorCode::Unreadable,
                super::ocr::image::too_large(*width, *height),
            ),
        }
    }

    /// In words, for a warning about one part.
    pub fn said(&self) -> String {
        self.failure().1
    }
}

/// The reader's answer and the bytes after it.
#[derive(Debug)]
pub struct Frame {
    pub answer: Answer,
    bytes: Bytes,
    at: usize,
}

impl Frame {
    /// The blobs of `lens` bytes each, in order, shared with the frame (not
    /// copied); `None` when the frame holds fewer bytes than that.
    pub fn blobs(&self, lens: &[u64]) -> Option<Vec<Bytes>> {
        let ranges = wire::blobs(&self.bytes, self.at, lens)?;
        Some(ranges.into_iter().map(|r| self.bytes.slice(r)).collect())
    }

    /// Everything after the answer as one blob (a page's pixels), shared
    /// with the frame; `None` when there is nothing.
    pub fn rest(&self) -> Option<Bytes> {
        (self.at < self.bytes.len()).then(|| self.bytes.slice(self.at..))
    }
}

#[cfg(test)]
tokio::task_local! {
    static TEST_BOUNDS: (Duration, u64);
}

/// Run `fut` with the reader bounded by `timeout` and `memory` instead of
/// the named constants: a test of a file that would otherwise run for
/// [`READER_TIMEOUT`] or fill [`READER_MEMORY_LIMIT_BYTES`].
#[cfg(test)]
pub async fn with_bounds<F: std::future::Future>(
    timeout: Duration,
    memory: u64,
    fut: F,
) -> F::Output {
    TEST_BOUNDS.scope((timeout, memory), fut).await
}

fn bounds(output: u64) -> Bounds {
    #[cfg(test)]
    if let Ok((timeout, memory)) = TEST_BOUNDS.try_with(|b| *b) {
        return Bounds {
            timeout,
            output,
            memory,
        };
    }
    Bounds {
        timeout: READER_TIMEOUT,
        output,
        memory: READER_MEMORY_LIMIT_BYTES,
    }
}

/// `op` on `input`, on a slot of its own; the answer is at most `output`
/// bytes.
pub async fn read(
    facts: &ToolFacts,
    op: Op,
    input: Input,
    output: u64,
) -> Result<Frame, ReadError> {
    let Some(bin) = path(facts) else {
        return Err(ReadError::Missing);
    };
    let slot = pool::global()
        .acquire_charged(1, pool::charge::READER_RUN)
        .await;
    read_with(&slot, &bin, op, input, output).await
}

/// [`read`] on a slot the caller holds (a page's decode and its recognition
/// share one).
pub async fn read_held(
    slot: &Slot,
    facts: &ToolFacts,
    op: Op,
    input: Input,
    output: u64,
) -> Result<Frame, ReadError> {
    let Some(bin) = path(facts) else {
        return Err(ReadError::Missing);
    };
    read_with(slot, &bin, op, input, output).await
}

/// [`read_held`] with the reader at `bin`.
pub async fn read_with(
    slot: &Slot,
    bin: &std::path::Path,
    op: Op,
    input: Input,
    output: u64,
) -> Result<Frame, ReadError> {
    let args = op.args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let tool = Tool {
        quiet: true,
        // It sets its own memory limit and closes its inherited
        // descriptors before it reads the file (its `main`).
        limits_itself: Some(wire::MEMORY_LIMIT_FLAG),
        ..Tool::new(bin, &args)
    };
    let out = run::run_held(slot, tool, input, bounds(output))
        .await
        .map_err(|e| match e {
            // The reader starts itself; the helper is not asked for it.
            RunError::Missing | RunError::NoReader => ReadError::Missing,
            RunError::Timeout => ReadError::Timeout,
            RunError::OutOfMemory => ReadError::OutOfMemory,
            RunError::TooLarge | RunError::FileTooLarge(_) => ReadError::TooLarge,
            RunError::Failed(status) | RunError::NotStarted(status) => ReadError::Stopped(status),
        })?;
    let bytes = Bytes::from(out);
    match wire::read(&bytes) {
        Ok((Answer::Failed(f), _)) => Err(ReadError::Named(f)),
        Ok((answer, at)) => Ok(Frame { answer, bytes, at }),
        Err(FrameError::OtherVersion(v)) => Err(ReadError::OtherVersion(v)),
        Err(FrameError::Malformed(why)) => Err(ReadError::Stopped(format!(
            "its answer could not be read: {why}"
        ))),
    }
}

/// An answer of another kind than the request's: the reader's bug.
pub fn unexpected() -> ReadError {
    ReadError::Stopped("it answered another request than the one it was given".to_string())
}

// They run the real executable and shell stand-ins.
#[cfg(all(test, unix))]
mod tests;
