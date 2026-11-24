//! The body's frames ([`Paced`]) and what feeds them: a task that hands a
//! body made whole to the connection chunk by chunk ([`paced`]), or a thread
//! that writes a large answer as JSON while the client reads it
//! ([`streamed`]).
//!
//! **The streamed answers' threads** (verification N-f) are threads of their
//! own, not the runtime's blocking pool, so conversion steps and file reads
//! (`spawn_blocking`) never queue behind slow readers. Each such answer
//! holds a share of more than half its room, and every room is at least the
//! output budget (`answers::output_budget`), so about 2 × the memory for
//! answers / the output budget of them write at once (16 at 8 GiB, 256 at
//! 256 GiB); more only where answers settled above their own room, which
//! then holds more of the memory for answers too. Where the memory is not
//! read (no bound), their number is not bounded either, as the answers'
//! memory is not.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use hyper::body::{Frame, SizeHint};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{Ended, To, CHUNK};
use crate::extract::convert::budget::answers::{self, Share};
use crate::server::conn::{Mark, WriteCut};

/// The streamed answers' threads' name.
pub(super) const THREAD: &str = "lmgw-answer";

/// A body's deadline and where its end is told.
struct Out {
    cut: Option<WriteCut>,
    ended: Option<super::OnEnd>,
    len: u64,
    limit: std::time::Duration,
    deadline: Instant,
    /// Where this answer began on its connection.
    mark: Option<Mark>,
}

impl Out {
    fn new(to: To, len: u64) -> Self {
        let limit = answers::write_deadline(len);
        Self {
            mark: to.cut.as_ref().map(WriteCut::begin),
            cut: to.cut,
            ended: to.ended,
            len,
            limit,
            deadline: Instant::now() + limit,
        }
    }

    /// The body ended `how`: said, told (unless [`whole_first`] told it
    /// already), and for a body handed over whole, its connection still
    /// watched until the deadline.
    async fn end(self, how: Ended) {
        if let Ended::Late { sent, .. } = how {
            missed(self.len, self.limit, sent);
            if let Some(c) = &self.cut {
                c.cut();
            }
        }
        if let Some(f) = self.ended {
            f(how).await;
        }
        if let (Ended::Whole, Some(c), Some(mark)) = (how, self.cut, self.mark) {
            tokio::time::sleep_until(self.deadline).await;
            c.check_tail(mark, self.len, self.limit.as_secs());
        }
    }
}

/// The deadline passed with `at` of `len` bytes handed over.
fn missed(len: u64, limit: std::time::Duration, at: u64) {
    tracing::warn!(
        "a conversion's answer of {} was not read by its client within its write deadline of {} \
         s (ANSWER_WRITE_GRACE_S {} s + 1 s per ANSWER_MIN_RATE_BYTES_PER_S {}/s; {} of it \
         handed over); its connection is ended and its memory given back",
        lmgw_api_types::units::fmt_bytes(len),
        limit.as_secs(),
        answers::ANSWER_WRITE_GRACE_S,
        lmgw_api_types::units::fmt_bytes(answers::ANSWER_MIN_RATE_BYTES_PER_S),
        lmgw_api_types::units::fmt_bytes(at),
    );
}

/// The body's frames, fed by a task that holds the bytes and the share
/// until the last chunk is taken, the client goes away or the deadline
/// passes.
pub(super) fn paced(bytes: Bytes, share: Share, to: To) -> Paced {
    let len = bytes.len() as u64;
    let share = share.settle(len);
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    let mut out = Out::new(to, len);
    let body = Paced::new(rx, len, out.cut.clone());
    tokio::spawn(async move {
        let how = hand_over(bytes, share, &tx, out.deadline, &mut out.ended).await;
        drop(tx);
        out.end(how).await;
    });
    body
}

/// The end told as whole before the body's last chunk is handed over, so a
/// client holding the whole answer finds its row written, as it did when
/// the row came before the body. It is told once the connection has taken
/// every chunk before it and has room for the last (its slot reserved), so
/// a deadline missed before that is a `Late` end, never a 200 (connection
/// cut review, nit 2). A client that leaves during that last chunk
/// ([`CHUNK`] at most) is recorded as answered.
async fn whole_first(ended: &mut Option<super::OnEnd>) {
    if let Some(f) = ended.take() {
        f(Ended::Whole).await;
    }
}

/// `chunk` into `tx` within `deadline`, the end told whole first when it is
/// the `last` ([`whole_first`]): `None` once handed over, else how it
/// ended with `sent` of `len` bytes handed over.
async fn hand(
    tx: &mpsc::Sender<Bytes>,
    chunk: Bytes,
    last: bool,
    deadline: Instant,
    ended: &mut Option<super::OnEnd>,
    (sent, len): (u64, u64),
) -> Option<Ended> {
    match tokio::time::timeout_at(deadline, tx.reserve()).await {
        Ok(Ok(slot)) => {
            if last {
                whole_first(ended).await;
            }
            slot.send(chunk);
            None
        }
        // The client went away: its connection dropped the body.
        Ok(Err(_)) => Some(Ended::Left { sent, len }),
        Err(_) => Some(Ended::Late { sent, len }),
    }
}

/// `bytes` into `tx` chunk by chunk until `deadline`; `share` and the bytes
/// go as it returns.
async fn hand_over(
    bytes: Bytes,
    share: Share,
    tx: &mpsc::Sender<Bytes>,
    deadline: Instant,
    ended: &mut Option<super::OnEnd>,
) -> Ended {
    let _share = share;
    let len = bytes.len();
    let mut at = 0;
    while at < len {
        let end = (at + CHUNK).min(len);
        let chunk = if at == 0 && end == len {
            bytes.clone()
        } else {
            Bytes::copy_from_slice(&bytes[at..end])
        };
        if let Some(how) = hand(
            tx,
            chunk,
            end == len,
            deadline,
            ended,
            (at as u64, len as u64),
        )
        .await
        {
            return how;
        }
        at = end;
    }
    Ended::Whole
}

/// Hands what it is written to the connection in [`CHUNK`]s, from the
/// answer's thread, within the deadline.
struct Feed {
    tx: mpsc::Sender<Bytes>,
    buf: Vec<u8>,
    deadline: Instant,
    rt: tokio::runtime::Handle,
    sent: u64,
    /// How the write stopped early, if it did.
    stopped: Option<Ended>,
    len: u64,
    /// Told before the last chunk ([`whole_first`]).
    ended: Option<super::OnEnd>,
}

impl Feed {
    fn push(&mut self) -> std::io::Result<()> {
        let chunk = Bytes::from(std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK)));
        let n = chunk.len() as u64;
        let last = self.sent + n >= self.len;
        // The timer is made inside the runtime: this is the answer's own
        // thread, outside it.
        let (tx, deadline, ended) = (&self.tx, self.deadline, &mut self.ended);
        let at = (self.sent, self.len);
        match self.rt.block_on(hand(tx, chunk, last, deadline, ended, at)) {
            None => {
                self.sent += n;
                Ok(())
            }
            Some(how) => {
                self.stopped = Some(how);
                Err(match how {
                    Ended::Left { .. } => std::io::ErrorKind::BrokenPipe,
                    _ => std::io::ErrorKind::TimedOut,
                }
                .into())
            }
        }
    }
}

impl std::io::Write for Feed {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = b.len().min(CHUNK - self.buf.len());
        self.buf.extend_from_slice(&b[..n]);
        if self.buf.len() == CHUNK {
            self.push()?;
        }
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.push()
    }
}

/// `body`, `len` bytes as JSON, written as the client reads it (module doc
/// of `send`), on a thread of its own (this module's doc).
pub(super) fn streamed<T>(body: T, len: u64, share: Share, to: To) -> Paced
where
    T: serde::Serialize + Send + 'static,
{
    let share = share.settle(len);
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    let mut out = Out::new(to, len);
    let paced = Paced::new(rx, len, out.cut.clone());
    let rt = tokio::runtime::Handle::current();
    let mut feed = Feed {
        tx,
        buf: Vec::with_capacity(CHUNK),
        deadline: out.deadline,
        rt: rt.clone(),
        sent: 0,
        stopped: None,
        len,
        ended: out.ended.take(),
    };
    let write = move || {
        let r = serde_json::to_writer(&mut feed, &body).map_err(std::io::Error::from);
        let r = r.and_then(|()| std::io::Write::flush(&mut feed));
        drop(body);
        drop(share);
        out.ended = feed.ended.take();
        let how = match (r, feed.stopped) {
            (_, Some(how)) => how,
            (Ok(()), None) => Ended::Whole,
            // Counted once already, so this does not happen; said as the
            // body's end, which the client sees short.
            (Err(e), None) => {
                tracing::warn!("a conversion's answer stopped while it was written: {e}");
                Ended::Late {
                    sent: feed.sent,
                    len,
                }
            }
        };
        drop(feed);
        rt.spawn(out.end(how));
    };
    // The closure back if the thread could not start: written on the
    // blocking pool then, said in the log.
    let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(write)));
    let taken = slot.clone();
    let started = std::thread::Builder::new()
        .name(THREAD.into())
        .spawn(move || {
            let w = taken.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(w) = w {
                w();
            }
        });
    if let Err(e) = started {
        tracing::warn!(
            "a thread for a conversion's answer could not start ({e}); it is written on the \
             runtime's blocking pool instead"
        );
        let w = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(w) = w {
            tokio::task::spawn_blocking(w);
        }
    }
    paced
}

/// A body of a known length whose chunks come from [`paced`]'s task or
/// [`streamed`]'s thread.
pub(super) struct Paced {
    rx: mpsc::Receiver<Bytes>,
    left: u64,
    /// Told when the last frame goes to hyper ([`WriteCut::last_frame`]).
    cut: Option<WriteCut>,
}

impl Paced {
    fn new(rx: mpsc::Receiver<Bytes>, left: u64, cut: Option<WriteCut>) -> Self {
        if left == 0 {
            if let Some(c) = &cut {
                c.last_frame();
            }
        }
        Self { rx, left, cut }
    }
}

impl hyper::body::Body for Paced {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.left == 0 {
            return Poll::Ready(None);
        }
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(b)) => {
                self.left = self.left.saturating_sub(b.len() as u64);
                if self.left == 0 {
                    if let Some(c) = &self.cut {
                        c.last_frame();
                    }
                }
                Poll::Ready(Some(Ok(Frame::data(b))))
            }
            // The feeding task ended before the whole body: its deadline.
            Poll::Ready(None) => Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the answer's write deadline passed (ANSWER_WRITE_GRACE_S, \
                 ANSWER_MIN_RATE_BYTES_PER_S)",
            )))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.left == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.left)
    }
}
