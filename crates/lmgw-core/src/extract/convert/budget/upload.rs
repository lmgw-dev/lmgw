//! A conversion request's body (security round A review, B1): how much of
//! the answers' bound it reserves before it is read, and how long it may
//! take to arrive.
//!
//! - **Reserved before reading** ([`request_share`]): the body's declared
//!   length (`content-length`; twice it for JSON, whose text and parsed value
//!   sit side by side while it is read) plus the request's output budget
//!   (`answers::Answers::output_budget`). A body sent without a length
//!   (chunked) reserves the most `max_body_mb` allows in its place.
//! - **Read deadline** ([`read_deadline`]): [`UPLOAD_READ_GRACE_S`] plus one
//!   second for every [`UPLOAD_MIN_RATE_BYTES_PER_S`] of the declared length
//!   (or of `max_body_mb` without one). A body still arriving past it is
//!   refused with 408 [`ReadDeadline::CODE`], naming both, and its share
//!   goes back: a client sending one byte every few seconds no longer holds
//!   a share for as long as it likes.

use std::time::Duration;

use axum::http::{header, HeaderMap};

/// A request's body arrives within this much time ...
pub const UPLOAD_READ_GRACE_S: u64 = 60;

/// ... plus one second for every this many bytes of it (256 KiB/s, a
/// quarter of the answers' write rate: uploads are often the slower half
/// of a link).
pub const UPLOAD_MIN_RATE_BYTES_PER_S: u64 = 256 << 10;

/// The body's declared length, when it has one.
pub fn declared(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

/// What a body counts as: its declared length, or the most `max_body`
/// bytes without one (and never more).
fn body_len(declared: Option<u64>, max_body: u64) -> u64 {
    declared.map_or(max_body, |d| d.min(max_body))
}

/// What a request reserves before its body is read (module doc).
pub fn request_share(declared: Option<u64>, json: bool, max_body: u64, output: u64) -> u64 {
    let upload = body_len(declared, max_body);
    let upload = if json {
        upload.saturating_mul(2)
    } else {
        upload
    };
    upload.saturating_add(output)
}

/// The read deadline of a body (module doc).
pub fn read_deadline(declared: Option<u64>, max_body: u64) -> Duration {
    Duration::from_secs(
        UPLOAD_READ_GRACE_S
            .saturating_add(body_len(declared, max_body) / UPLOAD_MIN_RATE_BYTES_PER_S),
    )
}

/// A body that did not arrive within its read deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadDeadline {
    pub secs: u64,
    /// The length it was given: declared, or `max_body_mb`'s.
    pub len: u64,
    pub declared: bool,
}

impl ReadDeadline {
    /// The refusal's code (408).
    pub const CODE: &'static str = "read_deadline";

    pub fn of(declared: Option<u64>, max_body: u64) -> Self {
        Self {
            secs: read_deadline(declared, max_body).as_secs(),
            len: body_len(declared, max_body),
            declared: declared.is_some(),
        }
    }

    /// The refusal's words.
    pub fn message(&self) -> String {
        use lmgw_api_types::units::fmt_bytes;
        format!(
            "the request's body did not arrive within its read deadline of {} s \
             (UPLOAD_READ_GRACE_S {} s + 1 s per UPLOAD_MIN_RATE_BYTES_PER_S {} of {}, {}); send \
             it faster, or in smaller requests ({})",
            self.secs,
            UPLOAD_READ_GRACE_S,
            fmt_bytes(UPLOAD_MIN_RATE_BYTES_PER_S),
            fmt_bytes(self.len),
            if self.declared {
                "its declared length"
            } else {
                "max_body_mb, as it declared no length"
            },
            Self::CODE
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    #[test]
    fn a_declared_body_reserves_its_length_and_a_chunked_one_the_most() {
        assert_eq!(request_share(Some(12), false, 64 * MIB, GIB), GIB + 12);
        assert_eq!(
            request_share(Some(10 * MIB), true, 64 * MIB, GIB),
            GIB + 20 * MIB
        );
        assert_eq!(request_share(None, false, 64 * MIB, GIB), GIB + 64 * MIB);
        assert_eq!(
            request_share(Some(GIB), false, 64 * MIB, GIB),
            GIB + 64 * MIB
        );
        assert_eq!(read_deadline(Some(MIB), 64 * MIB), Duration::from_secs(64));
        assert_eq!(read_deadline(None, 64 * MIB), Duration::from_secs(60 + 256));
        let m = ReadDeadline::of(Some(MIB), 64 * MIB).message();
        assert!(
            m.contains("64 s")
                && m.contains("UPLOAD_READ_GRACE_S 60 s")
                && m.contains("UPLOAD_MIN_RATE_BYTES_PER_S 256 KiB")
                && m.contains("read_deadline"),
            "{m}"
        );
    }
}
