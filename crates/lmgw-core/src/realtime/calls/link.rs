//! A call behind the transport seam (calls design §7.1): its reader and
//! its sending half.
//!
//! **The reader** passes on what the call received, in order, through the
//! inbox (`link::inbox`): a text message on `oai-events` as an
//! [`Incoming::Event`], stamped with when it came off the socket — a bound
//! client's `lmgw.event.part`s joined first, up to `realtime.max_message_mb`,
//! the joined event stamped as its last part came, and a broken set, one
//! past the setting or a part on an unbound call as the
//! [`Incoming::Refused`] the session answers (§6.3, `sizes`) — a binary
//! one as [`Incoming::Binary`] (refused by the session), each audio frame
//! decoded and placed on the wall clock as an [`Incoming::Audio`] (`input`,
//! §4), and the channel's close or the call's end as [`Incoming::End`],
//! behind every message that arrived before it — the End's order (§8.1).
//! What waited for the session past its budget ([`CallEvent::TooBig`], the
//! call still up) ends as a failed read the session closes with
//! `Close::TooBig`, naming `realtime.max_message_mb`, as a WebSocket message
//! over its setting does: a bound call's `lmgw.session.closed` says it,
//! delivered before the close like every other kind's. A push-to-talk
//! commit or a clear waits there for the audio it cuts (the commit window,
//! §4.4, `input`). It reads ahead at most the largest message str0m
//! receives ([`LOCAL_MAX_MESSAGE`]), so never less than one message. The
//! transport's own events (connected, open, the client's sender reports) it
//! passes by: the open path reads them before the session starts.
//!
//! **The sending half** ([`CallSink`]) sends an event as one message on
//! `oai-events`, waiting while str0m's send buffer is full (§6.3), and
//! closes the call: lmgw's close once what was written is delivered, so a
//! bound call's `lmgw.session.closed` reaches the client before `Rtc::close`
//! (§8.2, `CallControl::close_delivered`), at most `realtime.ping_interval_s`
//! later; the end without a close of lmgw's own (the client closed first, or
//! the session went) at once. A call sends no pings (§8.1): the liveness
//! evidence on a call is the client's own traffic and acknowledgements
//! (`liveness::call`), so [`Sink::ping`] sends nothing. An event over the
//! largest message a call sends (the client's `max-message-size`, or
//! `realtime.max_message_mb` below it) is shaped to fit before it is sent
//! (§6.3, `sizes::Shaper`, the frame clock's events alike): its parts on a
//! bound call, written back to back as one write, or on an unbound one the
//! event with its largest fields emptied and an `event_too_large` per field;
//! one that cannot be made to fit is logged naming the bound and not sent.
//! **Its audio** leaves on the call's frame clock, which
//! pulls the writer's paced queue (`audio::out`, §5): the sink starts it
//! when the writer hands it the queue ([`Sink::pull`]), and stops it with
//! the link.
//!
//! **What the open path hands on** ([`Start`]): the `session` part of the
//! POST, which the reader passes first, as a `session.update`, so the
//! session applies it before any event of the client's (§2.2); the audio
//! kept since the first frame (§4.6), decoded ahead of the live frames; and
//! what the audio both ways takes from the settings and the offer.
//!
//! **A bound call's last event** ([`closed_event`]): the session says why
//! it ends as `lmgw.session.closed`, each [`Close`] as the kind §8.2's
//! table gives it.

use std::sync::Arc;

use axum::extract::ws::Utf8Bytes;
use lmgw_api_types::realtime::{ClosedKind, SessionClosed};

use super::audio::out::{self, OutParams};
use super::handle::{CallControl, CallEvent, CallEvents, CallHandle, Kept, Message, Sent};
use super::offer::LOCAL_MAX_MESSAGE;
use super::sizes::{Joined, Joiner, Shaper};
use crate::realtime::link::{Close, End, Feed, Inbox, Incoming, Sink};
use crate::realtime::liveness::RoundTrip;
use crate::realtime::writer::Pulled;

mod input;

pub(crate) use input::InputParams;
use input::{Input, Pass};

/// What the open path hands the session's reader (module doc).
#[derive(Default)]
pub(crate) struct Start {
    /// The POST's `session` part as a `session.update` event's text.
    pub first: Option<String>,
    /// The audio frames that arrived before the session started, still
    /// counted against the call's budget (`handle::Kept`); none when the bind
    /// waited (§4.6).
    pub kept: Option<Kept>,
    /// What the input takes from the settings (§4).
    pub input: InputParams,
    /// What the output takes from the settings and the offer (§5); `None`:
    /// the call sends no audio (a test of the events alone).
    pub out: Option<OutParams>,
    /// What the call's messages are bound by each way (§6.3).
    pub sizes: SizeParams,
}

/// What a call's messages are bound by (§6.3, `sizes`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SizeParams {
    /// The call is bound to a Chat thread: an oversized event travels as
    /// parts both ways; an unbound call empties fields and refuses parts.
    pub bound: bool,
    /// `realtime.max_message_mb` in bytes, what a client's parts may join
    /// to; `None` at 0, no bound.
    pub join: Option<usize>,
}

/// A call split for its session: the sending half the writer drives, and
/// its reader, started on the writer's round trip (`ws::split`'s twin).
pub(crate) fn split(
    handle: CallHandle,
    start: Start,
) -> (
    CallSink,
    impl FnOnce(Arc<RoundTrip>) -> Inbox + Send + 'static,
) {
    let CallHandle { control, events } = handle;
    let id: Arc<str> = control.id().into();
    let mut start = start;
    let sink = CallSink {
        control,
        out: start.out.take(),
        bound: start.sizes.bound,
        shaper: None,
        clock: None,
    };
    (sink, move |_round_trip| reader_from(id, events, start))
}

/// Start the reader on `events` (module doc).
fn reader_from(id: Arc<str>, events: CallEvents, start: Start) -> Inbox {
    Inbox::spawn(LOCAL_MAX_MESSAGE, move |feed| read(id, events, start, feed))
}

/// The reader (module doc).
async fn read(id: Arc<str>, mut events: CallEvents, start: Start, feed: Feed) {
    let Start {
        first,
        kept,
        input,
        sizes,
        ..
    } = start;
    let mut input = Input::new(id.clone(), input);
    // A bound client's parts are joined before the session reads them
    // (§6.3, `sizes`).
    let mut joiner = Joiner::new(sizes.bound, sizes.join);
    if let Some(update) = first {
        let len = update.len();
        let at = tokio::time::Instant::now();
        if !feed.pass(Incoming::Event(Utf8Bytes::from(update)), at, len) {
            return;
        }
    }
    // The audio kept since the first frame, ahead of the live frames, at its
    // own stamps (§4.6), each frame behind the read-ahead's room as a live
    // one is; its bytes leave the call's budget once it is in.
    if let Some(kept) = kept {
        for f in kept.frames() {
            feed.room().await;
            if !pass(&feed, input.frame(f)) {
                return;
            }
        }
    }
    loop {
        feed.room().await;
        let deadline = input.deadline();
        let next = tokio::select! {
            e = events.next() => Ok(e),
            () = sleep_until(deadline), if deadline.is_some() => {
                Err(deadline.unwrap_or_else(tokio::time::Instant::now))
            }
        };
        let step = match next {
            Ok(e) => take(&mut input, &mut joiner, e),
            // A cut's wait is over at `cut`, its give-up time: what came off
            // the socket before it is taken first (`input`'s module doc). The
            // drain stops after the first event that came off the socket at
            // or after `cut` (which ends the wait itself), so one poll decodes
            // no more than what arrived within the cut's window; what is
            // behind it goes through this loop, each behind the read-ahead's
            // room.
            Err(cut) => {
                let mut step = Step::default();
                let mut past = false;
                while let Some(e) = events.try_next() {
                    past = socket_time(&e).is_some_and(|t| t >= cut);
                    let s = take(&mut input, &mut joiner, Some(e));
                    step.passes.extend(s.passes);
                    if s.end.is_some() {
                        step.end = s.end;
                        break;
                    }
                    if past {
                        break;
                    }
                }
                if step.end.is_none() && !past {
                    step.passes.extend(input.due(tokio::time::Instant::now()));
                }
                step
            }
        };
        if !pass(&feed, step.passes) {
            return;
        }
        if let Some(end) = step.end {
            if let Some((set, got, count)) = joiner.open_set() {
                tracing::debug!(
                    "WebRTC call {id}: ended with the part set {set} open ({got} of {count} \
                     parts); it is dropped"
                );
            }
            let _ = feed.pass(Incoming::End(end), tokio::time::Instant::now(), 0);
            return;
        }
    }
}

/// What one event of the call gave the reader: what goes on, and the end
/// behind it.
#[derive(Default)]
struct Step {
    passes: Vec<Pass>,
    end: Option<End>,
}

/// One event of the call, through the joiner and the input (module doc).
fn take(input: &mut Input, joiner: &mut Joiner, e: Option<CallEvent>) -> Step {
    let mut step = Step::default();
    match e {
        // A joined set goes on stamped as its last part came off the socket.
        Some(CallEvent::Message {
            message: Message::Text(t),
            at,
        }) => {
            for joined in joiner.push(t) {
                let passes = match joined {
                    Joined::Event(t) => {
                        let len = t.len();
                        input.event(Incoming::Event(Utf8Bytes::from(t)), at, len)
                    }
                    Joined::Refused(e) => input.event(Incoming::Refused(e), at, 0),
                };
                step.passes.extend(passes);
            }
        }
        Some(CallEvent::Message {
            message: Message::Binary(b),
            at,
        }) => step.passes = input.event(Incoming::Binary, at, b.len()),
        Some(CallEvent::Audio(f)) => step.passes = input.frame(&f),
        // What waits goes ahead of the end (§8.1).
        Some(CallEvent::TooBig { queued, limit }) => {
            step.passes = input.flush();
            step.end = Some(End::Failed(axum::Error::new(QueueOverrun {
                queued,
                limit,
            })));
        }
        Some(CallEvent::ChannelClosed | CallEvent::Ended(_)) | None => {
            step.passes = input.flush();
            step.end = Some(End::Closed);
        }
        // The client's sender reports would name the exact push-to-talk
        // cut, which is Later (§4.4); they measure the capture offset.
        Some(CallEvent::SenderReport(sr)) => input.sender_report(&sr),
        // Read by the open path (module doc).
        Some(CallEvent::Connected | CallEvent::Open) => {}
    }
    step
}

/// When `e` came off the socket, for an event that did (a frame, a message).
fn socket_time(e: &CallEvent) -> Option<tokio::time::Instant> {
    match e {
        CallEvent::Audio(f) => Some(f.arrived),
        CallEvent::Message { at, .. } => Some(*at),
        _ => None,
    }
}

/// Pass `passes` on in order; `false`: the session is gone.
fn pass(feed: &Feed, passes: Vec<Pass>) -> bool {
    passes.into_iter().all(|p| feed.pass(p.what, p.at, p.len))
}

/// Until `at`; never, for `None`.
async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The `lmgw.session.closed` a bound call's session sends before it closes
/// for `close` (§8.2's table): `by` the new binder or who hung up, `message`
/// the sentence a WebSocket's close reason carries for the same end.
pub(crate) fn closed_event(close: &Close) -> SessionClosed {
    let (kind, by, message) = match close {
        Close::TakenOver { by } => (
            ClosedKind::TakenOver,
            by.clone(),
            crate::realtime::thread::taken_over_reason(by.as_deref()),
        ),
        Close::Revoked(reason) => (ClosedKind::Revoked, None, reason.clone()),
        Close::OutOfReach(reason) => (ClosedKind::OutOfReach, None, reason.clone()),
        Close::GoingAway => (
            ClosedKind::ShuttingDown,
            None,
            crate::server::STOPPING.to_string(),
        ),
        Close::TooBig(reason) | Close::NoResponse(reason) => {
            (ClosedKind::Closed, None, reason.clone())
        }
        Close::HungUp { by } => (ClosedKind::Closed, by.clone(), hung_up(by.as_deref())),
    };
    SessionClosed { kind, by, message }
}

/// What a hangup's `lmgw.session.closed` says (§2.6).
pub(crate) fn hung_up(by: Option<&str>) -> String {
    match by {
        Some(by) => {
            format!("the call was hung up by {by} (POST /v1/realtime/calls/{{call_id}}/hangup)")
        }
        None => "the call was hung up (POST /v1/realtime/calls/{call_id}/hangup)".to_string(),
    }
}

/// What waited for a call's session passed `realtime.max_message_mb`: the
/// failed read the reader ends with (module doc).
#[derive(Debug)]
pub(crate) struct QueueOverrun {
    pub queued: usize,
    pub limit: usize,
}

impl std::fmt::Display for QueueOverrun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} bytes of the client's messages and audio waited unread, over \
             realtime.max_message_mb ({} bytes)",
            self.queued, self.limit
        )
    }
}

impl std::error::Error for QueueOverrun {}

/// The close reason for a call's failed read that is a [`QueueOverrun`]
/// (`Transport::too_big` on a call), or `None`.
pub(crate) fn too_big_reason(e: axum::Error) -> Option<String> {
    e.into_inner()
        .downcast::<QueueOverrun>()
        .ok()
        .map(|q| q.to_string())
}

/// A call's sending half (module doc). A struct of its own: the WebSocket's
/// is any `futures::Sink<Message>`.
pub(crate) struct CallSink {
    control: CallControl,
    /// What the frame clock is started with, until [`Sink::pull`] takes it.
    out: Option<OutParams>,
    /// The call is bound to a Chat thread (§6.3's parts).
    bound: bool,
    /// What its events are shaped to (§6.3), once the writer handed the
    /// sink the session's ids with its queue ([`Sink::pull`]).
    shaper: Option<Shaper>,
    /// The frame clock's task, once started: stopped when the sink goes,
    /// with the writer.
    clock: Option<tokio::task::JoinHandle<()>>,
}

impl CallSink {
    /// The frame clock stops: the session is over.
    fn stop_clock(&mut self) {
        if let Some(c) = self.clock.take() {
            c.abort();
        }
    }
}

impl Drop for CallSink {
    fn drop(&mut self) {
        self.stop_clock();
    }
}

impl Sink for CallSink {
    async fn event(&mut self, text: String) -> bool {
        // Shaped to the call's largest message (module doc): one message,
        // its parts, or the event emptied and its errors; nothing for one
        // that cannot be made to fit, said at WARN.
        let messages = match &self.shaper {
            Some(s) => s.shape(text),
            None => vec![text],
        };
        if messages.is_empty() {
            return true;
        }
        match self.control.send_all(messages).await {
            Sent::Sent => true,
            Sent::Gone => false,
            Sent::TooLarge { len, limit, by } => {
                tracing::warn!(
                    "WebRTC call {}: an event of {len} bytes is over {} and was not sent",
                    self.control.id(),
                    by.named(limit)
                );
                true
            }
        }
    }

    async fn ping(&mut self, _round_trip: &RoundTrip) -> bool {
        // A call sends no pings (module doc).
        true
    }

    async fn close(&mut self, close: Close) {
        tracing::info!("WebRTC call {}: closing ({close:?})", self.control.id());
        self.stop_clock();
        // Once the last events are delivered (module doc).
        self.control.close_delivered().await;
    }

    async fn end(&mut self) {
        self.control.close().await;
    }

    fn pull(&mut self, queue: Pulled) -> bool {
        let shaper = Shaper::new(
            self.control.id().into(),
            self.control.largest(),
            self.bound,
            queue.ids(),
        );
        self.shaper = Some(shaper.clone());
        let Some(params) = self.out.take() else {
            return false;
        };
        self.clock = Some(out::spawn(self.control.audio_link(shaper), queue, params));
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::{mpsc, watch};
    use tokio::time::Instant;

    use super::super::driver::{Command, Driver};
    use super::super::handle::{Ended, Frame, Limit};
    use super::*;
    use crate::realtime::handshake::Limits;
    use crate::realtime::link::{CallLimits, Transport};

    fn handle() -> (
        CallHandle,
        mpsc::UnboundedSender<CallEvent>,
        mpsc::UnboundedReceiver<Command>,
    ) {
        let (driver, commands) = Driver::for_tests();
        let (events, rx) = mpsc::unbounded_channel();
        let (_ending, heard) = watch::channel(None);
        let (_feedback, feedback) = watch::channel(Default::default());
        let handle = CallHandle {
            control: CallControl {
                id: "rtc_test".into(),
                driver,
                ending: heard,
                ended: false,
                phone: Default::default(),
                reorder: InputParams::default().reorder,
                feedback,
                largest: (crate::realtime::calls::LOCAL_MAX_MESSAGE, Limit::Client),
            },
            events: CallEvents {
                rx,
                queued: Default::default(),
            },
        };
        (handle, events, commands)
    }

    #[tokio::test]
    async fn a_call_over_its_budget_closes_its_session_as_too_big() {
        let (h, tx, _commands) = handle();
        let (_sink, start) = split(h, Start::default());
        tx.send(CallEvent::TooBig {
            queued: 70,
            limit: 64,
        })
        .unwrap();
        let mut inbox = start(Arc::default());
        let Incoming::End(End::Failed(e)) = inbox.next().await.what else {
            panic!("a failed read")
        };
        let reason = Transport::WebRtc(CallLimits {}).too_big(e).unwrap();
        assert!(
            reason.contains("realtime.max_message_mb") && reason.contains("70"),
            "{reason}"
        );
        // A WebSocket's reading of any other failure is untouched.
        let other = axum::Error::new(std::io::Error::other("x"));
        assert!(Transport::WebRtc(CallLimits {}).too_big(other).is_none());
        let ws = Limits::from_settings(&Default::default());
        let other = axum::Error::new(std::io::Error::other("x"));
        assert!(Transport::WebSocket(ws).too_big(other).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_reader_passes_messages_stamped_as_they_arrived_and_ends_behind_them() {
        let (h, tx, _commands) = handle();
        let (_sink, start) = split(h, Start::default());
        let at = Instant::now();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let msg = |m| CallEvent::Message { message: m, at };
        tx.send(CallEvent::Connected).unwrap();
        tx.send(CallEvent::Open).unwrap();
        tx.send(CallEvent::Audio(frame(0, at))).unwrap();
        tx.send(msg(Message::Text("{\"type\":\"a\"}".into())))
            .unwrap();
        tx.send(msg(Message::Binary(vec![1, 2]))).unwrap();
        tx.send(CallEvent::ChannelClosed).unwrap();
        tx.send(msg(Message::Text("after the close".into())))
            .unwrap();
        let mut inbox = start(Arc::default());
        // The frame decoded, stamped at its last sample's capture.
        let audio = inbox.next().await;
        let Incoming::Audio { pcm, captured } = audio.what else {
            panic!("audio first")
        };
        assert_eq!((pcm.len(), captured), (480, at));
        let first = inbox.next().await;
        assert_eq!(first.at, at, "stamped when it came off the socket");
        assert!(matches!(&first.what, Incoming::Event(t) if t.as_str() == "{\"type\":\"a\"}"));
        assert!(matches!(inbox.next().await.what, Incoming::Binary));
        assert!(matches!(
            inbox.next().await.what,
            Incoming::End(End::Closed)
        ));
        assert!(
            matches!(inbox.next().await.what, Incoming::End(End::Closed)),
            "nothing after the end"
        );
    }

    /// A reader that lags — it runs only once the clock is far past a
    /// commit's give-up time — still meets the frames and the commit in the
    /// order the driver met them, each judged on the socket's clock (§4.4,
    /// §5.5): the frames that came off the socket inside the commit's window
    /// go to the session before it, the one that came after its give-up
    /// time behind it, though stamped before the cut (it lands in the next
    /// turn). Whichever of the reader's timer and its next event it meets
    /// first, the order is the same.
    #[tokio::test(start_paused = true)]
    async fn a_lagging_reader_applies_no_commit_ahead_of_the_frames_inside_its_window() {
        use crate::realtime::calls::audio::fixtures::{opus_stream, voice};
        let ms = Duration::from_millis;
        let (h, tx, _commands) = handle();
        let start = Start {
            input: InputParams {
                reorder: ms(60),
                commit_window: ms(120),
            },
            ..Start::default()
        };
        let (_sink, reader) = split(h, start);
        let t0 = Instant::now();
        let stream = opus_stream(&voice(9));
        // Frame `k` came off the socket at `arrived`; a steady stream, so the
        // first eight are stamped at their arrivals, 20 ms apart.
        let frame = |k: usize, arrived: Instant| {
            CallEvent::Audio(Frame {
                rtp_time: 9_600 + 960 * k as u64,
                arrived,
                contiguous: true,
                seq: (k as u64 + 1, k as u64 + 1),
                payload: stream[k].clone().into(),
            })
        };
        for k in 0..3 {
            tx.send(frame(k, t0 + ms(20 * k as u64))).unwrap();
        }
        // The commit came off the socket at 40 ms: it cuts at 160 ms and gives
        // up at 220 ms.
        tx.send(CallEvent::Message {
            message: Message::Text("{\"type\":\"input_audio_buffer.commit\"}".into()),
            at: t0 + ms(40),
        })
        .unwrap();
        for k in 3..8 {
            tx.send(frame(k, t0 + ms(20 * k as u64))).unwrap();
        }
        // Frame 8, stamped at 160 ms, came off the socket at 230 ms.
        tx.send(frame(8, t0 + ms(230))).unwrap();
        // The reader's first poll comes a second later.
        let mut inbox = reader(Arc::default());
        tokio::time::advance(Duration::from_secs(1)).await;
        let mut seen = Vec::new();
        for _ in 0..10 {
            seen.push(match inbox.next().await.what {
                Incoming::Audio { captured, .. } => {
                    format!("audio@{}", captured.duration_since(t0).as_millis())
                }
                Incoming::Event(_) => "commit".to_string(),
                other => format!("{other:?}"),
            });
        }
        let expected: Vec<String> = (0..8)
            .map(|k| format!("audio@{}", 20 * k))
            .chain(["commit".to_string(), "audio@160".to_string()])
            .collect();
        assert_eq!(seen, expected);
    }

    /// The phone's frame `k`, real Opus, arriving at `arrived`.
    fn frame(k: usize, arrived: Instant) -> Frame {
        use crate::realtime::calls::audio::fixtures::{opus_stream, voice};
        let stream = opus_stream(&voice(k + 1));
        Frame {
            rtp_time: 9_600 + 960 * k as u64,
            arrived,
            contiguous: true,
            seq: (k as u64 + 1, k as u64 + 1),
            payload: stream[k].clone().into(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_audio_kept_before_the_open_is_decoded_ahead_of_the_live_frames() {
        let (h, tx, _commands) = handle();
        let t0 = Instant::now();
        let mut kept = h.events.keeper();
        for k in 0..3 {
            kept.push(frame(k, t0 + Duration::from_millis(20 * k as u64)));
        }
        let start = Start {
            first: Some("{\"type\":\"session.update\",\"session\":{}}".into()),
            kept: Some(kept),
            ..Start::default()
        };
        let (_sink, reader) = split(h, start);
        tx.send(CallEvent::Audio(frame(3, t0 + Duration::from_millis(60))))
            .unwrap();
        let mut inbox = reader(Arc::default());
        assert!(
            matches!(inbox.next().await.what, Incoming::Event(_)),
            "the session part first"
        );
        // One timeline from the first kept frame on, at their own stamps.
        for k in 0..4u64 {
            let Incoming::Audio { pcm, captured } = inbox.next().await.what else {
                panic!("frame {k}")
            };
            assert_eq!(pcm.len(), 480);
            assert_eq!(captured, t0 + Duration::from_millis(20 * k), "frame {k}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_commit_with_no_audio_behind_it_waits_out_its_window_in_the_reader() {
        let (h, tx, _commands) = handle();
        let (_sink, reader) = split(h, Start::default());
        let at = Instant::now();
        let msg = |t: &str| CallEvent::Message {
            message: Message::Text(t.into()),
            at,
        };
        tx.send(msg("{\"type\":\"input_audio_buffer.commit\"}"))
            .unwrap();
        tx.send(msg("{\"type\":\"response.cancel\"}")).unwrap();
        let mut inbox = reader(Arc::default());
        let commit = inbox.next().await;
        assert!(matches!(&commit.what, Incoming::Event(t) if t.as_str().contains("commit")));
        // The default window (120 ms) plus the reorder window (60 ms).
        assert_eq!(Instant::now(), at + Duration::from_millis(180));
        assert_eq!(commit.at, at, "stamped when it came off the socket");
        let cancel = inbox.next().await;
        assert!(matches!(&cancel.what, Incoming::Event(t) if t.as_str().contains("cancel")));
    }

    #[tokio::test]
    async fn a_calls_end_ends_the_reader() {
        let (h, tx, _commands) = handle();
        let (_sink, start) = split(h, Start::default());
        tx.send(CallEvent::Ended(Ended::PeerClosed)).unwrap();
        let mut inbox = start(Arc::default());
        assert!(matches!(
            inbox.next().await.what,
            Incoming::End(End::Closed)
        ));
    }

    #[tokio::test]
    async fn the_sink_sends_events_and_closes_the_call_once() {
        let (h, _tx, mut commands) = handle();
        let (mut sink, _start) = split(h, Start::default());
        // The driver's side, answering each send as str0m would.
        let driver = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(c) = commands.recv().await {
                match c {
                    Command::Send {
                        mut messages,
                        reply,
                        ..
                    } => {
                        let text = messages.remove(0);
                        let sent = match text.as_str() {
                            "big" => Sent::TooLarge {
                                len: 3,
                                limit: 2,
                                by: Limit::Client,
                            },
                            "gone" => Sent::Gone,
                            _ => Sent::Sent,
                        };
                        seen.push(text);
                        let _ = reply.send(sent);
                    }
                    Command::Close { done, .. } => {
                        seen.push("close now".into());
                        if let Some(done) = done {
                            let _ = done.send(());
                        }
                    }
                    Command::CloseDelivered { done, .. } => {
                        seen.push("close once delivered".into());
                        let _ = done.send(());
                    }
                    _ => {}
                }
            }
            seen
        });
        assert!(Sink::event(&mut sink, "{}".into()).await);
        assert!(
            Sink::event(&mut sink, "big".into()).await,
            "an oversized event is logged and the session goes on (WP4)"
        );
        assert!(!Sink::event(&mut sink, "gone".into()).await);
        assert!(
            Sink::ping(&mut sink, &RoundTrip::default()).await,
            "no ping"
        );
        Sink::close(&mut sink, Close::GoingAway).await;
        Sink::end(&mut sink).await;
        drop(sink);
        let seen = driver.await.unwrap();
        assert_eq!(
            seen,
            ["{}", "big", "gone", "close once delivered"],
            "lmgw's close waits for delivery; one close, none on the end or the drop"
        );
    }

    /// The end without a close of lmgw's own (the client closed first, or
    /// the session went) closes at once: nothing waits to be delivered.
    #[tokio::test]
    async fn the_sinks_end_closes_at_once() {
        let (h, _tx, mut commands) = handle();
        let (mut sink, _start) = split(h, Start::default());
        let driver = tokio::spawn(async move {
            match commands.recv().await {
                Some(Command::Close { done, .. }) => {
                    if let Some(done) = done {
                        let _ = done.send(());
                    }
                    "close now"
                }
                Some(Command::CloseDelivered { .. }) => "close once delivered",
                _ => "nothing",
            }
        });
        Sink::end(&mut sink).await;
        assert_eq!(driver.await.unwrap(), "close now");
    }
}
