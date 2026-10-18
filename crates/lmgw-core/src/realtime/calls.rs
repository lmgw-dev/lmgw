//! WebRTC realtime calls, the media plane (calls design §3; WP2 part A):
//! the UDP sockets, the one driver task that reads them and owns every
//! call's str0m `Rtc`, the registry of live calls, and the offer → answer
//! step `POST /v1/realtime/calls` takes.
//!
//! - **Socket groups** (§3.1, `sockets`): one UDP socket per address of a
//!   listener, grouped by the listener a call's POST came in on
//!   ([`Group`]). The plain group (stock clients) binds beside the HTTP
//!   listener at start on `realtime.webrtc_port` and is released with it,
//!   before the restart binds again ([`Calls::serve_plain`]). The device
//!   group (`devices`, WP7) follows the device listener live: one socket
//!   per address it binds, on its port, bound as it starts, moved as it
//!   moves and closed as it stops ([`Calls::follow_devices`]).
//! - **A group's close** leaves room for each call's last event: its owners
//!   are told ([`Ending::ShuttingDown`]), the sockets stay open until the
//!   calls are gone or [`crate::server::STOP_WITHIN`] has passed, and only
//!   the rest are closed by force.
//! - **The driver** (§3.3, `driver`): reads every socket through quinn-udp
//!   with each datagram's destination, routes binding requests by their
//!   ufrag and the rest by `Rtc::accepts`, sends what str0m transmits from
//!   the candidate's address, runs each call's timeouts and the
//!   never-opened bound (§8.1). A panic in str0m ends one call.
//! - **The registry** (§3.4, `registry`): what is known of each live call,
//!   in memory only; a hangup is signalled through it ([`Calls::hangup`]).
//! - **A call** (`offer`, `handle`): an `Rtc` configured as §3.5 with one
//!   host candidate, the local address its POST arrived on (§3.2),
//!   answered once the driver holds it ([`Calls::offer`]). Its
//!   [`CallHandle`] reads what the call receives and sends, closes and
//!   hears why it should end; `link` puts it behind the transport seam
//!   (§7).
//! - **Its audio** (§4, §5, `audio`; WP3): Opus in and out, the frame
//!   clock, the playing window and heard, as self-contained parts the
//!   call's reader and the session take up.
//! - **The route** (§2, `route`; WP2 part B): `POST /v1/realtime/calls`
//!   checks what the WebSocket's handshake checks, mutating nothing, and
//!   answers the offer; each [`OfferError`] names its refusal (§2.5).
//!   `POST …/{call_id}/hangup` asks the call's owner to end it, bounded
//!   ([`Calls::hangup_within`]).
//! - **The open path** (§2.8, `open`): the call's owner from the answer
//!   until its session runs — the bind when `oai-events` opens, the wait
//!   under `takeover=ask`, the open-time `lmgw.session.closed`, and the
//!   session started on the call's link.

use std::any::Any;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::config::{RealtimeSettings, WebRtcSettings};
use crate::principal::Principal;
use crate::server::{ServedAt, STOP_WITHIN};

pub(crate) mod audio;
mod devices;
mod driver;
mod handle;
pub(crate) mod link;
mod offer;
mod open;
mod registry;
pub(crate) mod route;
mod sizes;
mod sockets;
mod status;

pub use handle::{
    Asked, CallControl, CallEvent, CallEvents, CallHandle, Ended, Ending, Frame, LetGo, Limit,
    Message, Released, SenderReport, Sent,
};
pub use offer::LOCAL_MAX_MESSAGE;
pub use registry::{CallInfo, CallPlayout, CallState, CallStats, Evidence, Hangup};

use driver::{Command, Driver, NewCall};
use registry::Registry;
use sockets::SockId;

/// The listener a call's POST came in on, and so the sockets that carry it
/// (§3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Group {
    /// The plain listener's (`bind_addr`): stock clients.
    Plain,
    /// The device listener's (device link §4): paired devices, one socket
    /// per address that listener binds, on its port, following it live.
    Devices,
}

impl Group {
    /// The group of the listener a POST came in on (`Conn::via`, §3.2).
    pub fn of(via: crate::server::Via) -> Self {
        match via {
            crate::server::Via::Plain => Self::Plain,
            crate::server::Via::Devices => Self::Devices,
        }
    }

    /// The listener it follows, as a log line names it.
    fn listener(self) -> &'static str {
        match self {
            Self::Plain => "the plain listener",
            Self::Devices => "the device listener",
        }
    }

    /// How its sockets on an IPv6 address treat IPv4, as its listener's own
    /// TCP sockets do (§3.1): the plain listener's as the system's
    /// `bindv6only` has it; the device listener sets its own, `[::]`
    /// dual-stack and any other IPv6 address IPv6-only
    /// (`device_listener::bind`), so an IPv4 phone it took on `[::]` finds
    /// a UDP socket behind its candidate whatever the system's default.
    fn v6(self) -> sockets::V6 {
        match self {
            Self::Plain => sockets::V6::System,
            Self::Devices => sockets::V6::DualOnWildcard,
        }
    }
}

/// A socket group as the status and a refusal see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupState {
    /// `realtime.webrtc_enabled` is off.
    Off,
    /// Not bound: before its listener started, or after it stopped.
    Down,
    /// Bound on these addresses.
    Bound(Vec<SocketAddr>),
    /// Its bind failed, or its sockets were lost: why.
    Failed(String),
    /// The device group while its follower is between the device listener's
    /// change and the new bind (close, then bind, at most
    /// [`STOP_WITHIN`] for the close): it answers no call, and says so as
    /// its own 503.
    Moving,
}

/// What a call takes from the settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallParams {
    /// The never-opened bound (§8.1): `realtime.ping_interval_s`; `None` at
    /// 0, no bound.
    pub never_opened: Option<Duration>,
    /// How long a frame waits for a missing earlier one (§4.2):
    /// `realtime.webrtc_reorder_ms`.
    pub reorder: Duration,
    /// `realtime.max_message_mb` in bytes (`None` at 0, no bound): the
    /// largest message a call sends, below the client's own bound (§6.3),
    /// and the budget of what waits for the call's owner (`handle`).
    pub max_message: Option<usize>,
}

impl CallParams {
    pub fn from_settings(r: &RealtimeSettings) -> Self {
        Self {
            never_opened: (r.ping_interval_s > 0)
                .then(|| Duration::from_secs(r.ping_interval_s.into())),
            reorder: r.webrtc_audio.reorder(),
            max_message: (r.max_message_mb > 0).then_some((r.max_message_mb as usize) << 20),
        }
    }
}

/// An offer to answer.
pub struct Offer {
    /// The offer's SDP.
    pub sdp: String,
    /// The listener the POST came in on.
    pub group: Group,
    /// The local address the POST arrived on: the call's one candidate
    /// (§3.2), made canonical here.
    pub local: IpAddr,
    /// The start of the listener the POST's connection belongs to
    /// (`RequestCtx::served_at`): a device call on another start of the
    /// device listener than the group is bound for is refused
    /// [`OfferError::Moving`] (N3).
    pub served_at: Option<ServedAt>,
    /// Who made the call (the hangup's check, §2.6).
    pub creator: Principal,
    pub params: CallParams,
    /// Kept for the call's life and dropped when it ends, whoever ends it:
    /// the concurrency slot (C3).
    pub keep: Option<Box<dyn Any + Send>>,
}

/// An offer answered: the answer's SDP and the call.
pub struct Answered {
    pub sdp: String,
    pub handle: CallHandle,
}

/// Why an offer got no answer (part B's refusals, §2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfferError {
    /// 503 `webrtc_unavailable`: the group is off, failed to bind, is not
    /// bound, or has no socket for the candidate's address.
    Unavailable(String),
    /// 503 `webrtc_unavailable` with `Retry-After`: the device group is
    /// moving with the device listener, or the POST came in on a start of
    /// it that has ended (N2, N3).
    Moving,
    /// 400 `invalid_sdp`: str0m's reason.
    Invalid(String),
    /// 400 `unsupported_offer`: no Opus audio, or no data channel.
    Unsupported(&'static str),
}

impl std::fmt::Display for OfferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(m) | Self::Invalid(m) => f.write_str(m),
            Self::Moving => write!(
                f,
                "the WebRTC UDP sockets are moving with the device listener; they are back \
                 within {} s",
                STOP_WITHIN.as_secs()
            ),
            Self::Unsupported(m) => f.write_str(m),
        }
    }
}

struct GroupEntry {
    state: GroupState,
    sockets: Vec<(SockId, SocketAddr)>,
    /// The device listener's start the device group is bound for
    /// ([`crate::server::device_listener::Bound::served_at`]): a device
    /// call POSTed on another start's connection is refused (N3). `None`
    /// for the plain group.
    served_at: Option<ServedAt>,
}

#[derive(Default)]
struct Inner {
    driver: Mutex<Option<Driver>>,
    registry: Registry,
    groups: Mutex<HashMap<Group, GroupEntry>>,
    next_sock: AtomicU64,
    /// Datagrams the driver dropped: no call accepted them (§3.3).
    dropped: Arc<AtomicU64>,
    /// `realtime.webrtc_enabled` off as the gateway's start read it
    /// ([`Calls::serve_plain`]): the plain group is `Off`, and the device
    /// group binds nothing either; the status block's `enabled`. On until a
    /// start says otherwise.
    off_at_start: AtomicBool,
    /// Whether the device group's follower runs ([`Calls::follow_devices`]).
    following: AtomicBool,
    /// The follower is between a change of the device listener and the new
    /// bind: the device group reads [`GroupState::Moving`].
    moving: AtomicBool,
}

/// The gateway's calls: its socket groups, its driver, its registry.
#[derive(Clone, Default)]
pub struct Calls {
    inner: Arc<Inner>,
}

impl Calls {
    /// The driver, started at its first use, and started again when it
    /// died: its sockets died with it, so every bound group is `Failed`.
    fn driver(&self) -> Driver {
        let mut d = self.inner.driver.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(live) = d.as_ref().filter(|d| !d.is_gone()) {
            return live.clone();
        }
        if d.is_some() {
            tracing::error!(
                "WebRTC calls: the call driver stopped; started again, without the sockets \
                 and calls it had"
            );
            self.inner.registry.clear();
            for (group, g) in self.groups().iter_mut() {
                if matches!(g.state, GroupState::Bound(_)) {
                    let again = match group {
                        Group::Plain => "when the listener starts again (a restart of lmgw)",
                        Group::Devices => {
                            "when the device listener starts or moves again (a change of its \
                             settings, or a restart of lmgw)"
                        }
                    };
                    *g = GroupEntry {
                        state: GroupState::Failed(format!(
                            "the call driver stopped and its UDP sockets closed with it; they \
                             bind again {again}"
                        )),
                        sockets: Vec::new(),
                        served_at: None,
                    };
                }
            }
        }
        let fresh = Driver::spawn(self.inner.registry.clone(), self.inner.dropped.clone());
        *d = Some(fresh.clone());
        fresh
    }

    fn groups(&self) -> std::sync::MutexGuard<'_, HashMap<Group, GroupEntry>> {
        self.inner.groups.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_group(
        &self,
        group: Group,
        state: GroupState,
        sockets: Vec<(SockId, SocketAddr)>,
        served_at: Option<ServedAt>,
    ) {
        self.groups().insert(
            group,
            GroupEntry {
                state,
                sockets,
                served_at,
            },
        );
    }

    /// Where `group` stands.
    pub fn group(&self, group: Group) -> GroupState {
        // A dead driver is noticed, and said, here too.
        let dead = self
            .inner
            .driver
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(Driver::is_gone);
        if dead {
            drop(self.driver());
        }
        let state = self
            .groups()
            .get(&group)
            .map(|g| g.state.clone())
            .unwrap_or(GroupState::Down);
        if group != Group::Devices {
            return state;
        }
        // The device group is followed, not started with the gateway: what
        // it is not bound or failed on is the switch's as the start read it
        // (a restart may have flipped it while the device listener was off
        // or in error), or the follower's gap.
        match state {
            _ if self.inner.moving.load(Ordering::Acquire) => GroupState::Moving,
            GroupState::Off | GroupState::Down
                if self.inner.off_at_start.load(Ordering::Relaxed) =>
            {
                GroupState::Off
            }
            GroupState::Off => GroupState::Down,
            other => other,
        }
    }

    /// Bind the plain group beside an HTTP listener on `http` (§3.1): one
    /// socket on its IP, on `realtime.webrtc_port`, or nothing when
    /// `realtime.webrtc_enabled` is off. A failed bind does not stop lmgw:
    /// it is logged and said by [`Self::group`]. The switch as this start
    /// read it holds for the device group too, until the next start
    /// ([`Self::follow_devices`]).
    ///
    /// `released` is the HTTP server's "the listener is closed"; what comes
    /// back takes its place: fired (or dropped) when the listener closes, it
    /// closes the group as [`Self::close_group`] does, giving each call
    /// room for its last event, and only then fires `released`, so a
    /// restart that binds the same port again waits for both.
    pub async fn serve_plain(
        &self,
        settings: &WebRtcSettings,
        http: SocketAddr,
        released: Option<oneshot::Sender<()>>,
    ) -> Option<oneshot::Sender<()>> {
        self.inner
            .off_at_start
            .store(!settings.webrtc_enabled, Ordering::Relaxed);
        if !settings.webrtc_enabled {
            self.set_group(Group::Plain, GroupState::Off, Vec::new(), None);
            tracing::info!("WebRTC calls are off (realtime.webrtc_enabled)");
            return released;
        }
        let addr = SocketAddr::new(http.ip(), settings.plain_port(http.port()));
        let ids = match self.bind_group(Group::Plain, &[addr], None).await {
            GroupState::Bound(_) => self.socket_ids(Group::Plain),
            _ => return released,
        };
        let (closed, on_close) = oneshot::channel::<()>();
        let calls = self.clone();
        tokio::spawn(async move {
            // Fired or dropped: the listener is closed either way.
            let _ = on_close.await;
            calls.close_sockets(Group::Plain, ids, STOP_WITHIN).await;
            if let Some(r) = released {
                let _ = r.send(());
            }
        });
        Some(closed)
    }

    fn socket_ids(&self, group: Group) -> Vec<SockId> {
        self.groups()
            .get(&group)
            .map(|g| g.sockets.iter().map(|(id, _)| *id).collect())
            .unwrap_or_default()
    }

    /// Bind `group` on `addrs`, one socket each, closing what it had bound
    /// before. Every address binds, or none stays bound and the group is
    /// [`GroupState::Failed`] naming the one that did not. `served_at` is
    /// the device listener's start a device group is bound for (a call
    /// POSTed on another start's connection is refused); `None` for the
    /// plain group.
    pub async fn bind_group(
        &self,
        group: Group,
        addrs: &[SocketAddr],
        served_at: Option<ServedAt>,
    ) -> GroupState {
        self.close_group(group).await;
        let mut bound = Vec::new();
        for addr in addrs {
            let id = SockId(self.inner.next_sock.fetch_add(1, Ordering::Relaxed));
            match sockets::bind(id, *addr, group.v6()).await {
                Ok(s) => bound.push(s),
                Err(e) => return self.failed(group, format!("binding UDP {addr}: {e}"), served_at),
            }
        }
        let socks: Vec<(SockId, SocketAddr)> = bound.iter().map(|s| (s.id, s.local)).collect();
        let addrs: Vec<SocketAddr> = socks.iter().map(|(_, a)| *a).collect();
        if !self.driver().send(Command::AddSockets(bound)) {
            return self.failed(
                group,
                "the call driver did not take the sockets".into(),
                served_at,
            );
        }
        for a in &addrs {
            tracing::info!(
                "WebRTC calls on {} listening on udp://{a}",
                group.listener()
            );
        }
        let state = GroupState::Bound(addrs);
        self.set_group(group, state.clone(), socks, served_at);
        state
    }

    /// The group's bind failed. The reason names the remedy: nothing binds
    /// again by itself.
    fn failed(&self, group: Group, why: String, served_at: Option<ServedAt>) -> GroupState {
        let why = match group {
            Group::Plain => why,
            Group::Devices => {
                format!("{why}; free the port, then save the device settings again or restart lmgw")
            }
        };
        tracing::warn!(
            "WebRTC calls on {}: {why}; a call on this listener is refused 503 until it binds",
            group.listener()
        );
        self.set_group(
            group,
            GroupState::Failed(why.clone()),
            Vec::new(),
            served_at,
        );
        GroupState::Failed(why)
    }

    /// Close `group` (its listener stops or moves): its calls' owners are
    /// asked to end them ([`Ending::ShuttingDown`]), the sockets stay open
    /// for their last events until the calls are gone or
    /// [`STOP_WITHIN`] has passed, and the rest are closed by force
    /// ([`Ended::GroupClosed`]); the sockets are closed once this returns.
    pub async fn close_group(&self, group: Group) {
        self.close_group_within(group, STOP_WITHIN).await;
    }

    /// [`Self::close_group`] with its own bound on the wait.
    pub async fn close_group_within(&self, group: Group, within: Duration) {
        let ids = self.socket_ids(group);
        self.close_sockets(group, ids, within).await;
    }

    /// Close these sockets of `group` (module doc), leaving the group as it
    /// is when it was bound again meanwhile.
    async fn close_sockets(&self, group: Group, ids: Vec<SockId>, within: Duration) {
        if ids.is_empty() {
            return;
        }
        {
            // No new call on them from here on.
            let mut groups = self.groups();
            if let Some(g) = groups.get_mut(&group) {
                if g.sockets.iter().any(|(id, _)| ids.contains(id)) {
                    *g = GroupEntry {
                        state: GroupState::Down,
                        sockets: Vec::new(),
                        served_at: None,
                    };
                }
            }
        }
        let registry = &self.inner.registry;
        if registry.ask_on(&ids, Ending::ShuttingDown) > 0 {
            let left = registry.wait_gone(&ids, within).await;
            if left > 0 {
                tracing::info!(
                    "WebRTC calls: {left} call(s) had not ended {} s after their listener \
                     stopped; closed",
                    within.as_secs_f32()
                );
            }
        }
        let (done, closed) = oneshot::channel();
        if self.driver().send(Command::CloseSockets { ids, done }) {
            let _ = closed.await;
        }
    }

    /// Answer `o` (§2.4): read the offer, make its `Rtc` on the socket of
    /// its group that serves its candidate, hand it to the driver, and only
    /// then give the answer back. Dropped before it answers, it leaves no
    /// call behind: the handle exists before the wait, and its drop closes
    /// the call.
    ///
    /// The offer is read and answered on a blocking thread: str0m parses an
    /// SDP of up to `max_body_mb` there, off the async workers, and a panic
    /// in it is caught as the task's, logged at ERROR and refused — 400
    /// `invalid_sdp` for the read, 503 for the answer (and for a task the
    /// runtime cancelled).
    pub async fn offer(&self, o: Offer) -> Result<Answered, OfferError> {
        let driver = self.driver();
        let local = o.local.to_canonical();
        let (sock, port) = self.pick(o.group, local, o.served_at)?;
        let candidate = SocketAddr::new(local, port);
        let (sdp, setting) = (o.sdp, o.params.max_message);
        let read = tokio::task::spawn_blocking(move || offer::check(&sdp, setting));
        let checked = match read.await {
            Ok(checked) => checked?,
            Err(e) => {
                // A panic is the offer's; a cancelled task (the runtime
                // going away) is not.
                let panicked = e.is_panic();
                let why = blocking_failed("reading the offer", e);
                return Err(if panicked {
                    OfferError::Invalid(why)
                } else {
                    OfferError::Unavailable(why)
                });
            }
        };
        let (max_message, by, phone) = (checked.max_message, checked.by, checked.phone);
        let config = offer::Config {
            reorder: o.params.reorder,
        };
        let now = Instant::now();
        let at = now.into_std();
        let answer =
            tokio::task::spawn_blocking(move || offer::answer(checked, candidate, &config, at));
        let (mut rtc, sdp) = match answer.await {
            Ok(answered) => answered?,
            Err(e) => {
                let why = blocking_failed("answering the offer", e);
                return Err(OfferError::Unavailable(why));
            }
        };
        let creds = rtc.direct_api().local_ice_credentials();
        let id: Arc<str> = mint_id().into();
        let ending = self.inner.registry.insert(
            CallInfo {
                id: id.clone(),
                creator: o.creator,
                group: o.group,
                candidate,
                state: CallState::Answered,
                created: now,
                connected: None,
                opened: None,
                remote: None,
                last_heard: None,
                unacked_since: None,
                stats: CallStats::default(),
                playout: None,
                held_by: None,
                session: None,
                thread: None,
            },
            sock,
        );
        // A group's close asks the calls it finds in the registry once, after
        // it took the group's sockets out of `groups`: a call entered after
        // that would never be asked, so the group is read again now that this
        // one is in.
        let still = self
            .groups()
            .get(&o.group)
            .is_some_and(|g| g.sockets.iter().any(|(s, _)| *s == sock));
        if !still {
            self.inner.registry.remove(&id);
            return Err(OfferError::Unavailable(
                "the WebRTC UDP listener stopped meanwhile".into(),
            ));
        }
        let (events, rx) = mpsc::unbounded_channel();
        let (feedback, heard) = tokio::sync::watch::channel(handle::Feedback::default());
        let queued = Arc::new(AtomicUsize::new(0));
        let handle = CallHandle {
            control: CallControl {
                id: id.clone(),
                driver: driver.clone(),
                ending,
                ended: false,
                phone,
                reorder: o.params.reorder,
                feedback: heard,
                largest: (max_message, by),
            },
            events: CallEvents {
                rx,
                queued: queued.clone(),
            },
        };
        let (reply, registered) = oneshot::channel();
        let call = NewCall {
            id: id.clone(),
            ufrag: creds.ufrag,
            pass: creds.pass,
            sock,
            rtc,
            events,
            feedback,
            queued,
            queue_limit: o.params.max_message,
            max_message,
            by,
            window: offer::sctp_window(),
            bound: o.params.never_opened,
            keep: o.keep,
        };
        let sent = driver.send(Command::Register {
            call: Box::new(call),
            reply,
        });
        let taken = match (sent, registered.await) {
            (true, Ok(r)) => r,
            _ => Err("the call driver is gone".into()),
        };
        if let Err(why) = taken {
            let mut handle = handle;
            handle.control.ended = true;
            self.inner.registry.remove(&id);
            return Err(OfferError::Unavailable(why));
        }
        tracing::info!("WebRTC call {id} answered, its candidate {candidate}");
        Ok(Answered { sdp, handle })
    }

    /// Whether `group` can answer a call now, or the 503's reason (§2.5):
    /// what a POST asks before it checks anything that costs.
    /// `served_at` is the start of the listener the POST's connection came
    /// in on: the device group refuses another one's (N3).
    pub fn ready(&self, group: Group, served_at: Option<ServedAt>) -> Result<(), OfferError> {
        match self.group(group) {
            GroupState::Bound(_) => self.of_this_start(group, served_at),
            GroupState::Moving => Err(OfferError::Moving),
            state => Err(unavailable(&state)),
        }
    }

    /// Whether the connection of start `served_at` is the one `group` is
    /// bound for: a device call POSTed on a connection of an earlier start,
    /// still answered, would get a socket of the new group (N3). The plain
    /// group is bound once for the gateway's life.
    fn of_this_start(&self, group: Group, served_at: Option<ServedAt>) -> Result<(), OfferError> {
        let groups = self.groups();
        let bound_for = groups.get(&group).and_then(|g| g.served_at);
        Self::same_start(group, bound_for, served_at)
    }

    fn same_start(
        group: Group,
        bound_for: Option<ServedAt>,
        served_at: Option<ServedAt>,
    ) -> Result<(), OfferError> {
        if group == Group::Devices && bound_for != served_at {
            return Err(OfferError::Moving);
        }
        Ok(())
    }

    /// The socket of `group` that serves a candidate on `ip`, and its port.
    fn pick(
        &self,
        group: Group,
        ip: IpAddr,
        served_at: Option<ServedAt>,
    ) -> Result<(SockId, u16), OfferError> {
        // As the group reads (the gap, the switch), not as its entry stands.
        match self.group(group) {
            GroupState::Moving => return Err(OfferError::Moving),
            GroupState::Bound(_) => {}
            other => return Err(unavailable(&other)),
        }
        let groups = self.groups();
        let Some(g) = groups.get(&group) else {
            return Err(unavailable(&GroupState::Down));
        };
        Self::same_start(group, g.served_at, served_at)?;
        match &g.state {
            GroupState::Bound(_) => g
                .sockets
                .iter()
                .find(|(_, local)| sockets::serves(*local, ip))
                .map(|(id, local)| (*id, local.port()))
                .ok_or_else(|| {
                    OfferError::Unavailable(format!(
                        "no WebRTC UDP socket of this listener serves {ip}"
                    ))
                }),
            other => Err(unavailable(other)),
        }
    }

    /// Ask the owner of call `id` to end it for a hangup (§2.6), `by`
    /// naming who hung up; `None` for a call that is not here. The owner
    /// ends it — a bound call's last event is the session's to send first
    /// (§8.2) — and what comes back resolves once the call has let go, which
    /// is when the hangup answers (C3). A call that let go already is
    /// [`Hangup::Closing`]: nobody owns it to ask.
    pub fn hangup(&self, id: &str, by: Option<String>) -> Option<Hangup> {
        self.inner.registry.ask(id, Ending::HungUp { by })
    }

    /// [`Self::hangup`], and the wait for the call to let go, at most
    /// `within`: an owner that has not ended it by then (a session stuck
    /// behind a client that stopped reading, say) has its call closed by
    /// force, with a WARN line, as a group's close closes the calls its
    /// owners left (§3.7). A call that let go and is still closing
    /// ([`CallState::Closing`]: its last events not acknowledged yet) is
    /// closed at once, without them: the hangup asks for the end, and the
    /// owner that would wait for the delivery is gone. `None` for a call
    /// that is not here. The route takes [`STOP_WITHIN`], the bound a group's
    /// close gives the same owners (§8, changed 2026-10-15).
    pub async fn hangup_within(
        &self,
        id: &str,
        by: Option<String>,
        within: Duration,
    ) -> Option<()> {
        let released = match self.hangup(id, by)? {
            Hangup::Asked(released) => released,
            Hangup::Closing => {
                tracing::info!(
                    "WebRTC call {id}: hung up while it closed, its last events not \
                     acknowledged yet; closed now"
                );
                self.close_now(id).await;
                return Some(());
            }
        };
        let timed_out = tokio::time::timeout(within, released.wait()).await.is_err();
        // A call that let go as the bound passed needs no force.
        if timed_out && self.inner.registry.is_live(id) {
            tracing::warn!(
                "WebRTC call {id}: its owner had not ended it {} s after the hangup \
                 (server::STOP_WITHIN); closed by force, its session hears the end as \
                 the client's",
                within.as_secs_f32()
            );
            self.close_now(id).await;
        }
        Some(())
    }

    /// Close call `id` now (`Rtc::close`), whoever owns it: gone from the
    /// registry once this returns.
    async fn close_now(&self, id: &str) {
        let (done, closed) = oneshot::channel();
        if self.driver().send(Command::Close {
            id: id.into(),
            done: Some(done),
        }) {
            let _ = closed.await;
        }
    }

    /// Note that call `id` waits under `takeover=ask` for its thread, held
    /// by `held_by` (§2.8), or — `None` — no longer waits.
    pub fn note_pending(&self, id: &str, held_by: Option<String>) {
        self.inner.registry.update(id, |i| {
            if held_by.is_some() {
                i.state = CallState::Pending;
            }
            i.held_by = held_by;
        });
    }

    /// Note that call `id`'s session runs, as `session`, bound to `thread`
    /// when it is.
    pub fn note_running(&self, id: &str, session: String, thread: Option<i64>) {
        self.inner.registry.update(id, |i| {
            i.state = CallState::Running;
            i.held_by = None;
            i.session = Some(session);
            i.thread = thread;
        });
    }

    /// Whether `group` is bound, so a call POSTed to its listener could be
    /// answered (`/v1/models`, §9).
    pub fn serves(&self, group: Group) -> bool {
        matches!(self.group(group), GroupState::Bound(_))
    }

    /// What is known of call `id`, a closing one's included
    /// ([`CallState::Closing`]).
    pub fn get(&self, id: &str) -> Option<CallInfo> {
        self.inner.registry.get(id)
    }

    /// What a session's liveness check reads of call `id` (§8.1), while it
    /// is live: the client's last authenticated traffic, and since when the
    /// session's output has waited for an acknowledgement.
    pub fn evidence(&self, id: &str) -> Option<Evidence> {
        self.inner.registry.evidence(id)
    }

    /// Every call, closing ones included, oldest first.
    pub fn list(&self) -> Vec<CallInfo> {
        self.inner.registry.list()
    }

    /// How many datagrams no call accepted, dropped unanswered, since lmgw
    /// started (§3.3, §10): the count outlives a driver that is started
    /// again.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Make call `id` panic in its next tick, as str0m might.
    #[cfg(test)]
    fn panic_for_tests(&self, id: &str) {
        self.driver().send(Command::PanicForTests { id: id.into() });
    }

    /// Kill the driver task.
    #[cfg(test)]
    fn kill_driver_for_tests(&self) {
        self.driver().send(Command::DieForTests);
    }
}

/// Why a group that is not bound answers no call (§2.5's 503), naming the
/// setting or the bind error.
fn unavailable(state: &GroupState) -> OfferError {
    OfferError::Unavailable(match state {
        GroupState::Off => "WebRTC calls are off (realtime.webrtc_enabled)".into(),
        GroupState::Failed(why) => format!("the WebRTC UDP listener failed: {why}"),
        GroupState::Down | GroupState::Bound(_) | GroupState::Moving => {
            "the WebRTC UDP listener is not running".into()
        }
    })
}

/// A blocking step of [`Calls::offer`] that did not come back (`what` it
/// was): str0m panicked in it, or the runtime is going away. Logged at
/// ERROR; what the refusal says.
fn blocking_failed(what: &str, e: tokio::task::JoinError) -> String {
    let why = match e.try_into_panic() {
        Ok(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "(no message)".into());
            format!("str0m panicked {what}: {msg}")
        }
        Err(e) => format!("{what} did not finish: {e}"),
    };
    tracing::error!("WebRTC calls: {why}; the offer is refused");
    why
}

/// A call's id (§2.4): `rtc_` and 24 random hex digits, apart from the
/// `call_` of function calls.
fn mint_id() -> String {
    format!(
        "rtc_{:016x}{:08x}",
        rand::random::<u64>(),
        rand::random::<u32>()
    )
}

#[cfg(test)]
mod tests;
