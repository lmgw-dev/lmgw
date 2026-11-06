//! Transfers between devices (device transfers design §9, §11.3): a device
//! offers one file to another paired device of the same app, uploads it to
//! lmgw in ranges it can resume, and the other device, once offered it,
//! downloads it with ranges and says it arrived.
//!
//! The sender:
//! 1. [`targets`] (`app_id`): its other devices of the app, `free_bytes`
//!    (the largest offer lmgw takes now) and `expiry_days`;
//! 2. [`create`]: the offer, with the file's size and sha256 — a
//!    [`Transfer`], `uploading`;
//! 3. [`upload`] per range: method, path and headers only — the app sends
//!    the range's bytes as the body itself, streamed from its file — read
//!    with [`read_upload`]; after a break, [`transfer`]'s `received` is
//!    where to go on. The last range answers `ready` (or 422
//!    `transfer_hash_mismatch`: offer again). When `received` already is
//!    the size (the last range's answer was lost), [`upload_status`]
//!    completes it; 409 `transfer_state` with `ready` says it is done.
//!
//! The receiver: [`transfers`] (`Some(TransferRole::Incoming)`) at its
//! start, then the feed's `transfer.updated`
//! ([`crate::feed`], `FeedEvent::TransferUpdated`); [`download`] with an
//! optional [`ByteRange`] and the transfer's [`etag`] as `If-Range` to
//! resume (its head read with [`read_download`]); its own hash check;
//! [`received`] (or [`decline`]).
//!
//! Every refusal reads with [`read_transfer_refusal`] as a
//! [`TransferRefusal`]: the code ([`code`]) and the extra field it names
//! (`received` on 409 `transfer_offset`, `state` on 409 `transfer_state`,
//! `free_bytes` on 507 `transfer_storage_full`).

pub use lmgw_api_types::transfer::{
    app_id_refusal, code, content_range, etag, is_sha256_hex, parse_content_range,
    parse_upload_status_range, upload_status_range, ByteRange, ContentReceived, Target, Targets,
    Transfer, TransferCreate, TransferList, TransferParty, TransferRefusal, TransferRole,
    TransferState, APP_ID_RULE, BODY_LIMIT_BYTES, TARGETS_PATH, TRANSFERS_PATH,
};

use super::{Method, Request};

/// `GET /transfers/api/targets?app_id=<app_id>`; read with
/// [`read_targets`].
pub fn targets(app_id: &str) -> Request {
    Request::new(
        Method::Get,
        format!("{TARGETS_PATH}?app_id={}", encode(app_id)),
    )
}

/// `POST /transfers/api/transfers`; read with [`read_transfer`] (201).
pub fn create(offer: &TransferCreate) -> Request {
    Request::new(Method::Post, TRANSFERS_PATH.into())
        .json(serde_json::to_string(offer).expect("an offer always serializes"))
}

/// `GET /transfers/api/transfers[?role=…]`; read with [`read_transfers`].
pub fn transfers(role: Option<TransferRole>) -> Request {
    let path = match role {
        Some(r) => format!("{TRANSFERS_PATH}?role={}", r.as_str()),
        None => TRANSFERS_PATH.into(),
    };
    Request::new(Method::Get, path)
}

/// `GET /transfers/api/transfers/{id}`: the transfer now, its resume point
/// in `received`; read with [`read_transfer`].
pub fn transfer(id: &str) -> Request {
    Request::new(Method::Get, format!("{TRANSFERS_PATH}/{}", encode(id)))
}

/// `PUT /transfers/api/transfers/{id}/content` for the `len` bytes from
/// byte `first` of a file of `size` bytes: `Content-Range`, `Content-Length`
/// and `Content-Type: application/octet-stream`, and no body — the app
/// sends exactly those bytes as the body, streamed from its file. `first`
/// is the transfer's `received`. Read with [`read_upload`]. `None` for a
/// range that is not in the file (`len` 0, or past `size`).
pub fn upload(id: &str, first: u64, len: u64, size: u64) -> Option<Request> {
    let last = first.checked_add(len)?.checked_sub(1)?;
    if len == 0 || last >= size {
        return None;
    }
    let mut r = Request::new(
        Method::Put,
        format!("{TRANSFERS_PATH}/{}/content", encode(id)),
    )
    .header("Content-Range", &content_range(first, last, size))
    .header("Content-Length", &len.to_string());
    r = r.header("Content-Type", "application/octet-stream");
    Some(r)
}

/// `PUT /transfers/api/transfers/{id}/content` with `Content-Range: bytes
/// */<size>` and no body: where the upload of a file of `size` bytes
/// stands (`received`, `uploading`), and, when lmgw holds every byte, its
/// completion — `ready`, or 422 `transfer_hash_mismatch`. It takes the
/// upload's place as an upload does, so it stops one of this transfer that
/// still runs: ask it once the own upload's answer was lost, not beside
/// one. Read with [`read_upload`].
pub fn upload_status(id: &str, size: u64) -> Request {
    Request::new(
        Method::Put,
        format!("{TRANSFERS_PATH}/{}/content", encode(id)),
    )
    .header("Content-Range", &upload_status_range(size))
    .header("Content-Length", "0")
}

/// `GET /transfers/api/transfers/{id}/content`: the whole file, or `range`
/// of it; with `if_range` (the transfer's [`etag`]), a range only while the
/// bytes are the ones that ETag names — the whole file otherwise. Read its
/// head with [`read_download`]; the body is the bytes.
pub fn download(id: &str, range: Option<ByteRange>, if_range: Option<&str>) -> Request {
    let mut r = Request::new(
        Method::Get,
        format!("{TRANSFERS_PATH}/{}/content", encode(id)),
    );
    if let Some(range) = range {
        r = r.header("Range", &range.header());
        if let Some(tag) = if_range {
            r = r.header("If-Range", tag);
        }
    }
    r
}

/// `POST /transfers/api/transfers/{id}/received`, after the receiver's own
/// hash check: delivered, lmgw deletes the bytes. Read with
/// [`read_transfer`].
pub fn received(id: &str) -> Request {
    action(id, "received")
}

/// `POST /transfers/api/transfers/{id}/decline`: the receiver does not
/// want it. Read with [`read_transfer`].
pub fn decline(id: &str) -> Request {
    action(id, "decline")
}

/// `POST /transfers/api/transfers/{id}/cancel`: the sender ends it. Read
/// with [`read_transfer`].
pub fn cancel(id: &str) -> Request {
    action(id, "cancel")
}

fn action(id: &str, what: &str) -> Request {
    Request::new(
        Method::Post,
        format!("{TRANSFERS_PATH}/{}/{what}", encode(id)),
    )
}

/// `s` percent-encoded for a path segment or a query value: RFC 3986's
/// unreserved characters kept.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A transfers route's refusal: the flat `{code, message}` with its extra
/// field. A body that is not one is `http_<status>` with the body as the
/// message.
pub fn read_transfer_refusal(status: u16, body: &str) -> TransferRefusal {
    match serde_json::from_str::<TransferRefusal>(body) {
        Ok(r) if !r.code.is_empty() => r,
        _ => {
            let e = super::read_refusal(status, body);
            TransferRefusal::new(&e.code, e.message)
        }
    }
}

/// `T` from a 2xx answer, or the refusal.
fn read<T: serde::de::DeserializeOwned>(
    status: u16,
    body: &str,
    what: &str,
) -> Result<T, TransferRefusal> {
    if !(200..300).contains(&status) {
        return Err(read_transfer_refusal(status, body));
    }
    serde_json::from_str(body).map_err(|e| {
        TransferRefusal::new(
            "unreadable",
            format!("the answer does not read as {what}: {e}"),
        )
    })
}

/// [`targets`]' answer.
pub fn read_targets(status: u16, body: &str) -> Result<Targets, TransferRefusal> {
    read(status, body, "Targets")
}

/// The answer of [`create`], [`transfer`], [`received`], [`decline`] and
/// [`cancel`].
pub fn read_transfer(status: u16, body: &str) -> Result<Transfer, TransferRefusal> {
    read(status, body, "Transfer")
}

/// [`transfers`]' answer.
pub fn read_transfers(status: u16, body: &str) -> Result<TransferList, TransferRefusal> {
    read(status, body, "TransferList")
}

/// [`upload`]'s answer: how many bytes lmgw holds now and the state —
/// `ready` once the last range arrived and matched.
pub fn read_upload(status: u16, body: &str) -> Result<ContentReceived, TransferRefusal> {
    read(status, body, "ContentReceived")
}

/// What a download's head says (its status, and its `Content-Range` and
/// `ETag` headers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    /// The first and last byte of the body, both inclusive.
    pub first: u64,
    pub last: u64,
    /// The file's size.
    pub size: u64,
    /// `false` for the whole file (200), `true` for a range (206).
    pub partial: bool,
    /// The bytes' ETag (`"sha256:<hex>"`).
    pub etag: Option<String>,
}

/// [`download`]'s head: on 200 the whole file of `content_length` bytes, on
/// 206 the range its `Content-Range` names; any other status the refusal
/// its `body` carries (416 `range_not_satisfiable`, 409 `transfer_state`,
/// …). A 206 without a readable `Content-Range` is `unreadable`.
pub fn read_download(
    status: u16,
    content_range: Option<&str>,
    content_length: Option<u64>,
    etag: Option<&str>,
    body: &str,
) -> Result<Download, TransferRefusal> {
    let etag = etag.map(str::to_string);
    match status {
        200 => {
            let size = content_length.unwrap_or(0);
            Ok(Download {
                first: 0,
                last: size.saturating_sub(1),
                size,
                partial: false,
                etag,
            })
        }
        206 => {
            let Some((first, last, size)) = content_range.and_then(parse_content_range) else {
                return Err(TransferRefusal::new(
                    "unreadable",
                    format!("a 206 whose Content-Range is not bytes a-b/size: {content_range:?}"),
                ));
            };
            Ok(Download {
                first,
                last,
                size,
                partial: true,
                etag,
            })
        }
        _ => Err(read_transfer_refusal(status, body)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header<'a>(r: &'a Request, name: &str) -> Option<&'a str> {
        r.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    #[test]
    fn the_routes_build_with_their_methods_paths_and_headers() {
        let id = "0123456789abcdef0123456789abcdef";
        let r = targets("dev.example.storyplayer");
        assert_eq!(
            (r.method, r.path.as_str()),
            (
                Method::Get,
                "/transfers/api/targets?app_id=dev.example.storyplayer"
            )
        );
        let offer = TransferCreate {
            to_device: 7,
            app_id: "dev.example.storyplayer".into(),
            kind: "playthrough".into(),
            title: "Chapter 3".into(),
            filename: "save.zip".into(),
            size: 10,
            sha256: "ab".repeat(32),
        };
        let r = create(&offer);
        assert_eq!((r.method, r.path.as_str()), (Method::Post, TRANSFERS_PATH));
        let back: TransferCreate = serde_json::from_str(r.body.as_deref().unwrap()).unwrap();
        assert_eq!(back, offer);
        assert_eq!(
            transfers(Some(TransferRole::Incoming)).path,
            "/transfers/api/transfers?role=incoming"
        );
        assert_eq!(transfers(None).path, TRANSFERS_PATH);
        assert_eq!(transfer(id).path, format!("{TRANSFERS_PATH}/{id}"));

        let r = upload(id, 4, 6, 10).unwrap();
        assert_eq!(r.method.as_str(), "PUT");
        assert_eq!(r.path, format!("{TRANSFERS_PATH}/{id}/content"));
        assert_eq!(header(&r, "content-range"), Some("bytes 4-9/10"));
        assert_eq!(header(&r, "content-length"), Some("6"));
        assert!(r.body.is_none(), "the app streams the bytes itself");
        assert!(upload(id, 4, 7, 10).is_none(), "past the end");
        assert!(upload(id, 0, 0, 10).is_none(), "an empty range");
        let r = upload_status(id, 10);
        assert_eq!(
            (r.method, r.path.as_str()),
            (
                Method::Put,
                format!("{TRANSFERS_PATH}/{id}/content").as_str()
            )
        );
        assert_eq!(header(&r, "content-range"), Some("bytes */10"));
        assert_eq!(header(&r, "content-length"), Some("0"));
        assert!(r.body.is_none());

        let tag = etag(&"ab".repeat(32));
        let r = download(id, Some(ByteRange::From(100)), Some(&tag));
        assert_eq!(header(&r, "range"), Some("bytes=100-"));
        assert_eq!(header(&r, "if-range"), Some(tag.as_str()));
        let r = download(id, None, Some(&tag));
        assert!(
            header(&r, "if-range").is_none(),
            "If-Range goes with a range"
        );
        for (r, tail) in [
            (received(id), "received"),
            (decline(id), "decline"),
            (cancel(id), "cancel"),
        ] {
            assert_eq!(r.method, Method::Post);
            assert_eq!(r.path, format!("{TRANSFERS_PATH}/{id}/{tail}"));
        }
    }

    #[test]
    fn the_answers_and_every_refusal_read() {
        let t = read_transfer(
            201,
            r#"{"id":"0f","state":"uploading","received":0,"size":10,
                "from":{"device_id":3,"name":"desktop"},"to":{"device_id":7,"name":"phone"}}"#,
        )
        .unwrap();
        assert_eq!((t.state, t.to.device_id), (TransferState::Uploading, 7));
        let u = read_upload(200, r#"{"received":10,"state":"ready"}"#).unwrap();
        assert_eq!(u.state, TransferState::Ready);
        let l = read_transfers(200, r#"{"transfers":[],"stored_bytes":0,"free_bytes":9}"#).unwrap();
        assert_eq!(l.free_bytes, 9);
        let g = read_targets(
            200,
            r#"{"targets":[{"device_id":7,"name":"phone","online":true,"last_seen_at":null}],
                "free_bytes":5,"expiry_days":7}"#,
        )
        .unwrap();
        assert!(g.targets[0].online);

        let e = read_upload(
            409,
            r#"{"code":"transfer_offset","message":"m","received":4}"#,
        )
        .unwrap_err();
        assert_eq!(
            (e.code.as_str(), e.received),
            (code::TRANSFER_OFFSET, Some(4))
        );
        let e = read_transfer(
            409,
            r#"{"code":"transfer_state","message":"m","state":"expired"}"#,
        )
        .unwrap_err();
        assert_eq!(e.state, Some(TransferState::Expired));
        let e = read_transfer(
            507,
            r#"{"code":"transfer_storage_full","message":"m","free_bytes":12}"#,
        )
        .unwrap_err();
        assert_eq!(e.free_bytes, Some(12));
        for c in [
            code::TRANSFER_NOT_FOUND,
            code::TRANSFER_TARGET_INVALID,
            code::TRANSFER_DEVICE_ONLY,
            code::TRANSFER_HASH_MISMATCH,
            code::BAD_REQUEST,
        ] {
            let body = format!(r#"{{"code":"{c}","message":"m"}}"#);
            assert_eq!(read_transfer(400, &body).unwrap_err().code, c);
        }
        let e = read_transfer(502, "Bad Gateway").unwrap_err();
        assert_eq!(
            (e.code.as_str(), e.message.as_str()),
            ("http_502", "Bad Gateway")
        );
    }

    #[test]
    fn a_download_head_reads_its_range() {
        let d = read_download(
            206,
            Some("bytes 100-199/1000"),
            Some(100),
            Some("\"t\""),
            "",
        )
        .unwrap();
        assert_eq!((d.first, d.last, d.size, d.partial), (100, 199, 1000, true));
        let d = read_download(200, None, Some(1000), None, "").unwrap();
        assert_eq!((d.first, d.last, d.partial), (0, 999, false));
        let e = read_download(
            416,
            Some("bytes */1000"),
            None,
            None,
            r#"{"code":"range_not_satisfiable","message":"m"}"#,
        )
        .unwrap_err();
        assert_eq!(e.code, code::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            read_download(206, None, None, None, "").unwrap_err().code,
            "unreadable"
        );
    }
}
