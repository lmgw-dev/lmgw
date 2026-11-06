//! Device-to-device transfers (device transfers design §9): `/transfers/api`'s
//! bodies, answers and refusal codes, the app id's rule and the content
//! routes' header forms. Shared so the gateway, which serves and refuses,
//! and `lmgw-client`, which builds and reads, agree on every byte.
//!
//! A transfer moves one file from a paired device (the sender) to another
//! paired device of the same app (the receiver), through lmgw's disk: the
//! sender offers it and uploads it, lmgw checks its sha256 and offers it to
//! the receiver, the receiver downloads it and says it got it.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The routes' prefix.
pub const PREFIX: &str = "/transfers/api";

/// `GET` (`?app_id=`): who the caller can offer a file to.
pub const TARGETS_PATH: &str = "/transfers/api/targets";

/// `POST` an offer, `GET` the list; one transfer at `/{id}`, its bytes at
/// `/{id}/content`.
pub const TRANSFERS_PATH: &str = "/transfers/api/transfers";

/// The largest JSON body a `/transfers/api` route takes, in bytes: 16 KiB,
/// refused 413 [`code::BODY_TOO_LARGE`] naming it. The content route's body
/// is the file, bounded by the free space alone (§3).
pub const BODY_LIMIT_BYTES: usize = 16 * 1024;

/// The longest app id: a DNS name's length (§2).
pub const APP_ID_MAX_LEN: usize = 253;

/// The app id's rule in one sentence, as a refusal states it (§2).
pub const APP_ID_RULE: &str = "an app id is a reverse-DNS name in lowercase: two or more labels \
    separated by dots, each starting with a letter a-z and then letters, digits, '-' or '_' \
    (dev.example.storyplayer), at most 253 characters";

/// Why `app_id` is no app id, naming [`APP_ID_RULE`]; `None` for one.
pub fn app_id_refusal(app_id: &str) -> Option<String> {
    let label_ok = |l: &str| {
        let mut b = l.bytes();
        b.next().is_some_and(|c| c.is_ascii_lowercase())
            && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
    };
    let ok = app_id.len() <= APP_ID_MAX_LEN
        && app_id.split('.').count() >= 2
        && app_id.split('.').all(label_ok);
    (!ok).then(|| format!("'{app_id}' is no app id: {APP_ID_RULE}"))
}

/// Whether `s` is a sha256 as an offer names it: 64 lowercase hex digits.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The `ETag` a transfer's bytes carry once `ready` (§5): strong, quoted,
/// `"sha256:<hex>"`. What `If-Range` names to resume a download.
pub fn etag(sha256: &str) -> String {
    format!("\"sha256:{sha256}\"")
}

/// The refusal codes of `/transfers/api` (§9.4). Each is answered in the
/// flat `{code, message}` shape, [`TransferRefusal`], with the extra field
/// its doc names.
pub mod code {
    /// `400`: a malformed field, query or `Content-Range`, a
    /// `Content-Length` other than the range's, a body shorter or longer
    /// than its range, a `role` from the owner. (A `Content-Length` that is
    /// not a number never reaches the routes: hyper refuses it first with a
    /// bare 400 and no body.)
    pub const BAD_REQUEST: &str = "bad_request";
    /// `403`: a plain API key or an agent token on any route, or the owner
    /// on a device's route (targets, offer, upload, download, received,
    /// decline). Only paired devices send and receive.
    pub const TRANSFER_DEVICE_ONLY: &str = "transfer_device_only";
    /// `404`: no such transfer, or none this caller reaches that way.
    pub const TRANSFER_NOT_FOUND: &str = "transfer_not_found";
    /// `409`, with `received`: an upload whose range does not start at the
    /// resume point, or one a newer upload of the same transfer stopped.
    pub const TRANSFER_OFFSET: &str = "transfer_offset";
    /// `409`, with `state`: the transfer is not in a state for this; also
    /// an upload an end stopped (a cancel, an expiry, a device's end), with
    /// the state it left. `ready` on an upload says it is done.
    pub const TRANSFER_STATE: &str = "transfer_state";
    /// `413`: a JSON body over [`super::BODY_LIMIT_BYTES`].
    pub const BODY_TOO_LARGE: &str = "body_too_large";
    /// `416`, with `Content-Range: bytes */<size>`: a download's one byte
    /// range that names no byte of the file (it starts at or past the end,
    /// or a suffix of 0 bytes). A `Range` the gateway does not read is
    /// ignored instead: the whole file, 200.
    pub const RANGE_NOT_SATISFIABLE: &str = "range_not_satisfiable";
    /// `422`: the offer's `to_device` is no target for its `app_id`; the
    /// message says which part failed.
    pub const TRANSFER_TARGET_INVALID: &str = "transfer_target_invalid";
    /// `422`: the uploaded bytes' sha256 is not the offer's; the transfer is
    /// `failed` and its bytes deleted.
    pub const TRANSFER_HASH_MISMATCH: &str = "transfer_hash_mismatch";
    /// `507`, with `free_bytes`: the offer is larger than the space left for
    /// new offers, or an upload met a full disk.
    pub const TRANSFER_STORAGE_FULL: &str = "transfer_storage_full";
}

/// A transfer's state (§1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum TransferState {
    /// Offered; the sender uploads its bytes. The receiver does not see it.
    Uploading,
    /// Every byte arrived and its sha256 matched: offered to the receiver.
    Ready,
    /// The receiver said it got the bytes; they are deleted.
    Delivered,
    /// The receiver declined it; the bytes are deleted.
    Declined,
    /// The sender or the owner canceled it, or a party device was disabled
    /// or deleted; the bytes are deleted.
    Canceled,
    /// Not delivered by `expires_at`; the bytes are deleted.
    Expired,
    /// The uploaded bytes' sha256 was not the offer's; they are deleted.
    Failed,
    /// A state this build does not know (a newer gateway's), as sent; empty
    /// where none was sent.
    #[serde(untagged)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unknown(String),
}

impl Default for TransferState {
    /// None was sent: an empty [`TransferState::Unknown`].
    fn default() -> Self {
        Self::Unknown(String::new())
    }
}

impl TransferState {
    /// The wire token, `uploading` … `failed`, or the unknown one as sent.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Uploading => "uploading",
            Self::Ready => "ready",
            Self::Delivered => "delivered",
            Self::Declined => "declined",
            Self::Canceled => "canceled",
            Self::Expired => "expired",
            Self::Failed => "failed",
            Self::Unknown(v) => v,
        }
    }

    /// The state a token names; `None` for none of the seven.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "uploading" => Self::Uploading,
            "ready" => Self::Ready,
            "delivered" => Self::Delivered,
            "declined" => Self::Declined,
            "canceled" => Self::Canceled,
            "expired" => Self::Expired,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    /// Whether it is in progress (`uploading`, `ready`): lmgw holds bytes
    /// for it, and it can still end.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Uploading | Self::Ready)
    }
}

impl fmt::Display for TransferState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One side of a transfer: the device by its key id, as the owner's
/// `KeyRow` shows it, and its paired name without `device:`, as it was when
/// the offer was made.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TransferParty {
    pub device_id: i64,
    pub name: String,
}

/// A transfer, as every route and `transfer.updated` answer it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Transfer {
    /// Opaque: 32 random lowercase hex digits.
    pub id: String,
    /// The app id the offer names; the receiver declared it at pairing.
    pub app_id: String,
    /// The app's own word for what the file is.
    pub kind: String,
    /// What the card and the receiver show; may be empty.
    pub title: String,
    /// The receiver's name for the file.
    pub filename: String,
    /// The file's size in bytes.
    pub size: u64,
    /// The file's sha256, 64 lowercase hex digits; checked when the last
    /// byte arrives.
    pub sha256: String,
    pub from: TransferParty,
    pub to: TransferParty,
    pub state: TransferState,
    /// Bytes lmgw holds: the resume point while `uploading` (the next
    /// upload's range starts here), `size` from `ready` on, and how far it
    /// got for one that ended before.
    pub received: u64,
    /// RFC 3339 UTC.
    pub created_at: String,
    /// When it expires unless delivered first (RFC 3339 UTC):
    /// `devices.transfer_expiry_days` after the offer.
    pub expires_at: String,
    /// When it ended (RFC 3339 UTC); `null` while in progress.
    pub ended_at: Option<String>,
    /// Why it ended, a sentence for people ("not delivered within 7 days of
    /// the offer", "canceled by device 'phone'"), naming no setting and no
    /// raw time; `null` while in progress and once delivered. `state` is
    /// what a client acts on.
    pub end_reason: Option<String>,
}

/// `GET /transfers/api/targets`' query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TargetsQuery {
    /// The app id the offer will name ([`APP_ID_RULE`]).
    pub app_id: String,
}

/// A device the caller can offer a file to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Target {
    /// Its key id: the offer's `to_device`.
    pub device_id: i64,
    /// Its paired name, without `device:`.
    pub name: String,
    /// Whether it holds its Chat feed open now, so it hears an offer at
    /// once.
    pub online: bool,
    /// When one of its connections last opened or closed (RFC 3339 UTC);
    /// `null` for one that never connected.
    pub last_seen_at: Option<String>,
}

/// `GET /transfers/api/targets`' answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Targets {
    /// The caller's other devices that declared the app id and can receive
    /// now (enabled, confirmed, not expired); never the caller.
    pub targets: Vec<Target>,
    /// The largest offer lmgw takes now, in bytes: the data directory's
    /// free space less what the uploads in progress still need. The only
    /// size bound there is.
    pub free_bytes: u64,
    /// Days an offer made now lasts (`devices.transfer_expiry_days`).
    pub expiry_days: u32,
}

/// `POST /transfers/api/transfers`' body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TransferCreate {
    /// The receiver: a target's `device_id` for `app_id`.
    pub to_device: i64,
    /// The app id the receiver declared ([`APP_ID_RULE`]).
    pub app_id: String,
    /// The app's own word for what the file is (required; no control or
    /// invisible characters).
    pub kind: String,
    /// What the card and the receiver show (may be empty).
    #[serde(default)]
    pub title: String,
    /// The receiver's name for the file: no `/`, `\`, control or invisible
    /// characters, not `.` or `..`.
    pub filename: String,
    /// The file's size in bytes: at least 1, at most `free_bytes`.
    pub size: u64,
    /// The file's sha256: 64 lowercase hex digits.
    pub sha256: String,
}

/// `PUT /transfers/api/transfers/{id}/content`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContentReceived {
    /// Bytes lmgw holds now: where the next range starts.
    pub received: u64,
    /// `uploading`, or `ready` once the last byte arrived and matched.
    pub state: TransferState,
}

/// `GET /transfers/api/transfers`' `role` (a device's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum TransferRole {
    /// Offered to the caller (from `ready` on).
    Incoming,
    /// Offered by the caller.
    Outgoing,
}

impl TransferRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Incoming => "incoming",
            Self::Outgoing => "outgoing",
        }
    }
}

/// `GET /transfers/api/transfers`' query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TransferListQuery {
    /// A device's: only its incoming or its outgoing transfers; both
    /// without. The owner is no party, and is refused one.
    #[serde(default)]
    pub role: Option<TransferRole>,
}

/// `GET /transfers/api/transfers`' answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TransferList {
    /// Newest first, the ended ones included for as long as
    /// `chat_feed_retention_days` keeps them.
    pub transfers: Vec<Transfer>,
    /// Bytes the files of the transfers in progress hold on disk now.
    pub stored_bytes: u64,
    /// The largest offer lmgw takes now ([`Targets::free_bytes`]).
    pub free_bytes: u64,
}

/// A `/transfers/api` refusal: the flat `{code, message}` with the extra
/// field its code names ([`code`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TransferRefusal {
    pub code: String,
    pub message: String,
    /// [`code::TRANSFER_STORAGE_FULL`]'s: the space left for new offers.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub free_bytes: Option<u64>,
    /// [`code::TRANSFER_OFFSET`]'s: the resume point.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub received: Option<u64>,
    /// [`code::TRANSFER_STATE`]'s: the state the transfer is in.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub state: Option<TransferState>,
}

impl TransferRefusal {
    /// A refusal with `code` and `message` and no extra field.
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            ..Self::default()
        }
    }
}

/// One byte range of a download's `Range` (RFC 9110 §14.1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    /// `bytes=a-`: from `a` to the end.
    From(u64),
    /// `bytes=a-b`, both ends inclusive.
    Span(u64, u64),
    /// `bytes=-n`: the last `n` bytes.
    Suffix(u64),
}

impl ByteRange {
    /// The `Range` header's value.
    pub fn header(self) -> String {
        match self {
            Self::From(a) => format!("bytes={a}-"),
            Self::Span(a, b) => format!("bytes={a}-{b}"),
            Self::Suffix(n) => format!("bytes=-{n}"),
        }
    }

    /// One `Range` header's single range; `None` for a header the gateway
    /// ignores and answers with the whole file (RFC 9110 §14.2): another
    /// unit, several ranges, an invalid one (its last byte before its
    /// first), a form that does not parse. The unit
    /// is read case-insensitively, and empty list elements are skipped
    /// (RFC 9110 §5.6.1). A range that parses may still name no byte of
    /// the file: [`ByteRange::resolve`]. A number past `u64` is read as
    /// `u64::MAX`: a last position past the end of any file clips to it
    /// (RFC 9110 §14.1.2), a first position past it names no byte, and a
    /// suffix longer than any file is the whole file.
    pub fn parse(header: &str) -> Option<Self> {
        let (unit, set) = header.trim().split_once('=')?;
        if !unit.eq_ignore_ascii_case("bytes") {
            return None;
        }
        let mut specs = set.split(',').map(str::trim).filter(|s| !s.is_empty());
        let spec = specs.next()?;
        if specs.next().is_some() {
            return None;
        }
        let (a, b) = spec.split_once('-')?;
        let num = |s: &str| {
            let s = s.trim();
            (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
                .then(|| s.parse::<u64>().unwrap_or(u64::MAX))
        };
        match (a.trim().is_empty(), b.trim().is_empty()) {
            (true, false) => num(b).map(Self::Suffix),
            (false, true) => num(a).map(Self::From),
            (false, false) => {
                let (a, b) = (num(a)?, num(b)?);
                (a <= b).then_some(Self::Span(a, b))
            }
            (true, true) => None,
        }
    }

    /// The first and last byte it names in a file of `size` bytes, both
    /// inclusive; `None` when it names none (it starts at or past the end,
    /// or asks the last 0 bytes).
    pub fn resolve(self, size: u64) -> Option<(u64, u64)> {
        if size == 0 {
            return None;
        }
        let last = size - 1;
        match self {
            Self::From(a) => (a <= last).then_some((a, last)),
            Self::Span(a, b) => (a <= last).then_some((a, b.min(last))),
            Self::Suffix(0) => None,
            Self::Suffix(n) => Some((size.saturating_sub(n), last)),
        }
    }
}

/// An upload's `Content-Range`, `bytes a-b/size` (RFC 9110 §14.4): the
/// first and last byte of the body's range, both inclusive, and the file's
/// size.
pub fn content_range(first: u64, last: u64, size: u64) -> String {
    format!("bytes {first}-{last}/{size}")
}

/// The `Content-Range` of an upload that asks where it stands (device
/// transfers design §4, I25): `bytes */<size>` with no body — the resume
/// point, and the upload completed when the gateway holds every byte.
pub fn upload_status_range(size: u64) -> String {
    format!("bytes */{size}")
}

/// [`upload_status_range`]'s size; `None` for any other form.
pub fn parse_upload_status_range(header: &str) -> Option<u64> {
    let size = header
        .trim()
        .strip_prefix("bytes ")?
        .trim()
        .strip_prefix("*/")?;
    let size = size.trim();
    (!size.is_empty() && size.bytes().all(|c| c.is_ascii_digit()))
        .then(|| size.parse::<u64>().ok())
        .flatten()
}

/// [`content_range`]'s three numbers; `None` for any other form (an
/// unsatisfied `bytes */size`, another unit, `first > last`, `last` past
/// the size).
pub fn parse_content_range(header: &str) -> Option<(u64, u64, u64)> {
    let rest = header.trim().strip_prefix("bytes ")?.trim();
    let (range, size) = rest.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    let num = |s: &str| {
        let s = s.trim();
        (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    let (a, b, size) = (num(a)?, num(b)?, num(size)?);
    (a <= b && b < size).then_some((a, b, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_app_id_is_a_lowercase_reverse_dns_name() {
        for ok in [
            "dev.example.storyplayer",
            "a.b",
            "org.example.app_2",
            "dev.example.my-game",
        ] {
            assert_eq!(app_id_refusal(ok), None, "{ok}");
        }
        for bad in [
            "",
            "storyplayer",
            "Dev.Example.App",
            "dev..example",
            ".dev.example",
            "dev.example.",
            "dev.9example",
            "dev.example app",
            "dev.exämple",
        ] {
            let e = app_id_refusal(bad).unwrap_or_else(|| panic!("{bad:?} passed"));
            assert!(e.ends_with(APP_ID_RULE), "{e}");
        }
        let long = format!("a.{}", "b".repeat(APP_ID_MAX_LEN - 1));
        assert!(app_id_refusal(&long).is_some());
        assert!(app_id_refusal(&long[..APP_ID_MAX_LEN]).is_none());
    }

    #[test]
    fn a_sha256_is_64_lowercase_hex_digits() {
        assert!(is_sha256_hex(&"a1".repeat(32)));
        assert!(!is_sha256_hex(&"A1".repeat(32)));
        assert!(!is_sha256_hex(&"a1".repeat(31)));
        assert!(!is_sha256_hex(&"g1".repeat(32)));
    }

    #[test]
    fn a_range_reads_one_span_and_resolves_against_the_size() {
        assert_eq!(ByteRange::parse("bytes=0-99"), Some(ByteRange::Span(0, 99)));
        assert_eq!(ByteRange::parse("bytes=100-"), Some(ByteRange::From(100)));
        assert_eq!(ByteRange::parse("bytes=-5"), Some(ByteRange::Suffix(5)));
        assert_eq!(ByteRange::parse("Bytes=0-9"), Some(ByteRange::Span(0, 9)));
        assert_eq!(
            ByteRange::parse("bytes= 0-9 ,"),
            Some(ByteRange::Span(0, 9))
        );
        assert_eq!(ByteRange::parse("bytes=-0"), Some(ByteRange::Suffix(0)));
        // Ignored: the whole file.
        for ignored in [
            "bytes=5-1",
            "bytes=0-1,4-5",
            "items=0-1",
            "bytes=-",
            "bytes=a-b",
            // A first position past u64 is past the last byte, and past a
            // last position that still fits.
            "bytes=99999999999999999999999-5",
            "bytes=",
            "0-1",
        ] {
            assert_eq!(ByteRange::parse(ignored), None, "{ignored}");
        }
        // A number past u64 is u64::MAX: the last position clips to the
        // file, the first position names no byte of it.
        assert_eq!(
            ByteRange::parse("bytes=0-99999999999999999999999"),
            Some(ByteRange::Span(0, u64::MAX))
        );
        assert_eq!(
            ByteRange::parse("bytes=99999999999999999999999-"),
            Some(ByteRange::From(u64::MAX))
        );
        assert_eq!(
            ByteRange::parse("bytes=-99999999999999999999999"),
            Some(ByteRange::Suffix(u64::MAX))
        );
        assert_eq!(ByteRange::Span(5, u64::MAX).resolve(100), Some((5, 99)));
        assert_eq!(ByteRange::From(u64::MAX).resolve(100), None);
        assert_eq!(ByteRange::Suffix(u64::MAX).resolve(100), Some((0, 99)));
        for r in [
            ByteRange::Span(0, 99),
            ByteRange::From(100),
            ByteRange::Suffix(5),
        ] {
            assert_eq!(ByteRange::parse(&r.header()), Some(r));
        }
        assert_eq!(ByteRange::Span(5, 1000).resolve(10), Some((5, 9)));
        assert_eq!(ByteRange::From(10).resolve(10), None);
        assert_eq!(ByteRange::Suffix(20).resolve(10), Some((0, 9)));
        assert_eq!(ByteRange::Suffix(0).resolve(10), None);
        assert_eq!(ByteRange::Span(0, 0).resolve(0), None);
    }

    #[test]
    fn a_content_range_reads_back_as_it_is_written() {
        assert_eq!(content_range(0, 9, 10), "bytes 0-9/10");
        assert_eq!(parse_content_range("bytes 0-9/10"), Some((0, 9, 10)));
        assert_eq!(parse_content_range(" bytes 5-5/6 "), Some((5, 5, 6)));
        assert_eq!(upload_status_range(10), "bytes */10");
        assert_eq!(parse_upload_status_range(" bytes */10 "), Some(10));
        for bad in ["bytes 0-9/10", "bytes */", "bytes */x", "items */10"] {
            assert_eq!(parse_upload_status_range(bad), None, "{bad}");
        }
        for bad in [
            "bytes */10",
            "bytes 0-10/10",
            "bytes 5-4/10",
            "items 0-1/2",
            "bytes 0-1",
        ] {
            assert_eq!(parse_content_range(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_state_reads_its_token_and_keeps_an_unknown_one() {
        let s: TransferState = serde_json::from_str("\"ready\"").unwrap();
        assert_eq!(s, TransferState::Ready);
        let s: TransferState = serde_json::from_str("\"paused\"").unwrap();
        assert_eq!(s, TransferState::Unknown("paused".into()));
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"paused\"");
        assert_eq!(
            TransferState::parse("canceled"),
            Some(TransferState::Canceled)
        );
        assert!(TransferState::Ready.is_active() && !TransferState::Failed.is_active());
        // A refusal carries only the field its code names.
        let r = TransferRefusal {
            received: Some(4),
            ..TransferRefusal::new(code::TRANSFER_OFFSET, "no")
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"code":"transfer_offset","message":"no","received":4}"#
        );
        assert_eq!(
            etag(&"ab".repeat(32)),
            format!("\"sha256:{}\"", "ab".repeat(32))
        );
    }
}
