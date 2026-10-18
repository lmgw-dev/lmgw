//! A call as its owner holds it (calls design §3.4, §7): what the call
//! received, in the order the driver met it, and the way back — a message
//! on `oai-events`, the close, and the end a hangup or a stop asks for.
//!
//! **Who owns a call.** The [`CallHandle`] [`super::Calls::offer`] returns:
//! the route's open path first (part B), then the session it starts. The
//! driver ends a call by itself only when it never connects or never opens
//! (§8.1), when the client closes the peer connection, on a str0m failure,
//! or when its socket group closes and the owner has not ended it within
//! the stop's bound. Everything else is the owner's, told through [`Ending`]: a
//! `POST …/hangup` and a stop of the listener the call came in on — a bound
//! call's last event (`lmgw.session.closed`, §8.2) is the session's to send
//! before the close. Dropping the handle ends the call.
//!
//! **The close after delivery** (§8.2): an owner whose last events must
//! reach the client closes with [`CallControl::close_delivered`]. The call
//! lets go at once — what it kept (the slot) is dropped, and its
//! [`Released`] resolves — and str0m closes it once the client acknowledged
//! everything written on `oai-events`, at most `realtime.ping_interval_s`
//! later. The session lets its call go through [`LetGo`] as it ends, as a
//! WebSocket session gives its slot back with its core, while its writer
//! still holds the control and delivers the last events.
//!
//! **Order.** Everything arrives on one channel ([`CallEvents`]), so a
//! message the client sent before closing `oai-events` comes before
//! [`CallEvent::ChannelClosed`] (the End, §8.1), and audio from the first
//! frame after DTLS-SRTP connects comes before [`CallEvent::Open`] (§4.6).
//!
//! **What waits, and its budget.** The driver never waits for an owner:
//! RTP cannot be slowed (§4.5), and str0m reads SCTP out as it arrives, with
//! no back-pressure per channel. So the bytes of the messages and audio
//! payloads queued for the owner are counted — added as the driver queues
//! them, taken off as [`CallEvents`] hands them out — and past
//! `realtime.max_message_mb` (0: no bound, as the setting says) the driver
//! queues nothing more for the owner and hands it [`CallEvent::TooBig`]
//! behind what was queued. The call stays up: the owner ends it as it ends
//! a call for any other reason, a bound call's session saying why first
//! (`Close::TooBig`, its `lmgw.session.closed` delivered before the close
//! like every other kind's, §8.2). Audio an owner keeps for later
//! ([`Kept`]: the open path's, from the first frame until the session
//! starts, §4.6) stays counted until it is dropped, so it shares the one
//! budget.
//!
//! **The audio out** (§5, WP3) has a way of its own ([`AudioLink`]): the
//! frame clock writes each 20 ms of Opus to the driver, and hears what only
//! the sending side needs — the phone's receiver reports and the sender
//! reports lmgw sent ([`Feedback`]) — as the latest of each, never queued
//! for an owner that has not started reading.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

use super::audio::rtcp::EgressStats;
use super::driver::{Command, Driver};
use super::offer::PhoneAudio;
use super::sizes::Shaper;

/// What a call received, in order.
#[derive(Debug)]
pub enum CallEvent {
    /// ICE and DTLS-SRTP are up (str0m's `Connected`).
    Connected,
    /// The client opened the `oai-events` channel.
    Open,
    /// One message on `oai-events`, and when its last datagram came off
    /// the socket.
    Message { message: Message, at: Instant },
    /// The client closed `oai-events`. The call itself stays until its
    /// owner ends it.
    ChannelClosed,
    /// One audio frame as str0m depacketized it: an Opus payload, raw —
    /// decoding is the call's reader's (WP3).
    Audio(Frame),
    /// The client's RTCP sender report: its NTP time and the RTP time it
    /// names (§16; the exact push-to-talk cut of §4.4 maps through it).
    SenderReport(SenderReport),
    /// What waited for the owner passed `realtime.max_message_mb` (module
    /// doc): the bytes queued, and the bound. Nothing follows it but
    /// [`Self::Ended`]: what the client sends from here is dropped. The call
    /// is still up, for the owner to end.
    TooBig { queued: usize, limit: usize },
    /// The call ended; nothing follows.
    Ended(Ended),
}

impl CallEvent {
    /// The bytes it counts against what waits for the owner (module doc).
    pub(super) fn queued_len(&self) -> usize {
        match self {
            Self::Message {
                message: Message::Text(t),
                ..
            } => t.len(),
            Self::Message {
                message: Message::Binary(b),
                ..
            } => b.len(),
            Self::Audio(f) => f.payload.len(),
            _ => 0,
        }
    }
}

/// A message on `oai-events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A text message: one client event's JSON.
    Text(String),
    /// A binary message (no part of the Realtime protocol), or a text one
    /// that is not UTF-8.
    Binary(Vec<u8>),
}

/// One received audio frame.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Its RTP time, extended past the 32-bit wrap, in ticks of the 48 kHz
    /// RTP clock Opus always runs on.
    pub rtp_time: u64,
    /// When its first packet came off the socket.
    pub arrived: Instant,
    /// It follows the frame before it with no packet missing between.
    pub contiguous: bool,
    /// The extended RTP sequence numbers it was made of.
    pub seq: (u64, u64),
    /// The payload (Opus).
    pub payload: Arc<[u8]>,
}

/// A sender report the client sent (RTCP SR).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderReport {
    /// The client's NTP time it was made at.
    pub ntp_time: SystemTime,
    /// The RTP time that NTP time names, in 48 kHz ticks. str0m extends it
    /// past the 32-bit wrap anchored on the stream's previous report, not
    /// on [`Frame::rtp_time`]'s anchor, so the two are matched modulo 2^32.
    pub rtp_time: u64,
    /// When it came off the socket.
    pub received: Instant,
}

/// Why a call ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// ICE and DTLS did not connect within the never-opened bound after the
    /// answer (§8.1).
    NeverConnected,
    /// `oai-events` did not open within the bound after DTLS connected.
    NeverOpened,
    /// The client closed the peer connection (DTLS close or the SCTP
    /// association's end).
    PeerClosed,
    /// lmgw closed it: its owner, or a dropped handle.
    Closed,
    /// Its socket group closed (lmgw stopping or restarting, or the
    /// listener moved) and the owner had not ended it within the bound.
    GroupClosed,
    /// str0m failed on it, or panicked; the error.
    Failed(String),
}

/// Which bound a message was over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// The client's `max-message-size`.
    Client,
    /// `realtime.max_message_mb`, below the client's.
    Setting,
}

impl Limit {
    /// The bound of `limit` bytes, as a log line names it.
    pub(crate) fn named(self, limit: usize) -> String {
        match self {
            Self::Client => format!("the client's max-message-size of {limit} bytes"),
            Self::Setting => format!("realtime.max_message_mb ({limit} bytes)"),
        }
    }
}

/// What became of a message the owner sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    /// str0m took it.
    Sent,
    /// It is larger than one message may be: never sent. The call's link
    /// shapes an event to fit before it sends it (`sizes`: parts on a bound
    /// call, emptied fields on an unbound one, §6.3), so only an owner that
    /// sends past it meets this.
    TooLarge {
        /// The message's size and the bound, in bytes, and whose bound.
        len: usize,
        limit: usize,
        by: Limit,
    },
    /// The channel is not open, or the call has ended.
    Gone,
}

/// The receiving half of a call: everything it received, in order.
pub struct CallEvents {
    pub(super) rx: mpsc::UnboundedReceiver<CallEvent>,
    /// The bytes queued and not yet handed out (module doc).
    pub(super) queued: Arc<AtomicUsize>,
}

impl CallEvents {
    /// The next thing the call received; `None` once it ended and every
    /// event before the end was read.
    pub async fn next(&mut self) -> Option<CallEvent> {
        let e = self.rx.recv().await;
        self.taken(e)
    }

    /// The next one already there, without waiting.
    pub fn try_next(&mut self) -> Option<CallEvent> {
        let e = self.rx.try_recv().ok();
        self.taken(e)
    }

    fn taken(&self, e: Option<CallEvent>) -> Option<CallEvent> {
        if let Some(e) = &e {
            self.queued.fetch_sub(e.queued_len(), Ordering::AcqRel);
        }
        e
    }

    /// Somewhere to keep this call's audio for later, counted against what
    /// waits for the owner ([`Kept`]).
    pub(super) fn keeper(&self) -> Kept {
        Kept {
            frames: Vec::new(),
            bytes: 0,
            queued: self.queued.clone(),
        }
    }
}

/// Audio frames an owner keeps for later (module doc): their payload bytes
/// are counted again as they are kept, and let go when this is dropped. A
/// frame is off the count only between [`CallEvents::next`] handing it out
/// and [`Kept::push`] taking it, so a check of the driver's in between
/// misses that one frame, and the next check sees it.
pub(crate) struct Kept {
    frames: Vec<Frame>,
    bytes: usize,
    queued: Arc<AtomicUsize>,
}

impl Kept {
    /// Keep `f`, counted again.
    pub fn push(&mut self, f: Frame) {
        let len = f.payload.len();
        self.queued.fetch_add(len, Ordering::AcqRel);
        self.bytes += len;
        self.frames.push(f);
    }

    /// The frames kept, in the order they came.
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }
}

impl Drop for Kept {
    fn drop(&mut self) {
        self.queued.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// What the sending side hears of a call (module doc): the latest of each.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Feedback {
    /// The phone's receiver reports, as str0m's last stats event of lmgw's
    /// stream said them.
    pub egress: Option<EgressStats>,
    /// The last sender report lmgw sent.
    pub sent: Option<SentReport>,
}

/// A sender report lmgw sent (`RawPacket::RtcpTx`, §16): the pair it
/// carried. Its RTP time is the wire's 32 bits, matched modulo 2^32
/// (`audio::line::SentLine`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SentReport {
    pub ntp: SystemTime,
    pub rtp: u64,
}

/// One 20 ms frame of Opus for the phone (§5.1, §5.2): its slot's due time
/// as str0m's wallclock, its RTP time at 48 kHz, and whether it starts a
/// talkspurt (the marker bit after a DTX pause).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RtpOut {
    pub wallclock: Instant,
    pub rtp: u64,
    pub payload: Vec<u8>,
    pub talkspurt: bool,
}

/// The frame clock's way to its call (module doc): cheap to clone.
#[derive(Clone)]
pub(crate) struct AudioLink {
    id: Arc<str>,
    driver: Driver,
    feedback: watch::Receiver<Feedback>,
    /// What its events are shaped to (§6.3).
    shaper: Shaper,
}

impl AudioLink {
    /// The call's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Write one frame; `false`: the driver is gone.
    pub fn write(&self, frame: RtpOut) -> bool {
        self.driver.send(Command::Audio {
            id: self.id.clone(),
            frame,
        })
    }

    /// Send `text` on `oai-events`, queued with the driver now behind every
    /// message sent before it, without waiting for what becomes of it: a
    /// transcript delta, `started` or `starved` the frame clock released
    /// (§5.1, §6.2), whose clock never waits on the data channel (§6.3). An
    /// event over the largest a call sends is shaped as the session's are
    /// (`sizes`): parts, or emptied fields and their errors. The driver says
    /// at WARN what was not sent, as the session's sink does for its events:
    /// (once) the channel gone while the client had not closed it. A call
    /// that let go or is closing takes none (§8.2). `false`: the driver is
    /// gone.
    pub fn event(&self, text: String) -> bool {
        let messages = self.shaper.shape(text);
        if messages.is_empty() {
            return true;
        }
        self.driver.send(Command::ClockEvent {
            id: self.id.clone(),
            messages,
        })
    }

    /// The call's playout allowance changed: the status block shows it
    /// (calls design §9). `false`: the driver is gone.
    pub fn playout(&self, playout: super::CallPlayout) -> bool {
        self.driver.send(Command::Playout {
            id: self.id.clone(),
            playout,
        })
    }

    /// The latest feedback, if it changed since the last look.
    pub fn feedback(&mut self) -> Option<Feedback> {
        match self.feedback.has_changed() {
            Ok(true) => Some(self.feedback.borrow_and_update().clone()),
            _ => None,
        }
    }
}

/// Why lmgw asks a call's owner to end it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// `POST /v1/realtime/calls/{call_id}/hangup` (§2.6), by `by`, named as
    /// `voice.bound.by` names a client.
    HungUp { by: Option<String> },
    /// The listener the call came in on stops (lmgw stopping or
    /// restarting, or the device listener moving): a bound call's last
    /// event is `lmgw.session.closed {kind: "shutting_down"}` (§8.2). The
    /// group's sockets stay open for it until the stop's bound.
    ShuttingDown,
}

/// Resolves once a call has let go: its slot dropped, and its registry
/// entry gone or [`super::CallState::Closing`] (still delivering its last
/// events, §8.2). What a hangup answers after (C3).
pub struct Released(pub(super) watch::Receiver<Option<Ending>>);

impl Released {
    /// Wait until the call has let go.
    pub async fn wait(mut self) {
        while self.0.changed().await.is_ok() {}
    }
}

/// Where a call's owner hears why lmgw asks it to end the call: a copy of
/// [`CallControl::ending`]'s watch, for whoever owns the call once its
/// control went into the session's sending half (the session, calls design
/// §2.6, §3.1).
#[derive(Clone)]
pub struct Asked(watch::Receiver<Option<Ending>>);

impl Asked {
    /// Wait until lmgw asks, and why. Pends forever once the call is gone
    /// without being asked.
    pub async fn ending(&mut self) -> Ending {
        loop {
            if let Some(e) = self.0.borrow_and_update().clone() {
                return e;
            }
            if self.0.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

/// The controlling half of a call: send, close, hear why it should end.
/// Dropping it ends the call.
pub struct CallControl {
    pub(super) id: Arc<str>,
    pub(super) driver: Driver,
    pub(super) ending: watch::Receiver<Option<Ending>>,
    pub(super) ended: bool,
    /// What the phone's offer asked of lmgw's audio (§5.2).
    pub(super) phone: PhoneAudio,
    /// The reorder window str0m holds frames behind a gap for
    /// (`realtime.webrtc_reorder_ms` at the offer): what the reader's
    /// wall-clock bound and cuts take, so both match str0m's (§4.2, §4.4).
    pub(super) reorder: std::time::Duration,
    /// What the sending side hears of the call ([`Feedback`]).
    pub(super) feedback: watch::Receiver<Feedback>,
    /// The largest message the call sends, and whose bound it is (§6.3):
    /// what its link shapes an event to (`sizes`).
    pub(super) largest: (usize, Limit),
}

impl CallControl {
    /// The call's id (`rtc_…`, §2.4).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Send `text` on `oai-events`. A write waits while more than str0m's
    /// default send buffer is in flight (back-pressure, §6.3), whatever the
    /// client advertises.
    pub async fn send(&self, text: String) -> Sent {
        self.queue(text).await
    }

    /// Send `messages` on `oai-events` back to back, nothing of the call's
    /// between them (an event's parts, or an emptied event and its errors,
    /// §6.3), as one write: what became of the last, or the first refusal.
    pub(crate) async fn send_all(&self, messages: Vec<String>) -> Sent {
        self.queue_all(messages).await
    }

    /// [`Self::send`] without the wait: the message is queued with the
    /// driver now, behind every message sent on the call before it, and what
    /// comes back only hears what became of it. For an owner whose own
    /// bounds must not wait on a client that stopped reading (the open path,
    /// calls design §2.8).
    pub fn queue(&self, text: String) -> impl Future<Output = Sent> + Send + 'static {
        self.queue_all(vec![text])
    }

    /// [`Self::queue`] for messages that go back to back
    /// ([`Self::send_all`]).
    fn queue_all(&self, messages: Vec<String>) -> impl Future<Output = Sent> + Send + 'static {
        let (reply, sent) = oneshot::channel();
        let queued = self.driver.send(Command::Send {
            id: self.id.clone(),
            messages,
            reply,
        });
        async move {
            if !queued {
                return Sent::Gone;
            }
            sent.await.unwrap_or(Sent::Gone)
        }
    }

    /// Wait until lmgw asks the owner to end the call, and why. Pends
    /// forever once the call is gone without being asked.
    pub async fn ending(&mut self) -> Ending {
        Asked(self.ending.clone()).ending().await
    }

    /// The same question for whoever owns the call next ([`Asked`]).
    pub fn asked(&self) -> Asked {
        Asked(self.ending.clone())
    }

    /// End the call once what was sent on it is delivered (module doc): it
    /// lets go before this returns — what it kept is dropped and its
    /// [`Released`] resolves — and str0m closes it once the client
    /// acknowledged everything written (the writes still waiting included),
    /// the channel is gone, or `realtime.ping_interval_s` passed (the call's
    /// bound at its offer; none at 0). Closing again does nothing.
    pub async fn close_delivered(&mut self) {
        if std::mem::replace(&mut self.ended, true) {
            return;
        }
        let (done, let_go) = oneshot::channel();
        if self.driver.send(Command::CloseDelivered {
            id: self.id.clone(),
            done,
        }) {
            let _ = let_go.await;
        }
    }

    /// Where whoever owns the call next lets it go ([`LetGo`]).
    pub fn letting_go(&self) -> LetGo {
        LetGo {
            id: self.id.clone(),
            driver: self.driver.clone(),
        }
    }

    /// What the phone's offer asked of lmgw's audio (§5.2).
    pub(crate) fn phone(&self) -> PhoneAudio {
        self.phone
    }

    /// The reorder window the call's `Rtc` was made with (§4.2).
    pub(crate) fn reorder(&self) -> std::time::Duration {
        self.reorder
    }

    /// The largest message the call sends, and whose bound it is (§6.3).
    pub(crate) fn largest(&self) -> (usize, Limit) {
        self.largest
    }

    /// The frame clock's way to the call ([`AudioLink`]), its events shaped
    /// by `shaper` as the session's are (§6.3).
    pub(crate) fn audio_link(&self, shaper: Shaper) -> AudioLink {
        AudioLink {
            id: self.id.clone(),
            driver: self.driver.clone(),
            feedback: self.feedback.clone(),
            shaper,
        }
    }

    /// End the call now: str0m closes it (DTLS close_notify, the SCTP
    /// association's end), and it is gone from the registry once this
    /// returns. Closing again does nothing.
    pub async fn close(&mut self) {
        if std::mem::replace(&mut self.ended, true) {
            return;
        }
        let (done, closed) = oneshot::channel();
        if self.driver.send(Command::Close {
            id: self.id.clone(),
            done: Some(done),
        }) {
            let _ = closed.await;
        }
    }
}

impl Drop for CallControl {
    fn drop(&mut self) {
        if !self.ended {
            self.driver.send(Command::Close {
                id: self.id.clone(),
                done: None,
            });
        }
    }
}

/// Lets a call go without closing it (module doc): its slot dropped and its
/// [`Released`] resolved while its link still delivers the last events. For
/// the session, whose writer holds the call's control and closes it.
#[derive(Clone)]
pub struct LetGo {
    id: Arc<str>,
    driver: Driver,
}

impl LetGo {
    /// Let the call go now; nothing for a call that is gone.
    pub fn now(&self) {
        self.driver.send(Command::LetGo {
            id: self.id.clone(),
        });
    }
}

/// A call as [`super::Calls::offer`] answers it: both halves.
pub struct CallHandle {
    pub control: CallControl,
    pub events: CallEvents,
}

impl CallHandle {
    /// The call's id.
    pub fn id(&self) -> &str {
        self.control.id()
    }
}
