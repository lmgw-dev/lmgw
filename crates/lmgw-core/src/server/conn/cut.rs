//! A connection that an answer's write deadline can end (security round A
//! verification, SF2): every listener's stream is a [`Cuttable`], and its
//! [`WriteCut`] rides on the request's [`super::Conn`]. A conversion's answer
//! that its client has not read by its deadline ([`crate::server::documents::send`])
//! cuts it: the connection's next read or write fails, which ends it in
//! hyper, and its socket is closed with a reset (`SO_LINGER` 0), so neither
//! the descriptor nor the kernel's unsent buffer outlives the deadline.
//!
//! hyper polls a connection whose client reads nothing only when the socket
//! has room, which it never gets: the cut wakes the connection's task
//! itself, so its stalled write is polled again and fails.
//!
//! An answer handed over whole before its deadline can still be unread at
//! it: in hyper's buffer (a write that waits), or all of it taken by the
//! kernel's send queue, hyper done and the connection waiting for a next
//! request (verification of SF2's fix, 2026-11-20: a 1.6 MiB answer sat in
//! a 2.8 MiB send queue for 40 minutes). [`WriteCut::check_tail`] asks the
//! connection itself to judge at the deadline, and the judgement concerns
//! the answer's own bytes only, and only while the connection has done
//! nothing since (connection-cut review, B1):
//! - **Disarmed** the moment the connection reads any byte after the
//!   answer's [`Mark`] (an HTTP/1 client sends its next request, or an
//!   upgrade, only once it has taken the answer), starts writing past the
//!   answer's end (another response), or a later answer begins on it. On
//!   every system.
//! - **Still armed**, it is ended when the answer itself still waits to be
//!   written (the rest in hyper's buffer, the write waiting for room) or,
//!   where the kernel's send queue is read (`SIOCOUTQ` on Linux, `SO_NWRITE`
//!   on macOS), when some of the answer's own bytes are still in it. The
//!   queue is first in, first out, so that is `queued > written since the
//!   mark − the answer`, the answer being its head and body as this
//!   connection wrote them: counted from the mark to the flush that took
//!   its last frame ([`WriteCut::last_frame`]), never guessed. While armed
//!   nothing was written after the answer, so this is the queue itself.
//!   On Windows the queue is not read yet, so only the waiting write is
//!   judged there.
//!
//! HTTP/2 connections are never cut: their answers carry no [`WriteCut`]
//! (`send::To`), and a stream's body error resets that stream alone.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use std::sync::Mutex;

use futures::task::AtomicWaker;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

/// What a [`WriteCut`] shares with its stream.
struct State {
    cut: AtomicBool,
    /// The last write that waited for room on the socket and has not got
    /// it yet ([`WriteCut::check_tail`]).
    stalled: AtomicBool,
    /// Bytes the stream has taken to write, all told.
    written: AtomicU64,
    /// Bytes the stream has read, all told.
    read: AtomicU64,
    /// Answers begun on the connection ([`WriteCut::begin`]).
    answers: AtomicU64,
    /// The latest answer's last frame was handed to hyper: the next flush
    /// that finishes marks its end.
    ending: AtomicBool,
    /// Where the latest answer ended in `written` ([`UNKNOWN`] until its
    /// last frame was flushed).
    end: AtomicU64,
    /// A tail check waits in `tail`: read before the lock on every poll.
    armed: AtomicBool,
    /// The tail check asked for and not judged yet ([`WriteCut::check_tail`]).
    tail: Mutex<Option<Tail>>,
    waker: AtomicWaker,
}

impl Default for State {
    fn default() -> Self {
        Self {
            cut: AtomicBool::new(false),
            stalled: AtomicBool::new(false),
            written: AtomicU64::new(0),
            read: AtomicU64::new(0),
            answers: AtomicU64::new(0),
            ending: AtomicBool::new(false),
            end: AtomicU64::new(UNKNOWN),
            armed: AtomicBool::new(false),
            tail: Mutex::new(None),
            waker: AtomicWaker::new(),
        }
    }
}

/// An answer's end not known yet.
const UNKNOWN: u64 = u64::MAX;

/// Where an answer began on its connection ([`WriteCut::begin`]): which
/// answer it is and what the connection had written and read before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    answer: u64,
    written: u64,
    read: u64,
}

/// An answer's tail to judge: its mark, its body's length and its
/// deadline's seconds (for the log line).
#[derive(Debug, Clone, Copy)]
struct Tail {
    mark: Mark,
    len: u64,
    limit_s: u64,
}

/// What a still-armed tail check found ([`Cuttable::judge`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The connection did something since the answer, or the answer is
    /// gone from it: left alone.
    Left,
    /// The answer's last bytes still wait in hyper's buffer.
    Waits,
    /// `own` of the answer's `bytes` (head and body as written) are still
    /// in the kernel's send queue.
    Queued { own: u64, bytes: u64 },
}

/// A connection's counts as a tail check reads them ([`verdict`]).
#[derive(Debug, Clone, Copy)]
struct Now {
    /// Answers begun.
    answers: u64,
    /// Bytes read and written in all.
    read: u64,
    written: u64,
    /// The latest answer's end in `written` ([`UNKNOWN`] until flushed).
    end: u64,
    /// The last write waits for room.
    waits: bool,
}

impl State {
    fn now(&self) -> Now {
        Now {
            answers: self.answers.load(Ordering::SeqCst),
            read: self.read.load(Ordering::SeqCst),
            written: self.written.load(Ordering::SeqCst),
            end: self.end.load(Ordering::SeqCst),
            waits: self.stalled.load(Ordering::SeqCst),
        }
    }
}

/// The tail check's judgement of the answer marked `m` (module doc), from
/// the connection's counts `now` and the kernel's `queued` bytes where they
/// are read.
fn verdict(m: Mark, now: Now, queued: Option<u64>) -> Verdict {
    if now.answers != m.answer || now.read != m.read {
        return Verdict::Left;
    }
    let bytes = if now.end == UNKNOWN {
        // The last frame not flushed yet: the answer itself is still being
        // written, and all the connection wrote since the mark is its.
        if now.waits {
            return Verdict::Waits;
        }
        now.written.saturating_sub(m.written)
    } else if now.written > now.end {
        // Another response began after the answer.
        return Verdict::Left;
    } else {
        now.end.saturating_sub(m.written)
    };
    // Nothing was written after the answer, so the first-in-first-out rule
    // `queued > written since the mark − the answer` is `queued > 0`: every
    // queued byte is the answer's.
    match queued {
        Some(q) if q > 0 => Verdict::Queued {
            own: q.min(bytes),
            bytes,
        },
        _ => Verdict::Left,
    }
}

/// The handle that ends one connection (module doc).
#[derive(Clone, Default)]
pub struct WriteCut(Arc<State>);

impl std::fmt::Debug for WriteCut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteCut")
            .field("cut", &self.is_cut())
            .finish()
    }
}

impl WriteCut {
    /// End the connection: its next read or write fails, and its task is
    /// woken to make one.
    pub fn cut(&self) {
        self.0.cut.store(true, Ordering::SeqCst);
        self.0.waker.wake();
    }

    /// An answer begins on the connection: its mark, taken before its head
    /// ([`Self::check_tail`]). A tail check of an earlier answer is
    /// disarmed by it.
    pub fn begin(&self) -> Mark {
        let s = &self.0;
        s.ending.store(false, Ordering::SeqCst);
        s.end.store(UNKNOWN, Ordering::SeqCst);
        Mark {
            answer: s.answers.fetch_add(1, Ordering::SeqCst) + 1,
            written: s.written.load(Ordering::SeqCst),
            read: s.read.load(Ordering::SeqCst),
        }
    }

    /// The answer's last frame was handed to hyper: the next flush that
    /// finishes marks its end.
    pub fn last_frame(&self) {
        self.0.ending.store(true, Ordering::SeqCst);
    }

    /// At the deadline of an answer of `len` bytes handed over whole: the
    /// connection judges at its next poll, woken for it, whether some of
    /// the answer is still unread (module doc) and ends itself if so, said
    /// in the log. A connection that has done anything since the answer is
    /// left alone, and so is an answer a later one followed: its check,
    /// late, never takes the place of the later answer's (re-review nit).
    pub fn check_tail(&self, mark: Mark, len: u64, limit_s: u64) {
        {
            let mut tail = self.0.tail.lock().unwrap_or_else(|e| e.into_inner());
            // Read under the lock: a later answer's `begin` comes before
            // its own check, which then waits for this one and replaces it.
            if mark.answer != self.0.answers.load(Ordering::SeqCst) {
                return;
            }
            *tail = Some(Tail { mark, len, limit_s });
        }
        self.0.armed.store(true, Ordering::SeqCst);
        self.0.waker.wake();
    }

    pub fn is_cut(&self) -> bool {
        self.0.cut.load(Ordering::SeqCst)
    }
}

/// The TCP socket under a stream, for the reset.
pub trait TcpOf {
    fn tcp(&self) -> &TcpStream;
}

impl TcpOf for TcpStream {
    fn tcp(&self) -> &TcpStream {
        self
    }
}

impl TcpOf for super::TrackedStream {
    fn tcp(&self) -> &TcpStream {
        &self.inner
    }
}

impl<S: TcpOf> TcpOf for tokio_rustls::server::TlsStream<S> {
    fn tcp(&self) -> &TcpStream {
        self.get_ref().0.tcp()
    }
}

/// A listener's stream with its [`WriteCut`] (module doc).
pub struct Cuttable<S> {
    inner: S,
    cut: WriteCut,
    reset: bool,
}

impl<S: TcpOf> Cuttable<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            cut: WriteCut::default(),
            reset: false,
        }
    }

    /// Its handle.
    pub fn cut(&self) -> WriteCut {
        self.cut.clone()
    }

    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    /// A tail check asked for, judged now (module doc). Without one, a
    /// single atomic read.
    fn judge(&mut self) {
        let s = &self.cut.0;
        if !s.armed.load(Ordering::Acquire) || !s.armed.swap(false, Ordering::AcqRel) {
            return;
        }
        let Some(t) = s.tail.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let v = verdict(t.mark, s.now(), unsent(self.inner.tcp()));
        let still = match v {
            Verdict::Left => return,
            Verdict::Waits => "the rest of it still waited to be written".to_string(),
            Verdict::Queued { own, bytes } => format!(
                "{} of its {} (head and body) still unacknowledged in the connection's send \
                 queue",
                lmgw_api_types::units::fmt_bytes(own),
                lmgw_api_types::units::fmt_bytes(bytes),
            ),
        };
        tracing::warn!(
            "a conversion's answer of {} was handed over whole, but its client had not taken \
             it at its write deadline of {} s (ANSWER_WRITE_GRACE_S + 1 s per \
             ANSWER_MIN_RATE_BYTES_PER_S): {still}, and the connection had read and written \
             nothing since; its connection is ended",
            lmgw_api_types::units::fmt_bytes(t.len),
            t.limit_s,
        );
        s.cut.store(true, Ordering::SeqCst);
    }

    /// The error a cut connection answers; its socket set to close with a
    /// reset, once.
    fn ended(&mut self) -> std::io::Error {
        if !self.reset {
            self.reset = true;
            if let Err(e) =
                socket2::SockRef::from(self.inner.tcp()).set_linger(Some(Duration::ZERO))
            {
                tracing::debug!("a cut connection's SO_LINGER was not set: {e}");
            }
        }
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the connection was ended: an answer on it was not read within its write deadline \
             (ANSWER_WRITE_GRACE_S, ANSWER_MIN_RATE_BYTES_PER_S)",
        )
    }

    /// `r`, a poll of the inner stream, with the cut's waker kept while it
    /// waits, and the cut seen once more after (it may have come between).
    fn waiting<T>(
        &mut self,
        cx: &mut Context<'_>,
        r: Poll<std::io::Result<T>>,
    ) -> Poll<std::io::Result<T>> {
        if r.is_pending() {
            self.cut.0.waker.register(cx.waker());
            if self.cut.is_cut() {
                return Poll::Ready(Err(self.ended()));
            }
        }
        r
    }

    /// A write's poll: [`Self::waiting`], whether it waits for room, and
    /// the bytes it took.
    fn writing(
        &mut self,
        cx: &mut Context<'_>,
        r: Poll<std::io::Result<usize>>,
    ) -> Poll<std::io::Result<usize>> {
        if let Poll::Ready(Ok(n)) = &r {
            self.cut.0.written.fetch_add(*n as u64, Ordering::SeqCst);
        }
        self.cut.0.stalled.store(r.is_pending(), Ordering::SeqCst);
        self.waiting(cx, r)
    }
}

impl<S: AsyncRead + TcpOf + Unpin> AsyncRead for Cuttable<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.cut.is_cut() {
            return Poll::Ready(Err(self.ended()));
        }
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = r {
            let n = buf.filled().len().saturating_sub(before);
            if n > 0 {
                self.cut.0.read.fetch_add(n as u64, Ordering::SeqCst);
            }
        }
        // Judged after the read: a next request just read disarms it.
        self.judge();
        if self.cut.is_cut() {
            return Poll::Ready(Err(self.ended()));
        }
        self.waiting(cx, r)
    }
}

impl<S: AsyncWrite + TcpOf + Unpin> AsyncWrite for Cuttable<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.judge();
        if self.cut.is_cut() {
            return Poll::Ready(Err(self.ended()));
        }
        let r = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.writing(cx, r)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        self.judge();
        if self.cut.is_cut() {
            return Poll::Ready(Err(self.ended()));
        }
        let r = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.writing(cx, r)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.judge();
        if self.cut.is_cut() {
            return Poll::Ready(Err(self.ended()));
        }
        let r = Pin::new(&mut self.inner).poll_flush(cx);
        self.cut.0.stalled.store(r.is_pending(), Ordering::SeqCst);
        // hyper flushes its inner stream only once its own buffer is
        // written: after the answer's last frame, that is the answer's end.
        // A load first: every flush of every connection passes here, and
        // only the one after an answer's last frame swaps.
        if matches!(r, Poll::Ready(Ok(())))
            && self.cut.0.ending.load(Ordering::Relaxed)
            && self.cut.0.ending.swap(false, Ordering::SeqCst)
        {
            let at = self.cut.0.written.load(Ordering::SeqCst);
            self.cut.0.end.store(at, Ordering::SeqCst);
        }
        self.waiting(cx, r)
    }

    /// A cut connection is not shut down in order (a TLS close would want
    /// to write): it is closed with its reset as it is dropped.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.cut.is_cut() {
            let _ = self.ended();
            return Poll::Ready(Ok(()));
        }
        let r = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.waiting(cx, r)
    }
}

/// The bytes the kernel still holds for the socket, unsent or not yet
/// acknowledged (`SIOCOUTQ`).
#[cfg(target_os = "linux")]
fn unsent(tcp: &TcpStream) -> Option<u64> {
    use std::os::fd::AsRawFd;
    let mut n: libc::c_int = 0;
    // SAFETY: the descriptor is open while `tcp` is; `n` is the int the
    // request writes.
    let rc = unsafe { libc::ioctl(tcp.as_raw_fd(), libc::TIOCOUTQ, &mut n) };
    (rc == 0).then(|| u64::try_from(n).unwrap_or(0))
}

/// The bytes the kernel still holds for the socket, unsent or not yet
/// acknowledged (`SO_NWRITE`, the same quantity as Linux's `SIOCOUTQ`).
#[cfg(target_os = "macos")]
fn unsent(tcp: &TcpStream) -> Option<u64> {
    use std::os::fd::AsRawFd;
    let mut n: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the descriptor is open while `tcp` is; `n` and `size` are the
    // int and its length the option writes.
    let rc = unsafe {
        libc::getsockopt(
            tcp.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NWRITE,
            (&mut n as *mut libc::c_int).cast(),
            &mut size,
        )
    };
    (rc == 0).then(|| u64::try_from(n).unwrap_or(0))
}

/// Not read on this system yet (module doc).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn unsent(_tcp: &TcpStream) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests;
