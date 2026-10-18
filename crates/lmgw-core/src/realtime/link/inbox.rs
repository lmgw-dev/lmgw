//! A link's reader, on a task of its own (realtime design §4.1, §6.4,
//! §10.4): everything it reads stamped with when it came off the link.
//!
//! **Why a task of its own** (B3 review 10). The barge-in gate places input
//! on the wall clock by when each append arrived (§6.4). Read by the session
//! core, a frame's stamp would be when the core got round to it: a core busy
//! for a while — a large append, a writer with no room — would stamp the
//! frames that queued meanwhile all at once, late, and a gap between two of
//! them would look like a muted client. The reader stamps each frame as it
//! comes off the socket, and the ping's round trip (`liveness::round_trip`)
//! is measured on the same stamps. The reader is the link's: a WebSocket's
//! is `ws`'s.
//!
//! **Flow control stays.** The reader reads ahead only while less than its
//! budget waits for the core — on a WebSocket, `realtime.max_frame_mb`, the
//! largest frame a client may send, so never less than one frame, and no
//! number of its own. Past that it stops reading, the socket's buffers fill,
//! and TCP pushes back on the client, as when the core read the socket
//! itself. What it passes on is counted until the core takes it, wherever
//! it waits — each at its payload plus the [`Read`] it is kept in (B3
//! review L1): a ping or a pong with an empty payload used to count
//! nothing, so a client flooding them grew the queue, and a liveness
//! check's backlog, without bound.
//!
//! **The end is passed on too.** A close, the end of the stream or a failed
//! read is passed on as [`Incoming::End`] and ends the reader; the session
//! reads it in order. Dropping the [`Inbox`] — the session is over — aborts
//! the reader, which would otherwise keep the socket open behind a client
//! that never closes.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, Notify};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use super::{End, Incoming};

/// What the link read, and when it came off the link.
#[derive(Debug)]
pub(crate) struct Read {
    pub what: Incoming,
    pub at: Instant,
    /// Its size, held against the read-ahead until the core takes it.
    len: usize,
}

/// What the reader may read ahead (module doc).
#[derive(Debug)]
struct Ahead {
    bytes: AtomicUsize,
    budget: usize,
    taken: Notify,
}

impl Ahead {
    /// Wait until less than the budget waits for the core.
    async fn room(&self) {
        loop {
            // Made before the check: a release in between leaves its permit.
            let taken = self.taken.notified();
            if self.bytes.load(Ordering::Acquire) < self.budget {
                return;
            }
            taken.await;
        }
    }

    fn release(&self, len: usize) {
        self.bytes.fetch_sub(len, Ordering::AcqRel);
        self.taken.notify_one();
    }
}

/// The reader's end: where it passes on what it read, and the read-ahead
/// it waits for.
pub(crate) struct Feed {
    tx: mpsc::UnboundedSender<Read>,
    ahead: Arc<Ahead>,
}

impl Feed {
    /// Wait until less than the budget waits for the core (module doc).
    pub async fn room(&self) {
        self.ahead.room().await;
    }

    /// Pass `what` on, which came off the link at `at`; `payload` bytes of
    /// it count against the read-ahead until the core takes it, with the
    /// slot it waits in (module doc). `false`: the session is gone.
    pub fn pass(&self, what: Incoming, at: Instant, payload: usize) -> bool {
        let len = payload + std::mem::size_of::<Read>();
        self.ahead.bytes.fetch_add(len, Ordering::AcqRel);
        self.tx.send(Read { what, at, len }).is_ok()
    }
}

/// What the reader passed on and the core has not taken.
pub(crate) struct Inbox {
    rx: mpsc::UnboundedReceiver<Read>,
    /// What a liveness check took off the channel, kept in order for the
    /// core (`liveness::Liveness::answered_meanwhile`).
    backlog: VecDeque<Read>,
    ahead: Arc<Ahead>,
    reader: AbortHandle,
}

impl Inbox {
    /// Start `reader` on its feed, on a task of its own, reading ahead at
    /// most `budget` bytes (module doc).
    pub fn spawn<F>(budget: usize, reader: impl FnOnce(Feed) -> F) -> Self
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let ahead = Arc::new(Ahead {
            bytes: AtomicUsize::new(0),
            budget: budget.max(1),
            taken: Notify::new(),
        });
        let task = tokio::spawn(reader(Feed {
            tx,
            ahead: ahead.clone(),
        }));
        Self {
            rx,
            backlog: VecDeque::new(),
            ahead,
            reader: task.abort_handle(),
        }
    }

    /// The next read: one a liveness check kept first, else the reader's
    /// next. Once the reader has ended, the end of the link.
    pub async fn next(&mut self) -> Read {
        let read = match self.backlog.pop_front() {
            Some(r) => Some(r),
            None => self.rx.recv().await,
        };
        self.taken(read)
    }

    /// A read the reader already passed on, without waiting — for a
    /// liveness verdict, which keeps what it does not consume
    /// ([`Self::keep`]).
    pub fn ready(&mut self) -> Option<Read> {
        self.rx.try_recv().ok()
    }

    /// Put `read`, taken by [`Self::ready`], behind the reads kept before
    /// it: the core handles it in order.
    pub fn keep(&mut self, read: Read) {
        self.backlog.push_back(read);
    }

    /// A read [`Self::ready`] took and did not keep: its room goes back.
    pub fn consumed(&mut self, read: Read) -> Incoming {
        self.ahead.release(read.len);
        read.what
    }

    fn taken(&mut self, read: Option<Read>) -> Read {
        match read {
            Some(r) => {
                self.ahead.release(r.len);
                r
            }
            None => Read {
                what: Incoming::End(End::Closed),
                at: Instant::now(),
                len: 0,
            },
        }
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// The inbox on a WebSocket's reader (`ws`), fed through a channel standing
/// in for the socket.
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::extract::ws::Message;

    use super::super::ws;
    use super::*;
    use crate::realtime::liveness::RoundTrip;

    fn text(t: &str) -> Result<Message, axum::Error> {
        Ok(Message::Text(t.into()))
    }

    /// The paused clock: a sleep moves on only once every task is idle, so
    /// the reader has taken what was sent before it does — no wall-clock
    /// margin for a loaded runner to break (B3 review L4).
    #[tokio::test(start_paused = true)]
    async fn frames_are_stamped_as_they_come_off_the_socket_while_the_core_is_busy() {
        let (tx, socket) = futures::channel::mpsc::unbounded();
        let mut inbox = ws::reader(socket, usize::MAX, Arc::default());
        let t0 = Instant::now();
        tx.unbounded_send(text("a")).unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        tx.unbounded_send(text("b")).unwrap();
        // The core was busy for 120 ms before it read either.
        tokio::time::sleep(Duration::from_millis(60)).await;
        let a = inbox.next().await;
        let b = inbox.next().await;
        assert_eq!(a.at, t0, "stamped when it came, not when it was taken");
        assert_eq!(b.at - a.at, Duration::from_millis(60));
        assert_eq!(Instant::now() - t0, Duration::from_millis(120));
    }

    #[tokio::test(start_paused = true)]
    async fn the_reader_stops_reading_ahead_at_the_budget_and_a_pong_completes_the_round_trip() {
        let (tx, socket) = futures::channel::mpsc::unbounded();
        let rt = Arc::new(RoundTrip::default());
        let mut inbox = ws::reader(socket, 10, rt.clone());
        let ping = rt.pinged(Instant::now());
        for t in ["0123456789", "x", "y"] {
            tx.unbounded_send(text(t)).unwrap();
        }
        tx.unbounded_send(Ok(Message::Pong(ping.to_vec().into())))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        // Ten bytes wait: the reader took the first frame and stopped.
        let first = inbox.ready().unwrap();
        assert!(
            inbox.ready().is_none(),
            "nothing read ahead past the budget"
        );
        assert_eq!(rt.get(), None, "the pong is still on the socket");
        // Kept for the core, it still counts — until the core takes it.
        inbox.keep(first);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(inbox.ready().is_none());
        let mut seen = Vec::new();
        for _ in 0..4 {
            match inbox.next().await.what {
                Incoming::Event(t) => seen.push(t.as_str().to_owned()),
                Incoming::Pong => seen.push("pong".into()),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(seen, ["0123456789", "x", "y", "pong"]);
        assert!(rt.get().is_some(), "measured when it came off the socket");
        // The socket ends: so does the inbox, for good.
        drop(tx);
        assert!(matches!(
            inbox.next().await.what,
            Incoming::End(End::Closed)
        ));
        assert!(matches!(
            inbox.next().await.what,
            Incoming::End(End::Closed)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn control_frames_count_against_the_read_ahead() {
        // B3 review L1: empty pongs counted nothing, so a flood of them was
        // read without bound. A ping is answered by tungstenite and never
        // waits for the core (`ws`), so it takes no slot.
        let (tx, socket) = futures::channel::mpsc::unbounded();
        let slot = std::mem::size_of::<Read>();
        let mut inbox = ws::reader(socket, 3 * slot, Arc::default());
        for _ in 0..10 {
            tx.unbounded_send(Ok(Message::Ping(Default::default())))
                .unwrap();
            tx.unbounded_send(Ok(Message::Pong(Default::default())))
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        let mut read = 0;
        while let Some(r) = inbox.ready() {
            assert!(matches!(r.what, Incoming::Pong), "{:?}", r.what);
            inbox.keep(r);
            read += 1;
        }
        assert_eq!(read, 3, "three slots, then the reader waits");
        // The core takes them: the reader goes on.
        for _ in 0..3 {
            inbox.next().await;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(inbox.ready().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn pings_take_no_read_ahead() {
        // A ping flood is answered by tungstenite and skipped by the
        // reader: a text frame behind it is ready at once, even with room
        // for a single slot.
        let (tx, socket) = futures::channel::mpsc::unbounded();
        let slot = std::mem::size_of::<Read>();
        let mut inbox = ws::reader(socket, slot, Arc::default());
        for _ in 0..50 {
            tx.unbounded_send(Ok(Message::Ping(Default::default())))
                .unwrap();
        }
        tx.unbounded_send(text("after the pings")).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        match inbox.ready().map(|r| r.what) {
            Some(Incoming::Event(t)) => assert_eq!(t.as_str(), "after the pings"),
            other => panic!("{other:?}"),
        }
        assert!(inbox.ready().is_none());
    }
}
