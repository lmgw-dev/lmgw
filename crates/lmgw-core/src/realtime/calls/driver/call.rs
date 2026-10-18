//! One call inside the driver: its `Rtc` driven by str0m's rule — every
//! mutation (a datagram, a timeout, a write, the close) followed by a drain
//! of `poll_output` to its next timeout — and what the drain gives turned
//! into the call's events, its transmits and its registry entry.
//!
//! **What the client reads** (calls design §8.1): after each drain the call
//! looks how much of what it wrote on `oai-events` str0m still counts as
//! buffered — written data counts until the client acknowledges it — and
//! says in `CallInfo::unacked_since` since when output has waited with no
//! acknowledgement, the session's evidence that the client reads.
//!
//! **The close after delivery** (§8.2): a call its owner closes with
//! [`Call::close_delivered`] lets go at once (its slot dropped, its registry
//! entry no longer live) and keeps its `Rtc` until the client acknowledged
//! everything written — the last event among it — or the channel is gone,
//! at most its bound (`realtime.ping_interval_s`; none at 0); then str0m
//! closes it. What arrives meanwhile goes to no owner, and nothing more is
//! written on it.
//!
//! **Past its budget** (`handle`'s module doc) the call queues nothing more
//! for its owner and hands it `CallEvent::TooBig`; it closes only when the
//! owner closes it, so a bound call's last event says why first.

use std::any::Any;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::rtp::rtcp::Rtcp;
use str0m::rtp::RawPacket;
use str0m::{Event, IceConnectionState, Input, Output, Rtc};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

use super::super::audio::rtcp::{EgressStats, PhoneReports};
use super::super::handle::{
    CallEvent, Ended, Feedback, Frame, Limit, Message, RtpOut, SenderReport, Sent, SentReport,
};
use super::super::registry::{CallState, Registry};
use super::super::sockets::{SockId, UdpSock};

/// The channel a call's events travel on (OpenAI's name, §1.2).
const EVENTS_CHANNEL: &str = "oai-events";

/// A write waiting for room in the channel's send buffer: one message, and
/// the owner's reply on the last of the messages it sent together.
struct Write {
    data: Vec<u8>,
    reply: Option<oneshot::Sender<Sent>>,
}

/// A call the driver holds.
pub(in super::super) struct Call {
    pub id: Arc<str>,
    /// Its local ICE ufrag: what a binding request names it by.
    pub ufrag: String,
    /// Its local ICE password: what a binding request's MESSAGE-INTEGRITY is
    /// checked with.
    pub pass: String,
    /// The socket it lives on: its group's, for its candidate's address.
    pub sock: SockId,
    pub rtc: Rtc,
    events: mpsc::UnboundedSender<CallEvent>,
    /// The bytes queued for the owner and not yet handed out, shared with
    /// its `CallEvents`, and their bound (`None`: none).
    queued: Arc<AtomicUsize>,
    queue_limit: Option<usize>,
    /// The audio media, once str0m announced it.
    audio: Option<Mid>,
    /// Opus's payload type on it, found at the first write.
    opus: Option<Pt>,
    /// What the sending side hears (`handle::Feedback`): the phone's
    /// receiver reports and the sender reports lmgw sent.
    feedback: watch::Sender<Feedback>,
    /// The phone's receiver reports as the status block's figures take
    /// them: a new one sets them and their age (`CallStats`).
    reports: PhoneReports,
    /// `oai-events`, once open.
    channel: Option<ChannelId>,
    /// The client closed `oai-events` (its End, §8.1): the frame clock's
    /// events then go nowhere without a WARN ([`Self::write_clock`]).
    channel_closed: bool,
    /// That the frame clock's events go nowhere was said (once).
    clock_gone_said: bool,
    /// The largest message the call sends, and whose bound it is.
    max_message: usize,
    by: Limit,
    /// What is kept in flight at most before the next message is written:
    /// str0m's default send buffer.
    window: usize,
    writes: VecDeque<Write>,
    /// str0m's next timeout.
    pub next: Option<Instant>,
    /// The call's liveness interval (`realtime.ping_interval_s` at its
    /// offer, §8.1): the never-opened bound, and how long a close waits for
    /// delivery. `None` is no bound.
    bound: Option<Duration>,
    connect_by: Option<Instant>,
    open_by: Option<Instant>,
    /// What waited for the owner passed its budget (module doc): the owner
    /// was handed `CallEvent::TooBig`, and nothing more is queued for it.
    over: bool,
    /// The datagram being received proved itself: authenticated traffic,
    /// the liveness evidence (`CallInfo::last_heard`).
    evidence: bool,
    /// The bytes str0m took on `oai-events` so far, and how many of them
    /// the client had acknowledged at the last look (module doc).
    written: u64,
    acked: u64,
    /// `CallInfo::unacked_since` as last said (module doc).
    unacked_since: Option<Instant>,
    /// It let go (module doc): what it kept is dropped, its registry entry
    /// no longer live, and it hands its owner nothing more.
    let_go: bool,
    /// It closes once what it wrote is delivered (module doc), at this time
    /// at the latest (`None`: no bound).
    closing: Option<Option<Instant>>,
    /// str0m panicked on it: nothing touches its `Rtc` again.
    poisoned: bool,
    /// Set once it ends; the driver then removes it.
    pub ended: Option<Ended>,
    /// Dropped with the call: what its owner keeps for its life (the
    /// concurrency slot, C3).
    _keep: Option<Box<dyn Any + Send>>,
    /// A test asks it to panic in its next tick.
    #[cfg(test)]
    pub panic_next: bool,
}

/// What a new call carries into the driver.
pub(in super::super) struct NewCall {
    pub id: Arc<str>,
    pub ufrag: String,
    pub pass: String,
    pub sock: SockId,
    pub rtc: Rtc,
    pub events: mpsc::UnboundedSender<CallEvent>,
    pub feedback: watch::Sender<Feedback>,
    pub queued: Arc<AtomicUsize>,
    pub queue_limit: Option<usize>,
    pub max_message: usize,
    pub by: Limit,
    pub window: usize,
    pub bound: Option<Duration>,
    pub keep: Option<Box<dyn Any + Send>>,
}

impl Call {
    pub fn new(c: NewCall, now: Instant) -> Self {
        Self {
            id: c.id,
            ufrag: c.ufrag,
            pass: c.pass,
            sock: c.sock,
            rtc: c.rtc,
            events: c.events,
            queued: c.queued,
            queue_limit: c.queue_limit,
            audio: None,
            opus: None,
            feedback: c.feedback,
            reports: PhoneReports::default(),
            channel: None,
            channel_closed: false,
            clock_gone_said: false,
            max_message: c.max_message,
            by: c.by,
            window: c.window,
            writes: VecDeque::new(),
            next: None,
            bound: c.bound,
            connect_by: c.bound.map(|b| now + b),
            open_by: None,
            over: false,
            evidence: false,
            written: 0,
            acked: 0,
            unacked_since: None,
            let_go: false,
            closing: None,
            poisoned: false,
            ended: None,
            _keep: c.keep,
            #[cfg(test)]
            panic_next: false,
        }
    }

    /// When the driver must next look at this call.
    pub fn deadline(&self) -> Option<Instant> {
        if self.ended.is_some() {
            return None;
        }
        [
            self.next,
            self.connect_by,
            self.open_by,
            self.closing.flatten(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Its first drain, right after the offer's acceptance (a mutation).
    pub fn start(&mut self, now: Instant, sock: &UdpSock, reg: &Registry) {
        self.drain(sock, reg, now);
    }

    /// A datagram it accepted (`Rtc::accepts`), from `source`;
    /// `authenticated`: a binding request whose integrity the driver checked.
    pub fn receive(
        &mut self,
        input: Input<'_>,
        source: SocketAddr,
        authenticated: bool,
        now: Instant,
        sock: &UdpSock,
        reg: &Registry,
    ) {
        if self.poisoned {
            return;
        }
        // The fast path (`accepts` by the nominated pair's address) proves
        // nothing; what str0m made of it, after SRTP's or DTLS's checks, does
        // (`event`).
        self.evidence = authenticated;
        let r = self.rtc.handle_input(input);
        self.after(r, sock, reg, now);
        if std::mem::take(&mut self.evidence) {
            reg.update(&self.id, |i| {
                i.remote = Some(source);
                i.last_heard = Some(now);
            });
        }
        self.settle(sock, reg, now);
    }

    /// Run what is due at `now`: str0m's timeout, and the never-opened
    /// bound (§8.1).
    pub fn tick(&mut self, now: Instant, sock: &UdpSock, reg: &Registry) {
        if self.ended.is_some() || self.poisoned {
            return;
        }
        #[cfg(test)]
        if std::mem::take(&mut self.panic_next) {
            panic!("a panic a test injected");
        }
        if self.connect_by.is_some_and(|t| t <= now) {
            let b = self.bound.unwrap_or_default();
            tracing::info!(
                "WebRTC call {}: ICE and DTLS did not connect within {} s of the answer \
                 (realtime.ping_interval_s); ended",
                self.id,
                b.as_secs()
            );
            return self.close(Ended::NeverConnected, sock, reg, now);
        }
        if self.open_by.is_some_and(|t| t <= now) {
            let b = self.bound.unwrap_or_default();
            tracing::info!(
                "WebRTC call {}: the `{EVENTS_CHANNEL}` channel did not open within {} s of \
                 connecting (realtime.ping_interval_s); ended",
                self.id,
                b.as_secs()
            );
            return self.close(Ended::NeverOpened, sock, reg, now);
        }
        if self.next.is_some_and(|t| t <= now) {
            let r = self.rtc.handle_input(Input::Timeout(now.into_std()));
            self.after(r, sock, reg, now);
        }
        // sctp-proto says `ChannelBufferedAmountLow` only on a crossing from
        // above the mark to it: a write left waiting while the buffer was
        // already at or below the window hears nothing, so each tick looks.
        if let (false, Some(id)) = (self.writes.is_empty(), self.channel) {
            let window = self.window;
            let low = self
                .rtc
                .channel(id)
                .is_some_and(|mut ch| ch.buffered_amount() <= window);
            if low && self.ended.is_none() {
                self.flush();
                self.drain(sock, reg, now);
            }
        }
        self.settle(sock, reg, now);
    }

    /// Send `messages` on `oai-events`, back to back, in order behind any
    /// write that waits: nothing else the call sends goes between them (an
    /// event's parts, §6.3). One over the largest message refuses them all,
    /// unsent. Nothing is written once the call closes (module doc): the
    /// close comes behind the last event its owner said.
    pub fn write(
        &mut self,
        messages: Vec<String>,
        reply: oneshot::Sender<Sent>,
        sock: &UdpSock,
        reg: &Registry,
        now: Instant,
    ) {
        if self.ended.is_some() || self.poisoned || self.channel.is_none() || self.closing.is_some()
        {
            let _ = reply.send(Sent::Gone);
            return;
        }
        if let Some(len) = messages
            .iter()
            .map(String::len)
            .find(|len| *len > self.max_message)
        {
            let _ = reply.send(Sent::TooLarge {
                len,
                limit: self.max_message,
                by: self.by,
            });
            return;
        }
        let waiting = self.writes.len();
        let last = messages.len().saturating_sub(1);
        let mut reply = Some(reply);
        for (i, text) in messages.into_iter().enumerate() {
            self.writes.push_back(Write {
                data: text.into_bytes(),
                reply: if i == last { reply.take() } else { None },
            });
        }
        if let Some(reply) = reply {
            // Nothing to send: taken.
            let _ = reply.send(Sent::Sent);
        }
        if waiting == 0 {
            self.flush();
            self.drain(sock, reg, now);
        }
    }

    /// Write what waits, in order, while no more than the window is in
    /// flight; past it, hear when the buffer has drained to the window. The
    /// buffer is the window plus the largest message, so every write that
    /// passes the gate is taken, and the client's `max-message-size` never
    /// sets how much lmgw keeps in flight.
    fn flush(&mut self) {
        let Some(id) = self.channel else {
            return;
        };
        while let Some(w) = self.writes.front() {
            let Some(mut ch) = self.rtc.channel(id) else {
                return self.fail_writes();
            };
            if ch.buffered_amount() > self.window {
                ch.set_buffered_amount_low_threshold(self.window);
                return;
            }
            match ch.write(false, &w.data) {
                Ok(true) => {
                    let w = self.writes.pop_front().expect("the front was there");
                    self.written += w.data.len() as u64;
                    if let Some(reply) = w.reply {
                        let _ = reply.send(Sent::Sent);
                    }
                }
                Ok(false) => {
                    // Not reached while the buffer holds the window plus the
                    // largest message; waiting is the safe answer anyway,
                    // and the next tick tries again (`tick`).
                    ch.set_buffered_amount_low_threshold(self.window);
                    return;
                }
                Err(e) => {
                    tracing::debug!("WebRTC call {}: a write failed: {e}", self.id);
                    return self.fail_writes();
                }
            }
        }
    }

    /// What the frame clock released (a transcript delta, `started`,
    /// `starved`, §5.1, §6.2, shaped to fit by its link), written as
    /// [`Self::write`] writes, in order behind every write before it. A call
    /// that let go or is closing takes none: its session is over, and
    /// nothing may follow its last event (§8.2; the writer stops the clock
    /// before that, and this is the backstop). What was not sent is said at
    /// WARN, as the session's sink says its own: a message over the largest
    /// a call sends, and once the channel gone while the client had not
    /// closed it.
    pub fn write_clock(
        &mut self,
        messages: Vec<String>,
        sock: &UdpSock,
        reg: &Registry,
        now: Instant,
    ) {
        if self.let_go || self.closing.is_some() {
            return;
        }
        let (reply, mut sent) = oneshot::channel();
        self.write(messages, reply, sock, reg, now);
        match sent.try_recv() {
            Ok(Sent::TooLarge { len, limit, by }) => tracing::warn!(
                "WebRTC call {}: an event of {len} bytes is over {} and was not sent",
                self.id,
                by.named(limit)
            ),
            Ok(Sent::Gone) if !self.channel_closed && !self.clock_gone_said => {
                self.clock_gone_said = true;
                tracing::warn!(
                    "WebRTC call {}: `{EVENTS_CHANNEL}` is gone; the frame clock's transcript \
                     events are not sent",
                    self.id
                );
            }
            // Taken, waiting for the window, or gone with a channel the client
            // closed (the End is underway).
            _ => {}
        }
    }

    /// Write one frame of audio (§5.1): its slot's due time as str0m's
    /// wallclock, so every sender report lies on the frame clock's line
    /// (§5.3), and packetized at once — str0m payloads a written frame when
    /// it next handles a timeout. A write before the audio media is known,
    /// or that str0m refuses, is dropped with a DEBUG line: the stream's RTP
    /// time passes on either way. A call that let go or is closing takes
    /// none, as for the clock's events ([`Self::write_clock`]).
    pub fn write_audio(&mut self, f: RtpOut, sock: &UdpSock, reg: &Registry, now: Instant) {
        if self.ended.is_some() || self.poisoned || self.let_go || self.closing.is_some() {
            return;
        }
        let Some(mid) = self.audio else {
            return;
        };
        let Some(pt) = self.opus.or_else(|| {
            self.rtc.writer(mid).and_then(|w| {
                w.payload_params()
                    .find(|p| p.spec().codec == Codec::Opus)
                    .map(|p| p.pt())
            })
        }) else {
            tracing::debug!("WebRTC call {}: no Opus to write audio with", self.id);
            return;
        };
        self.opus = Some(pt);
        let Some(writer) = self.rtc.writer(mid) else {
            return;
        };
        let written = writer.start_of_talkspurt(f.talkspurt).write(
            pt,
            f.wallclock.into_std(),
            MediaTime::new(f.rtp, Frequency::FORTY_EIGHT_KHZ),
            f.payload,
        );
        if let Err(e) = written {
            tracing::debug!("WebRTC call {}: an audio frame not written: {e}", self.id);
        }
        let r = self.rtc.handle_input(Input::Timeout(now.into_std()));
        self.after(r, sock, reg, now);
    }

    fn fail_writes(&mut self) {
        for w in self.writes.drain(..) {
            if let Some(reply) = w.reply {
                let _ = reply.send(Sent::Gone);
            }
        }
    }

    /// End the call from lmgw's side: str0m closes it (RTCP BYE, the SCTP
    /// association's end, DTLS close_notify), and what that sends goes out
    /// before the call is removed.
    pub fn close(&mut self, why: Ended, sock: &UdpSock, reg: &Registry, now: Instant) {
        // The reason first: the drain below sees the `Rtc` closed, which on
        // its own reads as the client's close. An `Rtc` already closed
        // closes again as a no-op.
        self.end(why);
        if self.poisoned {
            return;
        }
        let r = self.rtc.close();
        self.after(r, sock, reg, now);
    }

    fn end(&mut self, why: Ended) {
        if self.ended.is_none() {
            self.ended = Some(why);
        }
    }

    /// Let go (module doc): what it kept (the slot) is dropped and its
    /// registry entry no longer live, so a hangup's `Released` resolves;
    /// the `Rtc` stays until the call is closed. Again does nothing.
    pub fn let_go(&mut self, reg: &Registry) {
        if std::mem::replace(&mut self.let_go, true) {
            return;
        }
        drop(self._keep.take());
        reg.let_go(&self.id);
    }

    /// Close once what was written is delivered (module doc): let go now,
    /// and close when the client acknowledged it all, the channel is gone,
    /// or the bound passed.
    pub fn close_delivered(&mut self, sock: &UdpSock, reg: &Registry, now: Instant) {
        self.let_go(reg);
        if self.closing.is_none() {
            self.closing = Some(self.bound.map(|b| now + b));
        }
        self.settle(sock, reg, now);
    }

    /// A call closing after delivery (module doc): closed once nothing it
    /// wrote is unacknowledged, or at its bound.
    fn settle(&mut self, sock: &UdpSock, reg: &Registry, now: Instant) {
        let Some(by) = self.closing else {
            return;
        };
        if self.ended.is_some() || self.poisoned {
            return;
        }
        if self.writes.is_empty() && self.in_flight() == 0 {
            return self.close(Ended::Closed, sock, reg, now);
        }
        if by.is_some_and(|t| t <= now) {
            tracing::info!(
                "WebRTC call {}: the client did not acknowledge the last events within {} s \
                 (realtime.ping_interval_s); closed without them",
                self.id,
                self.bound.unwrap_or_default().as_secs()
            );
            self.close(Ended::Closed, sock, reg, now);
        }
    }

    /// The bytes written on `oai-events` that the client has not
    /// acknowledged yet (str0m's buffered amount); 0 without the channel.
    fn in_flight(&mut self) -> usize {
        let Some(id) = self.channel else {
            return 0;
        };
        self.rtc
            .channel(id)
            .map_or(0, |mut ch| ch.buffered_amount())
    }

    /// After a drain: since when the output has waited for the client with
    /// no acknowledgement, said in the registry when it moved (module doc).
    fn note_flight(&mut self, reg: &Registry, now: Instant) {
        let buffered = self.in_flight() as u64;
        // str0m's own 1-byte DCEP ACK, sent on the same stream when the
        // client opens the channel, counts in the buffered amount but not in
        // `written`: `saturating_sub` absorbs it, the acknowledgements read
        // at most one byte short until it is acknowledged itself.
        let acked = self.written.saturating_sub(buffered);
        let since = if buffered == 0 && self.writes.is_empty() {
            None
        } else if acked > self.acked || self.unacked_since.is_none() {
            Some(now)
        } else {
            self.unacked_since
        };
        self.acked = acked;
        if since != self.unacked_since {
            self.unacked_since = since;
            reg.update(&self.id, |i| i.unacked_since = since);
        }
    }

    /// str0m panicked on this call (`what` it was doing, `msg` the panic's):
    /// it ends, and nothing touches its `Rtc` again.
    pub fn poison(&mut self, what: &str, msg: &str) {
        tracing::error!(
            "WebRTC call {}: str0m panicked in {what}: {msg}; ended",
            self.id
        );
        self.poisoned = true;
        self.end(Ended::Failed(format!("str0m panicked in {what}: {msg}")));
    }

    /// The call's last step in the driver: gone from the registry, its
    /// owner told why, its waiting writes failed, what it kept dropped.
    ///
    /// What it kept goes first: a hangup's `Released` and a group close's
    /// wait resolve on the registry entry's removal, and the slot must be
    /// free by then (C3). An `Rtc` str0m panicked on is dropped under
    /// `catch_unwind` too.
    pub fn finish(mut self, reg: &Registry) {
        self.fail_writes();
        drop(self._keep.take());
        let why = self.ended.take().unwrap_or(Ended::Closed);
        let Self {
            id,
            rtc,
            events,
            poisoned,
            ..
        } = self;
        if poisoned {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(rtc))).is_err() {
                tracing::error!("WebRTC call {id}: str0m panicked dropping it too");
            }
        } else {
            drop(rtc);
        }
        reg.remove(&id);
        let _ = events.send(CallEvent::Ended(why));
    }

    /// After a mutation: its error ends the call, else drain.
    fn after(
        &mut self,
        r: Result<(), str0m::RtcError>,
        sock: &UdpSock,
        reg: &Registry,
        now: Instant,
    ) {
        if let Err(e) = r {
            tracing::info!("WebRTC call {}: {e}; ended", self.id);
            self.end(Ended::Failed(e.to_string()));
        }
        self.drain(sock, reg, now);
    }

    /// Drain `poll_output` to its next timeout (str0m's single-mutation
    /// rule): transmits go out on `sock`, events to the call.
    fn drain(&mut self, sock: &UdpSock, reg: &Registry, now: Instant) {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => {
                    self.next = Some(Instant::from_std(t));
                    break;
                }
                Ok(Output::Transmit(t)) => {
                    sock.send(t.source, t.destination, &t.contents);
                }
                Ok(Output::Event(e)) => self.event(e, reg, now),
                Err(e) => {
                    tracing::info!("WebRTC call {}: {e}; ended", self.id);
                    self.end(Ended::Failed(e.to_string()));
                    self.next = None;
                    break;
                }
            }
        }
        if !self.rtc.is_alive() {
            self.end(Ended::PeerClosed);
        } else if !self.poisoned {
            self.note_flight(reg, now);
        }
    }

    /// Hand the owner `e`; nothing once it was told the call is over its
    /// budget (module doc).
    fn emit(&self, e: CallEvent) {
        if !self.over {
            let _ = self.events.send(e);
        }
    }

    /// Queue a message or a frame for the owner, counted against the
    /// budget; past it nothing more is queued, and the owner hears
    /// `CallEvent::TooBig` behind what was (`handle`'s module doc).
    fn emit_counted(&mut self, e: CallEvent) {
        // A call that let go has no owner left to read it (module doc).
        if self.ended.is_some() || self.let_go || self.over {
            return;
        }
        let len = e.queued_len();
        let queued = self.queued.fetch_add(len, Ordering::AcqRel) + len;
        self.emit(e);
        if let Some(limit) = self.queue_limit.filter(|l| queued > *l) {
            tracing::warn!(
                "WebRTC call {}: {queued} bytes of messages and audio wait for its session, \
                 over realtime.max_message_mb ({limit} bytes); nothing more is queued, and its \
                 owner ends it",
                self.id
            );
            self.emit(CallEvent::TooBig { queued, limit });
            self.over = true;
        }
    }

    fn event(&mut self, e: Event, reg: &Registry, now: Instant) {
        // What only the client's authenticated traffic makes (`receive`).
        if matches!(
            e,
            Event::Connected
                | Event::MediaData(_)
                | Event::SenderFeedback(_)
                | Event::ChannelOpen(..)
                | Event::ChannelData(_)
                | Event::ChannelClose(_)
                | Event::ChannelBufferedAmountLow(_)
        ) {
            self.evidence = true;
        }
        match e {
            Event::Connected => {
                self.connect_by = None;
                self.open_by = self.bound.map(|b| now + b);
                reg.update(&self.id, |i| {
                    i.state = CallState::Connected;
                    i.connected = Some(now);
                });
                self.emit(CallEvent::Connected);
            }
            Event::IceConnectionStateChange(s) => {
                // str0m's own ICE-lite verdict (a pair 15 s without a check)
                // is logged, not acted on: lmgw's reads the setting (§8.1).
                if s == IceConnectionState::Disconnected {
                    tracing::info!("WebRTC call {}: str0m reports ICE disconnected", self.id);
                } else {
                    tracing::debug!("WebRTC call {}: ICE {s:?}", self.id);
                }
            }
            Event::MediaAdded(m) => {
                if m.kind == MediaKind::Audio && self.audio.is_none() {
                    self.audio = Some(m.mid);
                }
            }
            Event::MediaData(d) => {
                if Some(d.mid) == self.audio {
                    self.emit_counted(CallEvent::Audio(Frame {
                        rtp_time: d.time.numer(),
                        arrived: Instant::from_std(d.network_time),
                        contiguous: d.contiguous,
                        seq: (**d.seq_range.start(), **d.seq_range.end()),
                        payload: d.data,
                    }));
                }
            }
            Event::SenderFeedback(f) => {
                if Some(f.mid) == self.audio {
                    self.emit(CallEvent::SenderReport(SenderReport {
                        ntp_time: f.sender_info.ntp_time,
                        rtp_time: f.sender_info.rtp_time.numer(),
                        received: Instant::from_std(f.received_at),
                    }));
                }
            }
            Event::ChannelOpen(id, label) => {
                if label == EVENTS_CHANNEL && self.channel.is_none() {
                    self.channel = Some(id);
                    self.open_by = None;
                    reg.update(&self.id, |i| {
                        i.state = CallState::Open;
                        i.opened = Some(now);
                    });
                    self.emit(CallEvent::Open);
                } else {
                    tracing::debug!("WebRTC call {}: channel '{label}' ignored", self.id);
                }
            }
            Event::ChannelData(d) => {
                if Some(d.id) == self.channel {
                    let m = if d.binary {
                        Message::Binary(d.data)
                    } else {
                        match String::from_utf8(d.data) {
                            Ok(t) => Message::Text(t),
                            Err(e) => Message::Binary(e.into_bytes()),
                        }
                    };
                    self.emit_counted(CallEvent::Message {
                        message: m,
                        at: now,
                    });
                }
            }
            Event::ChannelClose(id) => {
                if Some(id) == self.channel {
                    self.channel = None;
                    self.channel_closed = true;
                    self.fail_writes();
                    self.emit(CallEvent::ChannelClosed);
                }
            }
            Event::ChannelBufferedAmountLow(id) => {
                if Some(id) == self.channel {
                    self.flush();
                }
            }
            // lmgw's own count of the client's packets over the interval: an
            // interval with none is no figure, never the last one's. str0m's
            // round trip and egress loss here are its congestion feedback's;
            // the status block shows the receiver reports' instead, as the
            // session takes them.
            Event::PeerStats(s) => reg.update(&self.id, |i| {
                i.stats.ingress_loss = s.ingress_loss_fraction;
            }),
            Event::MediaEgressStats(s) if Some(s.mid) == self.audio => {
                let egress = EgressStats {
                    rtt: s.rtt,
                    loss: s.loss,
                    jitter: s.remote.as_ref().map(|r| r.jitter),
                    max_seq: s.remote.as_ref().map(|r| *r.maximum_sequence_number),
                };
                if let Some(r) = self.reports.stats(&egress) {
                    reg.update(&self.id, |i| {
                        i.stats.report_at = Some(now);
                        i.stats.client_jitter = Some(r.jitter);
                        i.stats.egress_loss = r.loss;
                    });
                }
                self.feedback.send_modify(|f| f.egress = Some(egress));
            }
            // Raw packets are on for one of them (`offer`'s module doc): the
            // sender reports lmgw sends, whose pair the playout mapping
            // reads (§5.3). Every other copy goes.
            // A report made before any frame was written names RTP time 0,
            // not a time of the stream's (str0m sends its first at once): it
            // is no line.
            Event::RawPacket(p) => {
                if let RawPacket::RtcpTx(Rtcp::SenderReport(sr)) = *p {
                    if sr.sender_info.sender_packet_count == 0 {
                        return;
                    }
                    let sent = SentReport {
                        ntp: sr.sender_info.ntp_time,
                        rtp: sr.sender_info.rtp_time.numer(),
                    };
                    self.feedback.send_modify(|f| f.sent = Some(sent));
                }
            }
            Event::Closed => {
                tracing::debug!("WebRTC call {}: the client closed it", self.id);
            }
            _ => {}
        }
    }
}
