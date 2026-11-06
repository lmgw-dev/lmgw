//! A transfer's bytes in and out (device transfers design §4, §5), streamed
//! between the connection and the file, never held whole.
//!
//! **The upload** (`PUT …/content`, the sender's, while `uploading`) is
//! `upload`.
//!
//! **The download** (`GET …/content`, the receiver's, while `ready`): the
//! whole file, or one `Range`, with `If-Range` against the strong
//! `ETag: "sha256:<hex>"`. A `Range` lmgw does not read — another unit,
//! several ranges, an invalid one — is ignored and the whole file served
//! (RFC 9110 §14.2); one that names no byte of the file is 416 (I27). The
//! body reads the file a block at a time through the downloads' registry,
//! and ends with an error once the transfer ended (I26). `HEAD` answers the
//! whole file's head, whatever `Range` it carries (RFC 9110 §14.2: a
//! server ignores `Range` on any method but `GET`).

mod upload;

pub(super) use upload::upload;

use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use lmgw_api_types::transfer::{self as wire, code, ByteRange, TransferRefusal};

use super::refuse::{answer, internal};
use super::{reached, wrong_state, Part, TransferCaller};
use crate::state::SharedState;
use crate::store::transfers as table;
use crate::transfers;
use crate::web::chat_extract::ChatPath;

/// How often an upload that writes tells the owner's Devices card, at most
/// (§11.1, I13): a progress line, not a log.
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

/// How a download reads the file: a block at a time.
const READ_BLOCK: usize = 256 * 1024;

/// `GET /transfers/api/transfers/{id}/content` (§5).
pub(super) async fn download(
    State(st): State<SharedState>,
    TransferCaller(p): TransferCaller,
    ChatPath(id): ChatPath<String>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = p.device("download files") {
        return r;
    }
    let row = match reached(&st, &p, &id, Part::Receive).await {
        Ok(row) => row,
        Err(r) => return r,
    };
    if row.state != "ready" {
        return wrong_state(&row, "only a ready transfer is downloaded");
    }
    let size = u64::try_from(row.size).unwrap_or(0);
    let etag = wire::etag(&row.sha256);
    if method == Method::HEAD {
        // The whole file's head, `Range` and `If-Range` ignored (RFC 9110
        // §14.2); no file opened.
        return headed(Body::empty(), &row, &etag, size, None);
    }
    // `If-Range` with another validator (another ETag, a date: lmgw keeps
    // no Last-Modified) serves the whole file.
    let range_applies = headers
        .get(header::IF_RANGE)
        .is_none_or(|v| v.to_str().is_ok_and(|v| v.trim() == etag));
    // A header lmgw does not read is ignored (RFC 9110 §14.2, I27): the
    // whole file. One range that names no byte of it is 416.
    let asked = headers
        .get(header::RANGE)
        .filter(|_| range_applies)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| ByteRange::parse(v).map(|r| (v, r)));
    let span = match asked {
        None => None,
        Some((v, r)) => match r.resolve(size) {
            Some(span) => Some(span),
            None => return unsatisfiable(size, v),
        },
    };
    let gone = |now: Option<table::TransferRow>| match now {
        Some(now) if now.state != "ready" => {
            wrong_state(&now, "only a ready transfer is downloaded")
        }
        _ => internal(format!(
            "the file of ready transfer {id} is not on lmgw's disk"
        )),
    };
    let file = match transfers::open_read(&st.data_dir, &id).await {
        Ok(f) => f,
        // An end deleted the bytes since the row was read (I26): the state
        // it left.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return match table::get(&st.db, &id).await {
                Ok(now) => gone(now),
                Err(e) => internal(e.to_string()),
            };
        }
        Err(e) => return internal(format!("opening the transfer's file failed: {e}")),
    };
    // Registered, then the state read again (I26): an end that committed
    // before the registration is seen here, and one after it closes this
    // download.
    let reading = st.transfers.downloads.open(&id, file);
    match table::get(&st.db, &id).await {
        Ok(Some(now)) if now.state == "ready" => {}
        Ok(now) => return gone(now),
        Err(e) => return internal(e.to_string()),
    }
    let (first, length) = match span {
        Some((first, last)) => (first, last - first + 1),
        None => (0, size),
    };
    let stream =
        futures::stream::try_unfold((reading, first, length), |(reading, at, left)| async move {
            if left == 0 {
                return Ok::<_, std::io::Error>(None);
            }
            let want = usize::try_from(left).unwrap_or(READ_BLOCK).min(READ_BLOCK);
            let block = reading.read_at(at, want).await?;
            if block.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the transfer's file is shorter than its size",
                ));
            }
            let n = block.len() as u64;
            Ok(Some((block, (reading, at + n, left - n))))
        });
    headed(Body::from_stream(stream), &row, &etag, size, span)
}

/// The download's answer with `body`: the whole file of `size` bytes, or
/// `span` of it (206).
fn headed(
    body: Body,
    row: &table::TransferRow,
    etag: &str,
    size: u64,
    span: Option<(u64, u64)>,
) -> Response {
    let length = span.map_or(size, |(first, last)| last - first + 1);
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(v) = HeaderValue::from_str(&length.to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    if let Ok(v) = HeaderValue::from_str(etag) {
        h.insert(header::ETAG, v);
    }
    if let Ok(v) = HeaderValue::from_str(&disposition(&row.filename)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Some((first, last)) = span {
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {first}-{last}/{size}")) {
            resp.headers_mut().insert(header::CONTENT_RANGE, v);
        }
    }
    super::super::stored::sandboxed(resp)
}

/// 416 for a `Range` that names no byte of the file (`asked`), with
/// `Content-Range: bytes */<size>`.
fn unsatisfiable(size: u64, asked: &str) -> Response {
    let mut resp = answer(
        StatusCode::RANGE_NOT_SATISFIABLE,
        &TransferRefusal::new(
            code::RANGE_NOT_SATISFIABLE,
            format!(
                "Range '{}' names no byte of this {size}-byte file: a range starts before byte \
                 {size}, and a suffix asks at least 1 byte",
                asked.escape_debug()
            ),
        ),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("bytes */{size}")) {
        resp.headers_mut().insert(header::CONTENT_RANGE, v);
    }
    super::super::stored::sandboxed(resp)
}

/// `Content-Disposition: attachment` with `name` (RFC 6266): an ASCII
/// `filename` with every other character as `_`, and the name whole as
/// `filename*` in UTF-8.
fn disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if (c.is_ascii_graphic() || c == ' ') && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_name_goes_out_in_both_forms() {
        assert_eq!(
            disposition("Spielstand \"1\" – Ö.zip"),
            "attachment; filename=\"Spielstand _1_ _ _.zip\"; \
             filename*=UTF-8''Spielstand%20%221%22%20%E2%80%93%20%C3%96.zip"
        );
    }
}
