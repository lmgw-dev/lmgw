//! A stored file served to a player: the whole file, or the one byte range
//! its `Range` asks (RFC 9110 §14), with `Accept-Ranges: bytes` either way.
//! Every route a dashboard `<audio>` element plays from answers through
//! here: a lab artifact, a Chat attachment, a voice-library clip, a Voices
//! lab candidate. Every answer built here (200, 206, 416) carries
//! `nosniff` and `Content-Security-Policy: sandbox` ([`super::stored`]),
//! because what it serves is stored content.
//!
//! **Why a player needs it.** WebKitGTK's media source (GStreamer) stops a
//! download once its queue is full — about 22 MB in — and resumes it later
//! with `Range: bytes=<where it stopped>-`. A 200 with the whole file again
//! is an error it ends the playback on ("Received unexpected 200 HTTP status
//! code for range request"), so every file past that size failed to play in
//! the app window while a short one, read in one go, played. Chromium sends
//! `Range: bytes=0-` and seeks only a resource that answers with ranges.
//!
//! **What it reads.** `bytes=a-b`, `bytes=a-` and `bytes=-n`, one of them,
//! answer 206 with `Content-Range: bytes a-b/size`; a range that names no
//! byte of the file is 416 `range_not_satisfiable` with
//! `Content-Range: bytes */size`. A `Range` lmgw does not read (another
//! unit, several ranges, an invalid one) is ignored and the whole file
//! served, as RFC 9110 §14.2 allows, and so is any `Range` on a method other
//! than GET (a HEAD answers the whole file's head). `If-Range` applies the
//! range only when it is the answer's own strong `ETag`; a route without one
//! (or an `If-Range` date: lmgw sends no `Last-Modified`) serves the whole
//! file to every `If-Range`. A file on disk is streamed a block at a time
//! from the range's first byte, never read whole into memory.

use std::convert::Infallible;
use std::io::SeekFrom;
use std::path::Path;

use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use lmgw_api_types::transfer::ByteRange;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::chat::err_json;

/// How a file on disk is read: a block at a time.
const READ_BLOCK: u64 = 256 * 1024;

/// What a request asks of a ranged answer: its `Range` (a GET's only) and
/// its `If-Range`. An extractor that never refuses.
#[derive(Debug, Clone, Default)]
pub(crate) struct RangeAsk {
    range: Option<String>,
    if_range: Option<String>,
}

impl<S: Send + Sync> FromRequestParts<S> for RangeAsk {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::of(&parts.method, &parts.headers))
    }
}

/// The part of the file an answer sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    /// All of it (200).
    Whole,
    /// Its bytes `first..=last` (206).
    Span(u64, u64),
    /// None: the range names no byte of it (416).
    Unsatisfiable,
}

impl RangeAsk {
    fn of(method: &Method, headers: &HeaderMap) -> Self {
        let text = |name| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        Self {
            range: (method == Method::GET)
                .then(|| text(header::RANGE))
                .flatten(),
            // A present `If-Range` that is no text is no validator of ours:
            // the empty string never equals an `ETag`.
            if_range: headers
                .get(header::IF_RANGE)
                .map(|v| v.to_str().map(|t| t.to_string()).unwrap_or_default()),
        }
    }

    /// What to send of a `size`-byte file whose strong `ETag` is `etag`.
    fn part(&self, size: u64, etag: Option<&str>) -> Part {
        // A range names no byte of an empty file, and there is nothing to
        // be unsatisfied by: the (empty) whole file.
        if size == 0 {
            return Part::Whole;
        }
        let applies = match &self.if_range {
            None => true,
            Some(v) => etag.is_some_and(|e| v.trim() == e),
        };
        let Some(asked) = self
            .range
            .as_deref()
            .filter(|_| applies)
            .and_then(ByteRange::parse)
        else {
            return Part::Whole;
        };
        match asked.resolve(size) {
            Some((first, last)) => Part::Span(first, last),
            None => Part::Unsatisfiable,
        }
    }
}

/// How the answer describes the file: its media type and, when the route
/// has one, its strong `ETag` (quoted, as sent).
pub(crate) struct Head {
    pub content_type: HeaderValue,
    pub etag: Option<String>,
}

impl Head {
    /// `content_type` as the header, `application/octet-stream` when it is
    /// none a header can carry; no `ETag`.
    pub fn typed(content_type: &str) -> Self {
        Self {
            content_type: HeaderValue::from_str(content_type)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
            etag: None,
        }
    }

    pub fn with_etag(self, etag: String) -> Self {
        Self {
            etag: Some(etag),
            ..self
        }
    }
}

/// The file at `path`, whole or the range `ask` names, streamed from the
/// disk. An error opening or reading its size is the caller's to word (a
/// missing file, or a directory, is `NotFound`); one while streaming ends
/// the body.
pub(crate) async fn file(ask: &RangeAsk, path: &Path, head: Head) -> std::io::Result<Response> {
    let mut f = tokio::fs::File::open(path).await?;
    let meta = f.metadata().await?;
    // A directory opens too; it is no file to serve.
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} is not a file", path.display()),
        ));
    }
    let size = meta.len();
    // A route without a validator gets one from the opened file, so a
    // resume across a replaced file (temp + rename) gets the whole new one.
    let head = if head.etag.is_none() {
        let etag = file_etag(&meta);
        head.with_etag(etag)
    } else {
        head
    };
    let part = ask.part(size, head.etag.as_deref());
    let (first, len) = match part {
        Part::Unsatisfiable => return Ok(unsatisfiable(size, ask)),
        Part::Whole => (0, size),
        Part::Span(first, last) => (first, last - first + 1),
    };
    if first > 0 {
        f.seek(SeekFrom::Start(first)).await?;
    }
    let stream = futures::stream::try_unfold((f, len), |(mut f, left)| async move {
        if left == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        let mut buf = vec![0u8; usize::try_from(left.min(READ_BLOCK)).unwrap_or(0)];
        let n = f.read(&mut buf).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the file got shorter while it was being sent",
            ));
        }
        buf.truncate(n);
        Ok(Some((Bytes::from(buf), (f, left - n as u64))))
    });
    Ok(headed(Body::from_stream(stream), size, part, head))
}

/// A strong validator for the file `meta` describes (the opened handle, so
/// it describes the bytes sent): inode, length and modification time in
/// nanoseconds.
fn file_etag(meta: &std::fs::Metadata) -> String {
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(meta);
    #[cfg(not(unix))]
    let ino = 0u64;
    let nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    format!("\"{ino:x}-{:x}-{nanos:x}\"", meta.len())
}

/// `data`, whole or the range `ask` names.
pub(crate) fn bytes(ask: &RangeAsk, data: Bytes, head: Head) -> Response {
    let size = data.len() as u64;
    let part = ask.part(size, head.etag.as_deref());
    let body = match part {
        Part::Unsatisfiable => return unsatisfiable(size, ask),
        Part::Whole => data,
        Part::Span(first, last) => {
            // Both ends are inside `data`: `resolve` keeps them below `size`.
            data.slice(usize::try_from(first).unwrap_or(0)..=usize::try_from(last).unwrap_or(0))
        }
    };
    headed(Body::from(body), size, part, head)
}

/// The answer with `body`: the whole `size`-byte file, or `part` of it (206).
fn headed(body: Body, size: u64, part: Part, head: Head) -> Response {
    let mut resp = Response::new(body);
    let length = match part {
        Part::Span(first, last) => {
            *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
            if let Ok(v) = HeaderValue::from_str(&format!("bytes {first}-{last}/{size}")) {
                resp.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            last - first + 1
        }
        _ => size,
    };
    let h = resp.headers_mut();
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CONTENT_TYPE, head.content_type);
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    if let Some(v) = head.etag.and_then(|e| HeaderValue::from_str(&e).ok()) {
        h.insert(header::ETAG, v);
    }
    super::stored::sandboxed(resp)
}

/// 416 for a `Range` that names no byte of a `size`-byte file, with
/// `Content-Range: bytes */<size>`.
fn unsatisfiable(size: u64, ask: &RangeAsk) -> Response {
    let asked = ask.range.as_deref().unwrap_or_default();
    let mut resp = err_json(
        StatusCode::RANGE_NOT_SATISFIABLE,
        "range_not_satisfiable",
        format!(
            "Range '{}' names no byte of this {size}-byte file: a range starts before byte \
             {size}, and a suffix asks at least 1 byte",
            asked.escape_debug()
        ),
    );
    let h = resp.headers_mut();
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(v) = HeaderValue::from_str(&format!("bytes */{size}")) {
        h.insert(header::CONTENT_RANGE, v);
    }
    super::stored::sandboxed(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(method: Method, range: Option<&str>, if_range: Option<&str>) -> RangeAsk {
        let mut h = HeaderMap::new();
        if let Some(r) = range {
            h.insert(header::RANGE, HeaderValue::from_str(r).unwrap());
        }
        if let Some(r) = if_range {
            h.insert(header::IF_RANGE, HeaderValue::from_str(r).unwrap());
        }
        RangeAsk::of(&method, &h)
    }

    #[test]
    fn a_get_range_is_read_and_resolved_against_the_size() {
        let get = |r| ask(Method::GET, Some(r), None).part(100, None);
        assert_eq!(get("bytes=10-19"), Part::Span(10, 19));
        assert_eq!(get("bytes=90-"), Part::Span(90, 99));
        assert_eq!(get("bytes=95-500"), Part::Span(95, 99));
        assert_eq!(get("bytes=-5"), Part::Span(95, 99));
        assert_eq!(get("bytes=-500"), Part::Span(0, 99));
        assert_eq!(get("bytes=100-"), Part::Unsatisfiable);
        assert_eq!(get("bytes=-0"), Part::Unsatisfiable);
    }

    #[test]
    fn an_empty_file_is_served_whole_for_any_range() {
        for r in ["bytes=0-", "bytes=-500", "bytes=0-9", "bytes=5-"] {
            assert_eq!(ask(Method::GET, Some(r), None).part(0, None), Part::Whole);
        }
    }

    #[test]
    fn an_unreadable_if_range_is_no_match() {
        let mut h = HeaderMap::new();
        h.insert(header::RANGE, HeaderValue::from_static("bytes=10-"));
        h.insert(
            header::IF_RANGE,
            HeaderValue::from_bytes(b"\"\xff\"").unwrap(),
        );
        let a = RangeAsk::of(&Method::GET, &h);
        assert_eq!(a.part(100, Some("\"x\"")), Part::Whole);
        assert_eq!(a.part(100, None), Part::Whole);
    }

    #[test]
    fn what_lmgw_does_not_read_serves_the_whole_file() {
        let get = |r| ask(Method::GET, Some(r), None).part(100, None);
        assert_eq!(get("bytes=0-1,5-6"), Part::Whole);
        assert_eq!(get("items=0-1"), Part::Whole);
        assert_eq!(get("bytes=9-3"), Part::Whole);
        assert_eq!(ask(Method::GET, None, None).part(100, None), Part::Whole);
        assert_eq!(
            ask(Method::HEAD, Some("bytes=10-19"), None).part(100, None),
            Part::Whole,
            "Range is a GET's only"
        );
    }

    #[test]
    fn if_range_applies_the_range_only_for_the_own_etag() {
        let e = Some("\"sha256:ab\"");
        let with = |v| ask(Method::GET, Some("bytes=10-"), Some(v));
        assert_eq!(with("\"sha256:ab\"").part(100, e), Part::Span(10, 99));
        assert_eq!(with("\"sha256:cd\"").part(100, e), Part::Whole);
        assert_eq!(with("W/\"sha256:ab\"").part(100, e), Part::Whole);
        assert_eq!(
            with("Tue, 27 Oct 2026 12:00:00 GMT").part(100, e),
            Part::Whole
        );
        assert_eq!(with("\"sha256:ab\"").part(100, None), Part::Whole);
    }
}
