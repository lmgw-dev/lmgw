//! A call from its answer until its session runs (calls design §2.8, C21):
//! the owner of the call's handle meanwhile.
//!
//! - **Until `oai-events` opens** nothing is bound. The audio that arrives
//!   from the first frame is kept for the session (§4.6), its bytes counted
//!   against the call's budget of what waits for its owner
//!   (`realtime.max_message_mb`, `handle::Kept`): past it the driver queues
//!   nothing more ([`CallEvent::TooBig`]) and the open path ends the call, so
//!   a client that never opens the channel cannot stream into memory. A hangup or a stop of the call's
//!   listener ([`super::Ending`]), or a stop of the server it came in on,
//!   closes the call: no channel is open to say why. So does a revocation of
//!   an unbound call's key; a bound call's waits for the open, where its
//!   `lmgw.session.closed` says it. The driver ends a call that never
//!   connects or never opens by itself (§8.1).
//! - **When it opens**, a bound call binds for real
//!   (`thread::check_and_bind`): the checks once more, the folder's decision
//!   under its lock, then by `takeover`. Before each bind and after it the
//!   call looks whether it is over: lmgw asked its owner to end it, or it is
//!   gone already (closed by force, by the client, by the driver). An ended
//!   call ends as asked; a binding that came back for it is marked closing
//!   and let go, its `voice.ended` saying `closed`, so no session starts and
//!   nothing holds the thread after the hangup. A key revoked since the POST
//!   — looked at before the bind and after it — ends the call instead, as
//!   does a refusal: its last event is `lmgw.session.closed` (§8.2),
//!   `thread_bound` for a held thread under `never`, `out_of_reach` for a
//!   thread or folder that left the caller's reach, `revoked`, and for any
//!   other refusal an `error` carrying its code, then `closed`.
//! - **Pending** (`pending`, `takeover=ask`): the thread is held, the
//!   client hears `lmgw.session.bind_pending {held_by}` and the call waits.
//!   The audio kept since the first frame goes then (§4.6).
//! - **Then the session starts** on the call's link (`link`), the `session`
//!   part its first update, the kept audio ahead of the live frames, the
//!   call's question ([`super::Asked`]) its to hear from then on. An unbound
//!   call starts at the open too, so every call has one rule.
//!
//! **What the open path says** (`bind_pending`, `error`,
//! `lmgw.session.closed`) carries `event_id`s of its own minting, as every
//! server event does, and is never waited for where the path waits for
//! anything else: each message is queued with the driver at once, in order
//! ahead of whatever the session sends later ([`super::CallControl::queue`]),
//! and the wait only hears it taken. While one is not taken yet the pending
//! wait reads no client event, so a client that floods events and takes
//! nothing back meets the call's budget rather than lmgw's memory, and its
//! deadline, release, revocation and stop still end the wait. The close
//! lets the call go at once and hands it to the driver to close once what
//! was said is taken and delivered (SCTP acknowledged it), at most one
//! `realtime.ping_interval_s` (none at 0), the grace a WebSocket's last
//! events get (§8.2), then closes it anyway
//! ([`super::CallControl::close_delivered`]).

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::FutureExt;
use lmgw_api_types::realtime::{ClosedKind, SessionClosed};
use serde_json::{Map, Value};

use super::super::asr::AsrResolution;
use super::super::handshake::Unbound;
use super::super::ids::Ids;
use super::super::link::{CallLimits, Transport};
use super::super::protocol::{ServerEvent, ServerFrame};
use super::super::resolve::ChatResolution;
use super::super::session::call::CallSide;
use super::super::session::speech::Speech;
use super::super::session::{self, SessionInit};
use super::super::thread::{self, Bind, Outcome, Pending, Refused, Target};
use super::super::Takeover;
use super::audio::encode;
use super::audio::out::OutParams;
use super::handle::{CallEvent, CallHandle, Ending, Kept, Sent};
use super::link::{self, Start};
use super::registry::CallState;
use crate::devices::{RevokeReason, Watched};
use crate::proxy::RequestCtx;
use crate::server::Running;
use crate::state::SharedState;

mod pending;

/// What the call's session binds.
pub(super) enum What {
    /// `chat_thread` or `chat_folder`, checked at the POST; bound at the
    /// open.
    Bound { target: Target, takeover: Takeover },
    /// No thread: `?model=`, and the models resolved at the POST.
    Unbound {
        requested: Option<String>,
        resolved: Box<Unbound>,
    },
}

/// What the POST hands the open path.
pub(super) struct Opening {
    pub state: SharedState,
    pub ctx: RequestCtx,
    pub handle: CallHandle,
    pub what: What,
    /// The POST's `session` part (§2.2).
    pub session: Option<Map<String, Value>>,
    /// The key's revocation watch, from the POST on; `None` for an anonymous
    /// caller.
    pub revocation: Option<Watched>,
    /// Counted by a stopping server, handed to the session.
    pub running: Running,
}

/// The call until its session runs (module doc).
pub(super) async fn run(o: Opening) {
    let Opening {
        state,
        ctx,
        handle,
        what,
        session,
        revocation,
        running,
    } = o;
    let mut call = Call {
        id: handle.id().into(),
        state,
        ctx,
        handle,
        session,
        revocation,
        running,
        ids: Ids::new(),
        said: FuturesUnordered::new(),
        connected: None,
        opened: None,
    };
    // A bound call's revocation waits for the open, where it can be said.
    let revocation_ends = matches!(what, What::Unbound { .. });
    let kept = match call.until_open(revocation_ends).await {
        Before::Open(kept) => kept,
        Before::Ends(why) => return call.end(&why, None).await,
        Before::Gone => return,
    };
    match what {
        What::Unbound {
            requested,
            resolved,
        } => {
            let Unbound { chat, asr, speech } = *resolved;
            if let Some(reason) = call.revoked_now() {
                return call
                    .end(&format!("its key was revoked ({reason:?})"), None)
                    .await;
            }
            call.start(requested, chat, asr, speech, None, Some(kept))
                .await
        }
        What::Bound { target, takeover } => call.bind(target, takeover, kept).await,
    }
}

/// The call while the open path owns it.
struct Call {
    id: Arc<str>,
    state: SharedState,
    ctx: RequestCtx,
    handle: CallHandle,
    session: Option<Map<String, Value>>,
    revocation: Option<Watched>,
    running: Running,
    /// The open path's own event ids (module doc).
    ids: Ids,
    /// What the open path said and the driver has not taken yet (module
    /// doc): heard in its waits, never awaited in one of their arms.
    said: FuturesUnordered<BoxFuture<'static, Sent>>,
    /// When ICE and DTLS connected, and when `oai-events` opened: the
    /// session's start says how long the audio before it was kept (§4.6).
    connected: Option<tokio::time::Instant>,
    opened: Option<tokio::time::Instant>,
}

/// How the wait for `oai-events` came out.
enum Before {
    /// It opened; the audio kept since the first frame.
    Open(Kept),
    /// lmgw ends the call before it opened, and why: asked to, the key
    /// revoked, the server stopping, or the audio kept past the budget.
    Ends(String),
    /// The driver ended it (never connected or opened, the client's close):
    /// gone already.
    Gone,
}

/// A call found over when the open path looked (module doc).
enum Over {
    /// lmgw asked its owner to end it: a hangup, a stop of its listener.
    Asked(Ending),
    /// It is gone from the registry already, or closing: closed by force
    /// past `server::STOP_WITHIN`, by the client, or by the driver.
    Gone,
}

/// What a bind came to for the call (module doc).
enum Came {
    Bound(Box<Bind>),
    Pending(Pending),
    /// It ends the call instead, and why.
    Ends(Ends),
}

/// Why a bind ends the call.
enum Ends {
    /// The bind's refusal.
    Refused(Refused),
    /// The key was revoked before the bind or while it ran.
    Revoked(RevokeReason),
    /// The call was over before the bind or by the time it came back.
    Over(Over),
}

/// The key's revocation, when it comes; never for an anonymous caller.
async fn revoked(w: Option<&mut Watched>) -> RevokeReason {
    match w {
        Some(w) => w.revoked().await,
        None => std::future::pending().await,
    }
}

impl Call {
    /// Wait for `oai-events` to open, keeping the audio from the first frame
    /// (§4.6) (module doc). `revocation_ends`: a revocation ends the call
    /// now — an unbound call's, which says nothing in band anyway; a bound
    /// call's waits for the open, where `lmgw.session.closed` says it.
    async fn until_open(&mut self, revocation_ends: bool) -> Before {
        let mut kept = self.handle.events.keeper();
        let stopped = self.server_stopped();
        tokio::pin!(stopped);
        let mut revocation = self.revocation.as_mut().filter(|_| revocation_ends);
        loop {
            tokio::select! {
                e = self.handle.events.next() => match e {
                    Some(CallEvent::Open) => {
                        self.opened = Some(tokio::time::Instant::now());
                        return Before::Open(kept);
                    }
                    Some(CallEvent::Audio(f)) => kept.push(f),
                    Some(CallEvent::Connected) => {
                        self.connected = Some(tokio::time::Instant::now());
                    }
                    Some(CallEvent::SenderReport(_) | CallEvent::Message { .. }) => {}
                    Some(CallEvent::TooBig { queued, limit }) => {
                        return Before::Ends(link::QueueOverrun { queued, limit }.to_string())
                    }
                    Some(CallEvent::ChannelClosed | CallEvent::Ended(_)) | None => {
                        return Before::Gone
                    }
                },
                why = self.handle.control.ending() => return Before::Ends(asked(&why)),
                reason = revoked(revocation.as_deref_mut()) => {
                    return Before::Ends(format!("its key was revoked ({reason:?})"));
                }
                () = &mut stopped => return Before::Ends(crate::server::STOPPING.into()),
            }
        }
    }

    /// The server the POST came in on stopped.
    fn server_stopped(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let stops = &self.state.stops;
        stops.stopped_after(stops.at_or_now(self.ctx.served_at))
    }

    /// The key's revocation, if it came already.
    fn revoked_now(&mut self) -> Option<RevokeReason> {
        self.revocation.as_mut()?.revoked().now_or_never()
    }

    /// Whether the call is over already (module doc): lmgw asked its owner
    /// to end it, or it left the registry.
    fn over_now(&mut self) -> Option<Over> {
        if let Some(why) = self.handle.control.ending().now_or_never() {
            return Some(Over::Asked(why));
        }
        self.state
            .calls
            .get(&self.id)
            .is_none_or(|i| i.state == CallState::Closing)
            .then_some(Over::Gone)
    }

    /// The bind at the open (module doc).
    async fn bind(mut self, target: Target, takeover: Takeover, kept: Kept) {
        match self.check_and_bind(target, takeover).await {
            Came::Bound(b) => self.bound(*b, Some(kept)).await,
            Came::Pending(p) => {
                // Discarded under `bind_pending` (§4.6), and off the budget.
                drop(kept);
                pending::wait(self, p).await
            }
            Came::Ends(e) => {
                drop(kept);
                self.ends(e).await
            }
        }
    }

    /// `thread::check_and_bind` for the call, with the call's end and its
    /// key's revocation looked at before it and after it (module doc).
    async fn check_and_bind(&mut self, target: Target, takeover: Takeover) -> Came {
        if let Some(reason) = self.revoked_now() {
            return Came::Ends(Ends::Revoked(reason));
        }
        if let Some(over) = self.over_now() {
            return Came::Ends(Ends::Over(over));
        }
        tracing::debug!(
            "WebRTC call {}: binding (thread::check_and_bind, takeover {takeover:?})",
            self.id
        );
        let outcome = thread::check_and_bind(&self.state, &self.ctx, target, takeover).await;
        if let Some(over) = self.over_now() {
            if let Ok(Outcome::Bound(b)) = &outcome {
                // Ending from here, so a `takeover=never` bind is not refused
                // by it (review F-5); dropped below, its `voice.ended` saying
                // `closed`.
                if let Some(g) = &b.binding.guard {
                    g.closing();
                }
            }
            tracing::info!(
                "WebRTC call {}: it ended while it bound; what the bind came to is let go",
                self.id
            );
            return Came::Ends(Ends::Over(over));
        }
        match outcome {
            Ok(Outcome::Bound(b)) => match self.revoked_now() {
                Some(reason) => {
                    // The binding goes, its `voice.ended` saying why.
                    if let Some(g) = &b.binding.guard {
                        g.revoked();
                    }
                    Came::Ends(Ends::Revoked(reason))
                }
                None => Came::Bound(b),
            },
            Ok(Outcome::Pending(p)) => Came::Pending(p),
            Err(r) => Came::Ends(Ends::Refused(r)),
        }
    }

    /// Bound: the session starts.
    async fn bound(self, b: Bind, kept: Option<Kept>) {
        let Bind {
            binding,
            chat,
            asr,
            speech,
        } = b;
        self.start(None, chat, asr, speech, Some(binding), kept)
            .await
    }

    /// End the call for what a bind came to instead (module doc).
    async fn ends(self, e: Ends) {
        match e {
            Ends::Refused(r) => self.refused(r).await,
            Ends::Revoked(reason) => self.end_revoked(reason).await,
            Ends::Over(over) => self.end_over(over).await,
        }
    }

    /// The call is over: as asked, `lmgw.session.closed` saying why (§2.6,
    /// §8.2); gone already, with nothing more to say.
    async fn end_over(self, over: Over) {
        match over {
            Over::Asked(why) => {
                let closed = closed_for(&why);
                let said = closed.message.clone();
                self.end(&said, Some(closed)).await
            }
            Over::Gone => {
                self.end(
                    "the call ended meanwhile (closed by force, by the client or by the driver)",
                    None,
                )
                .await
            }
        }
    }

    /// A refusal at the open (module doc).
    async fn refused(mut self, r: Refused) {
        let closed = match r.close_kind() {
            Some(ClosedKind::ThreadBound) => SessionClosed {
                kind: ClosedKind::ThreadBound,
                by: r.held_by.clone(),
                message: r.message.clone(),
            },
            Some(kind) => SessionClosed {
                kind,
                by: None,
                message: r.message.clone(),
            },
            None => {
                self.say(ServerEvent::error(r.error_object()));
                SessionClosed {
                    kind: ClosedKind::Closed,
                    by: None,
                    message: r.message.clone(),
                }
            }
        };
        let why = format!("the bind was refused, {} ({})", r.code, r.message);
        self.end(&why, Some(closed)).await
    }

    /// The key was revoked: `lmgw.session.closed {kind: "revoked"}`, its
    /// message led by the `RevokeKind` token, as a WebSocket's 4003 reason.
    async fn end_revoked(self, reason: RevokeReason) {
        let who = self.revocation.as_ref().map_or_else(
            || crate::devices::who(&self.ctx.principal),
            |w| w.who().to_string(),
        );
        let device = self.ctx.principal.is_device_key();
        let message = crate::devices::close_reason(reason, &who, device);
        let closed = SessionClosed {
            kind: ClosedKind::Revoked,
            by: None,
            message: message.clone(),
        };
        self.end(&message, Some(closed)).await
    }

    /// Say `event` on `oai-events`: queued with the driver now, in order,
    /// and heard taken in `said` (module doc).
    fn say(&mut self, event: ServerEvent) {
        let frame = ServerFrame {
            event_id: self.ids.event(),
            event,
        };
        let text = serde_json::to_string(&frame).expect("a server event serializes");
        self.said.push(Box::pin(self.handle.control.queue(text)));
    }

    /// End the call before its session: `closed` first when there is one
    /// (a bound call whose channel is open), then the close once what was
    /// said is delivered, at most one `realtime.ping_interval_s` (module
    /// doc): a client that stopped reading gets the grace a WebSocket's last
    /// events get, and the close comes anyway. The driver keeps one queue of
    /// writes per call, so the close comes behind every event said here.
    async fn end(mut self, why: &str, closed: Option<SessionClosed>) {
        tracing::info!("WebRTC call {}: ended before its session — {why}", self.id);
        if let Some(c) = closed {
            self.say(ServerEvent::LmgwSessionClosed(c));
        }
        self.handle.control.close_delivered().await;
    }

    /// Start the session on the call (module doc).
    async fn start(
        self,
        requested: Option<String>,
        chat: ChatResolution,
        asr: AsrResolution,
        speech: Speech,
        bound: Option<thread::Binding>,
        kept: Option<Kept>,
    ) {
        // What the open path said and the driver has not taken yet goes out
        // ahead of the session's first event, queued with the driver before
        // it: nothing waits for it here.
        let Call {
            id,
            state,
            ctx,
            handle,
            session,
            running,
            connected,
            opened,
            ..
        } = self;
        say_kept(&id, connected, opened, kept.as_ref());
        let first = session.map(|s| {
            serde_json::json!({"type": "session.update", "session": Value::Object(s)}).to_string()
        });
        let asked = handle.control.asked();
        let let_go = handle.control.letting_go();
        let start = {
            let snap = state.snapshot();
            let r = &snap.settings.realtime;
            let phone = handle.control.phone();
            Start {
                first,
                kept,
                input: link::InputParams {
                    reorder: handle.control.reorder(),
                    commit_window: r.webrtc_audio.commit_window(),
                },
                out: Some(OutParams {
                    bitrate: encode::bitrate(r.webrtc_audio.webrtc_output_kbps, phone.max_average),
                    dtx: phone.dtx,
                    bound: bound.is_some(),
                    interval: (r.ping_interval_s > 0)
                        .then(|| std::time::Duration::from_secs(r.ping_interval_s.into())),
                }),
                sizes: link::SizeParams {
                    bound: bound.is_some(),
                    join: (r.max_message_mb > 0).then_some((r.max_message_mb as usize) << 20),
                },
            }
        };
        let (sink, reader) = link::split(handle, start);
        let init = SessionInit {
            state,
            ctx,
            requested_model: requested,
            chat,
            asr,
            speech,
            // The call holds the slot from its POST until it ends (C3).
            slot: None,
            transport: Transport::WebRtc(CallLimits {}),
            bound,
            running: Some(running),
            call: Some(CallSide { id, asked, let_go }),
        };
        session::run(sink, reader, init).await
    }
}

/// The session's start, at DEBUG (§4.6, §13.2): how long after ICE and
/// DTLS connected and after `oai-events` opened it starts, and how much of
/// the client's audio arrived before the open and is kept for it (none
/// under `bind_pending`, which drops it).
fn say_kept(
    id: &str,
    connected: Option<tokio::time::Instant>,
    opened: Option<tokio::time::Instant>,
    kept: Option<&Kept>,
) {
    let now = tokio::time::Instant::now();
    let since = |t: Option<tokio::time::Instant>| {
        t.map_or("?".to_string(), |t| {
            format!("{} ms", now.saturating_duration_since(t).as_millis())
        })
    };
    let kept = match kept {
        Some(k) => {
            let frames = k.frames();
            // The audio's span on the client's RTP clock, its last frame's
            // 20 ms included.
            let span_ms = match (frames.first(), frames.last()) {
                (Some(a), Some(b)) => {
                    b.rtp_time.saturating_sub(a.rtp_time) * 1000 / u64::from(super::audio::RTP_RATE)
                        + super::audio::FRAME.as_millis() as u64
                }
                _ => 0,
            };
            format!(
                "{} frames ({span_ms} ms of audio) arrived before the open and are kept for it",
                frames.len()
            )
        }
        None => "what arrived before the open was dropped under bind_pending".to_string(),
    };
    tracing::debug!(
        "WebRTC call {id}: its session starts {} after ICE and DTLS connected, {} after \
         oai-events opened; {kept}",
        since(connected),
        since(opened),
    );
}

/// How an ask reads in the log.
fn asked(why: &Ending) -> String {
    match why {
        Ending::HungUp { by } => link::hung_up(by.as_deref()),
        Ending::ShuttingDown => crate::server::STOPPING.to_string(),
    }
}

/// The `lmgw.session.closed` for an ask (§2.6, §8.2).
fn closed_for(why: &Ending) -> SessionClosed {
    match why {
        Ending::HungUp { by } => SessionClosed {
            kind: ClosedKind::Closed,
            by: by.clone(),
            message: link::hung_up(by.as_deref()),
        },
        Ending::ShuttingDown => SessionClosed {
            kind: ClosedKind::ShuttingDown,
            by: None,
            message: crate::server::STOPPING.to_string(),
        },
    }
}
