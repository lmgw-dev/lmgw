//! The calls suites' client (calls design §13.1): str0m itself, in-process,
//! on a UDP socket of its own, offering what a browser's peer connection
//! offers — audio send-receive with Opus and the `oai-events` channel —
//! and driven in the test's task, so a test decides what it sends and when.
//!
//! Its str0m runs on tokio's clock ([`now`]), as lmgw's does: unpaused that
//! is the system's, and a test that pauses time pauses the phone with
//! lmgw, its consent checks, retransmissions and acknowledgements kept on
//! the one timeline the bounds are measured on.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use str0m::change::SdpAnswer;
use str0m::channel::{ChannelConfig, ChannelId};
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};

use crate::common::patience::within;

/// A 20 ms Opus frame's RTP ticks.
pub const FRAME_TICKS: u64 = 960;

/// The client's clock (module doc): tokio's, as a `std` instant str0m reads.
pub fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

pub struct Client {
    pub rtc: Rtc,
    /// Received on (async) and sent on (a clone of the same socket, so a
    /// send never waits on tokio's readiness).
    recv: tokio::net::UdpSocket,
    send: std::net::UdpSocket,
    pub local: SocketAddr,
    pub audio: Mid,
    pub channel: Option<ChannelId>,
    pending: Option<str0m::change::SdpPendingOffer>,
    /// Events the drain met that no wait has taken yet.
    backlog: VecDeque<Event>,
    next: Instant,
    buf: Vec<u8>,
    /// Audio frames written so far.
    pub frames: u64,
    /// When the first frame was written: the media clock's zero.
    first: Option<Instant>,
    /// Datagrams larger than this are lost on the way in ([`Self::lose_over`]).
    lose_over: Option<usize>,
    /// RTP packets still to lose: dropped as they would leave
    /// ([`Self::lose_next_rtp`]).
    lose_rtp: usize,
}

impl Client {
    /// A client on `ip` (port chosen by the OS) and its offer.
    pub async fn new(ip: IpAddr) -> (Self, String) {
        Self::with(ip, Rtc::builder()).await
    }

    /// [`Self::new`] on a builder the test configured (raw packets, a short
    /// RTCP interval).
    pub async fn with(ip: IpAddr, builder: str0m::RtcConfig) -> (Self, String) {
        Self::build(SocketAddr::new(ip, 0), builder, "oai-events")
    }

    /// A client whose one data channel is labelled `label`.
    pub async fn labelled(ip: IpAddr, label: &str) -> (Self, String) {
        Self::labelled_with(ip, Rtc::builder(), label).await
    }

    /// [`Self::labelled`] on a builder the test configured.
    pub async fn labelled_with(
        ip: IpAddr,
        builder: str0m::RtcConfig,
        label: &str,
    ) -> (Self, String) {
        Self::build(SocketAddr::new(ip, 0), builder, label)
    }

    /// A client on exactly `addr`.
    pub async fn at(addr: SocketAddr) -> (Self, String) {
        Self::build(addr, Rtc::builder(), "oai-events")
    }

    /// Its socket on `addr`: received on async, sent on a clone.
    fn socket(addr: SocketAddr) -> (tokio::net::UdpSocket, std::net::UdpSocket, SocketAddr) {
        let std = std::net::UdpSocket::bind(addr).unwrap();
        std.set_nonblocking(true).unwrap();
        let local = std.local_addr().unwrap();
        let send = std.try_clone().unwrap();
        (tokio::net::UdpSocket::from_std(std).unwrap(), send, local)
    }

    fn build(addr: SocketAddr, builder: str0m::RtcConfig, label: &str) -> (Self, String) {
        let (recv, send, local) = Self::socket(addr);
        let mut rtc = builder.clear_codecs().enable_opus(true, false).build(now());
        rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
        let mut change = rtc.sdp_api();
        let audio = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        change.add_channel(label.into());
        let (offer, pending) = change.apply().unwrap();
        let client = Self {
            rtc,
            recv,
            send,
            local,
            audio,
            channel: None,
            pending: Some(pending),
            backlog: VecDeque::new(),
            next: now(),
            buf: vec![0; 2048],
            frames: 0,
            first: None,
            lose_over: None,
            lose_rtp: 0,
        };
        (client, offer.to_sdp_string())
    }

    /// Lose the next `n` RTP packets the client sends, as a lossy network
    /// would: str0m numbers them, and they never leave.
    pub fn lose_next_rtp(&mut self, n: usize) {
        self.lose_rtp += n;
    }

    /// Send `bytes` to `to` from the client's own socket, behind whatever
    /// it sent before: a datagram the client's str0m did not make.
    pub fn send_raw(&self, bytes: &[u8], to: SocketAddr) {
        self.send.send_to(bytes, to).unwrap();
    }

    /// Its local ICE ufrag.
    pub fn ufrag(&mut self) -> String {
        self.rtc.direct_api().local_ice_credentials().ufrag
    }

    /// Move to a new source port mid-call, as a phone does after a network
    /// change (§3.6): a new socket and host candidate, the old one
    /// discarded, so str0m checks and nominates a pair from the new port.
    pub fn move_port(&mut self) {
        let old = Candidate::host(self.local, "udp").unwrap();
        let (recv, send, local) = Self::socket(SocketAddr::new(self.local.ip(), 0));
        self.recv = recv;
        self.send = send;
        self.local = local;
        self.rtc
            .add_local_candidate(Candidate::host(local, "udp").unwrap());
        self.drain();
        assert!(self.rtc.direct_api().invalidate_candidate(&old));
        self.drain();
    }

    /// Apply lmgw's answer.
    pub fn accept(&mut self, answer: &str) {
        let answer = SdpAnswer::from_sdp_string(answer).unwrap();
        let pending = self.pending.take().expect("an offer to answer");
        self.rtc.sdp_api().accept_answer(pending, answer).unwrap();
        self.drain();
    }

    /// Drain str0m's output: transmits leave, events wait in the backlog.
    fn drain(&mut self) {
        loop {
            match self.rtc.poll_output().unwrap() {
                Output::Timeout(t) => {
                    self.next = t;
                    return;
                }
                Output::Transmit(t) => {
                    if self.lose_rtp > 0 && is_rtp(&t.contents) {
                        self.lose_rtp -= 1;
                        continue;
                    }
                    let _ = self.send.send_to(&t.contents, t.destination);
                }
                Output::Event(e) => {
                    if let Event::ChannelOpen(id, label) = &e {
                        if label == "oai-events" {
                            self.channel = Some(*id);
                        }
                    }
                    self.backlog.push_back(e);
                }
            }
        }
    }

    /// Run the client until `f` takes an event it waits for (`what`, said
    /// while it waits): events before it are passed to `f` and dropped.
    pub async fn until<T>(
        &mut self,
        what: &str,
        mut f: impl FnMut(&mut Self, Event) -> Option<T>,
    ) -> T {
        within(what, async {
            loop {
                self.drain();
                while let Some(e) = self.backlog.pop_front() {
                    if let Some(v) = f(self, e) {
                        return v;
                    }
                    self.drain();
                }
                self.step().await;
            }
        })
        .await
    }

    /// Run the client for `d` of real time, events going to the backlog.
    pub async fn run_for(&mut self, d: Duration) {
        let end = now() + d;
        while now() < end {
            self.drain();
            self.step_until(end).await;
        }
        self.drain();
    }

    /// Wait for one datagram or str0m's next timeout, and feed it.
    async fn step(&mut self) {
        let next = self.next;
        self.step_until(next).await;
    }

    async fn step_until(&mut self, until: Instant) {
        let wait = until.min(self.next).saturating_duration_since(now());
        let read = tokio::time::timeout(wait, self.recv.recv_from(&mut self.buf)).await;
        let now = now();
        match read {
            Ok(Ok((n, _))) if self.lose_over.is_some_and(|max| n > max) => {}
            Ok(Ok((n, source))) => {
                if let Ok(r) = Receive::new(Protocol::Udp, source, self.local, &self.buf[..n]) {
                    self.rtc.handle_input(Input::Receive(now, r)).unwrap();
                }
            }
            Ok(Err(e)) => panic!("the client's socket: {e}"),
            Err(_) => self.rtc.handle_input(Input::Timeout(now)).unwrap(),
        }
    }

    /// Connect: run until ICE and DTLS are up and `oai-events` is open.
    pub async fn connect(&mut self) {
        self.until("the client connected", |_, e| {
            matches!(e, Event::Connected).then_some(())
        })
        .await;
        if self.channel.is_none() {
            self.until("oai-events open on the client", |_, e| {
                matches!(e, Event::ChannelOpen(..)).then_some(())
            })
            .await;
        }
    }

    /// Send `text` on `oai-events`.
    pub fn send_text(&mut self, text: &str) {
        let id = self.channel.expect("oai-events open");
        let taken = self
            .rtc
            .channel(id)
            .unwrap()
            .write(false, text.as_bytes())
            .unwrap();
        assert!(taken, "the client's channel took the message");
        self.drain();
    }

    /// Open `oai-events` now, through str0m's direct API, and run until it is
    /// open: for a client whose offer carried another channel
    /// ([`Self::labelled`]), so its call connected with `oai-events` shut.
    pub async fn open_events(&mut self) {
        self.rtc.direct_api().create_data_channel(ChannelConfig {
            label: "oai-events".into(),
            ..ChannelConfig::default()
        });
        self.drain();
        if self.channel.is_none() {
            self.until("oai-events open on the client", |c, _| {
                c.channel.is_some().then_some(())
            })
            .await;
        }
    }

    /// Run until lmgw acknowledged everything the client sent on
    /// `oai-events`: nothing of it is in flight any more.
    pub async fn until_acknowledged(&mut self) {
        let id = self.channel.expect("oai-events open");
        within("the client's messages acknowledged", async {
            loop {
                self.drain();
                let flown = self
                    .rtc
                    .channel(id)
                    .is_some_and(|mut ch| ch.buffered_amount() == 0);
                if flown {
                    return;
                }
                self.step().await;
            }
        })
        .await
    }

    /// Whether the client met an event `f` takes among those it has
    /// received so far, without waiting: the ones before it are dropped.
    pub fn met(&mut self, mut f: impl FnMut(&Event) -> bool) -> bool {
        self.drain();
        while let Some(e) = self.backlog.pop_front() {
            if f(&e) {
                return true;
            }
        }
        false
    }

    /// Await `f` while everything that reaches the client is lost: each
    /// datagram is read off its socket and dropped, and its str0m sees
    /// neither it nor a timeout — a network that drops the way in, for as
    /// long as `f` takes.
    pub async fn discard_while<F: std::future::Future>(&mut self, f: F) -> F::Output {
        tokio::pin!(f);
        loop {
            tokio::select! {
                out = &mut f => return out,
                read = self.recv.recv_from(&mut self.buf) => {
                    read.expect("the client's socket");
                }
            }
        }
    }

    /// From now on every datagram larger than `bytes` that reaches the
    /// client is lost (`None`: none is): a network that drops lmgw's DATA,
    /// every event's packets of hundreds of bytes and more, while the
    /// datagrams of a few dozen — SCTP's acknowledgements of what the client
    /// sends, STUN's answers, small RTCP — come through. So the client reads
    /// nothing of lmgw's output and acknowledges none of it, and still hears
    /// its own messages acknowledged, so it can go on sending.
    pub fn lose_over(&mut self, bytes: Option<usize>) {
        self.lose_over = bytes;
    }

    /// Close `oai-events`.
    pub fn close_channel(&mut self) {
        let id = self.channel.take().expect("oai-events open");
        self.rtc.direct_api().close_data_channel(id);
        self.drain();
    }

    /// Write the next 20 ms audio frame, its payload `payload`, on the RTP
    /// clock: Opus where lmgw is to hear it (`realtime_calls_audio`), any
    /// bytes where only its transport counts (lmgw's decoder takes a payload
    /// it cannot read as a lost frame).
    pub fn send_frame(&mut self, payload: &[u8]) {
        let first = *self.first.get_or_insert_with(now);
        let ticks = self.frames * FRAME_TICKS;
        let wallclock = first + Duration::from_millis(self.frames * 20);
        let pt = self
            .rtc
            .writer(self.audio)
            .unwrap()
            .payload_params()
            .find(|p| p.spec().codec == Codec::Opus)
            .unwrap()
            .pt();
        let writer = self.rtc.writer(self.audio).unwrap();
        writer
            .write(
                pt,
                wallclock,
                MediaTime::new(ticks, Frequency::FORTY_EIGHT_KHZ),
                payload.to_vec(),
            )
            .unwrap();
        self.frames += 1;
        self.drain();
    }

    /// [`Self::send_frame`], packetized and sent now rather than at the
    /// client's next step (str0m payloads one written frame per timeout it
    /// handles): for a test that streams faster than its steps come.
    pub fn send_frame_now(&mut self, payload: &[u8]) {
        self.send_frame(payload);
        self.rtc.handle_input(Input::Timeout(now())).unwrap();
        self.drain();
    }
}

/// Whether a datagram is (S)RTP rather than RTCP, DTLS or STUN: version 2
/// in its first byte (RFC 7983), and a payload type outside RTCP's 200–206
/// with the marker bit either way.
fn is_rtp(d: &[u8]) -> bool {
    d.len() >= 2 && d[0] >> 6 == 2 && !(200..=206).contains(&d[1])
}

/// The `a=ice-ufrag` of an SDP.
pub fn ufrag(sdp: &str) -> String {
    sdp.lines()
        .find_map(|l| l.strip_prefix("a=ice-ufrag:"))
        .expect("an ice-ufrag")
        .trim()
        .to_string()
}
