//! Keeping the index in step with the folder.
//!
//! One sync, in order:
//!
//! 1. **Claim the slot** — one sync per index at a time
//!    ([`IndexDir::try_begin_sync`]).
//! 2. **Check the chunk size** against the embedding model's context, when
//!    lmgw reports one: a chunk the model cannot read whole is a visible
//!    abort, never a silent clamp.
//! 3. **Check containment** ([`IndexDir::check_containment`]): the index
//!    directory is still the one the agent created, and none of its files is
//!    a symlink or has a second hard link — otherwise
//!    [`AbortKind::IndexContainment`], with nothing written. Checked again
//!    before every file.
//! 4. **Pin the corpus** to the embedding model (`embed_upstream` /
//!    `embed_model` / `embed_dims`), to the model's **fingerprint** — the
//!    vector of a fixed probe text, compared by cosine
//!    ([`PROBE_SAME_MODEL_MIN_COSINE`]), which catches an alias re-pointed to
//!    another model of the same width — and to the chunking (chunk size,
//!    chunker version). If any of them differs from what the index was built
//!    with, the whole corpus is deleted and rebuilt, and the sync says so
//!    ([`SyncEvent::IndexReset`]): vectors from two models are not
//!    comparable, and chunks cut two ways would rank inconsistently.
//! 5. **Scan** ([`crate::scan`]) and **plan**: a file whose size and mtime are
//!    what the index recorded is unchanged without being read (the fast path);
//!    any other file is read and hashed, and quickdoc's `upsert_document` gate
//!    decides whether its content really changed. A file in the index but not
//!    on disk is removed ([`quickdoc_core::store::delete_document`]; its chunks
//!    and FTS postings go with it). PDFs are also re-extracted when
//!    `pdftotext -v` reports another version than the index recorded (only
//!    PDFs; nothing is reset), and a file whose chunks have no stored line
//!    numbers is re-chunked once to record them.
//! 6. **Read, extract, chunk** each new or changed file: read through
//!    [`crate::source::read_beneath`] (no symlink anywhere on the path, a
//!    regular file only, at most [`LARGE_FILE_MEMORY_FRACTION`] of the
//!    container's memory), hashed, a PDF's bytes piped to `pdftotext`
//!    (bounded by [`crate::pdf::PDF_EXTRACT_TIMEOUT`]), chunked — the CPU work
//!    in `spawn_blocking`. With a vision model set, the PDF pages
//!    [`crate::vision::select`] picks are read first — from the page-reading
//!    cache ([`crate::index`], keyed by the bytes, the page, the alias, the
//!    mode, the prompt version and the resolution) when a reading is there,
//!    else rendered by `pdftoppm` and read by the model, each cached as soon
//!    as it is done ([`SyncEvent::Reading`] before each) — and appended to the
//!    PDF's text as that model's readings ([`crate::vision::append_readings`]),
//!    chunked on their own. A page that fails the same way every time (an
//!    empty or cut-off answer, a refusal of this page, a page that does not
//!    render) is cached as failed and reported by every sync, and read again
//!    only when its key changes; the rest of the file is indexed. A hold, a
//!    reading answered through a hold fallback ([`AbortKind::GpuHold`]), or a
//!    fault that is not about the page — a server error, an unreachable
//!    gateway ([`AbortKind::Vision`]) — stops the sync, and nothing of that
//!    reading is stored. Each PDF records the vision settings
//!    ([`crate::vision::settings`]) and the readings its indexed text was
//!    built with, and is marked pending before its first page is read; one
//!    whose settings differ, or that is still pending, goes through the
//!    refresh pass below, reusing every cached reading and vector.
//! 7. **Embed and store, batch by batch**: batches of [`EMBED_BATCH_SIZE`]
//!    (every batch is a progress event naming the size), each stored as soon
//!    as it is embedded, with its line numbers. A chunk already stored exactly
//!    as it would be again (same content-derived id, heading and span, with a
//!    vector) is kept, not re-embedded — which is how a file interrupted
//!    mid-way resumes. A batch that fails is sent once more
//!    ([`EMBED_BATCH_ATTEMPTS`]); if it fails again while a probe embedding
//!    still succeeds, the file is reported as an error and the sync goes on;
//!    only a gateway that fails the probe too stops the sync.
//!
//! **A file is only marked current after all its chunks are stored.** The
//! mark is the agent's `folder_chat_file` row ([`crate::index`]), written last
//! (after the chunks the file no longer holds are deleted) and carrying the
//! hash the chunks were built from; a sync that stops anywhere earlier — a
//! `gpu_hold`, a crash, a kill — leaves a file that the next sync sees as not
//! current and finishes, re-embedding only what was not stored yet (chunk ids
//! are content-derived and `insert_chunks` upserts, so nothing is stored
//! twice). Chunks already stored for an unfinished file are searchable after
//! the retriever is rebuilt; its earlier chunks, if it had any, stay until it
//! is finished. Chat during a sync keeps the previous retriever's vectors,
//! but reads BM25 and the excerpt text live from the index the sync is
//! writing, so a mid-sync answer can mix the two states (see
//! [`crate::app`]).
//!
//! Progress goes out as [`SyncEvent`]s over an unbounded channel, so a slow or
//! absent reader (a browser tab that went away) never stalls the sync; the
//! events are small and bounded by the number of files.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use quickdoc_core::embed::{validate_vector, EmbedIdentity, Embedder, TokenCounter};
use quickdoc_core::store::{self, Document, NewChunk, NewCorpus};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::chunk::{self, ChunkSpan, CHUNKER_VERSION};
use crate::gateway::{classify_quickdoc_error, GatewayError, EMBED_PROBE_TEXT};
use crate::index::{
    CachedReading, DocVision, FileState, IndexDir, IndexError, PageKey, VisionStatus,
};
use crate::pdf::{self, PdfError, RenderError};
use crate::scan::{self, Candidate, FileKind, SkipReason, Skipped};
use crate::source::{self, ReadError};
use crate::vision::{self, Mode, Reading, VisionOptions, VISION_DPI, VISION_PROMPT_VERSION};

/// Texts per `/v1/embeddings` request. The caller batches (the embedder sends
/// whatever it is handed as one request): 32 keeps one request comfortably
/// inside any server's body limit at the default chunk size while still
/// amortising the round trip. Every batch is an [`SyncEvent::Embedding`]
/// naming this size, and the report carries it. It is also the unit a file
/// is stored in: a batch is written as soon as it is embedded.
pub const EMBED_BATCH_SIZE: usize = 32;

/// How many times one batch is sent before the sync decides whose fault a
/// failure is: 2, the first try and one retry. A transient failure (a
/// restarting container, a dropped connection) gets its second chance; a
/// batch that fails twice is probed ([`EMBED_PROBE_TEXT`]): the probe
/// succeeding means this file's text is what the model cannot embed, and the
/// file is reported (not current, tried again next sync) while the sync goes
/// on; the probe failing too means the gateway is failing, and the sync
/// stops. A hold is never retried. Carried in the report and named in the
/// file's error.
pub const EMBED_BATCH_ATTEMPTS: usize = 2;

/// The lowest cosine between the probe embedding the index was built with
/// and the one the embedding model gives now at which the two are taken to be
/// the same model; below it the index is rebuilt ([`SyncEvent::IndexReset`],
/// naming the cosine and this constant).
///
/// 0.99. The same weights re-embed a fixed text to a cosine above 0.999 even
/// across batch sizes and GPU scheduling (llama.cpp's embeddings are not
/// bit-reproducible, which is why this is a cosine and not a hash of the
/// vector — a hash would flip on the last rounded digit and re-embed the
/// whole folder for nothing), and a re-quantisation of the same model stays
/// above 0.99 with vectors that remain comparable. Two different models of
/// the same width live in unrelated vector spaces: their cosine for one text
/// is near 0. The report carries every measured cosine
/// (`embed_probe_cosine`).
pub const PROBE_SAME_MODEL_MIN_COSINE: f32 = 0.99;

/// The largest share of the container's memory limit one file may take: a
/// file larger than this fraction of it is skipped as `too_large_for_memory`,
/// with a reason naming the limit, this fraction, and the two ways out
/// (raise `run.limits.memory_mb`, or list the file in `.ignore`).
///
/// 0.25. A file is held in memory whole while it is hashed and chunked, and
/// a PDF's extracted text sits beside its bytes (and `pdftotext` itself runs
/// inside the same limit), so one file costs up to about twice its size; a
/// quarter keeps that to half the limit, leaving the other half for what is
/// resident anyway — every vector of the folder (twice, briefly, while a
/// fresh retriever replaces the old one), SQLite's caches and the server.
/// Derived from the real limit, never a fixed byte count: with no limit
/// (`memory.max` absent or `max`) nothing is skipped for size.
pub const LARGE_FILE_MEMORY_FRACTION: f64 = 0.25;

/// Where cgroup v2 says the container's memory limit is (`max` or bytes).
pub const CGROUP_MEMORY_MAX: &str = "/sys/fs/cgroup/memory.max";

/// A file whose mtime is this close to the moment it was read (or later) is
/// not trusted to change its mtime on the next edit, git's "racily clean"
/// rule: an edit in the same timestamp tick would leave the mtime as
/// recorded, and the fast path would miss it forever. Such a file is stored
/// with mtime 0, so the next sync re-hashes it (and re-embeds nothing if it
/// did not change); so is a file whose size or mtime moved between the scan
/// and the read, or while it was read.
///
/// 2 s: the coarsest mtime granularity of a filesystem a folder is likely to
/// sit on (FAT's 2 seconds, on a USB stick), which also covers 1-second
/// filesystems (ext3, HFS+, many network and FUSE mounts) and the kernel's
/// coarse clock. The report counts the files it applied to (`racy_files`)
/// and says so in a note.
pub const RACY_MTIME_WINDOW: Duration = Duration::from_secs(2);

/// The corpus a folder is: `folder@1`. One folder, one index file, so the id
/// never needs to say which folder.
pub const CORPUS_LIBRARY: &str = "folder";
pub const CORPUS_VERSION: &str = "1";

/// quickdoc's `source.kind` is a CHECK-constrained list of web-ingestion kinds
/// (`llms_txt`, `markdown`, `rustdoc_json`, `html`); a folder of mixed files is
/// none of them. `markdown` is the nearest allowed label and nothing in
/// retrieval reads it — the per-file kind lives in the file's extension.
pub const SOURCE_KIND: &str = "markdown";

const META_CHUNK_TOKENS: &str = "chunk_tokens";
const META_CHUNKER_VERSION: &str = "chunker_version";
/// `{"text": …, "vector": […]}`: [`EMBED_PROBE_TEXT`] and its vector when
/// the index was built.
const META_EMBED_PROBE: &str = "embed_probe";
/// `pdftotext -v`'s first line when every indexed PDF was last extracted.
const META_PDFTOTEXT_VERSION: &str = "pdftotext_version";

pub type Events = mpsc::UnboundedSender<SyncEvent>;

/// What a sync is told beyond the index and the embedder.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// The owner's `chunk_tokens`.
    pub chunk_tokens: usize,
    /// The embedding model's `context_length`, when lmgw reports one.
    pub embed_context_length: Option<u64>,
    /// The vector of [`EMBED_PROBE_TEXT`] the embedder answered when it was
    /// connected ([`crate::gateway::GatewayEmbedder::probe`]); `None` makes
    /// the sync embed it itself.
    pub probe: Option<Vec<f32>>,
    /// [`CGROUP_MEMORY_MAX`]; a test points it at a file of its own.
    pub memory_max_path: PathBuf,
    /// [`pdf::PDFTOTEXT`]; a test points it at a script.
    pub pdftotext: PathBuf,
    /// [`pdf::PDFTOPPM`], the page renderer; a test points it at a script.
    pub pdftoppm: PathBuf,
    /// [`pdf::PDF_EXTRACT_TIMEOUT`] — for `pdftotext` and `pdftoppm` alike;
    /// a test shortens it.
    pub pdf_timeout: Duration,
    /// The vision model that reads PDF pages ([`crate::vision`]); `None`
    /// reads no page (and drops the readings an earlier sync stored).
    pub vision: Option<VisionOptions>,
    /// The owner's Retry failed pages: before anything is planned, forget
    /// every cached page failure and mark those PDFs pending
    /// ([`IndexDir::retry_failed_readings`]) — here, under the sync's own
    /// permit, so no other sync can have read the cache in between.
    pub retry_failed: bool,
}

impl SyncOptions {
    /// Everything but the chunk size and the context at its real default.
    pub fn new(chunk_tokens: usize, embed_context_length: Option<u64>) -> Self {
        Self {
            chunk_tokens,
            embed_context_length,
            probe: None,
            memory_max_path: PathBuf::from(CGROUP_MEMORY_MAX),
            pdftotext: PathBuf::from(pdf::PDFTOTEXT),
            pdftoppm: PathBuf::from(pdf::PDFTOPPM),
            pdf_timeout: pdf::PDF_EXTRACT_TIMEOUT,
            vision: None,
            retry_failed: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortKind {
    /// A 503 `gpu_hold`, or an embedding or a page reading answered by a
    /// hold fallback: local models are paused on purpose.
    GpuHold,
    /// `chunk_tokens` exceeds the embedding model's context.
    ChunkTooLarge,
    /// The embedder failed in a way that is not about one file (the probe
    /// failed too).
    Embedder,
    /// The index could not be read or written.
    Index,
    /// The folder is missing or cannot be listed.
    Folder,
    /// The index directory, or a file in it, is no longer only the agent's
    /// (replaced, a symlink, a second hard link): nothing was written.
    IndexContainment,
    /// The vision model failed in a way that is not about one page — a
    /// server error, an unreachable gateway, a refusal of the request itself,
    /// an answer in no known shape. Every page would fail the same way, and a
    /// crashing model would be restarted for each, so the sync stops; the
    /// pages read before it are cached, and the next sync resumes.
    Vision,
    /// Stopped from outside the sync — the owner's Stop sync, or the app
    /// shutting down; the reason says which (`crate::server::SyncStop`).
    /// Nothing failed: the next sync resumes where this one stopped.
    Stopped,
}

/// One progress event. Serialised with a `type` tag, ready for an SSE stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyncEvent {
    /// The walk finished: this many files of an indexable type were found.
    Scanning { found: usize },
    /// The index is being rebuilt from nothing, and why.
    IndexReset { reason: String },
    /// What the sync is about to do. `changed` counts indexed files that are
    /// re-read: their size or mtime moved (or their recorded mtime is not
    /// trusted, or they are PDFs of an older `pdftotext`, or PDFs indexed
    /// with other vision settings or whose pass had not finished, or they
    /// lack stored line numbers) — each is re-hashed, and one whose content is identical
    /// is reported as [`SyncEvent::Unchanged`] rather than re-embedded.
    /// `skipped_by_reason` is the walk's skips (including `ignored`);
    /// content-level skips arrive as [`SyncEvent::Skipped`] and in the
    /// report. `ignored_dirs` are the directories an ignore rule pruned, each
    /// counted once under `ignored`.
    Planned {
        new: usize,
        changed: usize,
        removed: usize,
        unchanged: usize,
        skipped_by_reason: BTreeMap<SkipReason, usize>,
        ignored_dirs: Vec<String>,
    },
    /// One embedding request: batch `batch` of `of` for `file`, `texts` texts
    /// in it, batches being at most `batch_size` texts.
    Embedding {
        file: String,
        batch: usize,
        of: usize,
        texts: usize,
        batch_size: usize,
    },
    /// The vision model is about to read page `page` of `of` (the PDF's
    /// page count) of `file` — sent only for a page that is not cached. A
    /// reading takes seconds; without this the sync would look stuck.
    Reading { file: String, page: u32, of: u32 },
    /// The vision model could not read page `page` of `file`, and why. The
    /// rest of the file goes on (the report's `errors` carries it too).
    /// `cached`: the failure is stored and the page is not read again until
    /// the file, the vision alias or the prompt changes; otherwise (no
    /// `pdftoppm`) the next sync tries it again.
    ReadingFailed {
        file: String,
        page: u32,
        model: String,
        reason: String,
        cached: bool,
    },
    /// Taken out of the index: gone from disk, or no longer indexable.
    Removed { file: String },
    /// Re-hashed after a size/mtime change; the content is what was indexed.
    Unchanged { file: String },
    /// Read, but not indexable.
    Skipped {
        file: String,
        reason: SkipReason,
        detail: Option<String>,
    },
    /// Indexed: `chunks` chunks stored.
    FileDone { file: String, chunks: usize },
    /// This file failed; the sync carries on with the rest.
    Error { file: String, message: String },
    /// Boxed: the report carries every skipped path, and an event enum sized
    /// for it would make every progress event that large.
    Done { report: Box<SyncReport> },
    /// The sync stopped. The index is consistent: every file marked current
    /// has all its chunks.
    Aborted { kind: AbortKind, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileError {
    pub file: String,
    pub message: String,
}

/// An indexed PDF with pages that have no text — typically scans without an
/// OCR layer — and no reading by a vision model. Those pages are not in the
/// index; the rest of the file is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextlessPages {
    pub path: String,
    /// Pages in the file.
    pub pages: u32,
    /// The pages without text and without a reading, 1-based.
    pub textless: Vec<u32>,
}

/// A page of an indexed PDF whose cached reading is a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisionFailure {
    pub path: String,
    /// 1-based.
    pub page: u32,
    /// The alias that could not read it.
    pub model: String,
    pub reason: String,
}

/// Everything a sync did and every number that shaped it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncReport {
    pub folder: String,
    pub index_path: String,
    /// `upstream/model (dims)` the corpus is pinned to.
    pub embed_model: String,
    pub embed_dims: usize,
    pub embed_context_length: Option<u64>,
    /// [`EMBED_BATCH_SIZE`].
    pub embed_batch_size: usize,
    /// [`EMBED_BATCH_ATTEMPTS`].
    #[serde(default)]
    pub embed_batch_attempts: usize,
    /// Cosine between the probe embedding the index was built with and the
    /// one the model gives now; `None` when this sync recorded it first.
    /// Below [`PROBE_SAME_MODEL_MIN_COSINE`] the index was rebuilt.
    #[serde(default)]
    pub embed_probe_cosine: Option<f32>,
    pub chunk_tokens: usize,
    /// How chunk sizes were measured.
    pub token_estimator: String,
    pub chunker_version: String,
    /// Why the index was rebuilt from nothing, when it was.
    pub index_reset: Option<String>,
    /// The container's memory limit in bytes (cgroup `memory.max`); `None`
    /// when there is none.
    #[serde(default)]
    pub memory_limit_bytes: Option<u64>,
    /// [`LARGE_FILE_MEMORY_FRACTION`] of `memory_limit_bytes`: a larger file
    /// is skipped as `too_large_for_memory`. `None`: no file is.
    #[serde(default)]
    pub large_file_limit_bytes: Option<u64>,
    /// [`pdf::PDF_EXTRACT_TIMEOUT`] as this sync applied it, milliseconds.
    #[serde(default)]
    pub pdf_extract_timeout_ms: u64,
    /// `pdftotext -v`'s first line, when there were PDFs to extract.
    #[serde(default)]
    pub pdftotext_version: Option<String>,
    /// [`RACY_MTIME_WINDOW`], milliseconds.
    #[serde(default)]
    pub racy_mtime_window_ms: u64,
    /// Files stored with an untrusted mtime (changing while read, or modified
    /// within [`RACY_MTIME_WINDOW`] of it): the next sync re-hashes them.
    #[serde(default)]
    pub racy_files: usize,
    /// Files of an indexable type the walk found.
    pub found: usize,
    pub new_files: usize,
    pub changed_files: usize,
    /// Size or mtime moved, content identical: re-hashed, not re-embedded.
    pub touched_unchanged: usize,
    /// Size and mtime as recorded: not even read.
    pub unchanged: usize,
    pub removed_files: usize,
    /// Texts embedded by this sync (a chunk kept from before is not one).
    pub embedded_chunks: usize,
    /// Chunks kept from an earlier (possibly interrupted) sync because they
    /// were already stored exactly as they would be again.
    #[serde(default)]
    pub reused_chunks: usize,
    /// Files re-chunked only to record their line numbers (an index from
    /// before they were stored).
    #[serde(default)]
    pub lines_recorded_files: usize,
    /// PDFs extracted again only to record their pages without text (an
    /// index from before they were recorded).
    #[serde(default)]
    pub pdf_pages_recorded_files: usize,
    /// Every indexed PDF with pages that have no text and no reading, by
    /// path — no cap. Read from the index, so it covers files this sync did
    /// not read too.
    #[serde(default)]
    pub pdf_pages_without_text: Vec<TextlessPages>,
    /// The vision alias PDF pages are read with; `None`: no page is read.
    #[serde(default)]
    pub vision_model: Option<String>,
    /// `vision_every_page`: every page with text is read, not only tables.
    #[serde(default)]
    pub vision_every_page: bool,
    /// [`VISION_DPI`], the resolution pages are rendered at.
    #[serde(default)]
    pub vision_dpi: u32,
    /// [`VISION_PROMPT_VERSION`], part of every cached reading's key.
    #[serde(default)]
    pub vision_prompt_version: String,
    /// Which pages are read, with the rule's constants ([`vision::rule`]).
    #[serde(default)]
    pub vision_rule: String,
    /// Pages the vision model read in this sync.
    #[serde(default)]
    pub vision_pages_read: usize,
    /// Pages whose stored reading was used instead of asking the model.
    #[serde(default)]
    pub vision_pages_reused: usize,
    /// Every page of an indexed PDF whose reading failed, with why — read
    /// from the index, so every sync reports it, not only the one that tried;
    /// no cap. A failure is not retried until the file, the vision alias or
    /// the prompt changes.
    #[serde(default)]
    pub vision_pages_failed: Vec<VisionFailure>,
    /// PDFs taken through the refresh pass for the vision model: indexed with
    /// other vision settings, or a pass over them had stopped part-way.
    /// Cached readings and chunks already embedded were kept.
    #[serde(default)]
    pub vision_refreshed_files: usize,
    /// Cached page readings deleted because no indexed file has their bytes
    /// any more.
    #[serde(default)]
    pub vision_readings_pruned: u64,
    /// Cached page failures forgotten at the start of this sync, by the
    /// owner's Retry failed pages; their pages are read again.
    #[serde(default)]
    pub vision_failures_cleared: u64,
    /// Probe readings sent ([`vision::probe_png`]): when the vision model
    /// refuses a page as an input, one small image tells a refusal of that
    /// page from a model that refuses every image — once per sync for each
    /// status.
    #[serde(default)]
    pub vision_probes: usize,
    /// Every `/v1/embeddings` request this sync sent: batches, their retries,
    /// and probes.
    pub embed_requests: usize,
    pub errors: Vec<FileError>,
    pub skipped_by_reason: BTreeMap<SkipReason, usize>,
    /// Every skipped entry, with its path — no cap. Ignore-rule exclusions
    /// are the one exception, and are counted in the three fields below.
    pub skipped: Vec<Skipped>,
    /// Files a `.gitignore` / `.ignore` rule excluded: counted, not listed.
    pub ignored_files: usize,
    /// Directories an ignore rule pruned, by path — each one entry under
    /// `ignored`, never walked.
    pub ignored_dirs: Vec<String>,
    /// The ignore files whose rules were applied.
    pub ignore_files: Vec<String>,
    /// In the index once the sync ended.
    pub documents: usize,
    pub chunks: usize,
    /// Facts worth a line of their own (an unchecked chunk size, a missing
    /// pdftotext, a pdftotext update, …).
    pub notes: Vec<String>,
    pub elapsed_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("a sync is already running for this folder")]
    AlreadyRunning,
    #[error("{reason}")]
    Aborted {
        kind: AbortKind,
        reason: String,
        partial: Box<SyncReport>,
    },
}

/// The container's memory limit as cgroup v2 states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryLimit {
    Bytes(u64),
    /// No limit applies, and why (`memory.max` says `max`, or is absent).
    Unlimited(String),
}

/// Read a cgroup v2 `memory.max` file: a byte count, or `max`.
pub fn read_memory_limit(path: &Path) -> MemoryLimit {
    match std::fs::read_to_string(path) {
        Ok(s) => match s.trim() {
            "max" => MemoryLimit::Unlimited(format!("{} says max", path.display())),
            n => match n.parse::<u64>() {
                Ok(b) => MemoryLimit::Bytes(b),
                Err(_) => MemoryLimit::Unlimited(format!(
                    "{} holds '{n}', which is not a byte count",
                    path.display()
                )),
            },
        },
        Err(e) => MemoryLimit::Unlimited(format!("{} cannot be read: {e}", path.display())),
    }
}

/// The skip reason for a file over the memory rule: the limit, the fraction,
/// what it came to, and the two ways out.
pub fn too_large_detail(size: u64, limit: u64, source: &Path) -> String {
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let threshold = (limit as f64 * LARGE_FILE_MEMORY_FRACTION) as u64;
    let needed_mb = (size as f64 / LARGE_FILE_MEMORY_FRACTION / (1024.0 * 1024.0)).ceil() as u64;
    format!(
        "{:.1} MiB is more than LARGE_FILE_MEMORY_FRACTION ({LARGE_FILE_MEMORY_FRACTION}) of the \
         container's memory limit ({:.1} MiB, from {}), i.e. {:.1} MiB; a file is held in memory \
         whole while it is indexed. Raise the agent's memory limit (run.limits.memory_mb) to at \
         least {needed_mb} MB, or list the file in the folder's .ignore",
        mib(size),
        mib(limit),
        source.display(),
        mib(threshold)
    )
}

/// Cosine similarity; `None` for vectors of different widths or a zero one.
pub fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        dot += *x as f64 * *y as f64;
        na += *x as f64 * *x as f64;
        nb += *y as f64 * *y as f64;
    }
    (na > 0.0 && nb > 0.0).then(|| (dot / (na.sqrt() * nb.sqrt())) as f32)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredProbe {
    text: String,
    vector: Vec<f32>,
}

async fn stored_probe(index: &IndexDir) -> Result<Option<StoredProbe>, IndexError> {
    Ok(index
        .meta(META_EMBED_PROBE)
        .await?
        .and_then(|v| serde_json::from_str::<StoredProbe>(&v).ok())
        .filter(|p| p.text == EMBED_PROBE_TEXT))
}

/// Whether `probe` (the vector of [`EMBED_PROBE_TEXT`] from the embedder at
/// hand) is the model the index was built with: `true` when nothing was
/// recorded yet or the cosine is at least [`PROBE_SAME_MODEL_MIN_COSINE`].
/// What start-up asks before it loads an index for answering.
pub async fn probe_matches(index: &IndexDir, probe: &[f32]) -> Result<bool, IndexError> {
    Ok(match stored_probe(index).await? {
        None => true,
        Some(p) => cosine(&p.vector, probe).is_some_and(|c| c >= PROBE_SAME_MODEL_MIN_COSINE),
    })
}

/// Run one sync. See the module docs for the order of things.
pub async fn run(
    index: &IndexDir,
    embedder: Arc<dyn Embedder>,
    opts: &SyncOptions,
    events: &Events,
) -> Result<SyncReport, SyncError> {
    let Some(_permit) = index.try_begin_sync() else {
        return Err(SyncError::AlreadyRunning);
    };
    let identity = embedder.identity();
    let tc = chunk::estimator();
    let mut run = Run {
        index,
        embedder,
        endpoint: format!("embedding model {identity}"),
        identity,
        opts,
        events,
        tc,
        started: Instant::now(),
        report: SyncReport::default(),
        corpus_id: 0,
        source_id: 0,
        pdf_tool_missing: 0,
        large_limit: None,
        memory_source: PathBuf::new(),
        pdf_version: None,
        pdftoppm_missing: 0,
        vision_read_ocr: 0,
        vision_read_structure: 0,
        vision_failed_now: 0,
        vision_failed_cached: 0,
        vision_settings: vision::settings(opts.vision.as_ref()),
        vision_had: BTreeSet::new(),
        vision_hashes_used: HashSet::new(),
        vision_probed: HashSet::new(),
    };
    run.report = SyncReport {
        folder: index.folder().display().to_string(),
        index_path: index.db_path().display().to_string(),
        embed_model: run.identity.to_string(),
        embed_dims: run.identity.dims,
        embed_context_length: opts.embed_context_length,
        embed_batch_size: EMBED_BATCH_SIZE,
        embed_batch_attempts: EMBED_BATCH_ATTEMPTS,
        chunk_tokens: opts.chunk_tokens,
        token_estimator: run.tc.name(),
        chunker_version: CHUNKER_VERSION.to_string(),
        pdf_extract_timeout_ms: opts.pdf_timeout.as_millis() as u64,
        racy_mtime_window_ms: RACY_MTIME_WINDOW.as_millis() as u64,
        vision_model: opts.vision.as_ref().map(|v| v.model.clone()),
        vision_every_page: opts.vision.as_ref().is_some_and(|v| v.every_page),
        vision_dpi: VISION_DPI,
        vision_prompt_version: VISION_PROMPT_VERSION.to_string(),
        vision_rule: vision::rule(),
        ..SyncReport::default()
    };
    match run.go().await {
        Ok(()) => Ok(run.report),
        Err(Abort { kind, reason }) => {
            // Best effort: the count is a cache, and failing to refresh it
            // must not replace the reason the sync stopped. Never when the
            // index is not ours alone any more: that stop writes nothing.
            if run.corpus_id != 0 && kind != AbortKind::IndexContainment {
                if let Ok(n) = store::refresh_chunk_count(index.pool(), run.corpus_id).await {
                    run.report.chunks = n.max(0) as usize;
                }
            }
            run.finish_report();
            run.emit(SyncEvent::Aborted {
                kind,
                reason: reason.clone(),
            });
            Err(SyncError::Aborted {
                kind,
                reason,
                partial: Box::new(run.report),
            })
        }
    }
}

struct Abort {
    kind: AbortKind,
    reason: String,
}

impl Abort {
    fn index(e: impl std::fmt::Display) -> Self {
        Self {
            kind: AbortKind::Index,
            reason: format!("the index could not be updated: {e}"),
        }
    }
}

impl From<quickdoc_core::QuickdocError> for Abort {
    fn from(e: quickdoc_core::QuickdocError) -> Self {
        Self::index(e)
    }
}

impl From<IndexError> for Abort {
    fn from(e: IndexError) -> Self {
        match e {
            IndexError::Containment(c) => Self {
                kind: AbortKind::IndexContainment,
                reason: c.to_string(),
            },
            e => Self::index(e),
        }
    }
}

impl From<crate::index::ContainmentError> for Abort {
    fn from(e: crate::index::ContainmentError) -> Self {
        Self {
            kind: AbortKind::IndexContainment,
            reason: e.to_string(),
        }
    }
}

/// What an index from before a fact was recorded lacks for one file; the
/// sync re-chunks such a file once, keeping every vector that is still right.
#[derive(Debug, Clone, Copy, Default)]
struct Backfill {
    /// Its chunks' line numbers.
    lines: bool,
    /// A PDF's pages without text.
    pdf_pages: bool,
    /// A PDF's page readings: it was indexed with other vision settings, or
    /// its last pass did not finish (a stop, or a page to try again without
    /// `pdftoppm`). A cached failure does not force it.
    vision: bool,
}

impl Backfill {
    fn any(self) -> bool {
        self.lines || self.pdf_pages || self.vision
    }
}

/// Why one page reading gave no reading.
enum PageFail {
    /// A hold, or an answer through a hold fallback: the sync stops
    /// ([`AbortKind::GpuHold`]).
    Hold(GatewayError),
    /// A fault that is not about this page (a server error, an unreachable
    /// gateway, an answer in no known shape, a refusal the probe image met
    /// too): the sync stops ([`AbortKind::Vision`]), for the reason given.
    Stop(String),
    /// This page, and it would fail the same way again: the render failed or
    /// timed out, the model refused this input (`400`, `413`, `422`) and not
    /// the probe, or its answer was empty or cut off. Cached as failed; the
    /// file goes on.
    Cached(String),
    /// This page, for now: `pdftoppm` is not installed. Not cached; the
    /// document stays pending, so the next sync tries again.
    Uncached(String),
}

/// How one file ended, for the caller's bookkeeping.
enum FileOutcome {
    Done,
    /// Content as indexed; re-chunked only to record what it lacked.
    Backfilled(Backfill),
    /// Nothing to do after all (identical content, a skip, a per-file error).
    Settled,
}

/// Why one embedding call did not give usable vectors.
enum Failure {
    Gateway(GatewayError),
    /// Vectors came back, but not the right number, width, or non-zero.
    Invalid(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gateway(g) => g.fmt(f),
            Self::Invalid(m) => f.write_str(m),
        }
    }
}

/// How a batch that could not be embedded ends the file (or the sync).
enum BatchFail {
    Hold(GatewayError),
    /// This file's text; the probe still works.
    File(String),
    /// The gateway: the probe failed too.
    Gateway(String),
}

/// What reading one candidate gave.
struct ReadOk {
    hash: String,
    size: u64,
    /// 0 when racy.
    mtime_ns: i64,
    racy: bool,
    content: Content,
}

enum Content {
    Text(String),
    NotUtf8 { at: usize },
    Pdf(Vec<u8>),
}

/// Read one candidate through the descriptor walk, hash it, and decode it —
/// blocking; called from `spawn_blocking`. The hash of a PDF also covers the
/// `pdftotext` version that will extract it ([`source::content_hash`]), so
/// text extracted by another version is never taken for current.
fn read_candidate(
    root: &Path,
    c: &Candidate,
    max_bytes: Option<u64>,
    pdf_version: Option<&str>,
) -> Result<ReadOk, ReadError> {
    let parts = source::check_path(&c.rel)?;
    let got = source::read_beneath(root, &c.rel, &parts, max_bytes)?;
    let read_at = SystemTime::now();
    let hash = source::content_hash(&got.bytes, c.kind, pdf_version);
    let before = scan::mtime_ns(&got.before);
    let after = scan::mtime_ns(&got.after);
    let size = got.bytes.len() as u64;
    // Changed since the scan, or while being read.
    let moved = got.before.len() != c.size
        || before != c.mtime_ns
        || got.after.len() != got.before.len()
        || after != before
        || size != got.after.len();
    // Modified within the window of this read (or in the future).
    let recent = got
        .after
        .modified()
        .map_or(true, |m| match read_at.duration_since(m) {
            Ok(age) => age < RACY_MTIME_WINDOW,
            Err(_) => true,
        });
    let racy = moved || recent || after == 0;
    let content = match c.kind {
        FileKind::Pdf => Content::Pdf(got.bytes),
        _ => match String::from_utf8(got.bytes) {
            Ok(t) => Content::Text(t),
            Err(e) => Content::NotUtf8 {
                at: e.utf8_error().valid_up_to(),
            },
        },
    };
    Ok(ReadOk {
        hash,
        size,
        mtime_ns: if racy { 0 } else { after },
        racy,
        content,
    })
}

/// A file's text, cut: the text (for a PDF, with its readings appended), its
/// chunks, each chunk's lines and id, and for a PDF its page count and pages
/// without text.
struct Cut {
    text: String,
    spans: Vec<ChunkSpan>,
    lines: Vec<(usize, usize)>,
    ids: Vec<String>,
    pdf_pages: Option<(u32, Vec<u32>)>,
}

/// Chunk `text` and work out every chunk's lines and content-derived id —
/// linear-ish CPU work, called from `spawn_blocking`.
///
/// A PDF's `readings` are appended to its text ([`vision::append_readings`])
/// after its own chunks and pages are cut from the `pdftotext` part, so those
/// spans and page numbers are what they are without a vision model. Each
/// reading is cut with [`chunk::chunk_plain`] on its own, its spans offset
/// into the combined text and headed [`vision::heading`]; every chunk's lines
/// count in the combined text.
fn cut(
    rel: &str,
    kind: FileKind,
    mut text: String,
    readings: &[Reading],
    max: usize,
    tc: &dyn TokenCounter,
) -> Cut {
    let mut spans = match kind {
        FileKind::Markdown => chunk::chunk_markdown(&text, max, tc),
        FileKind::Pdf => chunk::chunk_pdf(&text, max, tc),
        FileKind::Text | FileKind::Code => chunk::chunk_plain(&text, max, tc),
    };
    let pdf_pages = (kind == FileKind::Pdf).then(|| chunk::pdf_pages(&text));
    // A reading piece identical to a chunk of the file's own text adds
    // nothing — and, one content-derived id being one row, it would take that
    // chunk's place under the model's name. The file's own text wins.
    let own: HashSet<String> = if readings.is_empty() {
        HashSet::new()
    } else {
        spans
            .iter()
            .map(|s| store::chunk_id(rel, s.text(&text)))
            .collect()
    };
    for r in vision::append_readings(&mut text, readings) {
        let piece = &text[r.start..r.end];
        let pieces: Vec<ChunkSpan> = chunk::chunk_plain(piece, max, tc)
            .into_iter()
            .map(|s| ChunkSpan {
                heading_path: r.heading.clone(),
                start: r.start + s.start,
                end: r.start + s.end,
            })
            .filter(|s| !own.contains(&store::chunk_id(rel, s.text(&text))))
            .collect();
        spans.extend(pieces);
    }
    let lines = chunk::line_ranges(&text, &spans);
    let ids = spans
        .iter()
        .map(|s| store::chunk_id(rel, s.text(&text)))
        .collect();
    Cut {
        text,
        spans,
        lines,
        ids,
        pdf_pages,
    }
}

struct Run<'a> {
    index: &'a IndexDir,
    embedder: Arc<dyn Embedder>,
    identity: EmbedIdentity,
    /// How the embedder is named in an error.
    endpoint: String,
    opts: &'a SyncOptions,
    events: &'a Events,
    tc: Arc<dyn TokenCounter>,
    started: Instant,
    report: SyncReport,
    corpus_id: i64,
    source_id: i64,
    pdf_tool_missing: usize,
    /// [`LARGE_FILE_MEMORY_FRACTION`] of the memory limit, when there is one.
    large_limit: Option<u64>,
    memory_source: PathBuf,
    pdf_version: Option<String>,
    /// Pages that could not be rendered because `pdftoppm` is not there.
    pdftoppm_missing: usize,
    /// Pages the vision model read in this sync, by mode.
    vision_read_ocr: usize,
    vision_read_structure: usize,
    /// Pages that failed in this sync, and cached failures met again.
    vision_failed_now: usize,
    vision_failed_cached: usize,
    /// [`vision::settings`] of this sync.
    vision_settings: String,
    /// The other settings PDFs taken through the refresh pass were indexed
    /// with.
    vision_had: BTreeSet<String>,
    /// The content hashes whose pages this sync looked up or read: kept by
    /// the final prune, whatever became of their file.
    vision_hashes_used: HashSet<String>,
    /// The refusals — status, and the refused image's width and height — a
    /// probe showed to be about a page, not the model: probed once per sync
    /// each ([`vision::probe_png`]).
    vision_probed: HashSet<(u16, u32, u32)>,
}

impl Run<'_> {
    fn emit(&self, ev: SyncEvent) {
        // A reader that went away is not the sync's failure.
        let _ = self.events.send(ev);
    }

    fn pool(&self) -> &sqlx::SqlitePool {
        self.index.pool()
    }

    fn finish_report(&mut self) {
        self.report.skipped.sort_by(|a, b| a.path.cmp(&b.path));
        self.report.skipped_by_reason = scan::count_reasons(&self.report.skipped);
        scan::add_ignored(
            &mut self.report.skipped_by_reason,
            self.report.ignored_files + self.report.ignored_dirs.len(),
        );
        if self.pdf_tool_missing > 0 {
            self.report.notes.push(format!(
                "pdftotext is not installed (poppler-utils), so {} PDF file(s) could not be \
                 indexed",
                self.pdf_tool_missing
            ));
        }
        if self.report.racy_files > 0 {
            self.report.notes.push(format!(
                "{} file(s) changed while they were read, or were modified within \
                 RACY_MTIME_WINDOW ({} s) of it; their content is checked again on the next sync",
                self.report.racy_files,
                RACY_MTIME_WINDOW.as_secs()
            ));
        }
        if self.pdftoppm_missing > 0 {
            self.report.notes.push(format!(
                "pdftoppm is not installed (poppler-utils), so {} PDF page(s) could not be \
                 rendered for the vision model; nothing is cached for them, and the next sync \
                 tries again — install poppler-utils where the agent runs",
                self.pdftoppm_missing
            ));
        }
        if let Some(model) = &self.report.vision_model {
            let (read, reused) = (
                self.report.vision_pages_read,
                self.report.vision_pages_reused,
            );
            let (failed, cached) = (self.vision_failed_now, self.vision_failed_cached);
            if read + reused + failed + cached > 0 {
                self.report.notes.push(format!(
                    "the vision model {model} read {read} PDF page(s) in this sync — {} without \
                     text, from the page image alone, and {} with text, from the image and the \
                     page's text; {reused} cached reading(s) were reused, {failed} page(s) failed \
                     in this sync, and {cached} cached failure(s) were not tried again",
                    self.vision_read_ocr, self.vision_read_structure,
                ));
            }
        }
        if !self.report.vision_pages_failed.is_empty() {
            let list: Vec<String> = self
                .report
                .vision_pages_failed
                .iter()
                .map(|f| format!("{} page {} ({}: {})", f.path, f.page, f.model, f.reason))
                .collect();
            self.report.notes.push(format!(
                "{} PDF page(s) could not be read by the vision model, and each stays unread \
                 until the file, the vision alias or the prompt changes — or, once what failed \
                 is fixed, Retry failed pages on the Sync card is pressed: {}",
                list.len(),
                list.join("; ")
            ));
        }
        if self.report.vision_refreshed_files > 0 {
            let had: Vec<String> = self.vision_had.iter().map(|h| format!("'{h}'")).collect();
            self.report.notes.push(format!(
                "{} PDF(s) were taken through their page readings again — indexed with other \
                 vision settings ({}), or a pass over them had stopped part-way — for the \
                 current ones, '{}'; cached readings that still apply and chunks already \
                 embedded were kept",
                self.report.vision_refreshed_files,
                if had.is_empty() {
                    "none".to_string()
                } else {
                    had.join(", ")
                },
                self.vision_settings
            ));
        }
        if self.report.pdf_pages_recorded_files > 0 {
            self.report.notes.push(format!(
                "{} PDF(s) had no record of their pages without text (an index from before it \
                 was kept) and were extracted again once to record it; chunks already embedded \
                 were kept",
                self.report.pdf_pages_recorded_files
            ));
        }
        if !self.report.pdf_pages_without_text.is_empty() {
            let pages: usize = self
                .report
                .pdf_pages_without_text
                .iter()
                .map(|t| t.textless.len())
                .sum();
            let files: Vec<String> = self
                .report
                .pdf_pages_without_text
                .iter()
                .map(|t| {
                    let list: Vec<String> = t.textless.iter().map(u32::to_string).collect();
                    format!("{} (page {} of {})", t.path, list.join(", "), t.pages)
                })
                .collect();
            let remedy = match &self.report.vision_model {
                None => "set a vision model (vision_model) to have such pages read from their \
                         image"
                    .to_string(),
                Some(m) => format!(
                    "the vision model {m} could not read them (the failed pages are listed \
                     above, or pdftoppm is missing)"
                ),
            };
            self.report.notes.push(format!(
                "{pages} PDF page(s) in {} file(s) have no text to extract — typically a scan \
                 without an OCR layer — and are not in the index; the rest of each file is: {}; \
                 {remedy}",
                files.len(),
                files.join("; ")
            ));
        }
        if self.report.lines_recorded_files > 0 {
            self.report.notes.push(format!(
                "{} file(s) had no stored line numbers (an index from before they were \
                 recorded) and were re-chunked once to record them; chunks already embedded \
                 were kept",
                self.report.lines_recorded_files
            ));
        }
        self.report.elapsed_ms = self.started.elapsed().as_millis() as u64;
    }

    async fn go(&mut self) -> Result<(), Abort> {
        match self.opts.embed_context_length {
            Some(ctx) if self.opts.chunk_tokens as u64 > ctx => {
                return Err(Abort {
                    kind: AbortKind::ChunkTooLarge,
                    reason: format!(
                        "chunk_tokens is {}, but the embedding model {} reads at most {ctx} \
                         tokens; lower chunk_tokens to {ctx} or less, or pick an embedding \
                         model with a larger context",
                        self.opts.chunk_tokens, self.identity.model
                    ),
                });
            }
            Some(_) => {}
            None => self.report.notes.push(format!(
                "the embedding model {} reports no context_length, so chunk_tokens ({}) could \
                 not be checked against it; an over-long chunk fails that file's embedding",
                self.identity.model, self.opts.chunk_tokens
            )),
        }
        if !self.index.folder().is_dir() {
            return Err(Abort {
                kind: AbortKind::Folder,
                reason: format!("the folder {} is not there", self.index.folder().display()),
            });
        }
        // Nothing has been written yet, and nothing will be unless the index
        // directory is still the agent's alone.
        self.index.check_containment()?;

        self.memory_source = self.opts.memory_max_path.clone();
        match read_memory_limit(&self.opts.memory_max_path) {
            MemoryLimit::Bytes(b) => {
                self.report.memory_limit_bytes = Some(b);
                self.large_limit = Some((b as f64 * LARGE_FILE_MEMORY_FRACTION) as u64);
                self.report.large_file_limit_bytes = self.large_limit;
            }
            MemoryLimit::Unlimited(why) => self.report.notes.push(format!(
                "no container memory limit applies ({why}), so no file is skipped for its size \
                 (LARGE_FILE_MEMORY_FRACTION applies only to a real limit)"
            )),
        }

        let probe = self.probe_vector().await?;
        self.pin_corpus(&probe).await?;

        let root = self.index.folder().to_path_buf();
        let scanned = tokio::task::spawn_blocking(move || scan::scan(&root))
            .await
            .map_err(|e| Abort {
                kind: AbortKind::Folder,
                reason: format!("the folder scan stopped: {e}"),
            })?
            .map_err(|e| Abort {
                kind: AbortKind::Folder,
                reason: format!(
                    "the folder {} cannot be listed: {e}",
                    self.index.folder().display()
                ),
            })?;
        self.report.found = scanned.files.len();
        self.report.skipped = scanned.skipped.clone();
        self.report.ignored_files = scanned.ignored_files;
        self.report.ignored_dirs = scanned.ignored_dirs.clone();
        self.report.ignore_files = scanned.ignore_files.clone();
        self.report.notes.extend(scanned.notes.iter().cloned());
        self.emit(SyncEvent::Scanning {
            found: scanned.files.len(),
        });

        // ---- the pdftotext that will extract, against the one that did ----
        let mut pdf_recheck = false;
        if scanned.files.iter().any(|c| c.kind == FileKind::Pdf) {
            self.pdf_version = pdf::version(&self.opts.pdftotext).await;
            self.report.pdftotext_version = self.pdf_version.clone();
            let had = self.index.meta(META_PDFTOTEXT_VERSION).await?;
            if let Some(now) = &self.pdf_version {
                if had.as_deref() != Some(now.as_str()) {
                    pdf_recheck = true;
                    if let Some(had) = had {
                        self.report.notes.push(format!(
                            "pdftotext changed from '{had}' to '{now}': every PDF is extracted \
                             again (its text and line numbers may differ); nothing else is \
                             re-chunked, and chunks that come out the same keep their vectors"
                        ));
                    }
                }
            }
        }

        // ---- the owner's Retry failed pages, before anything is planned ----
        if self.opts.retry_failed {
            let (cleared, marked) = self.index.retry_failed_readings().await?;
            self.report.vision_failures_cleared = cleared;
            self.report.notes.push(format!(
                "Retry failed pages: {cleared} cached page failure(s) were forgotten and \
                 {marked} PDF(s) taken through again, so those pages are read again"
            ));
        }

        // ---- how each PDF was read, against how it would be read now ----
        let doc_vision = self.index.doc_vision(self.corpus_id).await?;

        // ---- plan ----
        let docs: HashMap<String, Document> = store::list_documents(self.pool(), self.corpus_id)
            .await?
            .into_iter()
            .map(|d| (d.url.clone(), d))
            .collect();
        let states = self.index.file_states(self.corpus_id).await?;
        let lines_missing = self.index.documents_missing_lines(self.corpus_id).await?;
        let pdf_pages_known = self.index.documents_with_pdf_pages(self.corpus_id).await?;
        let mut todo: Vec<(&Candidate, Option<&Document>, Backfill)> = Vec::new();
        let (mut new, mut changed) = (0usize, 0usize);
        for c in &scanned.files {
            match docs.get(&c.rel) {
                Some(d) => {
                    let state = states.get(&d.id);
                    let backfill = Backfill {
                        lines: lines_missing.contains(&d.id),
                        // Only with a pdftotext to record them with: without
                        // one the PDF stays as indexed, and quiet.
                        pdf_pages: c.kind == FileKind::Pdf
                            && self.pdf_version.is_some()
                            && !pdf_pages_known.contains(&d.id),
                        // Likewise: without a pdftotext nothing can be read.
                        // Other settings than its record says, or a pass
                        // over it that stopped: taken through again. No
                        // record is a PDF from before vision, read by none.
                        vision: c.kind == FileKind::Pdf
                            && self.pdf_version.is_some()
                            && match doc_vision.get(&d.id) {
                                Some(v) => v.pending || v.settings != self.vision_settings,
                                None => self.vision_settings != vision::SETTINGS_OFF,
                            },
                    };
                    if backfill.vision {
                        if let Some(v) = doc_vision.get(&d.id) {
                            if v.settings != self.vision_settings {
                                self.vision_had.insert(v.settings.clone());
                            }
                        }
                    }
                    let forced = backfill.any() || (c.kind == FileKind::Pdf && pdf_recheck);
                    let current = !forced
                        && state.is_some_and(|s| {
                            s.size == c.size
                                && s.mtime_ns == c.mtime_ns
                                // A recorded 0 is "not trusted", never a match.
                                && s.mtime_ns != 0
                                && !d.content_hash.is_empty()
                                && s.indexed_hash == d.content_hash
                        });
                    if current {
                        self.report.unchanged += 1;
                    } else {
                        if state.is_some() {
                            changed += 1;
                        } else {
                            new += 1;
                        }
                        todo.push((c, Some(d), backfill));
                    }
                }
                None => {
                    new += 1;
                    todo.push((c, None, Backfill::default()));
                }
            }
        }
        let present: HashSet<&str> = scanned.files.iter().map(|c| c.rel.as_str()).collect();
        let mut removed: Vec<&Document> = docs
            .values()
            .filter(|d| !present.contains(d.url.as_str()))
            .collect();
        removed.sort_by(|a, b| a.url.cmp(&b.url));
        self.emit(SyncEvent::Planned {
            new,
            changed,
            removed: removed.len(),
            unchanged: self.report.unchanged,
            skipped_by_reason: scanned.skipped_by_reason(),
            ignored_dirs: scanned.ignored_dirs.clone(),
        });

        // ---- removals first: they are cheap, and free the paths ----
        self.index.check_containment()?;
        // Their cached page readings stay until the end of the sync: a file
        // that moved is removed here and found under its new path below.
        for d in removed {
            store::delete_document(self.pool(), d.id).await?;
            self.report.removed_files += 1;
            self.emit(SyncEvent::Removed {
                file: d.url.clone(),
            });
        }

        // ---- new and changed files ----
        for (c, existing, backfill) in todo {
            self.index.check_containment()?;
            let state = existing.and_then(|d| states.get(&d.id));
            match self.file(c, existing, state, backfill).await? {
                FileOutcome::Backfilled(b) => {
                    self.report.lines_recorded_files += usize::from(b.lines);
                    self.report.pdf_pages_recorded_files += usize::from(b.pdf_pages);
                    self.report.vision_refreshed_files += usize::from(b.vision);
                }
                FileOutcome::Done => {
                    if state.is_some() {
                        self.report.changed_files += 1;
                    } else {
                        self.report.new_files += 1;
                    }
                }
                FileOutcome::Settled => {}
            }
        }

        let chunks = store::refresh_chunk_count(self.pool(), self.corpus_id).await?;
        store::set_corpus_status(self.pool(), self.corpus_id, "ready").await?;
        self.report.chunks = chunks.max(0) as usize;
        self.report.documents = store::list_documents(self.pool(), self.corpus_id)
            .await?
            .len();
        // Pages without text, less the ones a reading in the indexed text
        // covers — both from the index, so every sync reports them.
        let read = self.index.read_pages(self.corpus_id).await?;
        self.report.pdf_pages_without_text = self
            .index
            .textless_pdf_pages(self.corpus_id)
            .await?
            .into_iter()
            .filter_map(|(path, pages, textless)| {
                let textless: Vec<u32> = textless
                    .into_iter()
                    .filter(|p| !read.get(&path).is_some_and(|r| r.contains(p)))
                    .collect();
                (!textless.is_empty()).then_some(TextlessPages {
                    path,
                    pages,
                    textless,
                })
            })
            .collect();
        self.report.vision_pages_failed = self
            .index
            .failed_pages(self.corpus_id)
            .await?
            .into_iter()
            .map(|f| VisionFailure {
                path: f.path,
                page: f.page,
                model: f.model,
                reason: f.reason,
            })
            .collect();
        // Every file the folder holds has been through this sync, so a
        // cached reading no document refers to belongs to no file here —
        // removed ones included — unless this sync read pages of it for a
        // file that was not indexed after all.
        self.report.vision_readings_pruned =
            self.index.prune_readings(&self.vision_hashes_used).await?;
        // Every PDF has now been through this pdftotext (or failed and is
        // not current, so it is retried anyway).
        if let Some(v) = &self.pdf_version {
            self.index.set_meta(META_PDFTOTEXT_VERSION, v).await?;
        }
        self.finish_report();
        self.emit(SyncEvent::Done {
            report: Box::new(self.report.clone()),
        });
        Ok(())
    }

    /// One embedding call, with quickdoc's checks on what came back.
    async fn embed_checked(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, Failure> {
        let got = self
            .embedder
            .embed(inputs)
            .await
            .map_err(|e| Failure::Gateway(classify_quickdoc_error(&self.endpoint, &e)))?;
        if got.len() != inputs.len() {
            return Err(Failure::Invalid(format!(
                "{} returned {} vectors for {} texts",
                self.endpoint,
                got.len(),
                inputs.len()
            )));
        }
        for (j, v) in got.iter().enumerate() {
            validate_vector(&self.identity.model, j, v, self.identity.dims)
                .map_err(|e| Failure::Invalid(e.to_string()))?;
        }
        Ok(got)
    }

    async fn embed_probe(&mut self) -> Result<Vec<f32>, Failure> {
        self.report.embed_requests += 1;
        let mut v = self.embed_checked(&[EMBED_PROBE_TEXT.to_string()]).await?;
        Ok(v.remove(0))
    }

    /// The probe vector: the one the embedder answered when it was connected,
    /// or one embedded now.
    async fn probe_vector(&mut self) -> Result<Vec<f32>, Abort> {
        if let Some(p) = &self.opts.probe {
            return Ok(p.clone());
        }
        match self.embed_probe().await {
            Ok(v) => Ok(v),
            Err(Failure::Gateway(g)) if g.is_gpu_hold() => Err(Abort {
                kind: AbortKind::GpuHold,
                reason: format!("{}; nothing was synced", g.hold_reason()),
            }),
            Err(f) => Err(Abort {
                kind: AbortKind::Embedder,
                reason: format!("the {} could not embed a probe: {f}", self.endpoint),
            }),
        }
    }

    /// Embed one batch: [`EMBED_BATCH_ATTEMPTS`] tries, then a probe to tell
    /// a file the model cannot embed from a gateway that is failing.
    async fn embed_batch(
        &mut self,
        rel: &str,
        inputs: &[String],
    ) -> Result<Vec<Vec<f32>>, BatchFail> {
        let mut last = String::new();
        for _ in 0..EMBED_BATCH_ATTEMPTS {
            self.report.embed_requests += 1;
            match self.embed_checked(inputs).await {
                Ok(v) => return Ok(v),
                Err(Failure::Gateway(g)) if g.is_gpu_hold() => return Err(BatchFail::Hold(g)),
                Err(f) => last = f.to_string(),
            }
        }
        match self.embed_probe().await {
            Ok(_) => Err(BatchFail::File(format!(
                "embedding failed on all {EMBED_BATCH_ATTEMPTS} attempts (EMBED_BATCH_ATTEMPTS): \
                 {last}; the embedding model still answers a probe, so it is this file's text \
                 it cannot embed — the file is not indexed (what was stored of it before is \
                 kept) and is tried again on the next sync"
            ))),
            Err(Failure::Gateway(g)) if g.is_gpu_hold() => Err(BatchFail::Hold(g)),
            Err(p) => Err(BatchFail::Gateway(format!(
                "embedding stopped at {rel}: {last} (on all {EMBED_BATCH_ATTEMPTS} attempts, \
                 EMBED_BATCH_ATTEMPTS); a probe embedding failed too ({p}), so the {} itself \
                 is failing",
                self.endpoint
            ))),
        }
    }

    /// Find or create `folder@1`, rebuilding it when the embedding model, its
    /// fingerprint, or the chunking differs from what it was built with.
    async fn pin_corpus(&mut self, probe: &[f32]) -> Result<(), Abort> {
        let corpus_key = format!("{CORPUS_LIBRARY}@{CORPUS_VERSION}");
        let existing = store::get_corpus_by_id(self.pool(), &corpus_key).await?;
        let want_tokens = self.opts.chunk_tokens.to_string();
        let had_probe = stored_probe(self.index).await?;
        let mut reason = None;
        if let Some(c) = &existing {
            let pinned = c.embed_identity();
            let had_tokens = self.index.meta(META_CHUNK_TOKENS).await?;
            let had_chunker = self.index.meta(META_CHUNKER_VERSION).await?;
            let cos = had_probe.as_ref().map(|p| cosine(&p.vector, probe));
            if let Some(Some(c)) = cos {
                self.report.embed_probe_cosine = Some(c);
            }
            reason = if pinned != self.identity {
                Some(format!(
                    "the embedding model changed from {pinned} to {}; every file is re-embedded",
                    self.identity
                ))
            } else if let Some(c) =
                cos.filter(|c| !matches!(c, Some(c) if *c >= PROBE_SAME_MODEL_MIN_COSINE))
            {
                Some(format!(
                    "the model behind the embedding alias {} is not the one the index was built \
                     with: the probe text now embeds at cosine {} to what it did then (the same \
                     model gives at least PROBE_SAME_MODEL_MIN_COSINE, \
                     {PROBE_SAME_MODEL_MIN_COSINE}); every file is re-embedded",
                    self.identity.model,
                    c.map_or("(not comparable)".to_string(), |c| format!("{c:.3}"))
                ))
            } else if had_tokens.as_deref() != Some(want_tokens.as_str()) {
                Some(format!(
                    "chunk_tokens changed from {} to {want_tokens}; every file is re-chunked and \
                     re-embedded",
                    had_tokens.as_deref().unwrap_or("(not recorded)")
                ))
            } else if had_chunker.as_deref() != Some(CHUNKER_VERSION) {
                Some(format!(
                    "the chunking rules changed (version {} to {CHUNKER_VERSION}); every file is \
                     re-chunked and re-embedded",
                    had_chunker.as_deref().unwrap_or("(not recorded)")
                ))
            } else {
                None
            };
            if reason.is_some() {
                // Cascades through source → document → chunk (and the FTS
                // triggers, and our `folder_chat_file` and line rows).
                store::delete_corpus(self.pool(), c.id).await?;
            }
        }
        let fresh = !matches!((&existing, &reason), (Some(_), None));
        if fresh {
            let mut nc = NewCorpus::new(CORPUS_LIBRARY, CORPUS_VERSION, self.identity.clone());
            nc.source_kind = "folder".into();
            self.corpus_id = store::insert_corpus(self.pool(), &nc).await?;
            // Recorded with the corpus, not at the end of the sync, so an
            // aborted first sync still knows how its chunks were cut.
            self.index.set_meta(META_CHUNK_TOKENS, &want_tokens).await?;
            self.index
                .set_meta(META_CHUNKER_VERSION, CHUNKER_VERSION)
                .await?;
        } else if let Some(c) = &existing {
            self.corpus_id = c.id;
        }
        // The fingerprint is the model's when the corpus was built: recorded
        // with a fresh corpus, or the first time an older index sees one —
        // never overwritten by a nearly-equal vector, so small drifts cannot
        // add up.
        if fresh || had_probe.is_none() {
            let p = StoredProbe {
                text: EMBED_PROBE_TEXT.to_string(),
                vector: probe.to_vec(),
            };
            let json = serde_json::to_string(&p).map_err(Abort::index)?;
            self.index.set_meta(META_EMBED_PROBE, &json).await?;
        }
        if let Some(r) = reason {
            self.report.index_reset = Some(r.clone());
            self.emit(SyncEvent::IndexReset { reason: r });
        }
        self.source_id = match store::list_sources(self.pool(), self.corpus_id)
            .await?
            .first()
        {
            Some(s) => s.id,
            None => {
                store::insert_source(
                    self.pool(),
                    self.corpus_id,
                    &self.index.folder().display().to_string(),
                    SOURCE_KIND,
                    &[],
                )
                .await?
            }
        };
        Ok(())
    }

    fn skip(&mut self, file: &str, reason: SkipReason, detail: Option<String>) {
        self.report.skipped.push(Skipped {
            path: file.to_string(),
            reason,
            detail: detail.clone(),
        });
        self.emit(SyncEvent::Skipped {
            file: file.to_string(),
            reason,
            detail,
        });
    }

    fn file_error(&mut self, file: &str, message: String) {
        self.report.errors.push(FileError {
            file: file.to_string(),
            message: message.clone(),
        });
        self.emit(SyncEvent::Error {
            file: file.to_string(),
            message,
        });
    }

    /// A file that was in the index and can no longer be indexed leaves it.
    async fn drop_from_index(
        &mut self,
        file: &str,
        doc_id: i64,
        was_indexed: bool,
    ) -> Result<(), Abort> {
        store::delete_document(self.pool(), doc_id).await?;
        if was_indexed {
            self.report.removed_files += 1;
            self.emit(SyncEvent::Removed {
                file: file.to_string(),
            });
        }
        Ok(())
    }

    /// A file whose indexing stopped part-way. Its document row stays, with
    /// whatever chunks it has (earlier ones, and this sync's stored batches),
    /// so the next sync resumes it — unless the row is this sync's own and
    /// holds nothing, which would only be an empty document in the index.
    /// Its marker is untouched either way: it is not current. (Page readings
    /// are cached by content, not by document, so none goes with the row.)
    async fn unfinished(&self, doc_id: i64, had_row: bool, has_chunks: bool) -> Result<(), Abort> {
        if !had_row && !has_chunks {
            store::delete_document(self.pool(), doc_id).await?;
        }
        Ok(())
    }

    /// Render one page and have the vision model read it: the reading as it
    /// is stored ([`vision::usable`]), or why there is none, sorted by what
    /// it means for the sync ([`PageFail`]).
    async fn read_page(
        &mut self,
        v: &VisionOptions,
        bytes: Arc<Vec<u8>>,
        page: u32,
        mode: Mode,
        page_text: &str,
    ) -> Result<String, PageFail> {
        let png = match pdf::render_page(
            &self.opts.pdftoppm,
            bytes,
            page,
            VISION_DPI,
            self.opts.pdf_timeout,
        )
        .await
        {
            Ok(png) => png,
            Err(e @ RenderError::ToolMissing) => {
                self.pdftoppm_missing += 1;
                return Err(PageFail::Uncached(e.to_string()));
            }
            // This page does not render, or not in time: it would not the
            // next time either.
            Err(e) => return Err(PageFail::Cached(e.to_string())),
        };
        let prompt = vision::prompt(mode, page_text);
        let not_the_page = |g: &GatewayError| {
            format!(
                "the vision model {} failed in a way that is not about this page ({g}), so every \
                 page would fail the same way; see that the model runs in lmgw, then sync again",
                v.model
            )
        };
        let g = match v.gateway.read_page(&v.model, &png, &prompt).await {
            Ok(answer) => return vision::usable(&answer).map_err(PageFail::Cached),
            Err(g) if g.is_gpu_hold() => return Err(PageFail::Hold(g)),
            Err(g) if g.is_per_input() => g,
            // Anything else is not about the page, or not clearly: stop
            // rather than fail every page the same way.
            Err(g) => return Err(PageFail::Stop(not_the_page(&g))),
        };
        // Refused as an input. This page — or every image of its size, as a
        // model without a projector, or with too small a context for one,
        // refuses them? A blank probe image of the page's own size tells,
        // once per sync for each status and size, the way the embedding
        // batches probe to tell a file's fault from the gateway's.
        let status = g.status().unwrap_or_default();
        let (w, h) = vision::png_size(&png).unwrap_or((vision::PROBE_SIZE, vision::PROBE_SIZE));
        if self.vision_probed.contains(&(status, w, h)) {
            return Err(PageFail::Cached(g.to_string()));
        }
        self.report.vision_probes += 1;
        match v
            .gateway
            .read_page(&v.model, &vision::probe_png(w, h), vision::PROBE_PROMPT)
            .await
        {
            Ok(_) => {
                self.vision_probed.insert((status, w, h));
                Err(PageFail::Cached(format!(
                    "{g} (a blank {w}×{h} probe image, the page's size, was read, so it is this \
                     page the model refuses)"
                )))
            }
            Err(p) if p.is_gpu_hold() => Err(PageFail::Hold(p)),
            Err(p) => Err(PageFail::Stop(format!(
                "the vision model {} refused this page ({g}) and a blank {w}×{h} probe image of \
                 the same size as well ({p}), so it refuses such images, not this page — it may \
                 have no image projector, or too small a context or batch for one page image; \
                 fix it in lmgw, then sync again",
                v.model
            ))),
        }
    }

    /// A page the vision model could not read in this sync: in the report's
    /// `errors`, naming the page, the alias, why, and when it is tried again,
    /// and as a [`SyncEvent::ReadingFailed`]. A cached failure also lands in
    /// `vision_pages_failed`, from the index, at the end of the sync.
    fn page_failed(&mut self, file: &str, page: u32, model: &str, reason: String, cached: bool) {
        self.vision_failed_now += 1;
        let again = if cached {
            "it is not read again until the file, the vision alias or the prompt changes, or \
             Retry failed pages is pressed on the Sync card"
        } else {
            "the next sync tries it again"
        };
        self.report.errors.push(FileError {
            file: file.to_string(),
            message: format!(
                "page {page} could not be read by the vision model {model}: {reason}; {again}"
            ),
        });
        self.emit(SyncEvent::ReadingFailed {
            file: file.to_string(),
            page,
            model: model.to_string(),
            reason,
            cached,
        });
    }

    /// Say why a file the scan listed could not be read.
    fn read_refused(&self, rel: &str, e: &ReadError) -> (SkipReason, String) {
        match e {
            ReadError::Symlink { .. } => (
                SkipReason::Symlink,
                format!("{e} — the path changed after the folder was scanned"),
            ),
            ReadError::NotRegular(_) | ReadError::IsDir(_) => (
                SkipReason::Unsupported,
                format!("{e} (it changed after the folder was scanned)"),
            ),
            ReadError::TooLarge { size, .. } => (
                SkipReason::TooLargeForMemory,
                too_large_detail(
                    *size,
                    self.report.memory_limit_bytes.unwrap_or_default(),
                    &self.memory_source,
                ),
            ),
            ReadError::NotFound(_) => (
                SkipReason::Unreadable,
                format!("'{rel}' was removed after the folder was scanned"),
            ),
            e => (SkipReason::Unreadable, e.to_string()),
        }
    }

    async fn file(
        &mut self,
        c: &Candidate,
        existing: Option<&Document>,
        state: Option<&FileState>,
        backfill: Backfill,
    ) -> Result<FileOutcome, Abort> {
        let had_row = existing.is_some();
        // ---- read and hash, off the async workers ----
        let root = self.index.folder().to_path_buf();
        let cand = c.clone();
        let max = self.large_limit;
        let version = self.pdf_version.clone();
        let read = tokio::task::spawn_blocking(move || {
            read_candidate(&root, &cand, max, version.as_deref())
        })
        .await
        .map_err(|e| Abort::index(format!("reading {}: {e}", c.rel)))?;
        let got = match read {
            Ok(g) => g,
            Err(e) => {
                if let Some(d) = existing {
                    self.drop_from_index(&c.rel, d.id, true).await?;
                }
                let (reason, detail) = self.read_refused(&c.rel, &e);
                self.skip(&c.rel, reason, Some(detail));
                return Ok(FileOutcome::Settled);
            }
        };
        if got.racy {
            self.report.racy_files += 1;
        }
        // Its cached readings survive this sync's prune whatever becomes of
        // the file below — a timeout, say, on the new path of a moved PDF.
        if c.kind == FileKind::Pdf && self.opts.vision.is_some() {
            self.vision_hashes_used.insert(got.hash.clone());
        }
        let new_state = FileState {
            size: got.size,
            mtime_ns: got.mtime_ns,
            indexed_hash: got.hash.clone(),
        };

        // quickdoc's gate. `changed == false` means the stored hash is this
        // content; it is only *current* if our marker says the chunks built
        // from that hash were all stored — otherwise an earlier sync stopped
        // between the gate and the marker, and the file is finished now.
        let (doc_id, changed) =
            store::upsert_document(self.pool(), self.source_id, &c.rel, &got.hash).await?;
        let same_as_marked = state.is_some_and(|s| s.indexed_hash == got.hash);
        // Read again only to record what an older index lacks: the file is
        // indexed as it is, and nothing in this pass may drop it.
        let backfill_only = backfill.any() && !changed && same_as_marked;
        if !backfill.any() && !changed && same_as_marked {
            self.index.set_file_state(doc_id, &new_state).await?;
            self.report.touched_unchanged += 1;
            self.emit(SyncEvent::Unchanged {
                file: c.rel.clone(),
            });
            return Ok(FileOutcome::Settled);
        }

        // What this document already holds: its chunks (a resumed file keeps
        // them) — and whether it holds anything if it stops part-way.
        let stored = self.index.stored_chunks(doc_id).await?;
        let is_pdf = c.kind == FileKind::Pdf;

        // ---- extract ----
        let (text, pdf_bytes) = match got.content {
            Content::Text(t) => (Ok(t), None),
            Content::NotUtf8 { at } => {
                self.drop_from_index(&c.rel, doc_id, had_row).await?;
                self.skip(
                    &c.rel,
                    SkipReason::NotUtf8,
                    Some(format!("invalid UTF-8 at byte {at}")),
                );
                return Ok(FileOutcome::Settled);
            }
            Content::Pdf(bytes) => {
                let bytes = Arc::new(bytes);
                match pdf::extract_bytes_raw(
                    &self.opts.pdftotext,
                    bytes.clone(),
                    self.opts.pdf_timeout,
                )
                .await
                {
                    // The bytes stay only while a vision model may need a
                    // page image of them.
                    Ok(raw) => (Err(raw), self.opts.vision.is_some().then_some(bytes)),
                    Err(PdfError::ToolMissing) => {
                        self.pdf_tool_missing += 1;
                        self.unfinished(doc_id, had_row, false).await?;
                        self.file_error(&c.rel, PdfError::ToolMissing.to_string());
                        return Ok(FileOutcome::Settled);
                    }
                    Err(e @ PdfError::Timeout { .. }) if backfill_only => {
                        self.file_error(
                            &c.rel,
                            format!(
                                "{e}; the file stays indexed as it was, and the next sync tries \
                                 again to bring it up to date (a line or page record this index \
                                 lacks, or its page readings)"
                            ),
                        );
                        return Ok(FileOutcome::Settled);
                    }
                    Err(e @ PdfError::Timeout { .. }) => {
                        self.drop_from_index(&c.rel, doc_id, had_row).await?;
                        self.skip(&c.rel, SkipReason::PdfTimeout, Some(e.to_string()));
                        return Ok(FileOutcome::Settled);
                    }
                    Err(e) => {
                        self.unfinished(doc_id, had_row, false).await?;
                        self.file_error(&c.rel, e.to_string());
                        return Ok(FileOutcome::Settled);
                    }
                }
            }
        };

        // ---- decode; for a PDF, the pages a vision model reads ----
        let every_page = self.opts.vision.as_ref().map(|v| v.every_page);
        let (text, picks) = tokio::task::spawn_blocking(move || {
            let text = match text {
                Ok(t) => t,
                Err(raw) => pdf::decode(raw),
            };
            let picks = match every_page {
                Some(every) if is_pdf => vision::select(&text, every),
                _ => Vec::new(),
            };
            (text, picks)
        })
        .await
        .map_err(|e| Abort::index(format!("decoding {}: {e}", c.rel)))?;

        // A PDF is pending from here until it is indexed again: its chunks
        // may change from now on, and a pass that stops anywhere must be
        // taken through again, whatever its settings say by then.
        if is_pdf {
            self.index
                .mark_vision_pending(doc_id, vision::SETTINGS_OFF)
                .await?;
        }

        // ---- read those pages: cached readings first, the model for the rest ----
        let mut readings: Vec<Reading> = Vec::new();
        // The key each picked page is read under — the record of what the
        // indexed text holds.
        let mut picked: Vec<PageKey> = Vec::with_capacity(picks.len());
        // A page failed for now (no pdftoppm): nothing is cached, and the
        // document stays pending so the next sync tries again.
        let mut retry_next_sync = false;
        if let (Some(v), Some(bytes)) = (self.opts.vision.clone(), pdf_bytes) {
            let pages = vision::pages(&text);
            let of = u32::try_from(pages.len()).unwrap_or(u32::MAX);
            let cached = self.index.cached_readings(&got.hash).await?;
            for &(page, mode) in &picks {
                let key = PageKey {
                    page,
                    model: v.model.clone(),
                    mode,
                    prompt_version: VISION_PROMPT_VERSION.to_string(),
                    dpi: VISION_DPI,
                };
                picked.push(key.clone());
                match cached.get(&key) {
                    Some(CachedReading {
                        status: VisionStatus::Read,
                        text,
                    }) => {
                        readings.push(Reading {
                            page,
                            model: v.model.clone(),
                            text: text.clone(),
                        });
                        self.report.vision_pages_reused += 1;
                        continue;
                    }
                    // It would fail the same way: reported from the index
                    // by every sync, read again only under another key.
                    Some(CachedReading {
                        status: VisionStatus::Failed,
                        ..
                    }) => {
                        self.vision_failed_cached += 1;
                        continue;
                    }
                    None => {}
                }
                self.emit(SyncEvent::Reading {
                    file: c.rel.clone(),
                    page,
                    of,
                });
                let page_text = pages.get(page as usize - 1).copied().unwrap_or_default();
                let stop = |kind: AbortKind, why: String| Abort {
                    kind,
                    reason: format!(
                        "{why}; stopped at {}, page {page}, whose reading was not stored; the \
                         pages read before it are cached, and the next sync resumes from there",
                        c.rel
                    ),
                };
                match self
                    .read_page(&v, bytes.clone(), page, mode, page_text)
                    .await
                {
                    Ok(reading) => {
                        let row = CachedReading {
                            status: VisionStatus::Read,
                            text: reading,
                        };
                        self.index.cache_reading(&got.hash, &key, &row).await?;
                        self.report.vision_pages_read += 1;
                        match mode {
                            Mode::Ocr => self.vision_read_ocr += 1,
                            Mode::Structure => self.vision_read_structure += 1,
                        }
                        readings.push(Reading {
                            page,
                            model: v.model.clone(),
                            text: row.text,
                        });
                    }
                    Err(PageFail::Cached(reason)) => {
                        let row = CachedReading {
                            status: VisionStatus::Failed,
                            text: reason.clone(),
                        };
                        self.index.cache_reading(&got.hash, &key, &row).await?;
                        self.page_failed(&c.rel, page, &v.model, reason, true);
                    }
                    Err(PageFail::Uncached(reason)) => {
                        retry_next_sync = true;
                        self.page_failed(&c.rel, page, &v.model, reason, false);
                    }
                    Err(PageFail::Hold(g)) => {
                        self.unfinished(doc_id, had_row, !stored.is_empty()).await?;
                        return Err(stop(AbortKind::GpuHold, g.hold_reason()));
                    }
                    Err(PageFail::Stop(why)) => {
                        self.unfinished(doc_id, had_row, !stored.is_empty()).await?;
                        return Err(stop(AbortKind::Vision, why));
                    }
                }
            }
        }

        // ---- chunk, off the async workers ----
        let (rel, kind, max, tc) = (
            c.rel.clone(),
            c.kind,
            self.opts.chunk_tokens,
            self.tc.clone(),
        );
        let Cut {
            text,
            spans,
            lines,
            ids,
            pdf_pages,
        } = tokio::task::spawn_blocking(move || cut(&rel, kind, text, &readings, max, tc.as_ref()))
            .await
            .map_err(|e| Abort::index(format!("chunking {}: {e}", c.rel)))?;
        // A PDF a vision model was asked about is indexed even with nothing
        // to index: its pages failed, and the index is what reports them on
        // every sync. Without a vision model, an empty PDF is a skip as ever.
        if spans.is_empty() && !(is_pdf && self.opts.vision.is_some()) {
            self.drop_from_index(&c.rel, doc_id, had_row).await?;
            let reason = if c.kind == FileKind::Pdf {
                SkipReason::PdfNoText
            } else {
                SkipReason::Empty
            };
            self.skip(&c.rel, reason, None);
            return Ok(FileOutcome::Settled);
        }

        // Identical spans in one file share a content-derived id and are one
        // row, the last one's (quickdoc upserts): only that one is embedded.
        let mut last_of: HashMap<&str, usize> = HashMap::with_capacity(ids.len());
        for (i, id) in ids.iter().enumerate() {
            last_of.insert(id.as_str(), i);
        }
        let unique: Vec<usize> = (0..spans.len())
            .filter(|&i| last_of[ids[i].as_str()] == i)
            .collect();

        // ---- keep what is already stored exactly as it would be again ----
        let (reuse, todo): (Vec<usize>, Vec<usize>) = unique.iter().partition(|&&i| {
            stored.get(&ids[i]).is_some_and(|s| {
                s.embedded
                    && s.heading_path == spans[i].heading_path
                    && s.span_start == spans[i].start as i64
                    && s.span_end == spans[i].end as i64
            })
        });
        let reuse_lines: Vec<(&str, (usize, usize))> =
            reuse.iter().map(|&i| (ids[i].as_str(), lines[i])).collect();
        self.index.set_chunk_lines(&reuse_lines).await?;
        self.report.reused_chunks += reuse.len();

        // ---- a PDF's readings: the record first, then the chunks ----
        // Before a new reading chunk is stored, every stored reading chunk
        // that is not kept exactly as it is — gone, or to be stored again
        // under another heading or span (the same text read by another
        // alias has the same id) — leaves the index, and its record names
        // the readings it now has (still pending): whatever point a stop
        // leaves it at, the MCP `read` text built from the record and the
        // lines stored with its reading chunks agree.
        if is_pdf {
            let kept: HashSet<&str> = reuse.iter().map(|&i| ids[i].as_str()).collect();
            let gone: Vec<String> = stored
                .iter()
                .filter(|(id, s)| {
                    vision::is_reading(&s.heading_path) && !kept.contains(id.as_str())
                })
                .map(|(id, _)| id.clone())
                .collect();
            self.index.delete_chunks(&gone).await?;
            let record = DocVision {
                content_hash: got.hash.clone(),
                settings: self.vision_settings.clone(),
                pending: true,
            };
            self.index.set_doc_vision(doc_id, &record, &picked).await?;
        }

        // ---- embed and store, batch by batch ----
        let of = todo.len().div_ceil(EMBED_BATCH_SIZE);
        let mut stored_now = false;
        for (b, batch) in todo.chunks(EMBED_BATCH_SIZE).enumerate() {
            self.emit(SyncEvent::Embedding {
                file: c.rel.clone(),
                batch: b + 1,
                of,
                texts: batch.len(),
                batch_size: EMBED_BATCH_SIZE,
            });
            let inputs: Vec<String> = batch
                .iter()
                .map(|&i| embedding_input(&c.rel, &spans[i].heading_path, spans[i].text(&text)))
                .collect();
            let vectors = match self.embed_batch(&c.rel, &inputs).await {
                Ok(v) => v,
                Err(fail) => {
                    let has_chunks = stored_now || !stored.is_empty();
                    self.unfinished(doc_id, had_row, has_chunks).await?;
                    return match fail {
                        BatchFail::Hold(g) => Err(Abort {
                            kind: AbortKind::GpuHold,
                            reason: format!(
                                "{}; stopped at {}, and the next sync resumes from there",
                                g.hold_reason(),
                                c.rel
                            ),
                        }),
                        BatchFail::File(msg) => {
                            self.file_error(&c.rel, msg);
                            Ok(FileOutcome::Settled)
                        }
                        BatchFail::Gateway(reason) => Err(Abort {
                            kind: AbortKind::Embedder,
                            reason,
                        }),
                    };
                }
            };
            let chunks: Vec<NewChunk> = batch
                .iter()
                .zip(vectors)
                .map(|(&i, v)| {
                    let s = &spans[i];
                    let mut n = NewChunk::new(
                        self.corpus_id,
                        doc_id,
                        (s.start as i64, s.end as i64),
                        s.text(&text),
                    );
                    n.heading_path = s.heading_path.clone();
                    // quickdoc's `derived_title` is FTS-indexed and "surfaced
                    // only as a label": exactly what a file's path is to a
                    // search for "the podman notes". No model derived it —
                    // the path is the label.
                    n.derived_title = c.rel.clone();
                    n.embedding = Some(v);
                    n
                })
                .collect();
            store::insert_chunks(self.pool(), &c.rel, self.identity.dims, &chunks).await?;
            let batch_lines: Vec<(&str, (usize, usize))> =
                batch.iter().map(|&i| (ids[i].as_str(), lines[i])).collect();
            self.index.set_chunk_lines(&batch_lines).await?;
            stored_now = true;
            self.report.embedded_chunks += batch.len();
        }

        // ---- drop what the file no longer holds, then mark it current ----
        let keep: HashSet<&str> = unique.iter().map(|&i| ids[i].as_str()).collect();
        let stale: Vec<String> = stored
            .keys()
            .filter(|id| !keep.contains(id.as_str()))
            .cloned()
            .collect();
        self.index.delete_chunks(&stale).await?;
        if is_pdf {
            // The pass is done: pending only when a page is to be tried
            // again on the next sync.
            self.index
                .set_vision_pending(doc_id, retry_next_sync)
                .await?;
        }
        if let Some((pages, textless)) = &pdf_pages {
            self.index.set_pdf_pages(doc_id, *pages, textless).await?;
        }
        self.index.set_file_state(doc_id, &new_state).await?;
        self.emit(SyncEvent::FileDone {
            file: c.rel.clone(),
            chunks: keep.len(),
        });
        Ok(if backfill.any() && same_as_marked {
            FileOutcome::Backfilled(backfill)
        } else {
            FileOutcome::Done
        })
    }
}

/// What is embedded for one chunk: the file's path and the heading path above
/// the verbatim text, so "what does the podman section of setup.md say" can
/// match on the path as well as the words. The payload stored and shown is the
/// text alone.
pub fn embedding_input(rel: &str, heading_path: &str, payload: &str) -> String {
    if heading_path.is_empty() {
        format!("{rel}\n\n{payload}")
    } else {
        format!("{rel} — {heading_path}\n\n{payload}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_limits_are_read_as_cgroup_v2_writes_them() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("memory.max");
        std::fs::write(&p, "2147483648\n").unwrap();
        assert_eq!(read_memory_limit(&p), MemoryLimit::Bytes(2_147_483_648));
        std::fs::write(&p, "max\n").unwrap();
        assert!(matches!(read_memory_limit(&p), MemoryLimit::Unlimited(_)));
        assert!(matches!(
            read_memory_limit(&tmp.path().join("absent")),
            MemoryLimit::Unlimited(_)
        ));
        let d = too_large_detail(600 << 20, 2048 << 20, &p);
        assert!(
            d.contains("LARGE_FILE_MEMORY_FRACTION (0.25)")
                && d.contains("2048.0 MiB")
                && d.contains("run.limits.memory_mb")
                && d.contains("at least 2400 MB")
                && d.contains(".ignore"),
            "{d}"
        );
    }

    #[test]
    fn readings_are_cut_after_the_text_layer_which_they_never_displace() {
        let tc = chunk::estimator();
        let text = "Heron survey\n\u{0c}\u{0c}".to_string();
        let plain = cut("a.pdf", FileKind::Pdf, text.clone(), &[], 400, tc.as_ref());
        let reading = |page, text: &str| Reading {
            page,
            model: "vision-a".into(),
            text: text.into(),
        };
        let with = cut(
            "a.pdf",
            FileKind::Pdf,
            text.clone(),
            &[reading(1, "Heron survey"), reading(2, "Kittiwake ledger")],
            400,
            tc.as_ref(),
        );
        // The text layer's chunks and pages are as they were without readings.
        assert_eq!(&with.spans[..plain.spans.len()], &plain.spans[..]);
        assert_eq!(with.pdf_pages, plain.pdf_pages);
        assert_eq!(with.pdf_pages, Some((2, vec![2])));
        // Page 1's reading is its own text again: dropped, not a second row
        // under the model's name. Page 2's is cut after the text, offset into
        // the combined text, under its heading, with its lines.
        let heads: Vec<&str> = with.spans.iter().map(|s| s.heading_path.as_str()).collect();
        assert_eq!(heads, ["page 1", "page 2, read by vision-a"]);
        let last = with.spans.last().unwrap();
        assert_eq!(last.text(&with.text), "Kittiwake ledger");
        assert!(with.text.starts_with(&text));
        assert_eq!(
            *with.lines.last().unwrap(),
            chunk::line_range(&with.text, last.start, last.end)
        );
        assert_eq!(with.ids.len(), with.spans.len());
    }

    #[test]
    fn cosine_of_the_same_and_of_opposites() {
        assert!((cosine(&[1.0, 2.0], &[2.0, 4.0]).unwrap() - 1.0).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]).unwrap() + 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), None);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), None);
    }
}
