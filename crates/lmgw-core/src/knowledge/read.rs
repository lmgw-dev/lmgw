//! Reading a document rather than searching it: `kb__read` ("read on from
//! here", chat-complete design §9.3) and the source viewer behind a citation
//! (§9.3, the Knowledge page's file view).
//!
//! Both read the file's **extracted text** — what the chunk spans point into —
//! not the chunks themselves, so overlapping chunks never repeat text to the
//! reader.

use quickdoc_core::embed::TokenCounter;
use serde::Serialize;

use crate::quickdoc::query::TiktokenCounter;
use crate::state::SharedState;

use super::store::{self, KbChunk, KbFile};

/// Where a chunk sits in its file, without its payload.
#[derive(Debug, Clone, Serialize)]
pub struct ChunkSpan {
    pub id: String,
    pub seq: i64,
    pub page: Option<i64>,
    pub heading_path: String,
    pub span_start: i64,
    pub span_end: i64,
    pub tokens: i64,
}

impl From<&KbChunk> for ChunkSpan {
    fn from(c: &KbChunk) -> Self {
        Self {
            id: c.id.clone(),
            seq: c.seq,
            page: c.page,
            heading_path: c.heading_path.clone(),
            span_start: c.span_start,
            span_end: c.span_end,
            tokens: c.tokens,
        }
    }
}

/// The source viewer's answer: the file, its extracted text, every chunk's
/// span, and — when asked about one chunk — that chunk, to highlight and
/// scroll to.
#[derive(Debug, Clone, Serialize)]
pub struct Source {
    pub file: KbFile,
    pub kb_name: String,
    /// `None` until the file has been ingested once.
    pub text: Option<String>,
    pub chunks: Vec<ChunkSpan>,
    pub highlight: Option<ChunkSpan>,
    /// Said when the highlight is not the cited chunk itself: it was found by
    /// the citation's stored position because the chunk id no longer exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// What a stored citation remembers of the passage, beyond its chunk id: where
/// it was in the file's text, and which version of the file that was. A
/// re-ingest changes chunk ids (they hash the file row, the file's sha and the
/// position), so a citation from before it names a chunk that is gone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cited {
    pub span_start: Option<i64>,
    pub span_end: Option<i64>,
    /// The sha256 of the file when it was cited; `None` for a citation stored
    /// before that was kept.
    pub file_sha: Option<String>,
}

impl Cited {
    fn span(&self) -> Option<(i64, i64)> {
        match (self.span_start, self.span_end) {
            (Some(a), Some(b)) if a >= 0 && b >= a => Some((a, b)),
            _ => None,
        }
    }
}

/// A file's text and chunk spans; `chunk_id` picks the one to highlight.
///
/// A chunk id that is not in this file falls back to the citation's stored
/// span (`cited`), so a citation survives a re-ingest that renamed its chunk:
/// the passage is highlighted where it was, with a notice that it was found by
/// position. If the file's bytes are not the ones that were cited, or nothing
/// but the id is known, it is an error rather than a silent miss — a citation
/// that no longer resolves has to say so.
pub async fn source(
    state: &SharedState,
    file_id: i64,
    chunk_id: Option<&str>,
    cited: &Cited,
) -> Result<Source, String> {
    let pool = &state.knowledge.pool;
    let file = store::get_file(pool, file_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no knowledge-base file {file_id}"))?;
    let kb_name = store::get_kb(pool, file.kb_id)
        .await
        .map_err(|e| e.to_string())?
        .map(|k| k.name)
        .unwrap_or_default();
    let text = store::file_text(pool, file_id)
        .await
        .map_err(|e| e.to_string())?;
    let chunks: Vec<ChunkSpan> = store::file_chunks(pool, file_id)
        .await
        .map_err(|e| e.to_string())?
        .iter()
        .map(ChunkSpan::from)
        .collect();
    let mut notice = None;
    let highlight = match chunk_id.map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(id) => match chunks.iter().find(|c| c.id == id) {
            Some(c) => Some(c.clone()),
            None => {
                if cited.file_sha.as_deref().is_some_and(|s| s != file.sha256) {
                    return Err(format!(
                        "the document changed since this citation: '{}' holds other content \
                         now than when it was cited (the cited chunk {id} and its position \
                         no longer apply)",
                        file.name
                    ));
                }
                let by_position = cited.span().and_then(|(a, b)| {
                    let t = text.as_deref()?;
                    let (ua, ub) = (a as usize, b as usize);
                    (ub <= t.len() && t.is_char_boundary(ua) && t.is_char_boundary(ub))
                        .then_some((a, b))
                });
                match by_position {
                    Some((a, b)) => {
                        // The chunk now covering the start of the cited span
                        // names where it is.
                        let near = chunks
                            .iter()
                            .find(|c| c.span_start <= a && a < c.span_end)
                            .or_else(|| chunks.iter().find(|c| c.span_start >= a));
                        notice = Some(format!(
                            "the cited chunk {} is gone — '{}' was re-ingested since — so this \
                             is the passage at its stored position{}",
                            &id[..id.len().min(12)],
                            file.name,
                            if cited.file_sha.is_some() {
                                ""
                            } else {
                                " (the citation predates keeping the file's version, so the \
                                 file may have changed since)"
                            }
                        ));
                        Some(ChunkSpan {
                            id: id.to_string(),
                            seq: near.map_or(0, |c| c.seq),
                            page: near.and_then(|c| c.page),
                            heading_path: near.map(|c| c.heading_path.clone()).unwrap_or_default(),
                            span_start: a,
                            span_end: b,
                            tokens: 0,
                        })
                    }
                    None => {
                        return Err(format!(
                            "chunk {id} is not in '{}' any more — the file was re-ingested or \
                             changed since it was cited",
                            file.name
                        ))
                    }
                }
            }
        },
    };
    Ok(Source {
        file,
        kb_name,
        text,
        chunks,
        highlight,
        notice,
    })
}

/// One `kb__read` answer.
#[derive(Debug, Clone, Serialize)]
pub struct Reading {
    pub file_id: i64,
    pub file: String,
    pub kb: String,
    /// The file's pages (PDF), slides or sheets, when it has them.
    pub pages: Option<i64>,
    /// Where this reading starts and ends, as chunk ids and pages.
    pub first_chunk: String,
    pub last_chunk: String,
    pub first_page: Option<i64>,
    pub last_page: Option<i64>,
    pub text: String,
    pub tokens: usize,
    /// Where to continue with `from_chunk`; `None` at the end of the file.
    pub next_chunk: Option<String>,
    /// Chunks after this reading.
    pub remaining_chunks: usize,
}

/// Read `file` from `from_chunk`, from the first chunk of `page`, or from the
/// start, for as much text as fits `budget` tokens (at least one chunk's;
/// `None` reads to the end).
pub async fn read(
    state: &SharedState,
    file: &KbFile,
    kb_name: &str,
    page: Option<i64>,
    from_chunk: Option<&str>,
    budget: Option<usize>,
) -> Result<Reading, String> {
    let pool = &state.knowledge.pool;
    let chunks = store::file_chunks(pool, file.id)
        .await
        .map_err(|e| e.to_string())?;
    let text = store::file_text(pool, file.id)
        .await
        .map_err(|e| e.to_string())?
        .filter(|_| !chunks.is_empty())
        .ok_or_else(|| {
            format!(
                "'{}' has no text to read yet (status: {}{})",
                file.name,
                file.status,
                file.error
                    .as_deref()
                    .map(|e| format!(" — {e}"))
                    .unwrap_or_default()
            )
        })?;

    let start = match (from_chunk.map(str::trim).filter(|s| !s.is_empty()), page) {
        (Some(id), _) => chunks.iter().position(|c| c.id == id).ok_or_else(|| {
            format!(
                "chunk {id} is not in '{}' (file {}) — pass a chunk id that kb__search or an \
                 earlier kb__read returned for this file",
                file.name, file.id
            )
        })?,
        (None, Some(p)) => {
            if file.kind != "pdf" {
                return Err(format!(
                    "'{}' is not a PDF and has no pages — read it with from_chunk, or from the \
                     start",
                    file.name
                ));
            }
            chunks
                .iter()
                .position(|c| c.page == Some(p))
                .ok_or_else(|| match file.pages {
                    Some(n) if p < 1 || p > n => {
                        format!("'{}' has {n} page(s); there is no page {p}", file.name)
                    }
                    _ => format!(
                        "page {p} of '{}' has no text (it was skipped: no vision model read it)",
                        file.name
                    ),
                })?
        }
        (None, None) => 0,
    };

    let region = |end: usize| -> Option<&str> {
        text.get(
            chunks[start].span_start as usize
                ..chunks[end].span_end.max(chunks[start].span_start) as usize,
        )
    };
    let counter = TiktokenCounter;
    // The furthest chunk whose region still fits — token counts grow with
    // the text, so a binary search finds it.
    let mut end = start;
    match budget {
        None => end = chunks.len() - 1,
        Some(b) => {
            let (mut lo, mut hi) = (start, chunks.len() - 1);
            while lo < hi {
                let mid = (lo + hi).div_ceil(2);
                let fits = region(mid).is_some_and(|t| counter.count(t) <= b);
                if fits {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            end = end.max(lo);
        }
    }
    let body = region(end)
        .ok_or_else(|| format!("'{}': a chunk span lies outside its text", file.name))?
        .to_string();
    // The next chunk, even though it overlaps this reading's tail by the
    // base's overlap: starting past the overlap could skip the text between.
    let next = (end + 1 < chunks.len()).then_some(end + 1);
    Ok(Reading {
        file_id: file.id,
        file: file.name.clone(),
        kb: kb_name.to_string(),
        pages: file.pages,
        first_chunk: chunks[start].id.clone(),
        last_chunk: chunks[end].id.clone(),
        first_page: chunks[start].page,
        last_page: chunks[end].page,
        tokens: counter.count(&body),
        text: body,
        next_chunk: next.map(|k| chunks[k].id.clone()),
        remaining_chunks: next.map_or(0, |k| chunks.len() - k),
    })
}
