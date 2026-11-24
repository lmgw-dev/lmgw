//! A conversion's answer written to its client (security round A, H1): the
//! body goes out as it is read, under the conversion's answer share
//! (`budget::answers`), within a write deadline.
//!
//! - The share is set to the body's real size once it is made
//!   ([`answers::Share::settle`]) and given back when the last of it has
//!   been handed to the connection, or the client went away.
//! - **Write deadline:** `ANSWER_WRITE_GRACE_S` (60 s) plus one second for
//!   every `ANSWER_MIN_RATE_BYTES_PER_S` (1 MiB/s) of the body, announced in
//!   [`WRITE_DEADLINE`] (review S6). A client that has not read it by then
//!   has its connection ended: its socket is closed with a reset
//!   ([`crate::server::conn::WriteCut`], verification SF2), the gateway's
//!   log says so with both names, and the body's memory goes back with the
//!   share. An answer whose last bytes were handed over in time but whose
//!   own bytes still wait unread at the deadline, on a connection that has
//!   done nothing since, has its connection ended then too. On HTTP/2 only
//!   the answer's stream is reset (its body's error), never the connection.
//! - The body is handed over in copies of [`CHUNK`] bytes, so what the
//!   connection still holds after the deadline is one chunk, never the
//!   whole answer. Its length is announced as before (`content-length`).
//! - **The body's size is counted first** (verification SF1). It is made
//!   whole only when it is at most half the share, beside the answer it is
//!   made from; a larger one is written chunk by chunk as the client reads
//!   it ([`feed::streamed`]), so the memory is the answer and a chunk or
//!   two, never the answer and its body.
//! - **How it ended** ([`Ended`]) is told to [`To::then`]: the request's
//!   usage row is written then, so a client that left midway or missed the
//!   deadline is not recorded as a plain answer (verification N-b). A whole
//!   answer is told just before its last chunk is handed over, and one of a
//!   single chunk before its head goes, so a client that holds the whole
//!   answer finds its row.

use axum::body::Body;
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::future::BoxFuture;

use crate::extract::convert::budget::answers::{self, Share};
use crate::server::conn::{Conn, WriteCut};

mod feed;

/// The most of the body handed to the connection at once: 64 KiB.
pub const CHUNK: usize = 64 << 10;

/// The response header that names the body's write deadline.
pub const WRITE_DEADLINE: &str = "x-lmgw-write-deadline";

/// [`WRITE_DEADLINE`]'s value for a body of `len` bytes.
pub fn deadline_value(len: u64) -> HeaderValue {
    let text = format!(
        "{} (ANSWER_WRITE_GRACE_S + 1 s per ANSWER_MIN_RATE_BYTES_PER_S)",
        answers::write_deadline(len).as_secs()
    );
    HeaderValue::from_str(&text).unwrap_or_else(|_| HeaderValue::from_static("60"))
}

/// How an answer's body ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// Every byte was handed to the connection (told as the last chunk
    /// is: a client that leaves during it is heard as whole).
    Whole,
    /// The client went away after `sent` of the `len` bytes.
    Left { sent: u64, len: u64 },
    /// The write deadline passed with `sent` of the `len` bytes handed over;
    /// the connection was ended.
    Late { sent: u64, len: u64 },
    /// The answer could not be written as JSON: a 500.
    Unwritable,
}

/// What [`To::then`] is told, once.
pub type OnEnd = Box<dyn FnOnce(Ended) -> BoxFuture<'static, ()> + Send>;

/// Where an answer goes: its connection's cut, and who hears how it ended.
#[derive(Default)]
pub struct To {
    cut: Option<WriteCut>,
    ended: Option<OnEnd>,
}

impl To {
    /// The connection `req` came on, from its extensions (none outside a
    /// listener, as in a test that calls a handler).
    pub fn of<B>(req: &axum::http::Request<B>) -> Self {
        Self::on(
            req.extensions()
                .get::<axum::extract::ConnectInfo<Conn>>()
                .map(|c| &c.0),
            req.version(),
        )
    }

    /// The connection `conn`, for a request of `version`. An HTTP/2
    /// request's answer never cuts its connection, which carries other
    /// streams: a missed deadline resets the answer's own stream, through
    /// its body's error (connection cut review, nit 1).
    pub fn on(conn: Option<&Conn>, version: axum::http::Version) -> Self {
        Self {
            cut: if version >= axum::http::Version::HTTP_2 {
                None
            } else {
                conn.and_then(|c| c.cut.clone())
            },
            ended: None,
        }
    }

    /// `f` hears how the body ended.
    pub fn then(mut self, f: OnEnd) -> Self {
        self.ended = Some(f);
        self
    }
}

/// What [`made`] came to.
enum Made<T> {
    Whole(Vec<u8>),
    Streamed(T, u64),
}

/// Counts what is written.
struct Count(u64);

impl std::io::Write for Count {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(b.len() as u64);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `body` counted, then made whole when it fits beside itself in `room`
/// bytes, else kept for [`feed::streamed`] (module doc).
fn made<T: serde::Serialize>(body: T, room: u64) -> serde_json::Result<Made<T>> {
    let mut n = Count(0);
    serde_json::to_writer(&mut n, &body)?;
    let len = n.0;
    if len > room / 2 {
        return Ok(Made::Streamed(body, len));
    }
    let mut whole = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    serde_json::to_writer(&mut whole, &body)?;
    Ok(Made::Whole(whole))
}

/// `body` as JSON with `status`, written under `share` to `to`. A body that
/// cannot be written as JSON is the 500 it was before (axum's `Json`).
pub async fn json<T>(status: StatusCode, body: T, share: Share, mut to: To) -> Response
where
    T: serde::Serialize + Send + 'static,
{
    let room = share.bytes();
    let made = tokio::task::spawn_blocking(move || made(body, room))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()));
    let (len, paced) = match made {
        Ok(Made::Whole(bytes)) => {
            one_chunk_first(bytes.len(), &mut to).await;
            (
                bytes.len() as u64,
                feed::paced(Bytes::from(bytes), share, to),
            )
        }
        Ok(Made::Streamed(body, len)) => (len, feed::streamed(body, len, share, to)),
        Err(e) => {
            if let Some(f) = to.ended {
                f(Ended::Unwritable).await;
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the answer could not be written as JSON: {e}"),
            )
                .into_response();
        }
    };
    let mut r = (status, Body::new(paced)).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r.headers_mut()
        .insert(HeaderName::from_static(WRITE_DEADLINE), deadline_value(len));
    r
}

/// `bytes` as a body written under `share` to `to` (module doc), with its
/// [`WRITE_DEADLINE`] header's value.
pub async fn body(bytes: Bytes, share: Share, mut to: To) -> (Body, HeaderValue) {
    let value = deadline_value(bytes.len() as u64);
    one_chunk_first(bytes.len(), &mut to).await;
    (Body::new(feed::paced(bytes, share, to)), value)
}

/// A body of one [`CHUNK`] at most is told whole before its head goes
/// (module doc).
async fn one_chunk_first(len: usize, to: &mut To) {
    if len <= CHUNK {
        if let Some(f) = to.ended.take() {
            f(Ended::Whole).await;
        }
    }
}

#[cfg(test)]
mod tests;
