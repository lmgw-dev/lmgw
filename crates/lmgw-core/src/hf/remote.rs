//! A Hugging Face file's GGUF header, read over HTTP before anything is
//! downloaded (find-a-model design §2.1, D1, D2, D4).
//!
//! One `GET` of the file's resolve URL through [`super::get_with_commit`],
//! with `Range: bytes=0-` on the CDN's hop only and the token on the hub's
//! hops only. The body is read as a stream into
//! [`crate::gguf::read_header_from`], with the file's length from the CDN's
//! `Content-Range` as its total, and the stream is dropped as soon as the
//! reader has the last descriptor. Every guard of the reader on disk holds:
//! the total bounds each declared length, and a cut stream is
//! `UnexpectedEof`.
//!
//! **A bound said out loud** (D129). The total can be tens of gigabytes and
//! a header needs megabytes, so a hostile header declaring a string or an
//! array the size of the file would pull the whole file through the
//! reader. A read off the network therefore stops before any declared
//! length would carry the header past the memory this computer has
//! available (`MemAvailable`, read when the read starts): llama.cpp holds
//! every metadata value in memory while it loads a file, so a header past
//! that couldn't be loaded here anyway. The read then fails with a sentence
//! naming both sizes and what lmgw received. When the memory can't be read
//! (a machine without `/proc/meminfo`), the total alone bounds the read, as
//! on disk, and the log says so.
//!
//! The summaries are kept in memory for the life of the process by the
//! file's content hash: the tree listing's `lfs.oid`, which the hub's
//! resolver repeats as `x-linked-etag` (D4). A second ask for the same hash
//! costs no request. A file without an `lfs` object in its listing is read
//! every time (D43): only LFS files carry a hash the listing and the
//! resolver agree on, and every GGUF on the hub is one.

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Buf;

use crate::gguf::{self, ModelSummary};
use crate::state::SharedState;

use super::HfFile;

/// How long the hub or its CDN may send nothing before a header read stops
/// with a sentence that says so: the tree listing's page bound
/// (`hf/listing.rs`). A slow line that keeps sending is never cut (D44).
pub const STALL: Duration = Duration::from_secs(30);

/// What a header read found.
#[derive(Debug, Clone)]
pub struct RemoteHeader {
    /// The header's summary, what the planner reads (§2.3).
    pub summary: Arc<ModelSummary>,
    /// The whole file's bytes: the `Content-Range` total.
    pub total: u64,
    /// Where the file's tensor data starts: the bytes the header and its
    /// descriptors take.
    pub header_bytes: u64,
    /// What came over the wire before the stream was dropped: the header
    /// and the rest of the chunk it ended in ("lmgw read 11.0 MiB", D123).
    pub received_bytes: u64,
    /// The content hash the hub named for what was read (`x-linked-etag`),
    /// else the listing's.
    pub oid: Option<String>,
    /// The commit the hub resolved `main` to.
    pub commit: Option<String>,
    /// Seconds from the CDN's answer to the stream's drop: what the
    /// received bytes took to arrive, the rate a download estimate falls
    /// back on before any download finished (find-a-model design D23).
    pub read_secs: f64,
}

/// Why a header read failed (find-a-model design D66): the sentence, whether
/// the next read from the same hub would fail the same way, and what came
/// over the wire before it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadError {
    pub message: String,
    /// The hub refused or couldn't be reached: a 401 or 403 (a gated repo, a
    /// rejected token, no access), a 429 (its rate limit), a 5xx, or no
    /// answer at all (the connection failed, or nothing came for
    /// [`STALL`]). A caller reading several files of a repo stops there.
    /// `false` for a failure of this file's own: a 404, a stream cut short,
    /// a body that isn't a GGUF, a header past the memory bound.
    pub hub: bool,
    /// The bytes received before the read failed (D123's count).
    pub received: u64,
}

impl ReadError {
    /// A failure of this file's own.
    pub fn file(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hub: false,
            received: 0,
        }
    }

    /// A failure every read from this hub would meet.
    pub fn hub(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hub: true,
            received: 0,
        }
    }

    fn received(mut self, bytes: u64) -> Self {
        self.received = bytes;
        self
    }
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<ReadError> for String {
    fn from(e: ReadError) -> Self {
        e.message
    }
}

/// The process's remote headers by content hash (module doc).
#[derive(Default)]
pub struct RemoteHeaderCache {
    inner: std::sync::Mutex<HashMap<String, Arc<RemoteHeader>>>,
    /// One guard per hash, so the card and the file list asking for one
    /// file at once read it once.
    inflight: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Header reads that went to the network: what a hit doesn't count.
    reads: AtomicU64,
}

impl RemoteHeaderCache {
    /// Header reads made since the process started.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    fn get(&self, oid: &str) -> Option<Arc<RemoteHeader>> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(oid)
            .cloned()
    }

    fn put(&self, oid: String, header: Arc<RemoteHeader>) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(oid, header);
    }
}

/// The header of `file` in `repo` at `main`, from the cache by its listing
/// hash or read from the hub (module doc). `file` is the tree listing's
/// entry, which carries the hash.
pub async fn remote_header(
    state: &SharedState,
    repo: &str,
    file: &HfFile,
) -> Result<Arc<RemoteHeader>, String> {
    remote_header_fetched(state, repo, file)
        .await
        .map(|(h, _)| h)
        .map_err(String::from)
}

/// [`remote_header`], and whether this call fetched it (`false` for a
/// header the cache had, read by an earlier ask or by the one this call
/// waited on): what a plan counts as its own read (find-a-model design
/// D123).
pub async fn remote_header_fetched(
    state: &SharedState,
    repo: &str,
    file: &HfFile,
) -> Result<(Arc<RemoteHeader>, bool), ReadError> {
    super::validate_repo(repo).map_err(ReadError::file)?;
    // The path is the repo's own: nothing that climbs out of it.
    super::dest_rel_path(repo, &file.path).map_err(ReadError::file)?;
    let cache = &state.hf_headers;
    let key = file.oid().map(str::to_ascii_lowercase);
    if let Some(hit) = key.as_deref().and_then(|k| cache.get(k)) {
        return Ok((hit, false));
    }
    let guard = match &key {
        Some(k) => Some(
            cache
                .inflight
                .lock()
                .await
                .entry(k.clone())
                .or_default()
                .clone(),
        ),
        None => None,
    };
    let _held = match &guard {
        Some(g) => Some(g.lock().await),
        None => None,
    };
    // Read while this caller waited for the guard.
    if let Some(hit) = key.as_deref().and_then(|k| cache.get(k)) {
        return Ok((hit, false));
    }
    let snap = state.snapshot();
    let url = super::resolve_url(repo, "main", &file.path);
    let bound = match state.machine.memory_available() {
        Ok(bytes) => Some(bytes),
        Err(why) => {
            tracing::info!(
                "the header read of {repo}/{} is bounded by the file's length alone: the memory \
                 available is not known ({why})",
                file.path
            );
            None
        }
    };
    let read = read(
        &state.proxy_http,
        &state.http,
        &snap.settings.hf_token,
        repo,
        &file.path,
        &url,
        bound,
    )
    .await;
    let header = read.map(|read| {
        cache.reads.fetch_add(1, Ordering::Relaxed);
        let header = Arc::new(RemoteHeader {
            oid: read.oid.clone().or(key.clone()),
            ..read
        });
        // Under the hash of what was read, lowercased as the lookup is: a
        // listing older than the file names another, and its next ask reads
        // again.
        if let Some(oid) = header.oid.as_deref() {
            cache.put(oid.to_ascii_lowercase(), header.clone());
        }
        header
    });
    // The guard's entry goes once its read is over and its answer cached,
    // whatever came of it: a caller that waited on it holds its own handle
    // and finds the cache, and the next ask finds the cache or makes a new
    // guard.
    if let (Some(k), Some(g)) = (&key, &guard) {
        let mut inflight = cache.inflight.lock().await;
        if inflight.get(k).is_some_and(|e| Arc::ptr_eq(e, g)) {
            inflight.remove(k);
        }
    }
    header.map(|h| (h, true))
}

/// One header read (module doc), uncached.
pub(crate) async fn read(
    direct: &reqwest::Client,
    follow: &reqwest::Client,
    token: &str,
    repo: &str,
    file: &str,
    url: &str,
    bound: Option<u64>,
) -> Result<RemoteHeader, ReadError> {
    let mut range = reqwest::header::HeaderMap::new();
    range.insert(
        reqwest::header::RANGE,
        reqwest::header::HeaderValue::from_static("bytes=0-"),
    );
    let (resp, hop) = tokio::time::timeout(
        STALL,
        super::get_with_commit(direct, follow, token, url, &range),
    )
    .await
    .map_err(|_| ReadError::hub(stalled(file)))?
    // No answer from the hub or its CDN: the connection, not the file.
    .map_err(ReadError::hub)?;
    let status = resp.status();
    if !status.is_success() {
        let reset = super::ratelimit_reset(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        // What every read of this repo would meet (D66): access, the rate
        // limit, the hub's own trouble. A 404 is this file's.
        let hub_wide = matches!(status.as_u16(), 401 | 403 | 429) || status.is_server_error();
        let fail = |m: String| match hub_wide {
            true => ReadError::hub(m),
            false => ReadError::file(m),
        };
        if let Some(sentence) = super::hub_refusal(status, &body, repo, !token.is_empty()) {
            return Err(fail(sentence));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(fail(format!(
                "Hugging Face rate limit reached reading the header of {file}{}",
                reset
                    .map(|t| format!(" — it resets in {t} s"))
                    .unwrap_or_default()
            )));
        }
        return Err(fail(format!("GET {url}: {status}")));
    }
    let total = total_of(&resp, hop.linked_size).ok_or_else(|| {
        ReadError::file(format!(
            "Hugging Face answered the header read of {file} without the file's length, so \
             nothing in it can be checked against the file"
        ))
    })?;
    let began = std::time::Instant::now();
    let (meta, received) = read_stream(resp, total, file, bound).await?;
    let read_secs = began.elapsed().as_secs_f64();
    Ok(RemoteHeader {
        summary: Arc::new(ModelSummary::from_meta(&meta)),
        total,
        header_bytes: meta.data_start,
        received_bytes: received,
        oid: hop.linked_etag,
        commit: hop.commit,
        read_secs,
    })
}

fn stalled(file: &str) -> String {
    format!(
        "Hugging Face sent nothing for {} s while lmgw read the header of {file}",
        STALL.as_secs()
    )
}

/// The whole file's length: a `206`'s `Content-Range` total from byte 0, a
/// `200`'s `Content-Length` (a hub that answers the file itself sends it
/// whole), else what the hub's hop said.
fn total_of(resp: &reqwest::Response, linked_size: Option<u64>) -> Option<u64> {
    if resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        return match content_range(resp.headers())? {
            (0, total) => total,
            _ => None,
        };
    }
    resp.content_length().or(linked_size)
}

/// `Content-Range: bytes <start>-<end>/<total>`: the start, and the total
/// when it is known (`*` is not).
pub(crate) fn content_range(headers: &reqwest::header::HeaderMap) -> Option<(u64, Option<u64>)> {
    let v = headers
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .trim();
    let rest = v.strip_prefix("bytes ")?;
    let (span, total) = rest.split_once('/')?;
    let start = span.split_once('-')?.0.trim().parse().ok()?;
    Some((start, total.trim().parse().ok()))
}

/// The body into the header reader on the blocking pool, chunk by chunk,
/// until the reader has the header: then the stream is dropped. The bytes
/// that came over the wire with it.
async fn read_stream(
    mut resp: reqwest::Response,
    total: u64,
    file: &str,
    bound: Option<u64>,
) -> Result<(gguf::GgufMeta, u64), ReadError> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, String>>(8);
    let reader = tokio::task::spawn_blocking(move || {
        // Buffered as a file on disk is: the reader skips the tokenizer's
        // strings one by one through `io::copy`, which zeroes a fresh 8 KiB
        // buffer per call on a reader that isn't a `BufReader` (measured: a
        // 27B model's 11 MB header took 10 s that way, 0.8 s on the wire).
        // What it reads ahead is in chunks that arrived already.
        let buffered = std::io::BufReader::with_capacity(
            64 * 1024,
            ChunkReader {
                rx,
                buf: bytes::Bytes::new(),
            },
        );
        match bound {
            Some(bound) => gguf::read_header_within(buffered, total, bound),
            None => gguf::read_header_from(buffered, total),
        }
    });
    let file_name = file.to_string();
    let pump = async move {
        let mut received = 0u64;
        loop {
            let next = tokio::select! {
                // The reader has what it needs (or gave up): stop asking.
                _ = tx.closed() => break,
                c = tokio::time::timeout(STALL, resp.chunk()) => c,
            };
            match next {
                Ok(Ok(Some(chunk))) => {
                    received += chunk.len() as u64;
                    if tx.send(Ok(chunk)).await.is_err() {
                        break;
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(e)) => {
                    let _ = tx.send(Err(format!("reading {file_name}: {e}"))).await;
                    break;
                }
                Err(_) => {
                    let _ = tx.send(Err(stalled(&file_name))).await;
                    break;
                }
            }
        }
        // `resp` goes here: the connection is closed, not read to its end.
        received
    };
    let (received, meta) = tokio::join!(pump, reader);
    // A stream that stopped is this file's failure, with what it brought
    // (D66): the next read opens a connection of its own, and a hub that
    // is gone fails that one.
    let meta = meta
        .map_err(|e| {
            ReadError::file(format!("reading the header of {file} stopped: {e}")).received(received)
        })?
        .map_err(|e| {
            ReadError::file(match e {
                gguf::GgufError::OverBound {
                    declared_end,
                    bound,
                    ..
                } => over_bound(file, declared_end, bound, received),
                e => format!("the header of {file} on Hugging Face: {e}"),
            })
            .received(received)
        })?;
    Ok((meta, received))
}

/// The sentence a read stopped by its bound answers (module doc, D129).
fn over_bound(file: &str, declared_end: u64, bound: u64, received: u64) -> String {
    format!(
        "the header of {file} on Hugging Face declares {} of metadata, more than the {} of \
         memory this computer has available, and llama.cpp holds all of it in memory to load \
         the file, so lmgw stopped reading after {}",
        super::fmt_bytes(declared_end),
        super::fmt_bytes(bound),
        super::fmt_bytes(received)
    )
}

/// A `Read` over the chunks the pump hands across: a cut stream is an end
/// (`Ok(0)`, which the reader turns into `UnexpectedEof`), a failed one an
/// error.
struct ChunkReader {
    rx: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, String>>,
    buf: bytes::Bytes,
}

impl Read for ChunkReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.buf.is_empty() {
            match self.rx.blocking_recv() {
                Some(Ok(b)) => self.buf = b,
                Some(Err(e)) => return Err(std::io::Error::other(e)),
                None => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len());
        out[..n].copy_from_slice(&self.buf[..n]);
        self.buf.advance(n);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_content_range_gives_its_start_and_total() {
        let h = |v: &str| {
            let mut m = reqwest::header::HeaderMap::new();
            m.insert(reqwest::header::CONTENT_RANGE, v.parse().unwrap());
            m
        };
        assert_eq!(
            content_range(&h("bytes 0-99/16817244384")),
            Some((0, Some(16_817_244_384)))
        );
        assert_eq!(content_range(&h("bytes 500-999/*")), Some((500, None)));
        assert_eq!(content_range(&h("items 0-1/2")), None);
        assert_eq!(content_range(&reqwest::header::HeaderMap::new()), None);
    }
}
