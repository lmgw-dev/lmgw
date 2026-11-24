//! LibreOffice, headless (document-conversion design §4.2, §5.1, WP7): an
//! old Word or PowerPoint file (or RTF without pandoc) turned into a docx or
//! pptx, which lmgw's own office reader reads, so its headings, tables,
//! pictures and `json` come as a docx's do; a deck rendered to a PDF, which
//! the PDF path reads (slides whose content is a picture, for a vision
//! model: the seam is [`slides_pdf`]).
//!
//! LibreOffice is the heaviest and most exposed tool here, so each run:
//! - is **confined by the kernel** (Landlock, through lmgw's reader
//!   executable, `confine.rs`): it reads and executes `/usr`, reads the
//!   loader cache, fonts and user database it needs, writes its own
//!   directory alone, may make one socket in `/tmp` and nothing else
//!   there, and has no TCP (no UDP either from Landlock ABI 10). A
//!   document that names a file elsewhere (an RTF's `INCLUDEPICTURE
//!   "/any/path"`, a linked section) gets nothing: the profile's settings
//!   are not the boundary. Where it cannot be confined, it is not run
//!   (its probe fails by name);
//! - gets a **private directory** of its own (mode 0700) under the data
//!   dir's `convert-spool` (never `/tmp`, which is memory on many systems):
//!   the input as `in.<ext>`, the output directory, LibreOffice's own
//!   temporary files (`TMPDIR`), and a **profile of its own**
//!   (`-env:UserInstallation`), a copy of a template no document ran in
//!   ([`scratch`]), never the person's own and never one another run uses,
//!   so runs side by side never lock each other ([`profile`] has what it is
//!   seeded with: no macros, no link updates, no VBA);
//! - is started with `--headless --norestore --nolockcheck --nologo
//!   --nodefault`, the input filter named (`--infilter`: a `.doc` is opened
//!   as Word 97 and nothing else, whatever its bytes would suggest), VCL's
//!   headless backend (`SAL_USE_VCLPLUGIN=svp`: no display, no desktop
//!   session is opened) and an environment of its own (`HOME` and `TMPDIR`
//!   in its directory, `PATH`, `LANG`; nothing of the gateway's);
//! - runs through the runner every tool goes through ([`super::run`]): its
//!   own process group (Fedora's `soffice` is a script that starts
//!   `oosplash`, which starts `soffice.bin`: the whole group is killed on a
//!   timeout, a dropped caller or a deadline), no inherited descriptors,
//!   [`LIBREOFFICE_TIMEOUT`], [`LIBREOFFICE_MEMORY_LIMIT_BYTES`], a
//!   file-size limit (`RLIMIT_FSIZE`: `MAX_UNCOMPRESSED_BYTES`, or what is
//!   left of the request's output budget when that is less), a slot of the
//!   tool pool, and its stdout bounded;
//! - has its result read back bounded by `MAX_UNCOMPRESSED_BYTES` (its
//!   size first, then a read that stops there);
//! - leaves nothing behind: the directory and the socket are removed on
//!   every path (success, failure, timeout, a dropped caller, a panic), and
//!   only once the tool has been reaped ([`super::run::Tool::keep`]), so
//!   nothing is removed under a process still being killed. What a crashed
//!   gateway left is removed by the first run of the next one
//!   ([`scratch::sweep_once`]).
//!
//! What LibreOffice can still reach (not yet closed): UDP on a kernel
//! before Landlock ABI 10, Unix sockets by path before ABI 9; fonts the
//! person installed in their home directory are not visible to it (it
//! renders with the system's fonts).

pub mod profile;
pub mod scratch;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use super::caps::Format;
use super::pool;
use super::run::{self, Bounds, Input, Keep, RunError, Tool};
use crate::extract::office::MAX_UNCOMPRESSED_BYTES;

/// How long one LibreOffice run may take: five minutes, as poppler's
/// (`PDF_TIMEOUT`). A `.doc` of a few pages converts in under half a second
/// with a new profile (0.45 s, 2026-11-19, LibreOffice 26.8.2.2) and a deck
/// of a few hundred slides to PDF in tens of seconds; a run still going
/// after five minutes is a file that makes it loop.
pub const LIBREOFFICE_TIMEOUT: Duration = Duration::from_secs(300);

/// LibreOffice's address space: 4 GiB, a figure of its own (it was
/// poppler's until poppler's was lowered from measured peaks on 2026-11-20;
/// the owner's ruling, 2026-11-20, lowered poppler's limit only; this one is
/// not derived from measured peaks yet). Its step names no
/// charge of its own, so the pool charges it `pool::charge::HEAVIEST`,
/// which this limit and its outputs stay within. Measured 2026-11-19
/// (LibreOffice 26.8.2.2): 1 GiB is too little for it to start (it dies of
/// `SIGSEGV`), 2 GiB converts a deck to PDF; the rest is room for a large
/// document.
pub const LIBREOFFICE_MEMORY_LIMIT_BYTES: u64 = 4 << 30;

/// What LibreOffice may print on stdout: 64 KiB. It prints one line per
/// converted file; the result is a file, not its stdout.
const LIBREOFFICE_STDOUT_BYTES: u64 = 64 << 10;

/// What a file is turned into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Docx,
    Pptx,
    Pdf,
}

impl Target {
    fn ext(self) -> &'static str {
        match self {
            Self::Docx => "docx",
            Self::Pptx => "pptx",
            Self::Pdf => "pdf",
        }
    }

    /// `--convert-to`'s argument: the extension and LibreOffice's export
    /// filter, named so the output is exactly this format.
    fn convert_to(self, from: Format) -> String {
        let filter = match (self, from) {
            (Self::Docx, _) => "MS Word 2007 XML",
            (Self::Pptx, _) => "Impress MS PowerPoint 2007 XML",
            (Self::Pdf, Format::Doc | Format::Rtf | Format::Docx | Format::Odt) => {
                "writer_pdf_Export"
            }
            (Self::Pdf, _) => "impress_pdf_Export",
        };
        format!("{}:{filter}", self.ext())
    }

    /// What lmgw's sniffer must call the result.
    fn sub(self) -> &'static str {
        match self {
            Self::Docx => "docx",
            Self::Pptx => "pptx",
            Self::Pdf => "pdf",
        }
    }
}

/// The input's extension and LibreOffice's import filter for `format`
/// (`--infilter`): the file is opened as this and nothing else.
fn import(format: Format) -> Option<(&'static str, &'static str)> {
    Some(match format {
        Format::Doc => ("doc", "MS Word 97"),
        Format::Ppt => ("ppt", "MS PowerPoint 97"),
        Format::Rtf => ("rtf", "Rich Text Format"),
        Format::Docx => ("docx", "MS Word 2007 XML"),
        Format::Pptx => ("pptx", "Impress MS PowerPoint 2007 XML"),
        Format::Odp => ("odp", "impress8"),
        Format::Odt => ("odt", "writer8"),
        _ => return None,
    })
}

/// How a run failed, before it is worded for the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoError {
    /// `soffice` is not there.
    Missing,
    /// No private directory could be made (a full disk): the message.
    Scratch(String),
    /// LibreOffice could not open the file (damaged, or protected by a
    /// password; it says "source file could not be loaded"), or wrote no
    /// result.
    NotOpened,
    /// It ran past [`LIBREOFFICE_TIMEOUT`] (or the bound a test gave).
    Timeout,
    /// It said it ran out of [`LIBREOFFICE_MEMORY_LIMIT_BYTES`].
    OutOfMemory,
    /// It died of `SIGSEGV` or `SIGABRT` without saying why, which is how it
    /// ends when an allocation past its memory limit fails, and how it ends
    /// when a file crashes it: the exit status.
    Crashed(String),
    /// Its result is larger than `MAX_UNCOMPRESSED_BYTES`.
    TooLarge(u64),
    /// It wrote a file past its file-size limit (the bytes): the result
    /// would have been larger than `MAX_UNCOMPRESSED_BYTES` or than what is
    /// left of the request's output budget.
    FileTooLarge(u64),
    /// It failed otherwise: its exit status and first stderr lines.
    Failed(String),
    /// lmgw's reader could not confine it or set its limits
    /// (`RunError::NotStarted`): the message.
    NotStarted(String),
    /// Its single-instance socket in `/tmp` was there before it started, or
    /// another user's after it ended (security round B, L5): another
    /// process holds it, and LibreOffice hands its work to that one and
    /// ends without a result. lmgw's to look into, not the file's.
    SocketTaken,
    /// It ended by itself without a result and without saying the file
    /// could not be loaded (a damaged or locked file says so and exits
    /// with a failure): it did not convert here (hardening 6).
    NoResult,
}

impl LoError {
    /// The message a file's error carries.
    pub fn said(&self) -> String {
        match self {
            Self::Missing => "LibreOffice (soffice) was not found when it was run".to_string(),
            Self::Scratch(e) => format!(
                "no private directory for LibreOffice could be made under the data dir ({e})"
            ),
            Self::NotOpened => "LibreOffice could not open this file: it is damaged, or \
                                protected by a password"
                .to_string(),
            Self::Timeout => format!(
                "LibreOffice did not finish within LIBREOFFICE_TIMEOUT ({} s) and was stopped",
                LIBREOFFICE_TIMEOUT.as_secs()
            ),
            Self::OutOfMemory => format!(
                "LibreOffice ran out of LIBREOFFICE_MEMORY_LIMIT_BYTES ({}) and was stopped",
                crate::hf::fmt_bytes(LIBREOFFICE_MEMORY_LIMIT_BYTES)
            ),
            Self::Crashed(m) => format!(
                "LibreOffice stopped with a crash ({m}): the file needs more than \
                 LIBREOFFICE_MEMORY_LIMIT_BYTES ({}), or it crashes LibreOffice",
                crate::hf::fmt_bytes(LIBREOFFICE_MEMORY_LIMIT_BYTES)
            ),
            Self::TooLarge(n) => format!(
                "LibreOffice's result is {} or more, past MAX_UNCOMPRESSED_BYTES ({})",
                crate::hf::fmt_bytes(*n),
                crate::hf::fmt_bytes(MAX_UNCOMPRESSED_BYTES)
            ),
            Self::FileTooLarge(n) => format!(
                "LibreOffice wrote a file past its file-size limit of {} (the smaller of \
                 MAX_UNCOMPRESSED_BYTES, {}, and what is left of this request's output budget) \
                 and was stopped",
                crate::hf::fmt_bytes(*n),
                crate::hf::fmt_bytes(MAX_UNCOMPRESSED_BYTES)
            ),
            Self::Failed(m) => format!("LibreOffice failed: {m}"),
            Self::NotStarted(m) => m.clone(),
            Self::SocketTaken => "LibreOffice could not run here: its single-instance socket in \
                                  /tmp is held by another process, so it converted nothing; \
                                  this is not the file's fault (tool_not_started), and the \
                                  gateway's log names the socket"
                .to_string(),
            Self::NoResult => "LibreOffice ended without a result and without saying the file \
                               could not be loaded, so it did not convert here; this is not the \
                               file's fault (tool_not_started), and the gateway's log has what \
                               it said"
                .to_string(),
        }
    }
}

/// The bounds of a real run.
pub fn bounds() -> Bounds {
    Bounds {
        timeout: LIBREOFFICE_TIMEOUT,
        output: LIBREOFFICE_STDOUT_BYTES,
        memory: LIBREOFFICE_MEMORY_LIMIT_BYTES,
    }
}

/// Where runs make their private directories: `dir` (the data dir's
/// `convert-spool`), or the system's temporary directory where a
/// conversion has no data dir (a test's plan).
pub fn root(dir: Option<&Path>) -> PathBuf {
    dir.map_or_else(std::env::temp_dir, Path::to_path_buf)
}

/// A run's private directory and its socket in `/tmp`: removed when the
/// last holder lets go (the conversion, and the runner until the tool has
/// been reaped). The socket goes first (fields drop in order).
struct Scratch {
    socket: scratch::Socket,
    dir: tempfile::TempDir,
}

/// Where LibreOffice is and what confines it.
#[derive(Debug, Clone, Copy)]
pub struct Office<'a> {
    /// `soffice`.
    pub bin: &'a Path,
    /// lmgw's reader executable, which confines it (`confine.rs`).
    pub helper: &'a Path,
}

/// The environment of a run in `dir`: nothing of the gateway's.
fn environment(dir: &Path) -> [(String, String); 4] {
    [
        ("HOME".into(), dir.display().to_string()),
        ("TMPDIR".into(), dir.join("tmp").display().to_string()),
        ("SAL_USE_VCLPLUGIN".into(), "svp".into()),
        ("LANG".into(), "C.UTF-8".into()),
    ]
}

/// The arguments every run starts with, its profile at `profile`.
fn base_args(profile: &Path) -> Vec<String> {
    let mut a: Vec<String> = [
        "--headless",
        "--norestore",
        "--nolockcheck",
        "--nologo",
        "--nodefault",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push(format!(
        "-env:UserInstallation={}",
        scratch::profile_url(profile)
    ));
    a
}

/// One confined run of `office` in `dir` with `args`, on `slot`.
async fn run_in(
    slot: &pool::Slot,
    office: Office<'_>,
    dir: &Path,
    args: &[String],
    keep: Option<&Keep>,
    bounds: Bounds,
    file_size: u64,
) -> Result<Vec<u8>, RunError> {
    let rules =
        super::confine::libreoffice(office.bin, dir, file_size).map_err(RunError::NotStarted)?;
    let env = environment(dir);
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let tool = Tool {
        env: &env,
        dir: Some(dir),
        keep,
        confine: Some(run::Confined {
            helper: office.helper,
            rules: &rules,
        }),
        ..Tool::new(office.bin, &args)
    };
    run::run_held(slot, tool, Input::Bytes(Bytes::new()), bounds).await
}

/// The profile template under `root`, made now by a run without a
/// document where this process has none yet (`scratch`'s module doc).
async fn template(
    slot: &pool::Slot,
    office: Office<'_>,
    root: &Path,
    bounds: Bounds,
) -> Result<PathBuf, LoError> {
    static MADE: tokio::sync::Mutex<Vec<PathBuf>> = tokio::sync::Mutex::const_new(Vec::new());
    let dir = scratch::template_dir(root);
    let mut made = MADE.lock().await;
    if made.contains(&dir) && dir.join("profile").join(profile::REGISTRY_PATH).is_file() {
        return Ok(dir);
    }
    let fresh = dir.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        if fresh.exists() {
            std::fs::remove_dir_all(&fresh)?;
        }
        std::fs::create_dir(&fresh)?;
        std::fs::create_dir(fresh.join("tmp"))?;
        seed(&fresh.join("profile"))
    })
    .await
    .map_err(|e| LoError::Scratch(e.to_string()))?
    .map_err(|e| LoError::Scratch(e.to_string()))?;
    let profile = dir.join("profile");
    let socket = scratch::socket_path(&profile);
    if !scratch::free(&socket) {
        return Err(LoError::SocketTaken);
    }
    let _socket = scratch::Socket(socket.clone());
    let mut args = base_args(&profile);
    args.push("--terminate_after_init".into());
    let ran = run_in(
        slot,
        office,
        &dir,
        &args,
        None,
        bounds,
        MAX_UNCOMPRESSED_BYTES,
    )
    .await;
    if let Err(e) = ran {
        return Err(match failed(e) {
            LoError::Failed(m) => LoError::Failed(format!("its profile could not be made: {m}")),
            other => other,
        });
    }
    // Another user's socket where LibreOffice's own should be: it handed
    // its start to that one, and the profile is not its own.
    if scratch::foreign(&socket) {
        return Err(LoError::SocketTaken);
    }
    made.push(dir.clone());
    Ok(dir)
}

/// The seeded settings written into a new profile at `profile`.
fn seed(profile: &Path) -> std::io::Result<()> {
    let registry = profile.join(profile::REGISTRY_PATH);
    if let Some(parent) = registry.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&registry, profile::REGISTRY)
}

/// A run's failure as LibreOffice's.
fn failed(e: RunError) -> LoError {
    match e {
        RunError::Missing => LoError::Missing,
        RunError::NoReader => LoError::Failed(super::run::start::no_reader()),
        RunError::Timeout => LoError::Timeout,
        RunError::OutOfMemory => LoError::OutOfMemory,
        RunError::FileTooLarge(n) => LoError::FileTooLarge(n),
        RunError::TooLarge => LoError::Failed(format!(
            "it printed more than {} on stdout",
            crate::hf::fmt_bytes(LIBREOFFICE_STDOUT_BYTES)
        )),
        RunError::Failed(m) if m.contains("could not be loaded") => LoError::NotOpened,
        RunError::Failed(m) if killed_by_limit(&m) => LoError::Crashed(m),
        RunError::Failed(m) => LoError::Failed(m),
        RunError::NotStarted(m) => LoError::NotStarted(m),
    }
}

/// `bytes` (a `from` file) turned into `to` by `office`, confined, in a
/// private directory under `root`, on `slot`, within `bounds`, its files
/// within `file_size` bytes.
#[allow(clippy::too_many_arguments)]
pub async fn convert(
    slot: &pool::Slot,
    office: Office<'_>,
    root: &Path,
    bytes: Bytes,
    from: Format,
    to: Target,
    bounds: Bounds,
    file_size: u64,
) -> Result<Bytes, LoError> {
    let Some((ext, infilter)) = import(from) else {
        return Err(LoError::Failed(format!(
            "lmgw has no LibreOffice import for {} files",
            from.as_str()
        )));
    };
    let root = root.to_path_buf();
    let made_root = root.clone();
    tokio::task::spawn_blocking(move || {
        scratch::make_private(&made_root)?;
        scratch::sweep_once(&made_root);
        Ok::<_, std::io::Error>(())
    })
    .await
    .map_err(|e| LoError::Scratch(e.to_string()))?
    .map_err(|e| LoError::Scratch(e.to_string()))?;
    let template = template(slot, office, &root, bounds).await?;
    let made = tokio::task::spawn_blocking(move || prepare(&root, &template, ext, &bytes))
        .await
        .map_err(|e| LoError::Scratch(e.to_string()))?
        .map_err(|e| LoError::Scratch(e.to_string()))?;
    let dir = made.dir.path().to_path_buf();
    let socket = made.socket.0.clone();
    let keep: Keep = Arc::new(made);
    if !scratch::free(&socket) {
        return Err(LoError::SocketTaken);
    }
    let mut args = base_args(&dir.join("profile"));
    args.push(format!("--infilter={infilter}"));
    args.push("--convert-to".into());
    args.push(to.convert_to(from));
    args.push("--outdir".into());
    args.push("out".into());
    args.push(format!("in.{ext}"));
    let ran = run_in(slot, office, &dir, &args, Some(&keep), bounds, file_size).await;
    if let Err(e) = ran {
        // `soffice.bin` dies of `SIGXFSZ` under `oosplash`, which exits
        // with a status of its own: a file the limit cut is what says so.
        let (at, input) = (dir.clone(), dir.join(format!("in.{ext}")));
        let cut = tokio::task::spawn_blocking(move || reached(&at, file_size, &input))
            .await
            .unwrap_or(false);
        return Err(if cut {
            LoError::FileTooLarge(file_size)
        } else {
            failed(e)
        });
    }
    let out = PathBuf::from("out").join(format!("in.{}", to.ext()));
    let at = dir.clone();
    let read = tokio::task::spawn_blocking(move || {
        // No result after a run that ended by itself: not the file's fault
        // (a file LibreOffice cannot load fails the run above).
        let data = match read_bounded(&at, &out, MAX_UNCOMPRESSED_BYTES) {
            Err(LoError::NotOpened) if scratch::foreign(&socket) => {
                return Err(LoError::SocketTaken)
            }
            Err(LoError::NotOpened) => return Err(LoError::NoResult),
            other => other?,
        };
        // The result must be what was asked for, or lmgw's readers would
        // take it for something else.
        match crate::extract::sniff::sniff(&data) {
            Ok(s) if s.sub == to.sub() => Ok(data),
            _ => Err(LoError::NotOpened),
        }
    })
    .await
    .map_err(|e| LoError::Failed(e.to_string()))?;
    drop(keep);
    read
}

/// Whether a file under `dir` other than the input `input` (which the
/// gateway wrote) is exactly `limit` bytes: the file-size limit cut it
/// there (a write that crosses it is shortened to it, the next one is the
/// signal).
fn reached(dir: &Path, limit: u64, input: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| match e.file_type() {
        Ok(t) if t.is_dir() => reached(&e.path(), limit, input),
        Ok(t) if t.is_file() && e.path() != input => e.metadata().is_ok_and(|m| m.len() == limit),
        _ => false,
    })
}

/// A run died of a signal its memory limit causes (LibreOffice ends in
/// `SIGSEGV` or `SIGABRT` when an allocation fails, without saying so).
fn killed_by_limit(message: &str) -> bool {
    ["signal: 11", "signal: 6", "SIGSEGV", "SIGABRT"]
        .iter()
        .any(|s| message.contains(s))
}

/// Make a run's private directory under `root` with the input, the output
/// and temporary directories and a copy of the profile `template`.
fn prepare(root: &Path, template: &Path, ext: &str, bytes: &[u8]) -> std::io::Result<Scratch> {
    let dir = tempfile::Builder::new()
        .prefix(&scratch::own_prefix())
        .tempdir_in(root)?;
    let p = dir.path();
    scratch::copy_tree(&template.join("profile"), &p.join("profile"))?;
    std::fs::create_dir(p.join("out"))?;
    std::fs::create_dir(p.join("tmp"))?;
    std::fs::write(p.join(format!("in.{ext}")), bytes)?;
    Ok(Scratch {
        socket: scratch::Socket(scratch::socket_path(&p.join("profile"))),
        dir,
    })
}

/// The result at `rel` beneath the run's directory `dir`, refused past
/// `limit` (`MAX_UNCOMPRESSED_BYTES`) by its size and read no further than
/// that; opened without following a link (`crate::child::beneath`): what
/// is not a regular file there is no result.
fn read_bounded(dir: &Path, rel: &Path, limit: u64) -> Result<Bytes, LoError> {
    use std::io::Read;
    let file = crate::child::beneath::open(dir, rel).map_err(|_| LoError::NotOpened)?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len > limit {
        return Err(LoError::TooLarge(len));
    }
    let mut data = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    file.take(limit + 1)
        .read_to_end(&mut data)
        .map_err(|e| LoError::Failed(e.to_string()))?;
    if data.len() as u64 > limit {
        return Err(LoError::TooLarge(data.len() as u64));
    }
    Ok(Bytes::from(data))
}

/// A deck (`.ppt`, `.pptx`, `.odp`) rendered to a PDF by `office`, whose
/// pages the PDF path reads (text layer, pictures, text recognition) and
/// whose page images would go to the page-reading model (design §4.2's
/// "slides with charts"). No conversion calls this yet: the page-reading
/// model reads a PDF's pages (`model_help/pdf.rs`), and a deck is read as
/// the pptx LibreOffice writes; design §14 WP7's review round says what
/// wiring it into the model stage still needs.
pub async fn slides_pdf(
    slot: &pool::Slot,
    office: Office<'_>,
    root: &Path,
    deck: Bytes,
    format: Format,
    file_size: u64,
) -> Result<Bytes, LoError> {
    convert(
        slot,
        office,
        root,
        deck,
        format,
        Target::Pdf,
        bounds(),
        file_size,
    )
    .await
}

#[cfg(all(test, unix))]
mod tests;
