//! Walking the folder: what is indexed, what is skipped, and why.
//!
//! The rules, all of them here:
//!
//! - Every **hidden** entry (a name starting with `.`) is skipped, directory
//!   and all. That covers `.git`, editor state, and the agent's own index
//!   directory.
//! - **Symlinks are never followed** — a link could lead out of the folder the
//!   owner chose — and each one is listed as skipped.
//! - A file is included by its **type**: markdown, plain text, source code or
//!   PDF, by extension (case-insensitive) or, for a few build files, by name.
//!   Everything else is skipped as unsupported.
//! - **Size alone never skips a file**: a huge file is many chunks. The one
//!   exception is memory, and it is the sync's, not the walk's: a single file
//!   larger than `sync::LARGE_FILE_MEMORY_FRACTION` of the container's memory
//!   limit is skipped as [`SkipReason::TooLargeForMemory`], with a reason
//!   that names the limit, the fraction and how to raise it.
//! - The folder's own **`.gitignore` and `.ignore` files** are honoured,
//!   wherever they sit in the tree and whether or not the folder is a git
//!   repository ([`IGNORE_FILE_NAMES`]). Git's rules apply: a pattern is
//!   relative to the directory of the file it is in, a deeper file overrides a
//!   shallower one, `!pattern` re-includes, and an excluded directory is
//!   **pruned** — not walked, so nothing under it can be re-included. In one
//!   directory `.ignore` wins over `.gitignore`, as in ripgrep. Nothing else is
//!   read: not `.git/info/exclude`, not the user's global excludes file (the
//!   container has no user's home to find one in, and a rule the owner cannot
//!   see in the folder would be a hidden limit). An ignored entry is counted
//!   under [`SkipReason::Ignored`]; a pruned directory is **one** entry and is
//!   listed by path in [`Scan::ignored_dirs`], an ignored file is counted only
//!   ([`Scan::ignored_files`]) — a `*.log` rule over a busy folder would
//!   otherwise list thousands of paths nobody asked to see.
//!
//! The order of the checks keeps phase 1's accounting stable: a hidden entry
//! is `hidden` and a symlink is `symlink` whatever an ignore file says; the
//! ignore rules are asked only about what is left.
//!
//! **The walk holds descriptors, not paths.** Each directory is opened with
//! `openat(parent, name, O_DIRECTORY | O_NOFOLLOW)` and listed through that
//! descriptor; every entry is `fstatat(…, AT_SYMLINK_NOFOLLOW)`ed relative to
//! it; ignore files are opened relative to it with `O_NOFOLLOW | O_NONBLOCK`
//! and read only when they are regular files. A directory swapped for a
//! symlink while the walk runs is refused by the kernel (and listed as a
//! skipped symlink), so nothing outside the folder is ever listed or read.
//! The sync then reads each file through [`crate::source::read_beneath`],
//! which walks the path the same way again: what the walk saw is a plan, not
//! a promise.
//!
//! Content-level skips (not UTF-8, a PDF with no text, a PDF that timed out,
//! a file too large for memory) are only knowable once a file is opened, so
//! the sync adds those; every skip, from either place, is counted by
//! [`SkipReason`] in the sync report and listed with its path.

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::Match;
use serde::{Deserialize, Serialize};

use crate::source;

/// The ignore files honoured in every directory of the folder, in the order
/// their rules are applied: a later file overrides an earlier one in the same
/// directory (ripgrep's precedence: `.ignore` over `.gitignore`).
pub const IGNORE_FILE_NAMES: &[&str] = &[".gitignore", ".ignore"];

/// Markdown: split on headings, each chunk carries its heading path.
pub const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown"];

/// Plain text: split at blank lines, then at line boundaries.
pub const TEXT_EXTENSIONS: &[&str] = &["txt", "rst", "org", "adoc", "csv", "log"];

/// Source code, chunked like plain text. The list is deliberately explicit —
/// "anything that decodes as UTF-8" would index lockfiles, minified bundles
/// and data dumps nobody asked about.
pub const CODE_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "kt", "c", "h", "cpp", "hpp", "cs", "rb",
    "php", "swift", "sh", "bash", "zsh", "fish", "sql", "toml", "yaml", "yml", "json", "html",
    "css", "scss", "lua", "r", "jl", "ex", "exs", "hs", "ml", "scala", "dart", "vue", "svelte",
    "nix", "tf",
];

/// Build files that are source code but carry no extension. Matched exactly.
pub const CODE_FILE_NAMES: &[&str] = &["Makefile", "Dockerfile", "Containerfile"];

/// Extracted with `pdftotext`, chunked per page.
pub const PDF_EXTENSIONS: &[&str] = &["pdf"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Markdown,
    Text,
    Code,
    Pdf,
}

impl FileKind {
    /// The kind a file name is indexed as, or `None` for an unsupported type.
    pub fn of(name: &str) -> Option<Self> {
        if CODE_FILE_NAMES.contains(&name) {
            return Some(Self::Code);
        }
        let ext = Path::new(name)
            .extension()
            .and_then(|e| e.to_str())?
            .to_ascii_lowercase();
        let ext = ext.as_str();
        if MARKDOWN_EXTENSIONS.contains(&ext) {
            Some(Self::Markdown)
        } else if TEXT_EXTENSIONS.contains(&ext) {
            Some(Self::Text)
        } else if CODE_EXTENSIONS.contains(&ext) {
            Some(Self::Code)
        } else if PDF_EXTENSIONS.contains(&ext) {
            Some(Self::Pdf)
        } else {
            None
        }
    }
}

/// Why something in the folder is not in the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// A name starting with `.`; a hidden directory is one skip, not walked.
    Hidden,
    /// Never followed.
    Symlink,
    /// Excluded by a `.gitignore` or `.ignore` rule in the folder; an excluded
    /// directory is one skip, not walked.
    Ignored,
    /// Not markdown, plain text, source code or PDF — or not a regular file.
    Unsupported,
    /// The file or directory could not be read.
    Unreadable,
    /// A text-type file whose bytes are not UTF-8, or a path that is not.
    NotUtf8,
    /// `pdftotext` ran and found no text — typically a scan without OCR.
    PdfNoText,
    /// Readable, but nothing to index: empty or whitespace only.
    Empty,
    /// `pdftotext` did not finish within `pdf::PDF_EXTRACT_TIMEOUT` and was
    /// stopped; the detail names the constant and its value.
    PdfTimeout,
    /// One file larger than `sync::LARGE_FILE_MEMORY_FRACTION` of the
    /// container's memory limit; the detail names the limit, the fraction,
    /// and how to raise `run.limits.memory_mb` or ignore the file.
    TooLargeForMemory,
}

impl SkipReason {
    /// Every reason, in declaration order — the order a by-reason table
    /// (a `BTreeMap`) lists them in.
    pub const ALL: &'static [SkipReason] = &[
        Self::Hidden,
        Self::Symlink,
        Self::Ignored,
        Self::Unsupported,
        Self::Unreadable,
        Self::NotUtf8,
        Self::PdfNoText,
        Self::Empty,
        Self::PdfTimeout,
        Self::TooLargeForMemory,
    ];

    pub fn describe(self) -> &'static str {
        match self {
            Self::Hidden => "hidden (name starts with '.')",
            Self::Symlink => "symlink (never followed)",
            Self::Unsupported => "unsupported file type",
            Self::Unreadable => "unreadable",
            Self::NotUtf8 => "not UTF-8 text",
            Self::PdfNoText => "PDF without extractable text",
            Self::Ignored => "excluded by the folder's .gitignore or .ignore",
            Self::Empty => "empty (no text)",
            Self::PdfTimeout => "PDF text extraction timed out (PDF_EXTRACT_TIMEOUT)",
            Self::TooLargeForMemory => {
                "too large for the container's memory limit (LARGE_FILE_MEMORY_FRACTION)"
            }
        }
    }
}

/// One skipped entry, path relative to the folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub path: String,
    pub reason: SkipReason,
    /// The underlying error, when there is one worth reading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A file the scan will hand to the sync. There is deliberately no absolute
/// path here: the sync reads a candidate by `rel`, through
/// [`crate::source::read_beneath`], never by a path resolved again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Relative to the folder, `/`-separated — this is `document.url`.
    pub rel: String,
    pub kind: FileKind,
    /// `st_size` and the mtime (see [`mtime_ns_of`]) as the walk saw them.
    pub size: u64,
    pub mtime_ns: i64,
}

#[derive(Debug, Clone, Default)]
pub struct Scan {
    /// Sorted by `rel`, so a sync processes files in a reproducible order.
    pub files: Vec<Candidate>,
    pub skipped: Vec<Skipped>,
    /// Files an ignore rule excluded — counted, not listed.
    pub ignored_files: usize,
    /// Directories an ignore rule excluded, pruned without being walked —
    /// each one entry, listed by path, sorted.
    pub ignored_dirs: Vec<String>,
    /// The ignore files whose rules were applied, by path, sorted.
    pub ignore_files: Vec<String>,
    /// An ignore file that could not be read, or a line in one that is not a
    /// valid pattern — said once each, never dropped quietly.
    pub notes: Vec<String>,
}

impl Scan {
    pub fn skipped_by_reason(&self) -> BTreeMap<SkipReason, usize> {
        let mut out = count_reasons(&self.skipped);
        add_ignored(&mut out, self.ignored_files + self.ignored_dirs.len());
        out
    }
}

/// Fold the ignore-rule count into a by-reason table (absent when zero, like
/// every other reason).
pub fn add_ignored(by_reason: &mut BTreeMap<SkipReason, usize>, ignored: usize) {
    if ignored > 0 {
        *by_reason.entry(SkipReason::Ignored).or_insert(0) += ignored;
    }
}

/// The ignore rules in force in one directory: every ancestor's (and its own)
/// ignore files, shallowest first. Cheap to clone — the matchers are shared,
/// and a directory with ignore files of its own adds one onto its parent's.
#[derive(Clone, Default)]
pub struct IgnoreRules(Arc<Vec<Arc<Layer>>>);

struct Layer {
    /// The directory the ignore file is in, relative to the folder (`""` at
    /// the top).
    rel_dir: String,
    matcher: Gitignore,
}

/// Why an ignore rule excluded something: the pattern as written and the file
/// it is in, for a message a reader can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreHit {
    pub pattern: String,
    pub file: String,
}

impl IgnoreRules {
    /// Whether `rel` (relative to the folder, `/`-separated) is excluded. The
    /// deepest ignore file with an opinion decides, as in git.
    pub fn check(&self, rel: &str, is_dir: bool) -> Option<IgnoreHit> {
        for layer in self.0.iter().rev() {
            let local = if layer.rel_dir.is_empty() {
                rel
            } else {
                match rel
                    .strip_prefix(layer.rel_dir.as_str())
                    .and_then(|r| r.strip_prefix('/'))
                {
                    Some(r) => r,
                    None => continue,
                }
            };
            match layer.matcher.matched(local, is_dir) {
                Match::Ignore(g) => {
                    return Some(IgnoreHit {
                        pattern: g.original().to_string(),
                        file: g
                            .from()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                    })
                }
                Match::Whitelist(_) => return None,
                Match::None => {}
            }
        }
        None
    }

    /// These rules plus the ignore files of one directory. `files` are the
    /// ignore files that are regular files there (never symlinks), each with
    /// its bytes or the error reading it (see [`read_ignore_files`]); `notes`
    /// collects anything that could not be applied.
    pub fn descend(
        &self,
        rel_dir: &str,
        files: &[(&str, std::io::Result<Vec<u8>>)],
        found: &mut Vec<String>,
        notes: &mut Vec<String>,
    ) -> Self {
        if files.is_empty() {
            return self.clone();
        }
        // The builder's root only strips absolute paths; every path asked
        // about is relative, so `/` never matches as a prefix.
        let mut b = GitignoreBuilder::new("/");
        let mut any = false;
        for name in IGNORE_FILE_NAMES {
            let Some((_, read)) = files.iter().find(|(n, _)| n == name) else {
                continue;
            };
            let rel = if rel_dir.is_empty() {
                name.to_string()
            } else {
                format!("{rel_dir}/{name}")
            };
            let bytes = match read {
                Ok(b) => b,
                Err(e) => {
                    notes.push(format!(
                        "{rel} could not be read ({e}); its rules are not applied"
                    ));
                    continue;
                }
            };
            found.push(rel.clone());
            for (i, line) in String::from_utf8_lossy(bytes).lines().enumerate() {
                if let Err(e) = b.add_line(Some(PathBuf::from(&rel)), line) {
                    notes.push(format!(
                        "{rel} line {}: {e}; that line is not applied",
                        i + 1
                    ));
                } else {
                    any = true;
                }
            }
        }
        if !any {
            return self.clone();
        }
        match b.build() {
            Ok(matcher) => {
                let mut layers: Vec<Arc<Layer>> = self.0.as_ref().clone();
                layers.push(Arc::new(Layer {
                    rel_dir: rel_dir.to_string(),
                    matcher,
                }));
                Self(Arc::new(layers))
            }
            Err(e) => {
                notes.push(format!(
                    "the ignore rules in {} could not be built ({e}); they are not applied",
                    if rel_dir.is_empty() {
                        "the folder"
                    } else {
                        rel_dir
                    }
                ));
                self.clone()
            }
        }
    }
}

/// The ignore files of the directory behind `dir` that are regular files,
/// each read through the descriptor with `O_NOFOLLOW | O_NONBLOCK` — a
/// symlinked `.gitignore` is never followed, a FIFO never blocks.
pub fn read_ignore_files(dir: &OwnedFd) -> Vec<(&'static str, std::io::Result<Vec<u8>>)> {
    let mut out = Vec::new();
    for name in IGNORE_FILE_NAMES {
        let Ok(c) = CString::new(*name) else { continue };
        if source::stat_at(dir, &c).is_ok_and(|st| source::is_type(&st, libc::S_IFREG)) {
            out.push((*name, source::read_regular_at(dir, &c)));
        }
    }
    out
}

pub fn count_reasons(skipped: &[Skipped]) -> BTreeMap<SkipReason, usize> {
    let mut out = BTreeMap::new();
    for s in skipped {
        *out.entry(s.reason).or_insert(0) += 1;
    }
    out
}

/// One entry as `fstatat(…, AT_SYMLINK_NOFOLLOW)` saw it.
struct Entry {
    name: OsString,
    stat: std::io::Result<libc::stat>,
}

/// Walk `root`. Blocking — call it from `spawn_blocking`.
///
/// Iterative rather than recursive so a deep tree cannot overflow the stack,
/// and entries are sorted per directory so two scans of the same tree agree.
/// A pending directory is kept as its parent's descriptor and its name, and
/// opened only when its turn comes, so the descriptors open at once are the
/// ones on the current path, not one per directory waiting on the stack.
pub fn scan(root: &Path) -> std::io::Result<Scan> {
    let mut out = Scan::default();
    // The root itself failing to open or list is the sync's error, not a
    // skip: there would be nothing to report the skip against.
    let root_fd = Arc::new(source::open_dir_path(root)?);
    let root_list = source::list_dir(&root_fd)?;
    enum Pending {
        /// The folder, already open and listed.
        Root(Arc<OwnedFd>, Vec<OsString>),
        /// A subdirectory: its parent's descriptor and its name.
        Sub { parent: Arc<OwnedFd>, name: CString },
    }
    let mut stack: Vec<(Pending, String, IgnoreRules)> = vec![(
        Pending::Root(root_fd, root_list),
        String::new(),
        IgnoreRules::default(),
    )];
    while let Some((pending, rel_dir, parent_rules)) = stack.pop() {
        let (dir, listing) = match pending {
            Pending::Root(fd, names) => (fd, Ok(names)),
            Pending::Sub { parent, name } => {
                let opened = source::open_at(
                    &parent,
                    &name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                );
                match opened {
                    Ok(d) => {
                        let d = Arc::new(d);
                        let l = source::list_dir(&d);
                        (d, l)
                    }
                    // Listed as a directory a moment ago; a symlink (or
                    // anything else) now. Never followed.
                    Err(_)
                        if source::stat_at(&parent, &name)
                            .is_ok_and(|st| source::is_type(&st, libc::S_IFLNK)) =>
                    {
                        out.skipped.push(Skipped {
                            path: rel_dir,
                            reason: SkipReason::Symlink,
                            detail: Some(
                                "it was a directory when its parent was listed and a symlink \
                                 when the walk reached it; never followed"
                                    .into(),
                            ),
                        });
                        continue;
                    }
                    Err(e) => {
                        out.skipped.push(Skipped {
                            path: rel_dir,
                            reason: SkipReason::Unreadable,
                            detail: Some(e.to_string()),
                        });
                        continue;
                    }
                }
            }
        };
        let mut names = match listing {
            Ok(n) => n,
            Err(e) => {
                out.skipped.push(Skipped {
                    path: rel_dir,
                    reason: SkipReason::Unreadable,
                    detail: Some(e.to_string()),
                });
                continue;
            }
        };
        names.sort();
        let entries: Vec<Entry> = names
            .into_iter()
            .map(|name| {
                let stat = source::c_os(&name).and_then(|c| source::stat_at(&dir, &c));
                Entry { name, stat }
            })
            .collect();

        // This directory's own ignore files apply to its entries, so they are
        // read before any entry is judged. Only regular files count: a
        // symlinked `.gitignore` is never followed (it is still skipped below
        // as the hidden entry it is).
        let ignore_files = read_ignore_files(&dir);
        let rules = parent_rules.descend(
            &rel_dir,
            &ignore_files,
            &mut out.ignore_files,
            &mut out.notes,
        );

        let mut subdirs = Vec::new();
        for entry in entries {
            let lossy = entry.name.to_string_lossy().into_owned();
            let rel = if rel_dir.is_empty() {
                lossy.clone()
            } else {
                format!("{rel_dir}/{lossy}")
            };
            let skip = |reason, detail: Option<String>| Skipped {
                path: rel.clone(),
                reason,
                detail,
            };
            if lossy.starts_with('.') {
                out.skipped.push(skip(SkipReason::Hidden, None));
                continue;
            }
            // The entry itself, never what it points at.
            let st = match entry.stat {
                Ok(st) => st,
                Err(e) => {
                    out.skipped
                        .push(skip(SkipReason::Unreadable, Some(e.to_string())));
                    continue;
                }
            };
            if source::is_type(&st, libc::S_IFLNK) {
                out.skipped.push(skip(SkipReason::Symlink, None));
                continue;
            }
            // A path that is not UTF-8 cannot be a `document.url`, and a lossy
            // rendering could collide with a real name.
            if entry.name.to_str().is_none() {
                out.skipped.push(skip(
                    SkipReason::NotUtf8,
                    Some("the file name is not UTF-8".into()),
                ));
                continue;
            }
            let is_dir = source::is_type(&st, libc::S_IFDIR);
            if rules.check(&rel, is_dir).is_some() {
                if is_dir {
                    out.ignored_dirs.push(rel);
                } else {
                    out.ignored_files += 1;
                }
                continue;
            }
            if is_dir {
                match source::c_os(&entry.name) {
                    Ok(name) => subdirs.push((
                        Pending::Sub {
                            parent: dir.clone(),
                            name,
                        },
                        rel,
                        rules.clone(),
                    )),
                    Err(e) => out
                        .skipped
                        .push(skip(SkipReason::Unreadable, Some(e.to_string()))),
                }
                continue;
            }
            if !source::is_type(&st, libc::S_IFREG) {
                out.skipped.push(skip(
                    SkipReason::Unsupported,
                    Some("not a regular file".into()),
                ));
                continue;
            }
            let Some(kind) = FileKind::of(&lossy) else {
                out.skipped.push(skip(SkipReason::Unsupported, None));
                continue;
            };
            out.files.push(Candidate {
                rel,
                kind,
                size: st.st_size.max(0) as u64,
                mtime_ns: mtime_ns_of(st.st_mtime, st.st_mtime_nsec),
            });
        }
        // Reversed onto the stack so the walk pops them in name order.
        stack.extend(subdirs.into_iter().rev());
    }
    out.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    out.skipped.sort_by(|a, b| a.path.cmp(&b.path));
    out.ignored_dirs.sort();
    out.ignore_files.sort();
    Ok(out)
}

/// The ignore rules that apply to `rel` (a validated relative path, `/`-
/// separated), read from the ignore files of every directory from the folder
/// down to its parent — for a single path, where [`scan`] builds the same
/// rules incrementally. Directories on the way are checked too, so a file
/// under a pruned directory is excluded, exactly as the scan would have
/// pruned it. The directories are opened one by one with `O_NOFOLLOW`, as the
/// scan does; a path that cannot be walked that way ends the lookup (the
/// caller refuses such a path for it anyway). Blocking. `None`: not excluded.
pub fn ignored_by_rules(root: &Path, rel: &str) -> Option<IgnoreHit> {
    let parts: Vec<&str> = rel.split('/').collect();
    let mut rules = IgnoreRules::default();
    let mut dir = source::open_dir_path(root).ok()?;
    let mut rel_dir = String::new();
    let mut sink_found = Vec::new();
    let mut sink_notes = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let files = read_ignore_files(&dir);
        rules = rules.descend(&rel_dir, &files, &mut sink_found, &mut sink_notes);
        let here = if rel_dir.is_empty() {
            part.to_string()
        } else {
            format!("{rel_dir}/{part}")
        };
        let is_dir = i + 1 < parts.len();
        if let Some(hit) = rules.check(&here, is_dir) {
            return Some(hit);
        }
        if is_dir {
            let name = CString::new(*part).ok()?;
            dir = source::open_at(
                &dir,
                &name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
            .ok()?;
        }
        rel_dir = here;
    }
    None
}

/// Modification time in nanoseconds since the epoch, from `st_mtime` and
/// `st_mtime_nsec` — the one formula the walk and the sync's `fstat` both
/// use, so the two always agree. 0 before the epoch or on overflow; the sync
/// never trusts a recorded 0 (it always re-hashes such a file), so 0 only
/// ever makes the fast path miss.
pub fn mtime_ns_of(secs: i64, nsec: i64) -> i64 {
    if secs < 0 {
        return 0;
    }
    secs.checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(nsec))
        .unwrap_or(0)
}

/// [`mtime_ns_of`] for a [`std::fs::Metadata`] (an `fstat` of an open file).
pub fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    mtime_ns_of(meta.mtime(), meta.mtime_nsec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_every_reason_in_declaration_order() {
        // No wildcard: a new variant fails to compile here until it is listed.
        let position = |r: SkipReason| match r {
            SkipReason::Hidden => 0,
            SkipReason::Symlink => 1,
            SkipReason::Ignored => 2,
            SkipReason::Unsupported => 3,
            SkipReason::Unreadable => 4,
            SkipReason::NotUtf8 => 5,
            SkipReason::PdfNoText => 6,
            SkipReason::Empty => 7,
            SkipReason::PdfTimeout => 8,
            SkipReason::TooLargeForMemory => 9,
        };
        for (i, r) in SkipReason::ALL.iter().enumerate() {
            assert_eq!(position(*r), i, "{r:?}");
        }
        assert_eq!(SkipReason::ALL.len(), 10);
        let mut sorted = SkipReason::ALL.to_vec();
        sorted.sort();
        assert_eq!(sorted, SkipReason::ALL);
    }

    #[test]
    fn kinds_by_extension_and_name() {
        assert_eq!(FileKind::of("README.MD"), Some(FileKind::Markdown));
        assert_eq!(FileKind::of("notes.org"), Some(FileKind::Text));
        assert_eq!(FileKind::of("main.rs"), Some(FileKind::Code));
        assert_eq!(FileKind::of("Containerfile"), Some(FileKind::Code));
        assert_eq!(FileKind::of("paper.PDF"), Some(FileKind::Pdf));
        assert_eq!(FileKind::of("photo.png"), None);
        assert_eq!(FileKind::of("LICENSE"), None);
    }

    #[test]
    fn a_directory_swapped_for_a_symlink_is_never_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), "# outside").unwrap();
        std::fs::create_dir(tmp.path().join("docs")).unwrap();
        std::fs::write(tmp.path().join("docs/a.md"), "# a").unwrap();
        // The symlink the walk finds at listing time is skipped as one.
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("link")).unwrap();
        let s = scan(tmp.path()).unwrap();
        assert_eq!(
            s.files.iter().map(|c| c.rel.as_str()).collect::<Vec<_>>(),
            ["docs/a.md"]
        );
        assert!(s
            .skipped
            .iter()
            .any(|k| k.path == "link" && k.reason == SkipReason::Symlink));
        assert!(!s.skipped.iter().any(|k| k.path.contains("secret")));
    }

    #[test]
    fn mtimes_agree_with_fstat() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.md"), "x").unwrap();
        let s = scan(tmp.path()).unwrap();
        let meta = std::fs::metadata(tmp.path().join("a.md")).unwrap();
        assert_eq!(s.files[0].mtime_ns, mtime_ns(&meta));
        assert_ne!(s.files[0].mtime_ns, 0);
        assert_eq!(mtime_ns_of(-5, 0), 0);
    }
}
