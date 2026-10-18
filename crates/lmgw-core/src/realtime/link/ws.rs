//! The WebSocket behind the transport seam (calls design §7.1): its reader,
//! its sending half and its pings, as `GET /v1/realtime` has always had
//! them.
//!
//! **The reader** stamps every frame as it comes off the socket and passes
//! it on through the inbox (`inbox`, which keeps the read-ahead budget):
//! a text frame as an [`Incoming::Event`], a binary one as
//! [`Incoming::Binary`] (refused by the session), a pong as
//! [`Incoming::Pong`] once it completed its ping's round trip, and a close,
//! the end of the stream or a failed read as [`Incoming::End`]. A ping is
//! not passed on: tungstenite answers it itself, and nothing in it is the
//! session's.
//!
//! **The sending half** is any `Sink<Message>` (a WebSocket's, or a
//! channel standing in for one in a test): an event goes out as a text
//! frame, a ping carries the number its round trip is measured by
//! (`liveness::round_trip`), and a [`Close`] is the close frame it has
//! always been ([`close_frame`]).

use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::stream::SplitSink;
use futures::{SinkExt, Stream, StreamExt};
use tokio::time::Instant;

use super::{Close, End, Feed, Inbox, Incoming};
use crate::realtime::handshake::Limits;
use crate::realtime::liveness::RoundTrip;
use crate::realtime::thread::{CLOSE_OUT_OF_REACH, CLOSE_TAKEN_OVER};

/// RFC 6455's "message too big": a frame or message over the size settings.
pub(crate) const CLOSE_TOO_BIG: u16 = 1009;

/// RFC 6455 "unexpected condition" — what the `websockets` library behind
/// openai-python closes a keepalive timeout with.
pub(crate) const CLOSE_NO_PONG: u16 = 1011;

/// RFC 6455's normal closure: what a [`Close::HungUp`], which only a call
/// meets, would say on a WebSocket.
const CLOSE_NORMAL: u16 = 1000;

/// An upgraded WebSocket, split for its session (`session::run`): the
/// sending half the writer drives, and its reader, started on the writer's
/// round trip and reading ahead at most the largest frame `limits` let a
/// client send (`inbox`).
pub(crate) fn split(
    socket: WebSocket,
    limits: &Limits,
) -> (
    SplitSink<WebSocket, Message>,
    impl FnOnce(Arc<RoundTrip>) -> Inbox + Send + 'static,
) {
    let (sink, stream) = socket.split();
    let budget = limits.max_frame;
    (sink, move |round_trip| reader(stream, budget, round_trip))
}

/// Start the reader on `stream`, reading ahead at most `budget` bytes
/// (`inbox`), its pongs completing `round_trip`.
pub(crate) fn reader<S>(stream: S, budget: usize, round_trip: Arc<RoundTrip>) -> Inbox
where
    S: Stream<Item = Result<Message, axum::Error>> + Unpin + Send + 'static,
{
    Inbox::spawn(budget, move |feed| read(stream, feed, round_trip))
}

/// The reader (module doc).
async fn read<S>(mut stream: S, feed: Feed, round_trip: Arc<RoundTrip>)
where
    S: Stream<Item = Result<Message, axum::Error>> + Unpin,
{
    loop {
        feed.room().await;
        let frame = stream.next().await;
        let at = Instant::now();
        let (what, payload) = match frame {
            Some(Ok(Message::Text(t))) => {
                let len = t.len();
                (Incoming::Event(t), len)
            }
            Some(Ok(Message::Binary(b))) => (Incoming::Binary, b.len()),
            Some(Ok(Message::Pong(p))) => {
                round_trip.ponged(at, &p);
                (Incoming::Pong, p.len())
            }
            // tungstenite answers pings itself (module doc).
            Some(Ok(Message::Ping(_))) => continue,
            Some(Ok(Message::Close(_))) | None => (Incoming::End(End::Closed), 0),
            Some(Err(e)) => (Incoming::End(End::Failed(e)), 0),
        };
        let end = matches!(what, Incoming::End(_));
        if !feed.pass(what, at, payload) || end {
            return;
        }
    }
}

impl<S> super::Sink for S
where
    S: futures::Sink<Message> + Unpin + Send + 'static,
{
    async fn event(&mut self, text: String) -> bool {
        SinkExt::send(self, Message::text(text)).await.is_ok()
    }

    async fn ping(&mut self, round_trip: &RoundTrip) -> bool {
        // When it is sent, not when it was asked for: the round trip is the
        // network's, not the writer's queue — and before the send, so a
        // pong read while the send is still being flushed finds its ping
        // (E3).
        let id = round_trip.pinged(Instant::now());
        SinkExt::send(self, Message::Ping(id.to_vec().into()))
            .await
            .is_ok()
    }

    async fn close(&mut self, close: Close) {
        let (code, reason) = close_frame(close);
        let _ = SinkExt::send(
            self,
            Message::Close(Some(CloseFrame {
                code,
                reason: reason.into(),
            })),
        )
        .await;
    }

    async fn end(&mut self) {
        let _ = SinkExt::close(self).await;
    }
}

/// The close frame's code and reason a WebSocket says `close` with.
pub(crate) fn close_frame(close: Close) -> (u16, String) {
    match close {
        Close::TakenOver { by } => (
            CLOSE_TAKEN_OVER,
            crate::realtime::thread::taken_over_reason(by.as_deref()),
        ),
        Close::Revoked(reason) => (crate::devices::CLOSE_REVOKED, reason),
        Close::OutOfReach(reason) => (CLOSE_OUT_OF_REACH, reason),
        Close::GoingAway => (
            crate::server::CLOSE_GOING_AWAY,
            crate::server::STOPPING.to_string(),
        ),
        Close::TooBig(reason) => (CLOSE_TOO_BIG, reason),
        Close::NoResponse(reason) => (CLOSE_NO_PONG, reason),
        Close::HungUp { .. } => (CLOSE_NORMAL, "hung up".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_close_is_the_frame_a_websocket_has_always_sent() {
        let r = || "why".to_string();
        let cases = [
            (Close::Revoked(r()), 4003),
            (Close::OutOfReach(r()), 4004),
            (Close::TooBig(r()), 1009),
            (Close::NoResponse(r()), 1011),
        ];
        for (close, code) in cases {
            assert_eq!(close_frame(close), (code, r()));
        }
        assert_eq!(
            close_frame(Close::TakenOver { by: None }),
            (4000, crate::realtime::thread::taken_over_reason(None))
        );
        assert_eq!(
            close_frame(Close::GoingAway),
            (1001, crate::server::STOPPING.to_string())
        );
    }

    #[tokio::test]
    async fn the_sink_sends_events_pings_and_the_close_as_frames() {
        use super::super::Sink;
        let (mut tx, mut rx) = futures::channel::mpsc::unbounded::<Message>();
        let rt = RoundTrip::default();
        assert!(Sink::event(&mut tx, "{}".into()).await);
        assert!(Sink::ping(&mut tx, &rt).await);
        Sink::close(&mut tx, Close::NoResponse("no pong".into())).await;
        Sink::end(&mut tx).await;
        let mut frames = Vec::new();
        while let Some(m) = rx.next().await {
            frames.push(m);
        }
        let [Message::Text(t), Message::Ping(p), Message::Close(Some(c))] = &frames[..] else {
            panic!("{frames:?}");
        };
        assert_eq!(t.as_str(), "{}");
        assert_eq!(p.as_ref(), 1u64.to_be_bytes(), "the ping's number");
        assert_eq!((c.code, c.reason.as_str()), (1011, "no pong"));
    }
}
