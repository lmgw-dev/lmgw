//! The index directory — the one place this agent writes.
//!
//! [`IndexDir`] is the **only** code in the crate that opens anything for
//! writing: it creates `<folder>/.lmgw-folder-chat/`, opens the SQLite index
//! inside it, and hands out the pool every store call goes through. Source
//! files are only ever opened read-only, through
//! [`crate::source::read_beneath`]; `pdftotext` gets their bytes on stdin and
//! writes to stdout. The "nothing outside the index directory changes" test
//! pins that from the outside, by fingerprinting the whole folder (content,
//! size, mtime and mode of every entry) before and after syncs that go down
//! every path.
//!
//! **Containment, at open and before every sync ([`IndexDir::check_containment`]):**
//!
//! - The directory is created and opened relative to the folder's
//!   descriptor with `mkdirat` / `openat(…, O_DIRECTORY | O_NOFOLLOW)`, and
//!   that descriptor is kept. Before every sync (and before every file it
//!   writes) the path `<folder>/.lmgw-folder-chat` must still be that
//!   directory — same device and inode, not a symlink — or the sync stops
//!   with [`crate::sync::AbortKind::IndexContainment`] having written
//!   nothing.
//! - `index.sqlite` is created by this code (`O_CREAT | O_EXCL | O_NOFOLLOW`
//!   through the directory's descriptor), so SQLite never creates it:
//!   the pool opens it with `create_if_missing(false)`. It must stay the file
//!   the pool opened (same inode), a regular file, and have **one** link — a
//!   second hard link elsewhere in the folder would receive every write.
//!   SQLite's sidecars ([`SQLITE_SIDECARS`]: `-wal`, `-shm`, `-journal`) must
//!   be regular, singly-linked files when they exist.
//! - The pool is **fixed**: [`INDEX_POOL_CONNECTIONS`] connections, all
//!   opened at start-up (min = max), never idle-closed and never recycled
//!   (no idle timeout, no lifetime), so SQLite does not resolve the path again
//!   while the agent runs; the check above covers the rest.
//!
//! On creation the directory also gets a `.gitignore` of `*` and a
//! `CACHEDIR.TAG` ([`CACHEDIR_TAG`]), so git, backup tools that honour cache
//! directory tags, and many sync tools leave the index alone. Both are inside
//! the one directory the agent writes.
//!
//! Besides quickdoc's own tables, the index file holds seven tables of the
//! agent's own, created here with `CREATE TABLE IF NOT EXISTS` — **not** a
//! quickdoc migration, so quickdoc's schema stays exactly what its migrations
//! say it is:
//!
//! - `folder_chat_file` — per document: the file's size and mtime when it was
//!   last indexed (quickdoc's `document` table has no column for either; they
//!   are the change-detection fast path) and the content hash its chunks were
//!   built from. A row is written **only after** the file's chunks are stored,
//!   so it is the "this file is current" marker a crash cannot fake.
//! - `folder_chat_meta` — key/value facts about how the index was built (the
//!   chunk size, the chunker version, the embedding model's probe vector, the
//!   `pdftotext` version), so a change to any of them is noticed.
//! - `folder_chat_chunk_lines` — per chunk: the 1-based lines it spans in the
//!   text it was cut from, recorded when it was stored, so a citation's line
//!   numbers come from the index ([`IndexDir::fill_stored_lines`]) instead of
//!   a fresh read — and, for a PDF, a fresh `pdftotext` run — per question.
//!   It cascades with `chunk`, so it never outlives the chunk it describes.
//! - `folder_chat_pdf_pages` — per PDF: how many pages its `pdftotext` output
//!   had and which of them had no text (a scan without an OCR layer), so
//!   every sync can report them, not only the one that read the file. It is
//!   written just before the file's `folder_chat_file` row and cascades with
//!   `document`. A PDF without a row is from an index older than the table,
//!   and the sync extracts it once more to record it.
//! - `folder_chat_reading` — the **page-reading cache** ([`crate::vision`]):
//!   what a vision model read off one page, or why it could not, keyed by the
//!   bytes it was read from (a document's content hash: the PDF's bytes and
//!   the `pdftotext` version), the page, the alias, the mode, the prompt
//!   version and the resolution ([`PageKey`]). A page read under one key is
//!   never read under it again — a failure included, since the failures
//!   stored here are the ones that would fail the same way — until the
//!   owner's Retry failed pages deletes the failures
//!   ([`IndexDir::retry_failed_readings`]). It is **not**
//!   tied to a document: a rebuilt index (another embedding model, another
//!   chunk size) finds its readings still here, and several rows per page
//!   (one per alias, say) may stand side by side. A row is written as soon as
//!   its page is done, so a hold mid-file keeps the pages read before it.
//!   Rows whose content hash no document refers to any more are pruned
//!   ([`IndexDir::prune_readings`]) at the end of a sync that completed —
//!   never after one that stopped, and never those of a file this sync read
//!   pages of (moved, or not indexed after all).
//! - `folder_chat_doc_vision` — per PDF: the content hash, the vision
//!   settings ([`crate::vision::settings`]) and the pages its indexed text
//!   was last built with, and `pending`, set before the first page of a new
//!   pass is read and cleared when the pass is done. A PDF whose settings
//!   differ, or that is pending, goes through the refresh pass again.
//! - `folder_chat_doc_page` — per PDF and page picked for a reading: the key
//!   it was read under, less the hash. Joined with the cache through the
//!   document's hash, it says exactly which readings its indexed text holds —
//!   what the MCP `read` tool appends ([`IndexDir::pdf_readings`]) and what
//!   every sync reports as read or failed. Both cascade with `document`.
//!
//! The agent reads quickdoc's `chunk` table directly in one place
//! ([`IndexDir::stored_chunks`], to resume an interrupted file without
//! re-embedding what it already stored) and deletes from it in one
//! ([`IndexDir::delete_chunks`], the chunks a re-indexed file no longer
//! holds); quickdoc's FTS triggers keep BM25 in step with both.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::chat::Citation;
use crate::config::{INDEX_DIR_NAME, INDEX_FILE_NAME};
use crate::source;
use crate::vision::{Mode, Reading};

/// Connections in the index's pool — all opened at start-up and kept for the
/// life of the process (see the module docs). Eight is what quickdoc's own
/// `store::open` allows; one sync at a time plus concurrent questions and MCP
/// searches never need more. A concurrency bound only: a request beyond it
/// waits for a connection, it is never refused or cut short.
pub const INDEX_POOL_CONNECTIONS: u32 = 8;

/// SQLite's files beside `index.sqlite` — the WAL, its shared-memory index,
/// and the rollback journal (not used in WAL mode, checked all the same).
pub const SQLITE_SIDECARS: &[&str] = &["-wal", "-shm", "-journal"];

/// What `.gitignore` in the index directory says: everything in here.
pub const GITIGNORE: &str = "*\n";

/// The cache directory tag (<https://bford.info/cachedir/>): the signature
/// line is the standard; the comment says who wrote it.
pub const CACHEDIR_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
     # This file is a cache directory tag created by folder-chat, an lmgw agent:\n\
     # its search index of this folder. Deleting the directory makes the next sync\n\
     # start over. For information about cache directory tags, see\n\
     # https://bford.info/cachedir/\n";

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("the folder {0} does not exist or is not a directory")]
    FolderMissing(String),
    #[error(
        "{0} is not a plain directory (a symlink or a file); refusing to write the index \
         through it — remove it and sync again"
    )]
    NotAPlainDir(String),
    #[error("{0} is a symlink; refusing to write the index through it — remove it and sync again")]
    Symlink(String),
    #[error(transparent)]
    Containment(#[from] ContainmentError),
    #[error("index: {0}")]
    Io(#[from] std::io::Error),
    #[error("index: {0}")]
    Store(#[from] quickdoc_core::QuickdocError),
    #[error("index: {0}")]
    Db(#[from] sqlx::Error),
}

/// Why the index directory can no longer be written safely. Every message
/// ends with what to do; a sync that meets one stops before writing anything.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContainmentError {
    #[error(
        "{path} is now a symlink; the agent writes only inside the directory it created, so \
         nothing was written — remove the link and restart the agent"
    )]
    Symlink { path: String },
    #[error(
        "{path} is no longer the directory the index was opened in ({now}); nothing was \
         written — restart the agent to open the index where it is now"
    )]
    DirReplaced { path: String, now: String },
    #[error(
        "{path} is not the file the index was opened on ({now}); nothing was written — restart \
         the agent to reopen the index"
    )]
    FileReplaced { path: String, now: String },
    #[error(
        "{path} has {links} hard links; a second name for an index file would receive the \
         agent's writes outside its directory, so nothing was written — remove the other \
         link and sync again"
    )]
    HardLinked { path: String, links: u64 },
    #[error("{path} is not a regular file; nothing was written — remove it and sync again")]
    NotRegular { path: String },
}

/// What the index recorded about one file when its chunks were stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    pub size: u64,
    /// Nanoseconds since the epoch. Nanoseconds rather than seconds so two
    /// edits within one second that keep the size are still told apart on a
    /// filesystem that records them. `0` means "not trusted": the file was
    /// changing while it was read (see `sync::RACY_MTIME_WINDOW`), so the
    /// next sync re-hashes it whatever its mtime says.
    pub mtime_ns: i64,
    /// hex(sha256) of the bytes the chunks were built from (for a PDF, the
    /// bytes and the `pdftotext` version that extracted them).
    pub indexed_hash: String,
}

/// A chunk already stored for a document, as far as resuming it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredChunk {
    pub heading_path: String,
    pub span_start: i64,
    pub span_end: i64,
    /// It has a vector.
    pub embedded: bool,
}

/// Whether a vision model read a page, or failed on it in a way that would
/// fail the same way again (an empty or cut-off answer, a refusal of this
/// page, a page that does not render).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisionStatus {
    Read,
    Failed,
}

impl VisionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Failed => "failed",
        }
    }
}

/// How one page is read — the key a cached reading is kept under, less the
/// bytes it is read from (a document's content hash).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PageKey {
    pub page: u32,
    /// The alias.
    pub model: String,
    pub mode: Mode,
    /// `crate::vision::VISION_PROMPT_VERSION`.
    pub prompt_version: String,
    /// `crate::vision::VISION_DPI`.
    pub dpi: u32,
}

/// A cached reading, or a cached failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedReading {
    pub status: VisionStatus,
    /// The reading, or why the page could not be read.
    pub text: String,
}

/// How one PDF's indexed text was last built from page readings
/// (`folder_chat_doc_vision`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocVision {
    /// The content hash its readings were taken from; empty until a pass
    /// over it has finished.
    pub content_hash: String,
    /// `crate::vision::settings` of that pass.
    pub settings: String,
    /// A pass over it started and has not finished.
    pub pending: bool,
}

/// A page whose cached reading failed, in an indexed PDF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedPage {
    pub path: String,
    pub page: u32,
    pub model: String,
    pub reason: String,
}

/// Held for the length of one sync; dropping it lets the next one start.
pub type SyncPermit = OwnedMutexGuard<()>;

/// Device and inode: what "the same file" means.
type Ident = (u64, u64);

// `dev_t` / `ino_t` are `u64` here, not on every target.
#[allow(clippy::unnecessary_cast)]
fn ident(st: &libc::stat) -> Ident {
    (st.st_dev as u64, st.st_ino as u64)
}

/// A pre-release build of 0.2.0 kept its page readings in
/// `folder_chat_vision`, keyed by document, so an index rebuild lost them.
/// Its readings (it rendered only at 150 dpi) move into the cache, the table
/// goes, and every PDF is marked pending: the reading chunks it indexed are
/// then brought in line with the record the next sync writes. Nothing
/// happens when the table is not there.
async fn retire_prerelease_readings(pool: &SqlitePool) -> Result<(), IndexError> {
    let present: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'folder_chat_vision'",
    )
    .fetch_optional(pool)
    .await?;
    if present.is_none() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT OR IGNORE INTO folder_chat_reading
             (content_hash, page, model, mode, prompt_version, dpi, status, text)
         SELECT content_hash, page, model, mode, prompt_version, 150, status, text
         FROM folder_chat_vision WHERE status = 'read'",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO folder_chat_doc_vision (document_id, content_hash, settings, pending)
         SELECT document_id, '', ?1, 1 FROM folder_chat_pdf_pages WHERE true
         ON CONFLICT(document_id) DO UPDATE SET pending = 1",
    )
    .bind(crate::vision::SETTINGS_OFF)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DROP TABLE folder_chat_vision")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// The index directory, open.
#[derive(Debug, Clone)]
pub struct IndexDir {
    folder: PathBuf,
    dir: PathBuf,
    db_path: PathBuf,
    pool: SqlitePool,
    /// `<folder>/.lmgw-folder-chat`, held open since start-up.
    dir_fd: Arc<OwnedFd>,
    dir_ident: Ident,
    db_ident: Ident,
    /// One sync at a time, per index — held here rather than by the caller so
    /// even a direct [`crate::sync::run`] cannot race another.
    sync_lock: Arc<Mutex<()>>,
}

fn cname(name: &str) -> CString {
    CString::new(name).expect("the index's own file names have no NUL")
}

/// Create `name` in `dir` with `body`, unless something by that name is
/// already there (never through a symlink, never over an existing file).
fn create_new_at(dir: &OwnedFd, name: &str, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    match source::open_at_mode(
        dir,
        &cname(name),
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
        0o666,
    ) {
        Ok(fd) => std::fs::File::from(fd).write_all(body.as_bytes()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

impl IndexDir {
    /// Create (if needed) and open `<folder>/.lmgw-folder-chat/index.sqlite`.
    pub async fn open(folder: &Path) -> Result<Self, IndexError> {
        if !folder.is_dir() {
            return Err(IndexError::FolderMissing(folder.display().to_string()));
        }
        let dir = folder.join(INDEX_DIR_NAME);
        let db_path = dir.join(INDEX_FILE_NAME);
        let folder_fd = source::open_dir_path(folder)?;
        // `mkdirat`, not a recursive create: the folder itself must already
        // exist — it is the owner's, never ours to create.
        let dir_name = cname(INDEX_DIR_NAME);
        // SAFETY: an open descriptor and a NUL-terminated name.
        let created = if unsafe {
            libc::mkdirat(
                std::os::fd::AsRawFd::as_raw_fd(&folder_fd),
                dir_name.as_ptr(),
                0o777,
            )
        } == 0
        {
            true
        } else {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e.into());
            }
            false
        };
        let dir_fd = match source::open_at(
            &folder_fd,
            &dir_name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        ) {
            Ok(fd) => fd,
            Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
                return Err(IndexError::NotAPlainDir(dir.display().to_string()))
            }
            Err(e) => return Err(e.into()),
        };
        if created {
            create_new_at(&dir_fd, ".gitignore", GITIGNORE)?;
            create_new_at(&dir_fd, "CACHEDIR.TAG", CACHEDIR_TAG)?;
        }
        // The index file: ours to create, never SQLite's, never through a
        // symlink. An existing one is checked like the sidecars.
        match source::open_at_mode(
            &dir_fd,
            &cname(INDEX_FILE_NAME),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o666,
        ) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                return Err(IndexError::Symlink(db_path.display().to_string()))
            }
            Err(e) => return Err(e.into()),
        }
        for suffix in [""].iter().chain(SQLITE_SIDECARS) {
            let name = format!("{INDEX_FILE_NAME}{suffix}");
            if source::stat_at(&dir_fd, &cname(&name))
                .is_ok_and(|st| source::is_type(&st, libc::S_IFLNK))
            {
                return Err(IndexError::Symlink(dir.join(name).display().to_string()));
            }
        }
        let dir_ident = ident(&source::stat_fd(&dir_fd)?);
        let db_ident = ident(&source::stat_at(&dir_fd, &cname(INDEX_FILE_NAME))?);

        let connect = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(false)
            .foreign_keys(true);
        // WAL is a property of the file, set once by a lone connection:
        // switching the journal mode takes an exclusive lock SQLite does not
        // wait for, so pool connections racing to set it fail with "database
        // is locked". Every connection after this one finds the file in WAL
        // mode already.
        {
            use sqlx::Connection;
            let first = sqlx::SqliteConnection::connect_with(
                &connect.clone().journal_mode(SqliteJournalMode::Wal),
            )
            .await?;
            first.close().await?;
        }
        let pool_options = SqlitePoolOptions::new()
            .max_connections(INDEX_POOL_CONNECTIONS)
            .min_connections(INDEX_POOL_CONNECTIONS)
            .idle_timeout(None)
            .max_lifetime(None);
        let pool = quickdoc_core::store::open_with(connect, pool_options).await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS folder_chat_file (
                 document_id  INTEGER PRIMARY KEY REFERENCES document(id) ON DELETE CASCADE,
                 size         INTEGER NOT NULL,
                 mtime_ns     INTEGER NOT NULL,
                 indexed_hash TEXT NOT NULL
             )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS folder_chat_meta (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS folder_chat_pdf_pages (
                 document_id INTEGER PRIMARY KEY REFERENCES document(id) ON DELETE CASCADE,
                 pages       INTEGER NOT NULL,
                 textless    TEXT NOT NULL
             )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS folder_chat_reading (
                 content_hash   TEXT NOT NULL,
                 page           INTEGER NOT NULL,
                 model          TEXT NOT NULL,
                 mode           TEXT NOT NULL CHECK (mode IN ('ocr', 'structure')),
                 prompt_version TEXT NOT NULL,
                 dpi            INTEGER NOT NULL,
                 status         TEXT NOT NULL CHECK (status IN ('read', 'failed')),
                 text           TEXT NOT NULL,
                 PRIMARY KEY (content_hash, page, model, mode, prompt_version, dpi)
             )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS folder_chat_doc_vision (
                 document_id  INTEGER PRIMARY KEY REFERENCES document(id) ON DELETE CASCADE,
                 content_hash TEXT NOT NULL,
                 settings     TEXT NOT NULL,
                 pending      INTEGER NOT NULL
             )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS folder_chat_doc_page (
                 document_id    INTEGER NOT NULL REFERENCES document(id) ON DELETE CASCADE,
                 page           INTEGER NOT NULL,
                 model          TEXT NOT NULL,
                 mode           TEXT NOT NULL,
                 prompt_version TEXT NOT NULL,
                 dpi            INTEGER NOT NULL,
                 PRIMARY KEY (document_id, page)
             )",
        )
        .execute(&pool)
        .await?;
        retire_prerelease_readings(&pool).await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS folder_chat_chunk_lines (
                 chunk_id   TEXT PRIMARY KEY REFERENCES chunk(id) ON DELETE CASCADE,
                 start_line INTEGER NOT NULL,
                 end_line   INTEGER NOT NULL
             )",
        )
        .execute(&pool)
        .await?;
        let me = Self {
            folder: folder.to_path_buf(),
            dir,
            db_path,
            pool,
            dir_fd: Arc::new(dir_fd),
            dir_ident,
            db_ident,
            sync_lock: Arc::new(Mutex::new(())),
        };
        // Everything above could have raced a swap; the pool is open now and
        // will not open the path again.
        me.check_containment()?;
        Ok(me)
    }

    /// Whether the index can still be written without a write landing
    /// anywhere but the directory this agent created — see the module docs.
    /// A handful of `fstatat` calls; the sync runs it before it writes
    /// anything and again before each file.
    pub fn check_containment(&self) -> Result<(), ContainmentError> {
        let path = self.dir.display().to_string();
        match std::fs::symlink_metadata(&self.dir) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(ContainmentError::Symlink { path });
            }
            Ok(m) => {
                use std::os::unix::fs::MetadataExt;
                if (m.dev(), m.ino()) != self.dir_ident {
                    return Err(ContainmentError::DirReplaced {
                        path,
                        now: "another directory or file is there now".into(),
                    });
                }
            }
            Err(e) => {
                return Err(ContainmentError::DirReplaced {
                    path,
                    now: e.to_string(),
                })
            }
        }
        for suffix in [""].iter().chain(SQLITE_SIDECARS) {
            let name = format!("{INDEX_FILE_NAME}{suffix}");
            let path = self.dir.join(&name).display().to_string();
            let st = match source::stat_at(&self.dir_fd, &cname(&name)) {
                Ok(st) => st,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !suffix.is_empty() => {
                    continue
                }
                Err(e) => {
                    return Err(ContainmentError::FileReplaced {
                        path,
                        now: e.to_string(),
                    })
                }
            };
            if source::is_type(&st, libc::S_IFLNK) {
                return Err(ContainmentError::Symlink { path });
            }
            if !source::is_type(&st, libc::S_IFREG) {
                return Err(ContainmentError::NotRegular { path });
            }
            if suffix.is_empty() && ident(&st) != self.db_ident {
                return Err(ContainmentError::FileReplaced {
                    path,
                    now: "a different file is there now".into(),
                });
            }
            let links = st.st_nlink as u64;
            if links != 1 {
                return Err(ContainmentError::HardLinked { path, links });
            }
        }
        Ok(())
    }

    /// The owner's folder — read, never written.
    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// `<folder>/.lmgw-folder-chat`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// The index's pool — the handle every quickdoc store call takes.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Claim the sync slot, or `None` when a sync is already running.
    pub fn try_begin_sync(&self) -> Option<SyncPermit> {
        self.sync_lock.clone().try_lock_owned().ok()
    }

    pub fn is_syncing(&self) -> bool {
        self.sync_lock.try_lock().is_err()
    }

    /// Every file state of one corpus, by document id.
    pub async fn file_states(&self, corpus_id: i64) -> Result<HashMap<i64, FileState>, IndexError> {
        let rows = sqlx::query(
            "SELECT f.document_id, f.size, f.mtime_ns, f.indexed_hash
             FROM folder_chat_file f
             JOIN document d ON d.id = f.document_id
             JOIN source s ON s.id = d.source_id
             WHERE s.corpus_id = ?1",
        )
        .bind(corpus_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get::<i64, _>("document_id"),
                    FileState {
                        size: r.get::<i64, _>("size").max(0) as u64,
                        mtime_ns: r.get("mtime_ns"),
                        indexed_hash: r.get("indexed_hash"),
                    },
                )
            })
            .collect())
    }

    /// How many documents (files) one corpus holds — the "files" the UI shows.
    pub async fn document_count(&self, corpus_id: i64) -> Result<usize, IndexError> {
        let n: i64 = sqlx::query(
            "SELECT COUNT(*) AS n FROM document d
             JOIN source s ON s.id = d.source_id
             WHERE s.corpus_id = ?1",
        )
        .bind(corpus_id)
        .fetch_one(&self.pool)
        .await?
        .get("n");
        Ok(n.max(0) as usize)
    }

    /// Record a file as current. Called only once its chunks are stored.
    pub async fn set_file_state(&self, document_id: i64, s: &FileState) -> Result<(), IndexError> {
        sqlx::query(
            "INSERT INTO folder_chat_file (document_id, size, mtime_ns, indexed_hash)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(document_id) DO UPDATE SET
                 size = excluded.size, mtime_ns = excluded.mtime_ns,
                 indexed_hash = excluded.indexed_hash",
        )
        .bind(document_id)
        .bind(i64::try_from(s.size).unwrap_or(i64::MAX))
        .bind(s.mtime_ns)
        .bind(&s.indexed_hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn meta(&self, key: &str) -> Result<Option<String>, IndexError> {
        Ok(
            sqlx::query("SELECT value FROM folder_chat_meta WHERE key = ?1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?
                .map(|r| r.get("value")),
        )
    }

    pub async fn set_meta(&self, key: &str, value: &str) -> Result<(), IndexError> {
        sqlx::query(
            "INSERT INTO folder_chat_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every chunk stored for one document, by id — what the sync compares a
    /// file's new chunks with, so a chunk that is already stored exactly as
    /// it would be again (same id, heading and span, with a vector) is kept
    /// rather than embedded a second time.
    pub async fn stored_chunks(
        &self,
        document_id: i64,
    ) -> Result<HashMap<String, StoredChunk>, IndexError> {
        let rows = sqlx::query(
            "SELECT id, heading_path, span_start, span_end, embedding IS NOT NULL AS embedded
             FROM chunk WHERE document_id = ?1",
        )
        .bind(document_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get::<String, _>("id"),
                    StoredChunk {
                        heading_path: r.get("heading_path"),
                        span_start: r.get("span_start"),
                        span_end: r.get("span_end"),
                        embedded: r.get::<i64, _>("embedded") != 0,
                    },
                )
            })
            .collect())
    }

    /// Delete chunks by id, in one transaction; their FTS postings and stored
    /// lines go with them (quickdoc's trigger, our cascade). The caller
    /// refreshes the corpus's chunk count.
    pub async fn delete_chunks(&self, ids: &[String]) -> Result<u64, IndexError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let mut n = 0;
        for id in ids {
            n += sqlx::query("DELETE FROM chunk WHERE id = ?1")
                .bind(id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        tx.commit().await?;
        Ok(n)
    }

    /// Record the lines each chunk spans (1-based, inclusive), in one
    /// transaction. The chunks must be stored already.
    pub async fn set_chunk_lines(&self, rows: &[(&str, (usize, usize))]) -> Result<(), IndexError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for (id, (a, b)) in rows {
            sqlx::query(
                "INSERT INTO folder_chat_chunk_lines (chunk_id, start_line, end_line)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(chunk_id) DO UPDATE SET
                     start_line = excluded.start_line, end_line = excluded.end_line",
            )
            .bind(*id)
            .bind(i64::try_from(*a).unwrap_or(i64::MAX))
            .bind(i64::try_from(*b).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Record a PDF's pages and the ones without text ([`crate::chunk::pdf_pages`]).
    /// Called only once its chunks are stored, just before
    /// [`Self::set_file_state`].
    pub async fn set_pdf_pages(
        &self,
        document_id: i64,
        pages: u32,
        textless: &[u32],
    ) -> Result<(), IndexError> {
        let textless = serde_json::to_string(textless).expect("a list of numbers serialises");
        sqlx::query(
            "INSERT INTO folder_chat_pdf_pages (document_id, pages, textless)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(document_id) DO UPDATE SET
                 pages = excluded.pages, textless = excluded.textless",
        )
        .bind(document_id)
        .bind(i64::from(pages))
        .bind(textless)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The documents of one corpus whose PDF pages are recorded; a PDF not
    /// among them is from an index older than [`Self::set_pdf_pages`].
    pub async fn documents_with_pdf_pages(
        &self,
        corpus_id: i64,
    ) -> Result<HashSet<i64>, IndexError> {
        let rows = sqlx::query(
            "SELECT p.document_id AS id FROM folder_chat_pdf_pages p
             JOIN document d ON d.id = p.document_id
             JOIN source s ON s.id = d.source_id
             WHERE s.corpus_id = ?1",
        )
        .bind(corpus_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<i64, _>("id")).collect())
    }

    /// Every PDF of one corpus with at least one page without text, by path —
    /// `(path, pages, textless)`.
    pub async fn textless_pdf_pages(
        &self,
        corpus_id: i64,
    ) -> Result<Vec<(String, u32, Vec<u32>)>, IndexError> {
        let rows = sqlx::query(
            "SELECT d.url, p.pages, p.textless FROM folder_chat_pdf_pages p
             JOIN document d ON d.id = p.document_id
             JOIN source s ON s.id = d.source_id
             WHERE s.corpus_id = ?1 AND p.textless <> '[]'
             ORDER BY d.url",
        )
        .bind(corpus_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let textless: String = r.get("textless");
                (
                    r.get("url"),
                    u32::try_from(r.get::<i64, _>("pages")).unwrap_or(0),
                    serde_json::from_str(&textless).unwrap_or_default(),
                )
            })
            .collect())
    }

    /// Every cached reading (and failure) of the bytes `content_hash`, by key.
    pub async fn cached_readings(
        &self,
        content_hash: &str,
    ) -> Result<HashMap<PageKey, CachedReading>, IndexError> {
        let rows = sqlx::query(
            "SELECT page, model, mode, prompt_version, dpi, status, text
             FROM folder_chat_reading WHERE content_hash = ?1",
        )
        .bind(content_hash)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                // The table's CHECKs admit only these.
                let mode = Mode::parse(r.get("mode"))?;
                let status = match r.get::<&str, _>("status") {
                    "read" => VisionStatus::Read,
                    _ => VisionStatus::Failed,
                };
                Some((
                    PageKey {
                        page: u32::try_from(r.get::<i64, _>("page")).ok()?,
                        model: r.get("model"),
                        mode,
                        prompt_version: r.get("prompt_version"),
                        dpi: u32::try_from(r.get::<i64, _>("dpi")).ok()?,
                    },
                    CachedReading {
                        status,
                        text: r.get("text"),
                    },
                ))
            })
            .collect())
    }

    /// Cache what became of one page of the bytes `content_hash` read under
    /// `key` — as soon as the page is done, so a sync stopped later keeps it.
    pub async fn cache_reading(
        &self,
        content_hash: &str,
        key: &PageKey,
        reading: &CachedReading,
    ) -> Result<(), IndexError> {
        sqlx::query(
            "INSERT INTO folder_chat_reading
                 (content_hash, page, model, mode, prompt_version, dpi, status, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(content_hash, page, model, mode, prompt_version, dpi) DO UPDATE SET
                 status = excluded.status, text = excluded.text",
        )
        .bind(content_hash)
        .bind(i64::from(key.page))
        .bind(&key.model)
        .bind(key.mode.as_str())
        .bind(&key.prompt_version)
        .bind(i64::from(key.dpi))
        .bind(reading.status.as_str())
        .bind(&reading.text)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// How every PDF of one corpus was last read ([`DocVision`]), by document
    /// id. A PDF without a row is from before vision, or was never read:
    /// no readings, no settings.
    pub async fn doc_vision(&self, corpus_id: i64) -> Result<HashMap<i64, DocVision>, IndexError> {
        let rows = sqlx::query(
            "SELECT v.document_id, v.content_hash, v.settings, v.pending
             FROM folder_chat_doc_vision v
             JOIN document d ON d.id = v.document_id
             JOIN source s ON s.id = d.source_id
             WHERE s.corpus_id = ?1",
        )
        .bind(corpus_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get::<i64, _>("document_id"),
                    DocVision {
                        content_hash: r.get("content_hash"),
                        settings: r.get("settings"),
                        pending: r.get::<i64, _>("pending") != 0,
                    },
                )
            })
            .collect())
    }

    /// Mark a PDF as in a pass: set before the first of its pages is read
    /// and before any of its chunks change, so a pass that stops anywhere
    /// leaves it pending and the next sync takes it through again. What its
    /// indexed text was built with stays recorded until the pass is done.
    pub async fn mark_vision_pending(
        &self,
        document_id: i64,
        settings_off: &str,
    ) -> Result<(), IndexError> {
        sqlx::query(
            "INSERT INTO folder_chat_doc_vision (document_id, content_hash, settings, pending)
             VALUES (?1, '', ?2, 1)
             ON CONFLICT(document_id) DO UPDATE SET pending = 1",
        )
        .bind(document_id)
        .bind(settings_off)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record how a PDF's indexed text was built: `vision`, and every page
    /// picked for a reading with the key it was read under. Called once its
    /// chunks are stored, just before its pages and its marker are recorded.
    pub async fn set_doc_vision(
        &self,
        document_id: i64,
        vision: &DocVision,
        pages: &[PageKey],
    ) -> Result<(), IndexError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO folder_chat_doc_vision (document_id, content_hash, settings, pending)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(document_id) DO UPDATE SET
                 content_hash = excluded.content_hash, settings = excluded.settings,
                 pending = excluded.pending",
        )
        .bind(document_id)
        .bind(&vision.content_hash)
        .bind(&vision.settings)
        .bind(i64::from(vision.pending))
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM folder_chat_doc_page WHERE document_id = ?1")
            .bind(document_id)
            .execute(&mut *tx)
            .await?;
        for k in pages {
            sqlx::query(
                "INSERT INTO folder_chat_doc_page
                     (document_id, page, model, mode, prompt_version, dpi)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .bind(document_id)
            .bind(i64::from(k.page))
            .bind(&k.model)
            .bind(k.mode.as_str())
            .bind(&k.prompt_version)
            .bind(i64::from(k.dpi))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The readings the indexed text of the PDF at `url` holds, in page order
    /// — its recorded pages joined with the cache through its recorded hash —
    /// when that hash is `content_hash`, the hash of the bytes on disk now.
    /// What the MCP `read` tool appends, so it returns the text the sync
    /// indexed: a pass that stopped part-way has not changed the record yet,
    /// and a file changed since gets none (they were readings of other
    /// bytes).
    pub async fn pdf_readings(
        &self,
        url: &str,
        content_hash: &str,
    ) -> Result<Vec<Reading>, IndexError> {
        let rows = sqlx::query(
            "SELECT p.page, p.model, c.text
             FROM folder_chat_doc_page p
             JOIN document d ON d.id = p.document_id
             JOIN folder_chat_doc_vision v ON v.document_id = p.document_id
             JOIN folder_chat_reading c ON c.content_hash = v.content_hash
                  AND c.page = p.page AND c.model = p.model AND c.mode = p.mode
                  AND c.prompt_version = p.prompt_version AND c.dpi = p.dpi
             WHERE d.url = ?1 AND v.content_hash = ?2 AND c.status = 'read'
             ORDER BY p.page",
        )
        .bind(url)
        .bind(content_hash)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| Reading {
                page: u32::try_from(r.get::<i64, _>("page")).unwrap_or(0),
                model: r.get("model"),
                text: r.get("text"),
            })
            .collect())
    }

    /// Every page of an indexed PDF of one corpus whose recorded reading is a
    /// cached failure, by path and page.
    pub async fn failed_pages(&self, corpus_id: i64) -> Result<Vec<FailedPage>, IndexError> {
        let rows = sqlx::query(
            "SELECT d.url, p.page, p.model, c.text
             FROM folder_chat_doc_page p
             JOIN document d ON d.id = p.document_id
             JOIN source s ON s.id = d.source_id
             JOIN folder_chat_doc_vision v ON v.document_id = p.document_id
             JOIN folder_chat_reading c ON c.content_hash = v.content_hash
                  AND c.page = p.page AND c.model = p.model AND c.mode = p.mode
                  AND c.prompt_version = p.prompt_version AND c.dpi = p.dpi
             WHERE s.corpus_id = ?1 AND c.status = 'failed'
             ORDER BY d.url, p.page",
        )
        .bind(corpus_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| FailedPage {
                path: r.get("url"),
                page: u32::try_from(r.get::<i64, _>("page")).unwrap_or(0),
                model: r.get("model"),
                reason: r.get("text"),
            })
            .collect())
    }

    /// The pages of each indexed PDF of one corpus whose indexed text holds a
    /// reading of them, by path.
    pub async fn read_pages(
        &self,
        corpus_id: i64,
    ) -> Result<HashMap<String, HashSet<u32>>, IndexError> {
        let rows = sqlx::query(
            "SELECT d.url, p.page
             FROM folder_chat_doc_page p
             JOIN document d ON d.id = p.document_id
             JOIN source s ON s.id = d.source_id
             JOIN folder_chat_doc_vision v ON v.document_id = p.document_id
             JOIN folder_chat_reading c ON c.content_hash = v.content_hash
                  AND c.page = p.page AND c.model = p.model AND c.mode = p.mode
                  AND c.prompt_version = p.prompt_version AND c.dpi = p.dpi
             WHERE s.corpus_id = ?1 AND c.status = 'read'",
        )
        .bind(corpus_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out: HashMap<String, HashSet<u32>> = HashMap::new();
        for r in &rows {
            if let Ok(page) = u32::try_from(r.get::<i64, _>("page")) {
                out.entry(r.get("url")).or_default().insert(page);
            }
        }
        Ok(out)
    }

    /// Mark a PDF pending or not — at the end of its pass, pending only when
    /// a page is to be tried again on the next sync.
    pub async fn set_vision_pending(
        &self,
        document_id: i64,
        pending: bool,
    ) -> Result<(), IndexError> {
        sqlx::query("UPDATE folder_chat_doc_vision SET pending = ?2 WHERE document_id = ?1")
            .bind(document_id)
            .bind(i64::from(pending))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The owner's Retry failed pages: delete every cached failure, and mark
    /// every PDF whose indexed text records one pending, so the next sync
    /// takes it through and reads those pages again. One transaction.
    /// Returns `(failures deleted, documents marked)`.
    pub async fn retry_failed_readings(&self) -> Result<(u64, u64), IndexError> {
        let mut tx = self.pool.begin().await?;
        let marked = sqlx::query(
            "UPDATE folder_chat_doc_vision SET pending = 1 WHERE document_id IN (
                 SELECT p.document_id FROM folder_chat_doc_page p
                 JOIN folder_chat_doc_vision v ON v.document_id = p.document_id
                 JOIN folder_chat_reading c ON c.content_hash = v.content_hash
                      AND c.page = p.page AND c.model = p.model AND c.mode = p.mode
                      AND c.prompt_version = p.prompt_version AND c.dpi = p.dpi
                 WHERE c.status = 'failed')",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let deleted = sqlx::query("DELETE FROM folder_chat_reading WHERE status = 'failed'")
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok((deleted, marked))
    }

    /// Delete cached readings whose content hash no document refers to any
    /// more — not quickdoc's `document`, not a file marker, not a PDF's
    /// vision record — except those of `keep`: the hashes whose pages this
    /// sync read or reused, which a file not indexed after all (its
    /// embedding failed) still needs next time. Only a sync that completed
    /// may call this: after one that stopped, files not reached yet may have
    /// no document for a while (an index being rebuilt, say), and their
    /// readings must wait for them. Returns how many rows went.
    pub async fn prune_readings(&self, keep: &HashSet<String>) -> Result<u64, IndexError> {
        let hashes: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT content_hash FROM folder_chat_reading r
             WHERE NOT EXISTS (SELECT 1 FROM document d WHERE d.content_hash = r.content_hash)
               AND NOT EXISTS (SELECT 1 FROM folder_chat_file f
                               WHERE f.indexed_hash = r.content_hash)
               AND NOT EXISTS (SELECT 1 FROM folder_chat_doc_vision v
                               WHERE v.content_hash = r.content_hash)",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut tx = self.pool.begin().await?;
        let mut n = 0;
        for h in hashes.iter().filter(|h| !keep.contains(*h)) {
            n += sqlx::query("DELETE FROM folder_chat_reading WHERE content_hash = ?1")
                .bind(h)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        tx.commit().await?;
        Ok(n)
    }

    /// The documents of one corpus that have a chunk without stored lines —
    /// an index from before lines were recorded; the sync re-chunks those
    /// files once (keeping every vector that is still right) to record them.
    pub async fn documents_missing_lines(
        &self,
        corpus_id: i64,
    ) -> Result<HashSet<i64>, IndexError> {
        let rows = sqlx::query(
            "SELECT DISTINCT c.document_id AS id FROM chunk c
             LEFT JOIN folder_chat_chunk_lines l ON l.chunk_id = c.id
             WHERE c.corpus_id = ?1 AND l.chunk_id IS NULL",
        )
        .bind(corpus_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<i64, _>("id")).collect())
    }

    /// The stored lines of these chunks, by id — 1-based and inclusive, as
    /// recorded when each chunk was indexed (for a PDF: lines of the
    /// `pdftotext` output of that time). A chunk with no stored lines is
    /// absent from the map.
    pub async fn chunk_lines(
        &self,
        chunk_ids: &[String],
    ) -> Result<HashMap<String, (usize, usize)>, IndexError> {
        let mut out = HashMap::new();
        // Well under SQLite's bound-parameter limit per statement.
        for part in chunk_ids.chunks(500) {
            let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
                "SELECT chunk_id, start_line, end_line FROM folder_chat_chunk_lines \
                 WHERE chunk_id IN (",
            );
            let mut sep = qb.separated(", ");
            for id in part {
                sep.push_bind(id);
            }
            qb.push(")");
            for r in qb.build().fetch_all(&self.pool).await? {
                out.insert(
                    r.get::<String, _>("chunk_id"),
                    (
                        r.get::<i64, _>("start_line").max(0) as usize,
                        r.get::<i64, _>("end_line").max(0) as usize,
                    ),
                );
            }
        }
        Ok(out)
    }

    /// Give each citation the line numbers stored with its chunk
    /// ([`IndexDir::chunk_lines`]) — the lines of the file as it was indexed.
    /// No file is read and no `pdftotext` is run, so a file edited since the
    /// last sync is still cited where the excerpt was then. A chunk without
    /// stored lines leaves its citation's lines unset. What every citation
    /// and MCP search hit gets its lines from.
    pub async fn fill_stored_lines(&self, cites: &mut [Citation]) -> Result<(), IndexError> {
        let ids: Vec<String> = cites.iter().map(|c| c.chunk_id.clone()).collect();
        let lines = self.chunk_lines(&ids).await?;
        for c in cites.iter_mut() {
            if let Some(&(a, b)) = lines.get(&c.chunk_id) {
                c.start_line = Some(a);
                c.end_line = Some(b);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refuses_a_symlinked_index_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), tmp.path().join(INDEX_DIR_NAME)).unwrap();
        let e = IndexDir::open(tmp.path()).await.unwrap_err();
        assert!(matches!(e, IndexError::NotAPlainDir(_)), "{e}");
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn refuses_a_symlinked_index_file() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(INDEX_DIR_NAME)).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path().join("x.sqlite"),
            tmp.path().join(INDEX_DIR_NAME).join(INDEX_FILE_NAME),
        )
        .unwrap();
        let e = IndexDir::open(tmp.path()).await.unwrap_err();
        assert!(matches!(e, IndexError::Symlink(_)), "{e}");
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_new_index_dir_is_tagged_for_git_and_backups() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = IndexDir::open(tmp.path()).await.unwrap();
        let dir = tmp.path().join(INDEX_DIR_NAME);
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            "*\n"
        );
        let tag = std::fs::read_to_string(dir.join("CACHEDIR.TAG")).unwrap();
        assert!(
            tag.starts_with("Signature: 8a477f597d28d172789f06886806bc55"),
            "{tag}"
        );
        assert!(idx.check_containment().is_ok());
        // A second open neither rewrites nor duplicates them.
        std::fs::write(dir.join(".gitignore"), "*\n# owner's note\n").unwrap();
        drop(idx);
        IndexDir::open(tmp.path()).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            "*\n# owner's note\n"
        );
    }

    #[tokio::test]
    async fn the_pool_is_fixed_and_never_recycles() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = IndexDir::open(tmp.path()).await.unwrap();
        let o = idx.pool().options();
        assert_eq!(o.get_min_connections(), INDEX_POOL_CONNECTIONS);
        assert_eq!(o.get_max_connections(), INDEX_POOL_CONNECTIONS);
        assert_eq!(o.get_idle_timeout(), None);
        assert_eq!(o.get_max_lifetime(), None);
        assert_eq!(
            idx.pool().size(),
            INDEX_POOL_CONNECTIONS,
            "all open at start-up"
        );
    }

    #[tokio::test]
    async fn one_sync_at_a_time() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = IndexDir::open(tmp.path()).await.unwrap();
        let permit = idx.try_begin_sync().unwrap();
        assert!(idx.is_syncing());
        assert!(
            idx.clone().try_begin_sync().is_none(),
            "a clone shares the slot"
        );
        drop(permit);
        assert!(idx.try_begin_sync().is_some());
    }
}
