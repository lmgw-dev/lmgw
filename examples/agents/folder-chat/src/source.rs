//! Reading one file of the folder by the path the index knows it by — the
//! sync's reads and the MCP `read` tool. (A citation's line numbers are not
//! read from the file: the sync stores them with each chunk, see
//! [`crate::index::IndexDir::fill_stored_lines`].)
//!
//! **Every read of a file's content goes through [`read_beneath`]**, the sync
//! as much as a model's tool call: a path from outside is untrusted, and a
//! path the scan listed a moment ago is not trustworthy either — the owner (or
//! anything else writing the folder) can swap a directory for a symlink
//! between the scan and the read. Every refusal names its rule
//! ([`ReadError`]), and the checks run in this order:
//!
//! 1. **Lexically**: not absolute, no `..` component, no hidden component (a
//!    name starting with `.` — which also keeps the agent's own index
//!    directory out), and a supported file type ([`FileKind`]).
//! 2. **On the filesystem, one component at a time**: every directory is
//!    opened with `openat(…, O_DIRECTORY | O_NOFOLLOW)` relative to the one
//!    before it, and the file itself with `O_NOFOLLOW | O_NONBLOCK`, so a
//!    symlink anywhere on the path is refused by the kernel rather than by a
//!    check that could race a rename, and a FIFO cannot hang the open. The
//!    open descriptor is then `fstat`ed and anything but a regular file is
//!    refused before a byte is read; the bytes come from that descriptor.
//! 3. **The folder's `.gitignore` / `.ignore` rules**
//!    ([`crate::scan::ignored_by_rules`], MCP `read` only — the sync's scan
//!    has applied them already), read through the same kind of descriptor
//!    walk.
//!
//! A PDF's bytes — the ones read here, the ones the sync hashes — are piped to
//! `pdftotext` on stdin ([`crate::pdf::extract_bytes`]); no PDF is ever handed
//! to it by path, which it would resolve again. Inside the container a
//! followed symlink could reach `/lmgw/secrets.json` (the agent token), which
//! is why no path is ever followed, by the sync or by a tool call.
//!
//! **No size cap on a tool call's read.** A file is read whole; a line range
//! selects from it, and a range past the end is said, not silently shortened.
//! The sync passes a ceiling ([`read_beneath`]'s `max_bytes`) derived from the
//! container's memory limit, and reports a file over it as a skip that names
//! the limit (see `sync::LARGE_FILE_MEMORY_FRACTION`).

use std::ffi::{CString, OsStr, OsString};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::pdf::{self, PdfError};
use crate::scan::FileKind;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReadError {
    #[error("no path was given; pass a path relative to the folder, as search returns it")]
    Empty,
    #[error(
        "'{0}' is an absolute path; paths are relative to the folder, exactly as search \
         returns them (rule: no absolute paths)"
    )]
    Absolute(String),
    #[error("'{0}' has a '..' component; a path may not leave the folder (rule: no '..')")]
    ParentDir(String),
    #[error(
        "'{path}' goes through the hidden entry '{component}'; hidden files and directories \
         are never read (rule: no hidden components)"
    )]
    Hidden { path: String, component: String },
    #[error(
        "'{0}' is not a supported type; only markdown, plain text, source code and PDF files \
         are read (rule: supported types only)"
    )]
    Unsupported(String),
    #[error(
        "'{path}' is excluded by the pattern '{pattern}' in {file}; what the folder's ignore \
         files exclude is never read (rule: .gitignore and .ignore are honoured)"
    )]
    Ignored {
        path: String,
        pattern: String,
        file: String,
    },
    #[error(
        "'{path}' goes through the symlink '{component}'; symlinks are never followed (rule: \
         no symlinks on the path)"
    )]
    Symlink { path: String, component: String },
    #[error("'{0}' does not exist in the folder")]
    NotFound(String),
    #[error("'{0}' is a directory, not a file")]
    IsDir(String),
    #[error("'{0}' is not a regular file")]
    NotRegular(String),
    /// Over the caller's `max_bytes` — only the sync passes one. The sync
    /// words its own skip reason from `size` and `max`.
    #[error("'{path}' is {size} bytes, more than the {max} bytes this read may hold")]
    TooLarge { path: String, size: u64, max: u64 },
    #[error("'{path}' is not UTF-8 text (invalid UTF-8 at byte {at})")]
    NotUtf8 { path: String, at: usize },
    #[error("'{path}': {error}")]
    Pdf { path: String, error: PdfError },
    #[error("'{path}' could not be read: {message}")]
    Io { path: String, message: String },
    #[error("{0}")]
    Range(String),
}

/// A file's text as the index sees it: the file's own UTF-8, or pdftotext's
/// output for a PDF (pages separated by form feeds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceText {
    pub path: String,
    pub kind: FileKind,
    pub text: String,
    /// For a PDF, [`content_hash`] of the bytes read under the `pdftotext`
    /// that extracted them — what the index keys a PDF's stored page
    /// readings by, so the caller can append the ones that belong to these
    /// bytes ([`crate::FolderChat::read_file`]). `None` for other kinds.
    pub content_hash: Option<String>,
}

/// hex(sha256) of a file's bytes, as the index records it. The hash of a PDF
/// also covers the `pdftotext` version that extracts it (`pdf_version`,
/// `pdftotext -v`'s first line), so text extracted by another version is
/// never taken for current.
pub fn content_hash(bytes: &[u8], kind: FileKind, pdf_version: Option<&str>) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    if kind == FileKind::Pdf {
        h.update([0u8]);
        h.update(pdf_version.unwrap_or("").as_bytes());
    }
    hex::encode(h.finalize())
}

/// Some lines of a [`SourceText`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LineSlice {
    pub path: String,
    pub kind: FileKind,
    /// 1-based, inclusive. `0..0` for an empty file.
    pub start_line: usize,
    pub end_line: usize,
    pub total_lines: usize,
    pub text: String,
    /// Set when the range asked for ran past the end and was ended there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Check `rel` lexically and split it into its components. `.` components are
/// dropped (`./notes.md` is `notes.md`); everything else that is not a plain
/// name is refused, naming the rule.
pub fn check_path(rel: &str) -> Result<Vec<String>, ReadError> {
    if rel.trim().is_empty() {
        return Err(ReadError::Empty);
    }
    if rel.starts_with('/') {
        return Err(ReadError::Absolute(rel.to_string()));
    }
    let mut parts = Vec::new();
    for part in rel.split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err(ReadError::ParentDir(rel.to_string())),
            p if p.starts_with('.') => {
                return Err(ReadError::Hidden {
                    path: rel.to_string(),
                    component: p.to_string(),
                })
            }
            p => parts.push(p.to_string()),
        }
    }
    let Some(name) = parts.last() else {
        return Err(ReadError::Empty);
    };
    if FileKind::of(name).is_none() {
        return Err(ReadError::Unsupported(rel.to_string()));
    }
    Ok(parts)
}

fn c_name(path: &str, name: &str) -> Result<CString, ReadError> {
    CString::new(name).map_err(|_| ReadError::Io {
        path: path.to_string(),
        message: "the path contains a NUL byte".into(),
    })
}

/// A name for the `*at` calls. A name with a NUL byte cannot exist on disk.
pub(crate) fn c_os(name: &OsStr) -> std::io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "a NUL byte in a name"))
}

/// Open a directory by path, following a symlink in it — only for the folder
/// itself (the mount point the owner chose) and never for anything under it.
pub(crate) fn open_dir_path(path: &Path) -> std::io::Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path)?
        .into())
}

/// `openat(dir, name, flags | O_CLOEXEC)`. The caller picks the flags; every
/// caller in this crate passes `O_NOFOLLOW`.
pub(crate) fn open_at(
    dir: &OwnedFd,
    name: &CString,
    flags: libc::c_int,
) -> std::io::Result<OwnedFd> {
    open_at_mode(dir, name, flags, 0)
}

/// [`open_at`] with a mode, for the index directory's `O_CREAT` opens.
pub(crate) fn open_at_mode(
    dir: &OwnedFd,
    name: &CString,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> std::io::Result<OwnedFd> {
    // SAFETY: `dir` is an open descriptor and `name` a NUL-terminated string;
    // a non-negative return is a fresh descriptor this function now owns.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: see above — `fd` is valid and owned by nobody else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// `fstatat(dir, name, AT_SYMLINK_NOFOLLOW)`: the entry itself, never what it
/// points at.
pub(crate) fn stat_at(dir: &OwnedFd, name: &CString) -> std::io::Result<libc::stat> {
    // SAFETY: an all-zero `stat` is a valid value to be overwritten.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor, NUL-terminated name, a writable `stat`.
    let r = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if r == 0 {
        Ok(st)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// `fstat` of an open descriptor.
pub(crate) fn stat_fd(fd: &OwnedFd) -> std::io::Result<libc::stat> {
    // SAFETY: as in `stat_at`.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor, a writable `stat`.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } == 0 {
        Ok(st)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) fn is_type(st: &libc::stat, fmt: libc::mode_t) -> bool {
    (st.st_mode & libc::S_IFMT) == fmt
}

fn is_symlink_at(dir: &OwnedFd, name: &CString) -> bool {
    stat_at(dir, name).is_ok_and(|st| is_type(&st, libc::S_IFLNK))
}

/// Every name in the directory behind `dir` (not `.` or `..`), unsorted —
/// read through the descriptor, so a directory swapped for a symlink after it
/// was opened is still the one listed.
pub(crate) fn list_dir(dir: &OwnedFd) -> std::io::Result<Vec<OsString>> {
    use std::os::unix::ffi::OsStringExt;
    // `fdopendir` takes ownership of the descriptor it is given; it gets a
    // duplicate so `dir` stays ours for the `*at` calls.
    // SAFETY: `dir` is an open descriptor.
    let dup = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `dup` is a fresh descriptor; on success the `DIR` owns it.
    let dp = unsafe { libc::fdopendir(dup) };
    if dp.is_null() {
        let e = std::io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so `dup` is still ours to close.
        unsafe { libc::close(dup) };
        return Err(e);
    }
    struct Dir(*mut libc::DIR);
    impl Drop for Dir {
        fn drop(&mut self) {
            // SAFETY: a `DIR` from `fdopendir`, closed exactly once.
            unsafe { libc::closedir(self.0) };
        }
    }
    let d = Dir(dp);
    // A duplicate shares the file offset: start from the top whatever read
    // the directory before.
    // SAFETY: a valid `DIR`.
    unsafe { libc::rewinddir(d.0) };
    let mut out = Vec::new();
    loop {
        // `readdir` returns NULL both at the end and on an error; only errno
        // tells them apart.
        // SAFETY: errno is thread-local; writing it is always allowed.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: a valid `DIR`.
        let ent = unsafe { libc::readdir(d.0) };
        if ent.is_null() {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error().unwrap_or(0) != 0 {
                return Err(e);
            }
            break;
        }
        // SAFETY: `d_name` is NUL-terminated and lives until the next
        // `readdir`; it is copied out before then.
        let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        out.push(OsString::from_vec(name.to_vec()));
    }
    Ok(out)
}

/// Read `name` in `dir` if it is a regular file: never through a symlink,
/// never blocking on a FIFO (the ignore files the scan honours).
pub(crate) fn read_regular_at(dir: &OwnedFd, name: &CString) -> std::io::Result<Vec<u8>> {
    let fd = open_at(
        dir,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
    )?;
    let mut f = std::fs::File::from(fd);
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// What [`read_beneath`] read: the bytes, and what `fstat` said about the open
/// descriptor just before and just after reading them — the sync compares
/// both with what the scan saw (see `sync::RACY_MTIME_WINDOW`).
#[derive(Debug)]
pub struct Beneath {
    pub bytes: Vec<u8>,
    pub before: std::fs::Metadata,
    pub after: std::fs::Metadata,
}

/// Open `parts` beneath `root` without following a symlink anywhere, check
/// the open descriptor is a regular file, and read it whole. Blocking.
///
/// `max_bytes`: refuse ([`ReadError::TooLarge`]) a file larger than this,
/// judged by `fstat` before anything is read, and again while reading, so a
/// file that grows mid-read is not held past it either. `None` reads whatever
/// is there.
pub fn read_beneath(
    root: &Path,
    rel: &str,
    parts: &[String],
    max_bytes: Option<u64>,
) -> Result<Beneath, ReadError> {
    let io = |e: std::io::Error| ReadError::Io {
        path: rel.to_string(),
        message: e.to_string(),
    };
    let mut dir: OwnedFd = open_dir_path(root).map_err(io)?;
    let (last, dirs) = parts.split_last().ok_or(ReadError::Empty)?;
    let mut walked = String::new();
    for d in dirs {
        if !walked.is_empty() {
            walked.push('/');
        }
        walked.push_str(d);
        let name = c_name(rel, d)?;
        dir = match open_at(
            &dir,
            &name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        ) {
            Ok(fd) => fd,
            Err(_) if is_symlink_at(&dir, &name) => {
                return Err(ReadError::Symlink {
                    path: rel.to_string(),
                    component: walked,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ReadError::NotFound(rel.to_string()))
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => {
                return Err(ReadError::NotFound(rel.to_string()))
            }
            Err(e) => return Err(io(e)),
        };
    }
    let name = c_name(rel, last)?;
    let fd = match open_at(
        &dir,
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
    ) {
        Ok(fd) => fd,
        Err(_) if is_symlink_at(&dir, &name) => {
            return Err(ReadError::Symlink {
                path: rel.to_string(),
                component: rel.to_string(),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ReadError::NotFound(rel.to_string()))
        }
        Err(e) => return Err(io(e)),
    };
    let mut file = std::fs::File::from(fd);
    let before = file.metadata().map_err(io)?;
    if before.is_dir() {
        return Err(ReadError::IsDir(rel.to_string()));
    }
    if !before.is_file() {
        return Err(ReadError::NotRegular(rel.to_string()));
    }
    let too_large = |size: u64, max: u64| ReadError::TooLarge {
        path: rel.to_string(),
        size,
        max,
    };
    if let Some(max) = max_bytes {
        if before.len() > max {
            return Err(too_large(before.len(), max));
        }
    }
    let mut buf = Vec::with_capacity(before.len() as usize);
    match max_bytes {
        // One byte past the ceiling is enough to know it was passed.
        Some(max) => {
            (&mut file)
                .take(max.saturating_add(1))
                .read_to_end(&mut buf)
                .map_err(io)?;
            if buf.len() as u64 > max {
                return Err(too_large(buf.len() as u64, max));
            }
        }
        None => {
            file.read_to_end(&mut buf).map_err(io)?;
        }
    }
    let after = file.metadata().map_err(io)?;
    Ok(Beneath {
        bytes: buf,
        before,
        after,
    })
}

/// Read `rel` from the folder at `root` as the index sees it. Every refusal
/// names its rule; see the module docs.
pub async fn read_text(root: &Path, rel: &str) -> Result<SourceText, ReadError> {
    let parts = check_path(rel)?;
    let clean = parts.join("/");
    let kind = FileKind::of(parts.last().map(String::as_str).unwrap_or_default())
        .ok_or_else(|| ReadError::Unsupported(rel.to_string()))?;
    let root_buf = root.to_path_buf();
    let (clean2, rel2) = (clean.clone(), rel.to_string());
    let bytes = tokio::task::spawn_blocking(move || {
        // The descriptor walk first: a symlink on the path is refused before
        // anything looks for ignore files along it.
        let bytes = read_beneath(&root_buf, &rel2, &parts, None)?.bytes;
        if let Some(hit) = crate::scan::ignored_by_rules(&root_buf, &clean2) {
            return Err(ReadError::Ignored {
                path: rel2,
                pattern: hit.pattern,
                file: hit.file,
            });
        }
        Ok(bytes)
    })
    .await
    .map_err(|e| ReadError::Io {
        path: rel.to_string(),
        message: e.to_string(),
    })??;
    let (text, content_hash) = match kind {
        FileKind::Pdf => {
            let version = pdf::version(Path::new(pdf::PDFTOTEXT)).await;
            let hash = content_hash(&bytes, kind, version.as_deref());
            let text = pdf::extract_bytes(bytes)
                .await
                .map_err(|error| ReadError::Pdf {
                    path: rel.to_string(),
                    error,
                })?;
            (text, Some(hash))
        }
        _ => (
            String::from_utf8(bytes).map_err(|e| ReadError::NotUtf8 {
                path: rel.to_string(),
                at: e.utf8_error().valid_up_to(),
            })?,
            None,
        ),
    };
    Ok(SourceText {
        path: clean,
        kind,
        text,
        content_hash,
    })
}

/// Lines in `text` as the chunker counts them: a line ends at `\n`, and a
/// final `\n` does not start another one.
pub fn total_lines(text: &str) -> usize {
    text.split_inclusive('\n').count()
}

impl SourceText {
    /// Lines `start..=end` (1-based). No range is the whole file; only a start
    /// reads to the end; only an end reads from the first line. An end past
    /// the last line ends there and says so in [`LineSlice::note`].
    pub fn lines(&self, start: Option<usize>, end: Option<usize>) -> Result<LineSlice, ReadError> {
        let total = total_lines(&self.text);
        let slice = |start_line: usize, end_line: usize, text: String, note| LineSlice {
            path: self.path.clone(),
            kind: self.kind,
            start_line,
            end_line,
            total_lines: total,
            text,
            note,
        };
        if start.is_none() && end.is_none() {
            return Ok(slice(total.min(1), total, self.text.clone(), None));
        }
        let s = start.unwrap_or(1);
        if s == 0 {
            return Err(ReadError::Range(
                "start_line counts from 1; the first line is line 1".into(),
            ));
        }
        if let Some(e) = end {
            if e < s {
                return Err(ReadError::Range(format!(
                    "end_line ({e}) is before start_line ({s})"
                )));
            }
        }
        if s > total {
            return Err(ReadError::Range(format!(
                "start_line {s} is past the end: {} has {total} line{}",
                self.path,
                if total == 1 { "" } else { "s" }
            )));
        }
        let (e, note) = match end {
            Some(e) if e > total => (
                total,
                Some(format!(
                    "end_line {e} is past the end; {} has {total} lines, so this ends at line \
                     {total}",
                    self.path
                )),
            ),
            Some(e) => (e, None),
            None => (total, None),
        };
        let text: String = self
            .text
            .split_inclusive('\n')
            .skip(s - 1)
            .take(e - s + 1)
            .collect();
        Ok(slice(s, e, text, note))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(text: &str) -> SourceText {
        SourceText {
            path: "a.md".into(),
            kind: FileKind::Markdown,
            text: text.into(),
            content_hash: None,
        }
    }

    #[test]
    fn lexical_refusals_name_their_rule() {
        let e = check_path("/etc/passwd.txt").unwrap_err();
        assert!(e.to_string().contains("no absolute paths"), "{e}");
        let e = check_path("docs/../../x.md").unwrap_err();
        assert!(e.to_string().contains("no '..'"), "{e}");
        let e = check_path(".lmgw-folder-chat/index.md").unwrap_err();
        assert!(e.to_string().contains("no hidden components"), "{e}");
        let e = check_path("photo.png").unwrap_err();
        assert!(e.to_string().contains("supported types only"), "{e}");
        assert_eq!(check_path("./docs//a.md").unwrap(), ["docs", "a.md"]);
    }

    #[test]
    fn line_slices_count_like_the_chunker() {
        let s = src("one\ntwo\nthree\n");
        assert_eq!(total_lines(&s.text), 3);
        let all = s.lines(None, None).unwrap();
        assert_eq!((all.start_line, all.end_line, all.total_lines), (1, 3, 3));
        let mid = s.lines(Some(2), Some(2)).unwrap();
        assert_eq!(mid.text, "two\n");
        let tail = s.lines(Some(2), None).unwrap();
        assert_eq!(tail.text, "two\nthree\n");
        let past = s.lines(Some(3), Some(9)).unwrap();
        assert_eq!(past.end_line, 3);
        assert!(past.note.unwrap().contains("past the end"));
        assert!(s.lines(Some(4), None).is_err());
        assert!(s.lines(Some(0), None).is_err());
        assert!(s.lines(Some(3), Some(2)).is_err());
        let empty = src("").lines(None, None).unwrap();
        assert_eq!((empty.start_line, empty.end_line), (0, 0));
    }
}
