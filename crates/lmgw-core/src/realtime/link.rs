//! The transport seam (calls design §7): what a session reads from its
//! client, and how it writes to it and closes it, whatever carries them.
//!
//! A session runs on a link: a WebSocket (`ws`), or a WebRTC call's data
//! channel and audio track (`calls::link`; calls design WP2 to WP5). The session loop
//! and its core see only what is here:
//!
//! - **In.** The link's reader passes on what it read as an [`Incoming`],
//!   stamped as it came, through the [`Inbox`] (`inbox`): an event's text,
//!   decoded audio, a binary message (no part of the protocol), what the
//!   link refused itself (a call's broken parts), a pong, and last the end
//!   ([`End`]), behind everything read before it.
//! - **Out.** The writer drives the link's [`Sink`]: an event's text, a
//!   ping, the close. The paced queue, purges, the playback record and the
//!   drained marker are the writer's (`writer`), whatever the link.
//! - **Close.** Why lmgw ends a session is a [`Close`]; each link says it in
//!   its own way, a WebSocket as a close frame's code and reason
//!   (`ws::close_frame`).
//! - **Liveness.** The liveness check (`liveness`) takes its evidence and
//!   its round trips from the link: on a WebSocket, the pings the sink
//!   sends and the pongs the reader reads ([`Incoming::Pong`]), both
//!   measured on the session's `RoundTrip`.
//!
//! What the session needs to know of its transport, its bounds included, is
//! its [`Transport`].

use std::future::Future;

use tokio::time::Instant;

use super::handshake::Limits;
use super::liveness::RoundTrip;

mod inbox;
pub(crate) mod ws;

pub(crate) use inbox::{Feed, Inbox};

/// What a link's reader read, in order (calls design §7.1).
#[derive(Debug)]
pub(crate) enum Incoming {
    /// One client event's JSON text, as the frame carried it (axum's
    /// `Utf8Bytes`, so a large message is not copied). A call's reader
    /// builds it with `From<String>`.
    Event(axum::extract::ws::Utf8Bytes),
    /// Input audio, decoded to PCM16 at 24 kHz and placed on the wall clock
    /// at `captured` (calls design §4.1, §4.3): a call's. A WebSocket's
    /// audio comes as `input_audio_buffer.append` events.
    // A call's reader is the first to send it (WP3).
    #[allow(dead_code)]
    Audio { pcm: Vec<i16>, captured: Instant },
    /// A binary message: no part of the Realtime protocol.
    Binary,
    /// What the link refused before the session saw it, answered as this
    /// `error`: a call's broken set of `lmgw.event.part`s, or one joined past
    /// `realtime.max_message_mb`, or a part on an unbound call (calls design
    /// §6.3). A WebSocket's reader never yields it.
    Refused(super::protocol::ErrorObject),
    /// The answer to a liveness ping (`liveness`).
    Pong,
    /// The read side ended, and how. Nothing follows it.
    End(End),
}

/// How a client's read side ended (A2 review 1): the session closes the way
/// its loop would have for the same end.
#[derive(Debug)]
pub(crate) enum End {
    /// The client closed, or the link ended.
    Closed,
    /// A failed read: on a WebSocket, a frame or message over the size
    /// settings is one, which closes with a reason naming the setting
    /// ([`Transport::too_big`]).
    Failed(axum::Error),
}

/// Why lmgw closes a session's link (calls design §7.1). A WebSocket says
/// each as the close frame it has always sent (`ws::close_frame`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Close {
    /// Another session took the thread over; `by` names the new binder, and
    /// a WebSocket's reason is built from it (`thread::taken_over_reason`).
    TakenOver { by: Option<String> },
    /// The key was revoked; the reason is led by its `RevokeKind` token
    /// (`devices::close_reason`).
    Revoked(String),
    /// The thread left the key's reach (`thread::out_of_reach_reason`).
    OutOfReach(String),
    /// lmgw is stopping or restarting.
    GoingAway,
    /// A message or frame over the size settings; the reason names the one.
    TooBig(String),
    /// The client did not answer for a whole `realtime.ping_interval_s`;
    /// the reason names the setting.
    NoResponse(String),
    /// The call was hung up over HTTP (calls design §2.6), by `by` when a
    /// client did, named as `voice.bound.by` names it. Only a call meets it.
    HungUp { by: Option<String> },
}

/// A link's sending half, as the writer drives it (calls design §7.1).
///
/// Audio is the one branch the writer keeps for itself: a WebSocket's
/// leaves as `response.output_audio.delta` events, released by the writer's
/// pacer; a call's own clock pulls it ([`Sink::pull`]).
pub(crate) trait Sink: Send + 'static {
    /// Send one event's JSON text. `false`: the client is gone.
    fn event(&mut self, text: String) -> impl Future<Output = bool> + Send;

    /// Probe the client for the liveness check, noting the probe on
    /// `round_trip` (`liveness::round_trip`). `false`: the client is gone.
    fn ping(&mut self, round_trip: &RoundTrip) -> impl Future<Output = bool> + Send;

    /// Say `close` to the client; the link ends with it.
    fn close(&mut self, close: Close) -> impl Future<Output = ()> + Send;

    /// End the link without a close of lmgw's own: every handle of the
    /// writer is gone, or the client closed first (`writer`'s module doc).
    fn end(&mut self) -> impl Future<Output = ()> + Send;

    /// Take the writer's paced queue for the link's own clock, if its audio
    /// is pulled rather than paced (a WebRTC call's frame clock, calls
    /// design §5.1): `true`, and no pacer times the audio from here on. A
    /// WebSocket's audio leaves as events the writer paces: `false`.
    fn pull(&mut self, _queue: super::writer::Pulled) -> bool {
        false
    }
}

/// What carries a session, with its bounds (calls design §7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    /// `GET /v1/realtime`: a WebSocket, with its message and frame bounds.
    WebSocket(Limits),
    /// `POST /v1/realtime/calls`: a WebRTC call.
    WebRtc(CallLimits),
}

impl Transport {
    /// The close reason for a read that failed by tripping a size bound,
    /// or `None` for any other failure: on a WebSocket a frame or message
    /// over its settings ([`Limits::close_reason`]). A call's single message
    /// is bounded by the WebRTC stack, which closes no session for it
    /// (calls design §6.3); what waited for its session past
    /// `realtime.max_message_mb` is (`calls::link::QueueOverrun`).
    pub fn too_big(&self, e: axum::Error) -> Option<String> {
        match self {
            Self::WebSocket(limits) => limits.close_reason(e),
            Self::WebRtc(_) => super::calls::link::too_big_reason(e),
        }
    }
}

/// A call's bounds (calls design §6.3). Its single message is bounded by the
/// WebRTC stack (256 KiB), with no setting of lmgw's; what a bound client's
/// parts may join to (`realtime.max_message_mb`) the call's reader bounds as
/// it joins them, before the session reads anything (`calls::sizes`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CallLimits {}
