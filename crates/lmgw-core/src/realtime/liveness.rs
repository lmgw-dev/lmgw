//! Whether the client is still there (realtime design §4.1, §10.4).
//!
//! A session lasts as long as its socket, and a socket can outlive its
//! client: a laptop lid closed mid-session, a peer that stopped reading. Its
//! writes then fill the kernel buffers and stall, and nothing ever errors —
//! the session, its writer and its socket would stay for good.
//!
//! So the session pings every `realtime.ping_interval_s` and closes, with a
//! reason naming that setting, when a ping has had no pong for a whole
//! interval. The clock keeps running while the core waits for the writer:
//! a peer that has not taken the session's output for that long has stopped
//! reading, and gets the same verdict (the pong could not even be read). On
//! the way out the writer gets the same interval to drain before it is
//! aborted, so a peer that stopped reading cannot keep the task or the
//! socket either. `0` turns all of it off — the owner's call — and then
//! there is no bound at all: a peer that is alive but stopped reading
//! answers TCP's own probes with a zero window, so TCP never gives up, and
//! the session keeps its task, its socket and the key's concurrency slot
//! for good (§10.4; the setting's text says so).
//!
//! **A verdict looks at what was read first** ([`Liveness::answered_meanwhile`]).
//! The core can be busy past a tick — a large append, a writer with no room
//! — while the client's pong already waits for it (`inbox`: the socket's
//! reader passes frames on as they come); the session's unbiased `select!`
//! can then take the overdue tick before the frame, and a client that did
//! answer would be closed (WP1c review #6). So before closing, every frame
//! already read is looked at: a pong lifts the verdict, and the rest stay
//! queued, in order, for the session. Only a pong does: a read side that
//! ended instead — a close, a FIN, a frame over the size limit — is no
//! answer but the end, and ends the session on the spot (package A review
//! #1). Treating it as an answer let a peer that closed and stopped reading
//! keep a session stuck behind a full writer, its task, socket and
//! concurrency slot, for good: the end can be read again on every tick.
//!
//! **The round trip** of each ping is measured as well (`round_trip`): the
//! barge-in window's end leans on it (§6.4).

use std::time::Duration;

mod round_trip;

pub(crate) use round_trip::RoundTrip;

use axum::extract::ws::Message;
use tokio::time::{Instant, Interval, MissedTickBehavior};

use super::inbox::Inbox;

/// What the socket's read half yields, kept as it came.
pub(crate) type Frame = Option<Result<Message, axum::Error>>;

/// RFC 6455 "unexpected condition" — what the `websockets` library behind
/// openai-python closes a keepalive timeout with.
pub(crate) const CLOSE_NO_PONG: u16 = 1011;

/// What a tick of the liveness clock asks for.
pub(crate) enum Beat {
    /// Send a ping.
    Ping,
    /// The last ping had no pong for a whole interval: close, with this.
    Dead(String),
}

/// What [`Liveness::answered_meanwhile`] found on the socket.
#[derive(Debug)]
pub(crate) enum Answer {
    /// The pong: the verdict is lifted.
    Pong,
    /// The read side ended before any pong, and how: the client is gone,
    /// and the session ends now (module doc).
    Ended(End),
    /// Nothing answered: the verdict stands.
    Nothing,
}

/// How a client's read side ended (A2 review 1): the session closes the
/// way the reader loop would have for the same frame.
#[derive(Debug)]
pub(crate) enum End {
    /// A close frame, or the end of the stream.
    Closed,
    /// A failed read — a frame over the size limit, say, which closes with
    /// 1009 and a reason naming the setting.
    Failed(axum::Error),
}

pub(crate) struct Liveness {
    /// `None`: `realtime.ping_interval_s` is 0.
    clock: Option<Interval>,
    every: Duration,
    /// A ping went out and no pong has come back yet.
    unanswered: bool,
}

impl Liveness {
    pub fn new(ping_interval_s: u32) -> Self {
        Self::every(Duration::from_secs(u64::from(ping_interval_s)))
    }

    fn every(every: Duration) -> Self {
        let clock = (!every.is_zero()).then(|| {
            let mut c = tokio::time::interval_at(Instant::now() + every, every);
            // A tick the session was too busy to take is taken late, not
            // twice: two ticks in a row would call a client dead that had no
            // chance to answer the first ping.
            c.set_missed_tick_behavior(MissedTickBehavior::Delay);
            c
        });
        Self {
            clock,
            every,
            unanswered: false,
        }
    }

    /// The next tick; never, when the check is off.
    pub async fn beat(&mut self) -> Beat {
        let Some(clock) = &mut self.clock else {
            return std::future::pending().await;
        };
        clock.tick().await;
        if std::mem::replace(&mut self.unanswered, true) {
            Beat::Dead(format!(
                "no pong within {} s, setting realtime.ping_interval_s",
                self.every.as_secs_f64()
            ))
        } else {
            Beat::Ping
        }
    }

    pub fn pong(&mut self) {
        self.unanswered = false;
    }

    /// How long the writer may take to drain once the session is over;
    /// `None` when the check is off.
    pub fn grace(&self) -> Option<Duration> {
        self.clock.is_some().then_some(self.every)
    }

    /// Before a [`Beat::Dead`] verdict (module doc): look at every frame
    /// the reader already passed on, without waiting. A pong is the answer
    /// ([`Answer::Pong`]); the end of the read side is [`Answer::Ended`],
    /// with how it ended, and the session ends without waiting for anything
    /// else. Every other frame is kept in the inbox, in order, for the
    /// session to handle before it reads on.
    pub fn answered_meanwhile(&mut self, inbox: &mut Inbox) -> Answer {
        while let Some(read) = inbox.ready() {
            match &read.frame {
                Some(Ok(Message::Pong(_))) => {
                    inbox.consumed(read);
                    self.pong();
                    return Answer::Pong;
                }
                // Not queued: the session ends here (module doc).
                None | Some(Ok(Message::Close(_))) => {
                    inbox.consumed(read);
                    return Answer::Ended(End::Closed);
                }
                Some(Err(_)) => {
                    return Answer::Ended(match inbox.consumed(read) {
                        Some(Err(e)) => End::Failed(e),
                        _ => End::Closed,
                    })
                }
                _ => inbox.keep(read),
            }
        }
        Answer::Nothing
    }

    /// Whether pings are sent at all (`realtime.ping_interval_s` > 0).
    pub fn on(&self) -> bool {
        self.clock.is_some()
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    #[tokio::test]
    async fn a_ping_without_a_pong_for_a_whole_interval_is_the_end() {
        let mut l = Liveness::every(Duration::from_millis(20));
        assert!(matches!(l.beat().await, Beat::Ping));
        l.pong();
        assert!(matches!(l.beat().await, Beat::Ping));
        let Beat::Dead(reason) = l.beat().await else {
            panic!("no pong came back");
        };
        assert_eq!(
            reason,
            "no pong within 0.02 s, setting realtime.ping_interval_s"
        );
        assert_eq!(l.grace(), Some(Duration::from_millis(20)));
        // The setting's whole seconds read as such, and fit a close frame.
        let Beat::Dead(reason) = ({
            let mut l = Liveness::new(20);
            l.unanswered = true;
            l.clock = Some(tokio::time::interval(Duration::from_millis(1)));
            l.every = Duration::from_secs(20);
            l.beat().await
        }) else {
            panic!("an unanswered ping");
        };
        assert_eq!(
            reason,
            "no pong within 20 s, setting realtime.ping_interval_s"
        );
        assert!(reason.len() <= 123);
    }

    fn text(t: &str) -> Result<Message, axum::Error> {
        Ok(Message::Text(t.into()))
    }

    /// An inbox over `frames` whose reader has passed them all on.
    async fn read(frames: Vec<Result<Message, axum::Error>>, ended: bool) -> Inbox {
        let socket = futures::stream::iter(frames);
        let inbox = if ended {
            Inbox::spawn(socket, usize::MAX, Default::default())
        } else {
            Inbox::spawn(
                socket.chain(futures::stream::pending()),
                usize::MAX,
                Default::default(),
            )
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        inbox
    }

    #[tokio::test]
    async fn a_pong_waiting_behind_other_frames_lifts_the_verdict_and_keeps_them() {
        let mut l = Liveness::every(Duration::from_millis(20));
        assert!(matches!(l.beat().await, Beat::Ping));
        // The core was busy past the next tick, and the select took it: the
        // pong is already read, behind two events.
        let pong = Ok(Message::Pong(Default::default()));
        let later = text("after the pong");
        let mut inbox = read(vec![text("a"), text("b"), pong, later], false).await;
        assert!(matches!(l.answered_meanwhile(&mut inbox), Answer::Pong));
        // The next tick is a ping again, not a second verdict.
        assert!(matches!(l.beat().await, Beat::Ping));
        // The session handles the kept frames first, in order, then reads
        // on — the pong taken.
        let mut order = Vec::new();
        for _ in 0..3 {
            match inbox.next().await.frame {
                Some(Ok(Message::Text(t))) => order.push(t.to_string()),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(order, ["a", "b", "after the pong"]);
    }

    #[tokio::test]
    async fn no_pong_read_leaves_the_verdict_and_an_ended_read_side_is_the_end() {
        let mut l = Liveness::every(Duration::from_millis(20));
        let mut inbox = read(vec![text("a")], false).await;
        assert!(matches!(l.answered_meanwhile(&mut inbox), Answer::Nothing));
        assert!(matches!(
            inbox.next().await.frame,
            Some(Ok(Message::Text(_)))
        ));

        // The end of the stream, a close and a failed read (a frame over the
        // size limit) are each the end, never an answer — and the verdict's
        // pending ping stays unanswered.
        let closed = || Ok(Message::Close(None));
        let failed = || Err(axum::Error::new(std::io::Error::other("too big")));
        let cases = [
            (vec![text("a")], true),
            (vec![text("a"), closed()], false),
            (vec![text("a"), failed()], false),
        ];
        for (k, (frames, ended)) in cases.into_iter().enumerate() {
            let mut l = Liveness::every(Duration::from_millis(20));
            l.unanswered = true;
            let mut inbox = read(frames, ended).await;
            // A failed read keeps its error, for the close it calls for.
            match (k, l.answered_meanwhile(&mut inbox)) {
                (0 | 1, Answer::Ended(End::Closed)) | (2, Answer::Ended(End::Failed(_))) => {}
                (k, other) => panic!("case {k}: {other:?}"),
            }
            assert!(l.unanswered, "the end is no pong");
        }
    }

    #[tokio::test]
    async fn zero_turns_the_check_off() {
        let mut l = Liveness::new(0);
        assert_eq!(l.grace(), None);
        let beat = tokio::time::timeout(Duration::from_millis(50), l.beat()).await;
        assert!(beat.is_err(), "no tick ever comes");
    }
}
