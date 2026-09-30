//! Shared by the integration tests: a counting fixture embedder, a sync
//! runner that collects its events, a whole-tree fingerprint, a hand-built
//! PDF, a fake `pdftotext` and a fake `pdftoppm`, and a fake lmgw
//! ([`fake`]).
#![allow(dead_code)]

pub mod fake;
pub mod harness;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use folder_chat::gateway::EMBED_PROBE_TEXT;
use folder_chat::index::IndexDir;
use folder_chat::sync::{self, SyncError, SyncEvent, SyncOptions, SyncReport};
use quickdoc_core::embed::{EmbedIdentity, Embedder, FixtureEmbedder};
use quickdoc_core::QuickdocError;
use sha2::{Digest, Sha256};

pub const DIMS: usize = 64;

/// The lmgw error body a held GPU answers with, as `HttpEmbedder` reports it.
pub fn gpu_hold_error() -> QuickdocError {
    QuickdocError::Embedder(format!("503 Service Unavailable: {}", gpu_hold_body()))
}

pub fn gpu_hold_body() -> String {
    serde_json::json!({"error": {
        "message": "the GPU hold is on: local models are paused",
        "type": "api_error", "param": null, "code": "gpu_hold"
    }})
    .to_string()
}

/// A hook [`Counting`] calls with every batch it is about to embed — how a
/// test changes the folder mid-sync, after the scan and before a later read.
pub type Hook = Box<dyn Fn(&[String]) + Send + Sync>;

/// quickdoc's fixture embedder, counting what it is asked, and optionally
/// answering `gpu_hold` from the `hold_after`-th call on, or a 500 for any
/// batch with a text containing `fail_containing`.
///
/// The model's probe ([`EMBED_PROBE_TEXT`], the index's fingerprint and the
/// sync's "is the gateway still up" check) is answered without being counted
/// or held: `calls` and `texts` count the folder's batches only.
pub struct Counting {
    inner: FixtureEmbedder,
    pub calls: AtomicUsize,
    pub texts: AtomicUsize,
    pub probes: AtomicUsize,
    pub hold_after: Option<usize>,
    pub fail_containing: Option<String>,
    pub hook: Option<Hook>,
}

impl Counting {
    pub fn new() -> Arc<Self> {
        Self::with(FixtureEmbedder::new(DIMS), None)
    }

    pub fn with(inner: FixtureEmbedder, hold_after: Option<usize>) -> Arc<Self> {
        Arc::new(Self::plain(inner, hold_after))
    }

    pub fn plain(inner: FixtureEmbedder, hold_after: Option<usize>) -> Self {
        Self {
            inner,
            calls: AtomicUsize::new(0),
            texts: AtomicUsize::new(0),
            probes: AtomicUsize::new(0),
            hold_after,
            fail_containing: None,
            hook: None,
        }
    }

    /// Fails (500) every batch with a text containing `needle`.
    pub fn failing_on(needle: &str) -> Arc<Self> {
        Arc::new(Self {
            fail_containing: Some(needle.to_string()),
            ..Self::plain(FixtureEmbedder::new(DIMS), None)
        })
    }

    /// Calls `hook` with every batch before embedding it.
    pub fn hooked(hook: impl Fn(&[String]) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            hook: Some(Box::new(hook)),
            ..Self::plain(FixtureEmbedder::new(DIMS), None)
        })
    }

    pub fn texts(&self) -> usize {
        self.texts.load(Ordering::SeqCst)
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn probes(&self) -> usize {
        self.probes.load(Ordering::SeqCst)
    }
}

pub fn is_probe(texts: &[String]) -> bool {
    texts.len() == 1 && texts[0] == EMBED_PROBE_TEXT
}

/// The error body of a gateway that failed, as an embedder reports it.
pub fn server_error() -> QuickdocError {
    QuickdocError::Embedder(format!(
        "500 Internal Server Error: {}",
        serde_json::json!({"error": {"message": "upstream exploded", "type": "api_error",
                                     "code": "upstream_error"}})
    ))
}

#[async_trait]
impl Embedder for Counting {
    fn identity(&self) -> EmbedIdentity {
        self.inner.identity()
    }

    async fn embed(&self, texts: &[String]) -> quickdoc_core::Result<Vec<Vec<f32>>> {
        if is_probe(texts) {
            self.probes.fetch_add(1, Ordering::SeqCst);
            return self.inner.embed(texts).await;
        }
        if let Some(h) = &self.hook {
            h(texts);
        }
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.hold_after.is_some_and(|h| n >= h) {
            return Err(gpu_hold_error());
        }
        if let Some(needle) = &self.fail_containing {
            if texts.iter().any(|t| t.contains(needle.as_str())) {
                return Err(server_error());
            }
        }
        self.texts.fetch_add(texts.len(), Ordering::SeqCst);
        self.inner.embed(texts).await
    }
}

pub struct Outcome {
    pub result: Result<SyncReport, SyncError>,
    pub events: Vec<SyncEvent>,
}

impl Outcome {
    pub fn report(&self) -> &SyncReport {
        match &self.result {
            Ok(r) => r,
            Err(e) => panic!("the sync failed: {e}"),
        }
    }
}

pub async fn sync_with(
    idx: &IndexDir,
    embedder: Arc<dyn Embedder>,
    chunk_tokens: usize,
    embed_context_length: Option<u64>,
) -> Outcome {
    sync_opts(
        idx,
        embedder,
        &SyncOptions::new(chunk_tokens, embed_context_length),
    )
    .await
}

/// A sync with every option spelled out (the memory, pdftotext and timeout
/// seams).
pub async fn sync_opts(idx: &IndexDir, embedder: Arc<dyn Embedder>, opts: &SyncOptions) -> Outcome {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = sync::run(idx, embedder, opts, &tx).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    Outcome { result, events }
}

pub async fn sync(idx: &IndexDir, embedder: Arc<dyn Embedder>) -> Outcome {
    sync_with(idx, embedder, 400, None).await
}

/// How far in the past [`write`] dates a file: well outside the sync's
/// `RACY_MTIME_WINDOW`, so a file a test wrote a moment ago is not "racily
/// clean" and takes the fast path on the next sync, as a file the owner saved
/// earlier would. Each write still gets its own mtime (it is taken from the
/// clock at the time), so two writes are told apart.
pub const WRITTEN_AGO: Duration = Duration::from_secs(3600);

pub fn write(root: &Path, rel: &str, body: impl AsRef<[u8]>) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, body).unwrap();
    set_mtime(&p, SystemTime::now() - WRITTEN_AGO);
}

/// [`write`] without back-dating: the file looks just saved.
pub fn write_now(root: &Path, rel: &str, body: impl AsRef<[u8]>) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, body).unwrap();
}

pub fn set_mtime(p: &Path, t: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

/// Every entry under `root` except `skip` (a top-level name): its type and
/// mode bits, and for a file its size, mtime and content hash, for a link its
/// target. What "nothing else was written" is checked against. A FIFO or
/// other special file is described, never opened.
pub fn fingerprint(root: &Path, skip: &str) -> BTreeMap<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            if rel == skip {
                continue;
            }
            let m = std::fs::symlink_metadata(&p).unwrap();
            let mode = m.permissions().mode();
            let v = if m.file_type().is_symlink() {
                format!(
                    "link {mode:o} -> {}",
                    std::fs::read_link(&p).unwrap().display()
                )
            } else if m.is_dir() {
                stack.push(p.clone());
                format!("dir {mode:o} mtime {:?}", m.modified().ok())
            } else if m.is_file() {
                let bytes = std::fs::read(&p).unwrap_or_default();
                format!(
                    "file {mode:o} {} {:?} {}",
                    m.len(),
                    m.modified().ok(),
                    hex::encode(Sha256::digest(&bytes))
                )
            } else {
                format!("special {mode:o} mtime {:?}", m.modified().ok())
            };
            out.insert(rel, v);
        }
    }
    out
}

/// A fake `pdftotext` in its own directory: `-v` prints `version`, anything
/// else reads stdin to the end and prints `pages` separated by form feeds —
/// or, with `sleep`, sleeps that many seconds without reading anything. A
/// sync is pointed at it through `SyncOptions::pdftotext`, so nothing on
/// `PATH` changes for the rest of the process.
pub struct FakePdftotext {
    _dir: tempfile::TempDir,
    pub path: std::path::PathBuf,
}

pub fn fake_pdftotext(version: &str, pages: &[&str], sleep: Option<u64>) -> FakePdftotext {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pdftotext");
    let body = match sleep {
        Some(s) => format!("exec sleep {s}\n"),
        None => format!("cat >/dev/null\nprintf '%s' '{}'\n", pages.join("\x0c")),
    };
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"-v\" ]; then echo 'pdftotext version {version}' >&2; exit 0; fi\n{body}"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    FakePdftotext { _dir: dir, path }
}

/// What a fake `pdftoppm` prints for every page: a valid 1×1 PNG.
pub const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

/// A fake `pdftoppm` in its own directory: it reads stdin to the end, notes
/// its arguments (one line per run, [`FakePdftoppm::calls`]) and prints
/// [`TINY_PNG`] — or, with `fail`, says so on stderr and exits 1. A sync is
/// pointed at it through `SyncOptions::pdftoppm`.
pub struct FakePdftoppm {
    dir: tempfile::TempDir,
    pub path: std::path::PathBuf,
}

impl FakePdftoppm {
    /// The arguments of every run so far, in order.
    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}

pub fn fake_pdftoppm(fail: bool) -> FakePdftoppm {
    fake_pdftoppm_with(fail, TINY_PNG)
}

/// [`fake_pdftoppm`] printing `page` for every page.
pub fn fake_pdftoppm_png(page: &[u8]) -> FakePdftoppm {
    fake_pdftoppm_with(false, page)
}

fn fake_pdftoppm_with(fail: bool, page: &[u8]) -> FakePdftoppm {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pdftoppm");
    let png = dir.path().join("page.png");
    std::fs::write(&png, page).unwrap();
    let out = if fail {
        "echo 'Syntax Error: the fake cannot render this page' >&2\nexit 1".to_string()
    } else {
        format!("exec cat '{}'", png.display())
    };
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncat >/dev/null\necho \"$*\" >> '{}'\n{out}\n",
            dir.path().join("calls").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    FakePdftoppm { dir, path }
}

/// A tiny valid PDF, one page per string, Helvetica text. Built by hand so
/// the test needs nothing but `pdftotext`; the strings must not contain
/// parentheses or backslashes.
pub fn minimal_pdf(pages: &[&str]) -> Vec<u8> {
    let n = pages.len();
    let mut objs: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {n} >>",
            (0..n)
                .map(|i| format!("{} 0 R", 4 + 2 * i))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ];
    for (i, text) in pages.iter().enumerate() {
        let content = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
        objs.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            5 + 2 * i
        ));
        objs.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ));
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for off in offsets {
        out.extend(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// How many FTS rows match one term — the "is it gone from BM25 too" check.
pub async fn fts_count(idx: &IndexDir, term: &str) -> i64 {
    use sqlx::Row;
    sqlx::query("SELECT COUNT(*) AS n FROM chunk_fts WHERE chunk_fts MATCH ?1")
        .bind(format!("\"{term}\""))
        .fetch_one(idx.pool())
        .await
        .unwrap()
        .get("n")
}
