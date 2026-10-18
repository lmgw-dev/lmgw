//! The driver (calls design §3.3): one task owns every socket of every
//! group and every call's str0m `Rtc`.
//!
//! It waits for one of three things: a command (a new call, a message to
//! send, a close, sockets added or closed), a socket with a datagram to
//! read, or the earliest deadline among its calls (str0m's next timeout,
//! the never-opened bound). Then:
//!
//! - **A binding request** goes to the call its USERNAME names — the local
//!   ufrag before its colon, through the driver's map of the calls' ufrags —
//!   and only to it, once its MESSAGE-INTEGRITY checks out with that call's
//!   password, which the driver checks itself (HMAC-SHA1 through aws-lc-rs).
//!   Both happen before `accepts`, because str0m's fast path takes any
//!   datagram from a call's nominated remote address, STUN included: a stale
//!   call whose remote is a new call's source would otherwise swallow the
//!   new call's checks, and a request forged as the client, with a bad
//!   integrity, would pass `accepts` and count as the client's traffic.
//!   STUN that is no binding request is dropped.
//! - **Any other datagram** goes to the call whose binding request last
//!   passed its check from that source address, when there is one: a source
//!   belongs to the call that proved it most recently, so a stale call whose
//!   nominated remote is the same address takes none of the new call's DTLS,
//!   SRTP or SCTP. Else to the first call on the socket it arrived on whose
//!   `Rtc::accepts` it — the nominated pair's source address, else a
//!   validated remote address. Its destination and source come from the
//!   kernel (`sockets`).
//! - One that does not even parse as STUN, DTLS, RTP or RTCP, and one no
//!   call takes, is dropped: a stranger costs one parse, a map lookup or a
//!   scan over the socket's calls, and no state; a DEBUG line counts them
//!   once a minute. Nothing is answered, so the port neither amplifies nor
//!   tells a scanner that a call exists (§10).
//! - **A timeout** reaches every call that is due; a call that ends is
//!   removed and its owner told why.
//!
//! **A panic in str0m ends one call, not the driver.** Each step on a
//! call's `Rtc` (`accepts`, `handle_input` and the drain, a write, the
//! close) runs under `catch_unwind`; a panic is logged at ERROR and ends
//! that call with `Ended::Failed`. A driver that died anyway is started
//! again by the next use (`Calls::driver`).
//!
//! The driver does no audio work and never waits: a call's output goes out
//! on its socket as str0m gives it, its events to its owner's queue. A scan
//! over the calls is enough — a personal gateway runs a handful at most.
//!
//! **The sockets are read in groups** (§3.1): the hub knows each group's
//! sockets, a call lives on one socket of its own group, and closing a
//! group drops its sockets and ends their calls only
//! ([`Command::CloseSockets`]).

use std::collections::HashMap;
use std::future::poll_fn;
use std::net::SocketAddr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use str0m::ice::StunMessage;
use str0m::net::{DatagramRecv, Protocol, Receive};
use str0m::Input;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::handle::{Ended, RtpOut, Sent};
use super::registry::Registry;
use super::sockets::{Datagram, SockId, UdpSock, RECV_BUF};

mod call;

use call::Call;
pub(super) use call::NewCall;

/// How many receives one socket gets before the driver looks at its
/// commands and timers again: a scheduling turn, so a flood on one socket
/// cannot starve the rest. Nothing is dropped by it — the socket is read
/// again on the next turn.
const READS_PER_TURN: usize = 64;

/// How often the count of dropped datagrams is logged (§3.3).
const STRAY_LOG_EVERY: Duration = Duration::from_secs(60);

/// What the rest of lmgw asks of the driver.
pub(super) enum Command {
    /// Take a new call; `reply` once it is in (the answer goes out only
    /// then), or why it could not be. A call whose `reply` nobody hears any
    /// more (the POST was dropped) is closed at once.
    Register {
        call: Box<NewCall>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Send messages on a call's `oai-events`, back to back: one event, or
    /// its parts, or an emptied event and its errors (§6.3). `reply` hears
    /// what became of the last, or of the first that was not taken.
    Send {
        id: Arc<str>,
        messages: Vec<String>,
        reply: oneshot::Sender<Sent>,
    },
    /// Write one frame of a call's audio (§5.1): str0m packetizes it at
    /// once. Dropped for a call that is gone, let go or closing.
    Audio { id: Arc<str>, frame: RtpOut },
    /// Send what the frame clock released (a transcript delta, `started`,
    /// `starved`, §5.1, §6.2; shaped as `Send`'s are) on a call's
    /// `oai-events`, in order behind every write before it, without an
    /// answer: what becomes of it the call says in its log. Dropped for a
    /// call that is gone, let go or closing.
    ClockEvent { id: Arc<str>, messages: Vec<String> },
    /// A call's playout allowance changed (§5.3): its registry entry shows
    /// it to the status block (§9).
    Playout {
        id: Arc<str>,
        playout: super::registry::CallPlayout,
    },
    /// Close a call; `done` once it is gone.
    Close {
        id: Arc<str>,
        done: Option<oneshot::Sender<()>>,
    },
    /// Let a call go (§8.2): what it kept (the slot) is dropped and its
    /// registry entry no longer counts as live, so a hangup's `Released`
    /// resolves; its `Rtc` stays, for its last events, until it is closed.
    LetGo { id: Arc<str> },
    /// Close a call once what was written on its `oai-events` is delivered
    /// (SCTP acknowledged it), at most its bound from now
    /// (`realtime.ping_interval_s` at its offer; none at 0). It lets go at
    /// once; `done` then.
    CloseDelivered {
        id: Arc<str>,
        done: oneshot::Sender<()>,
    },
    /// Read these sockets too.
    AddSockets(Vec<UdpSock>),
    /// Close these sockets (a group's, by their ids, so a group bound again
    /// meanwhile keeps its new ones): end their calls, drop them; `done`
    /// once they are closed.
    CloseSockets {
        ids: Vec<SockId>,
        done: oneshot::Sender<()>,
    },
    /// Make the call panic in its next tick, as str0m might.
    #[cfg(test)]
    PanicForTests { id: Arc<str> },
    /// Panic outside any call: the driver task dies.
    #[cfg(test)]
    DieForTests,
}

/// Where the driver's commands go. The driver runs while any handle does.
#[derive(Clone)]
pub(super) struct Driver(mpsc::UnboundedSender<Command>);

impl Driver {
    /// Start the driver task; `dropped` counts every datagram it drops.
    pub fn spawn(registry: Registry, dropped: Arc<AtomicU64>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = Task {
            rx,
            sockets: Vec::new(),
            calls: Vec::new(),
            routes: Routes::default(),
            registry,
            buf: vec![0; RECV_BUF].into_boxed_slice(),
            datagrams: Vec::new(),
            turn: 0,
            stray: 0,
            stray_since: Instant::now(),
            dropped,
        };
        tokio::spawn(task.run());
        Self(tx)
    }

    /// `false`: the driver is gone.
    pub fn send(&self, c: Command) -> bool {
        self.0.send(c).is_ok()
    }

    /// The driver task has ended (it panicked, or every handle was gone).
    pub fn is_gone(&self) -> bool {
        self.0.is_closed()
    }

    /// A driver whose commands a test answers itself.
    #[cfg(test)]
    pub fn for_tests() -> (Self, mpsc::UnboundedReceiver<Command>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self(tx), rx)
    }
}

struct Task {
    rx: mpsc::UnboundedReceiver<Command>,
    sockets: Vec<UdpSock>,
    calls: Vec<Call>,
    routes: Routes,
    registry: Registry,
    buf: Box<[u8]>,
    datagrams: Vec<Datagram>,
    /// The socket the next readiness scan starts at, for fairness.
    turn: usize,
    /// Dropped since the last DEBUG line.
    stray: u64,
    stray_since: Instant,
    /// Dropped since the driver started (`Calls::dropped`).
    dropped: Arc<AtomicU64>,
}

enum Wake {
    Command(Option<Command>),
    Socket(usize),
    Timer,
}

/// Where a datagram goes before `accepts` is asked (module doc).
#[derive(Default)]
struct Routes {
    /// Each call's local ufrag → its id: where a binding request goes.
    ufrags: HashMap<String, Arc<str>>,
    /// Each source address → the call whose binding request last passed
    /// its check from it.
    sources: HashMap<SocketAddr, Arc<str>>,
}

impl Routes {
    /// A call is gone: nothing routes to it.
    fn forget(&mut self, c: &Call) {
        if self.ufrags.get(&c.ufrag) == Some(&c.id) {
            self.ufrags.remove(&c.ufrag);
        }
        self.sources.retain(|_, id| *id != c.id);
    }
}

/// When the task ends — every handle gone, or unwinding from a panic outside
/// any call's guard — its calls go with it: the registry forgets them, so a
/// hangup's `Released` and a group close's wait resolve and no dead call is
/// listed. What each kept (the slot) drops with the task's calls.
impl Drop for Task {
    fn drop(&mut self) {
        if std::thread::panicking() {
            tracing::error!(
                "WebRTC calls: the call driver panicked; its {} call(s) are gone, and it starts \
                 again at the next use",
                self.calls.len()
            );
        }
        self.registry.clear();
    }
}

/// Run `f` on `c` with str0m's panics caught: one ends only that call.
fn guarded<T>(c: &mut Call, what: &str, f: impl FnOnce(&mut Call) -> T) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(|| f(c))) {
        Ok(v) => Some(v),
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "(no message)".into());
            c.poison(what, &msg);
            None
        }
    }
}

impl Task {
    async fn run(mut self) {
        loop {
            let deadline = self.calls.iter().filter_map(Call::deadline).min();
            let wake = {
                let sockets = &self.sockets;
                let turn = self.turn;
                tokio::select! {
                    biased;
                    c = self.rx.recv() => Wake::Command(c),
                    i = ready(sockets, turn) => Wake::Socket(i),
                    () = sleep(deadline) => Wake::Timer,
                }
            };
            match wake {
                Wake::Command(None) => break,
                Wake::Command(Some(c)) => self.command(c),
                Wake::Socket(i) => {
                    self.read(i);
                    self.turn = i + 1;
                }
                Wake::Timer => {}
            }
            self.tick();
        }
        // Every handle is gone, so is every owner: the calls go with the
        // sockets.
        for c in std::mem::take(&mut self.calls) {
            c.finish(&self.registry);
        }
    }

    fn command(&mut self, c: Command) {
        let now = Instant::now();
        let reg = self.registry.clone();
        match c {
            Command::Register { call, reply } => {
                let Some(sock) = self.sockets.iter().find(|s| s.id == call.sock) else {
                    let _ = reply.send(Err("its socket group closed meanwhile".into()));
                    return;
                };
                let mut c = Call::new(*call, now);
                // str0m's rule: the offer's acceptance was a mutation, so
                // drain before anything else reaches the call.
                guarded(&mut c, "the answer's drain", |c| c.start(now, sock, &reg));
                if let Some(why) = &c.ended {
                    // Failed (or panicked) on its first drain: no answer
                    // goes out for a dead call.
                    let _ = reply.send(Err(format!("the call failed at once: {why:?}")));
                    c.finish(&reg);
                    return;
                }
                if reply.send(Ok(())).is_err() {
                    // The POST is gone, and with it the answer: nobody will
                    // ever own this call.
                    guarded(&mut c, "the close", |c| {
                        c.close(Ended::Closed, sock, &reg, now)
                    });
                }
                self.routes.ufrags.insert(c.ufrag.clone(), c.id.clone());
                self.calls.push(c);
                self.reap();
            }
            Command::Send {
                id,
                messages,
                reply,
            } => match self.find(&id) {
                Some((c, sock)) => {
                    let mut reply = Some(reply);
                    guarded(c, "a write", |c| {
                        if let Some(reply) = reply.take() {
                            c.write(messages, reply, sock, &reg, now);
                        }
                    });
                    // A panic dropped the reply: the owner hears `Gone`.
                }
                None => {
                    let _ = reply.send(Sent::Gone);
                }
            },
            Command::Audio { id, frame } => {
                if let Some((c, sock)) = self.find(&id) {
                    guarded(c, "an audio write", |c| {
                        c.write_audio(frame, sock, &reg, now)
                    });
                }
            }
            Command::ClockEvent { id, messages } => {
                if let Some((c, sock)) = self.find(&id) {
                    guarded(c, "a write", |c| c.write_clock(messages, sock, &reg, now));
                }
            }
            Command::Playout { id, playout } => {
                reg.update(&id, |i| i.playout = Some(playout));
            }
            Command::Close { id, done } => {
                if let Some((c, sock)) = self.find(&id) {
                    guarded(c, "the close", |c| c.close(Ended::Closed, sock, &reg, now));
                }
                self.reap();
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
            Command::LetGo { id } => {
                if let Some((c, _)) = self.find(&id) {
                    c.let_go(&reg);
                }
            }
            Command::CloseDelivered { id, done } => {
                if let Some((c, sock)) = self.find(&id) {
                    guarded(c, "the close", |c| c.close_delivered(sock, &reg, now));
                }
                self.reap();
                let _ = done.send(());
            }
            Command::AddSockets(s) => self.sockets.extend(s),
            Command::CloseSockets { ids, done } => {
                for c in &mut self.calls {
                    if !ids.contains(&c.sock) {
                        continue;
                    }
                    if let Some(sock) = self.sockets.iter().find(|s| s.id == c.sock) {
                        guarded(c, "the close", |c| {
                            c.close(Ended::GroupClosed, sock, &reg, now)
                        });
                    }
                }
                self.reap();
                self.sockets.retain(|s| !ids.contains(&s.id));
                let _ = done.send(());
            }
            #[cfg(test)]
            Command::PanicForTests { id } => {
                if let Some(c) = self.calls.iter_mut().find(|c| c.id == id) {
                    c.panic_next = true;
                    c.next = Some(now);
                }
            }
            #[cfg(test)]
            Command::DieForTests => panic!("the driver dies, as a test asked"),
        }
    }

    fn find(&mut self, id: &str) -> Option<(&mut Call, &UdpSock)> {
        let c = self.calls.iter_mut().find(|c| &*c.id == id)?;
        let sock = self.sockets.iter().find(|s| s.id == c.sock)?;
        Some((c, sock))
    }

    /// Read socket `i` for one turn and hand each datagram to its call.
    fn read(&mut self, i: usize) {
        let Self {
            sockets,
            calls,
            routes,
            registry,
            buf,
            datagrams,
            stray,
            dropped,
            ..
        } = self;
        let Some(sock) = sockets.get(i) else {
            return;
        };
        for _ in 0..READS_PER_TURN {
            datagrams.clear();
            match sock.recv(buf, datagrams) {
                Ok(Some(())) => {}
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!("WebRTC calls: a receive on {} failed: {e}", sock.local);
                    continue;
                }
            }
            let now = Instant::now();
            for d in datagrams.iter() {
                let bytes = &buf[d.start..d.end];
                if !deliver(sock, calls, routes, registry, bytes, d, now) {
                    *stray += 1;
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Run what is due, remove what ended, log the strays once a minute.
    fn tick(&mut self) {
        let now = Instant::now();
        for c in &mut self.calls {
            if c.deadline().is_some_and(|t| t <= now) {
                if let Some(sock) = self.sockets.iter().find(|s| s.id == c.sock) {
                    guarded(c, "a timeout", |c| c.tick(now, sock, &self.registry));
                }
            }
        }
        self.reap();
        if now.duration_since(self.stray_since) >= STRAY_LOG_EVERY {
            if self.stray > 0 {
                tracing::debug!(
                    "WebRTC calls: {} datagram(s) no call accepted in the last {} s, dropped",
                    self.stray,
                    STRAY_LOG_EVERY.as_secs()
                );
            }
            self.stray = 0;
            self.stray_since = now;
        }
    }

    /// Remove the calls that ended, telling each owner why.
    fn reap(&mut self) {
        let mut k = 0;
        while k < self.calls.len() {
            if self.calls[k].ended.is_some() {
                let c = self.calls.swap_remove(k);
                self.routes.forget(&c);
                c.finish(&self.registry);
            } else {
                k += 1;
            }
        }
    }
}

/// What a datagram is, read before any call sees it.
// Not boxed: a stranger's datagram must not cost an allocation (str0m keeps
// its own `DatagramRecvInner` unboxed for the same reason).
#[allow(clippy::large_enum_variant)]
enum Kind<'a> {
    /// A binding request: the local ufrag its USERNAME names
    /// (`<receiver's ufrag>:<sender's ufrag>`), and the message, whose
    /// integrity is still to be checked.
    Binding {
        ufrag: Option<&'a str>,
        message: StunMessage<'a>,
    },
    /// Other STUN (a response, an indication), or STUN that does not parse.
    OtherStun,
    /// Not STUN.
    Other,
}

fn classify(bytes: &[u8]) -> Kind<'_> {
    // str0m's own test for STUN (RFC 7983's first byte, a full header).
    if !(bytes.first().is_some_and(|b| *b < 2) && bytes.len() >= 20) {
        return Kind::Other;
    }
    match StunMessage::parse(bytes) {
        Ok(m) if m.is_binding_request() => Kind::Binding {
            ufrag: m.username().and_then(|u| u.split(':').next()),
            message: m,
        },
        _ => Kind::OtherStun,
    }
}

/// STUN's MESSAGE-INTEGRITY: HMAC-SHA1 over `payloads` with `key`
/// (RFC 8489 §14.5), through aws-lc-rs.
fn hmac_sha1(key: &[u8], payloads: &[&[u8]]) -> [u8; 20] {
    use aws_lc_rs::hmac;
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key);
    let mut ctx = hmac::Context::with_key(&key);
    for p in payloads {
        ctx.update(p);
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(ctx.sign().as_ref());
    out
}

/// Hand one datagram to its call (module doc); `false` when none takes it
/// (or it parses as nothing str0m reads).
fn deliver(
    sock: &UdpSock,
    calls: &mut [Call],
    routes: &mut Routes,
    registry: &Registry,
    bytes: &[u8],
    d: &Datagram,
    now: Instant,
) -> bool {
    // A stranger's bytes: str0m's parsers run under `catch_unwind`, and a
    // datagram they panic on is dropped like one they refuse.
    let read = catch_unwind(AssertUnwindSafe(|| {
        (classify(bytes), DatagramRecv::try_from(bytes).ok())
    }));
    let Ok((kind, Some(contents))) = read else {
        return false;
    };
    let input = Input::Receive(
        now.into_std(),
        Receive {
            proto: Protocol::Udp,
            source: d.source,
            destination: d.destination,
            contents,
        },
    );
    match kind {
        // A response or an indication is no ICE-lite server's business.
        Kind::OtherStun => false,
        // A binding request goes to the call it names, and to no other.
        Kind::Binding { ufrag, message } => {
            let Some(id) = ufrag.and_then(|u| routes.ufrags.get(u)).cloned() else {
                return false;
            };
            let Some(c) = calls
                .iter_mut()
                .find(|c| c.id == id && c.sock == sock.id && c.ended.is_none())
            else {
                return false;
            };
            // The integrity is checked here, with the call's password, and
            // not left to `accepts`: str0m's takes anything from the
            // nominated remote address before it looks at STUN, so a request
            // with a forged MESSAGE-INTEGRITY sent as the client would pass
            // it, and count as the client's traffic.
            let verified = catch_unwind(AssertUnwindSafe(|| {
                message.verify(c.pass.as_bytes(), hmac_sha1)
            }));
            if !matches!(verified, Ok(true)) {
                return false;
            }
            if guarded(c, "accepts", |c| c.rtc.accepts(&input)) != Some(true) {
                return false;
            }
            // Proved: this source is this call's now.
            routes.sources.insert(d.source, id);
            guarded(c, "a datagram", |c| {
                c.receive(input, d.source, true, now, sock, registry)
            });
            true
        }
        Kind::Other => {
            // The call that last proved this source first, then the rest.
            let owner = routes.sources.get(&d.source).and_then(|id| {
                calls
                    .iter()
                    .position(|c| c.id == *id && c.sock == sock.id && c.ended.is_none())
            });
            if let Some(k) = owner {
                let c = &mut calls[k];
                if guarded(c, "accepts", |c| c.rtc.accepts(&input)) == Some(true) {
                    guarded(c, "a datagram", |c| {
                        c.receive(input, d.source, false, now, sock, registry)
                    });
                    return true;
                }
            }
            for (k, c) in calls.iter_mut().enumerate() {
                if Some(k) == owner {
                    continue;
                }
                if c.sock != sock.id || c.ended.is_some() {
                    continue;
                }
                if guarded(c, "accepts", |c| c.rtc.accepts(&input)) == Some(true) {
                    guarded(c, "a datagram", |c| {
                        c.receive(input, d.source, false, now, sock, registry)
                    });
                    return true;
                }
            }
            false
        }
    }
}

/// The first socket from `turn` on with a datagram to read.
async fn ready(sockets: &[UdpSock], turn: usize) -> usize {
    poll_fn(|cx| {
        let n = sockets.len();
        for k in 0..n {
            let i = (turn + k) % n;
            if sockets[i].poll_ready(cx).is_ready() {
                return Poll::Ready(i);
            }
        }
        Poll::Pending
    })
    .await
}

/// Until `deadline`, or forever without one.
async fn sleep(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}
