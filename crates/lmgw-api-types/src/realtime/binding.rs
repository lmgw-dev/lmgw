//! A bound realtime session's own wire (realtime calls design §2.2, §2.8,
//! §6.4, §8.2): the turn mode lmgw resolves for it, and the events of a
//! call's bind and close. lmgw's own, for its own clients; a stock client
//! never meets any of them.
//!
//! - `session.lmgw.turn_mode` ([`TurnMode`]) is on every session bound to a
//!   Chat thread, on either transport.
//! - [`BindPending`], [`TakeOver`] and [`SessionClosed`] travel on a WebRTC
//!   call bound to a Chat thread (its `oai-events` channel), each a JSON
//!   object with its `type` ([`BIND_PENDING`], [`TAKE_OVER`],
//!   [`SESSION_CLOSED`]); lmgw's carry an `event_id`, as all its events do.
//!   A WebSocket session sends none of them: it closes with its close codes.
//!
//! `by` and `held_by` name a client as the Chat feed's `voice.bound.by`
//! does: "the dashboard", "device 'phone'".

use serde::{Deserialize, Serialize};

/// `lmgw.session.bind_pending`'s `type`.
pub const BIND_PENDING: &str = "lmgw.session.bind_pending";

/// `lmgw.session.take_over`'s `type`.
pub const TAKE_OVER: &str = "lmgw.session.take_over";

/// `lmgw.session.closed`'s `type`.
pub const SESSION_CLOSED: &str = "lmgw.session.closed";

/// `session.lmgw.turn_mode`: how a bound session's turns end. lmgw resolves
/// it at the bind from the thread's voice settings
/// (`voice_resolved.turn_detection`: `push_to_talk` there is
/// [`TurnMode::PushToTalk`], anything else [`TurnMode::Automatic`]) and sets
/// `audio.input.turn_detection` to match: the thread's rule in automatic mode
/// (`semantic_vad` where it names none), `null` in push-to-talk. A client
/// switches it with a `session.update` naming it; one that also names
/// `audio.input.turn_detection` must agree with it (`null` with
/// push-to-talk, a rule with automatic). Turn detection named alone still
/// works, and the mode follows it. Absent on a session bound to no thread,
/// which refuses it.
///
/// Forward compatible like every enum a client reads: a mode a newer
/// gateway sends reads as [`TurnMode::Unknown`], as sent. lmgw itself
/// refuses an unknown one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum TurnMode {
    /// lmgw detects the end of a turn (`server_vad` or `semantic_vad`).
    Automatic,
    /// A turn is what the client commits: no turn detection.
    PushToTalk,
    /// A mode this build does not know (a newer gateway's), as sent.
    #[serde(untagged)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unknown(String),
}

impl TurnMode {
    /// The wire token: `automatic`, `push_to_talk`, or the unknown one as
    /// sent.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Automatic => "automatic",
            Self::PushToTalk => "push_to_talk",
            Self::Unknown(v) => v,
        }
    }
}

impl std::fmt::Display for TurnMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `lmgw.session.bind_pending {held_by}` (lmgw → client): a call that asked
/// with `takeover=ask` found its thread held by another session when its
/// channel opened. No session runs: the client answers with
/// [`TakeOver`] to take the thread, or hangs up to decline. A holder that
/// ends meanwhile lets the call bind by itself (`session.created` follows
/// with no word from the client); a holder that changes brings another
/// `bind_pending` naming the new one. Every other client event meanwhile is
/// answered `error` `bind_pending`, and the audio is discarded. The wait is
/// bounded by `realtime.ping_interval_s`, after which the call ends with
/// [`SessionClosed`] of kind [`ClosedKind::ThreadBound`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BindPending {
    /// Who holds the thread, as `voice.bound.by` names it.
    pub held_by: String,
}

/// `lmgw.session.take_over {}` (client → lmgw): while a call waits after
/// [`BindPending`], take the thread. lmgw runs the bind's checks once more
/// (for a folder, it decides the folder's current thread again) and takes
/// the thread; its holder closes as any session taken over does, and this
/// call's session starts with `session.created`. Outside the wait it is
/// ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TakeOver {}

/// `lmgw.session.closed {kind, by, message}` (lmgw → client): the last event
/// before lmgw ends a call bound to a Chat thread, saying why; the peer
/// connection closes once it is delivered. An unbound call closes with
/// nothing in band, as OpenAI's do, and a call the client ended itself gets
/// none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionClosed {
    pub kind: ClosedKind,
    /// Who: for [`ClosedKind::TakenOver`] the new binder, for
    /// [`ClosedKind::ThreadBound`] the holder, for [`ClosedKind::Closed`]
    /// who hung up; `null` otherwise. Named as `voice.bound.by` names a
    /// client.
    pub by: Option<String>,
    /// The sentence a WebSocket close's reason carries for the same end;
    /// for [`ClosedKind::Revoked`] led by the [`super::RevokeKind`] token,
    /// so [`super::RevokeKind::of_close_reason`] reads it unchanged.
    pub message: String,
}

/// Why a bound call ended ([`SessionClosed::kind`]).
///
/// Forward compatible like every enum a client reads: a kind a newer
/// gateway sends reads as [`ClosedKind::Unknown`], as sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum ClosedKind {
    /// Another client took the thread over (a WebSocket's 4000).
    TakenOver,
    /// The key was revoked (a WebSocket's 4003); `message` leads with the
    /// [`super::RevokeKind`] token.
    Revoked,
    /// The thread left the key's reach, or was deleted (a WebSocket's 4004).
    OutOfReach,
    /// lmgw is stopping or restarting (a WebSocket's 1001).
    ShuttingDown,
    /// lmgw ended it for no reason above: a hangup over HTTP, a dead or
    /// stalled link, a refusal at the open that an `error` just before it
    /// explains, an internal failure (`message` says which).
    Closed,
    /// The thread is held by another session: `takeover=never` when the
    /// channel opened, or the wait after [`BindPending`] ran out. The call
    /// never bound.
    ThreadBound,
    /// A kind this build does not know (a newer gateway's), as sent.
    #[serde(untagged)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unknown(String),
}

impl ClosedKind {
    /// The wire token, or the unknown one as sent.
    pub fn as_str(&self) -> &str {
        match self {
            Self::TakenOver => "taken_over",
            Self::Revoked => "revoked",
            Self::OutOfReach => "out_of_reach",
            Self::ShuttingDown => "shutting_down",
            Self::Closed => "closed",
            Self::ThreadBound => "thread_bound",
            Self::Unknown(v) => v,
        }
    }
}

impl std::fmt::Display for ClosedKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_events_read_and_write_as_the_design_spells_them() {
        let closed = SessionClosed {
            kind: ClosedKind::ThreadBound,
            by: Some("device 'desktop'".into()),
            message: "voice is in use on device 'desktop'".into(),
        };
        assert_eq!(
            serde_json::to_value(&closed).unwrap(),
            json!({"kind": "thread_bound", "by": "device 'desktop'",
                   "message": "voice is in use on device 'desktop'"})
        );
        // `by` is written as null, never left out.
        let shut = SessionClosed {
            kind: ClosedKind::ShuttingDown,
            by: None,
            message: "lmgw is stopping or restarting".into(),
        };
        assert_eq!(serde_json::to_value(&shut).unwrap()["by"], json!(null));
        for (kind, token) in [
            (ClosedKind::TakenOver, "taken_over"),
            (ClosedKind::Revoked, "revoked"),
            (ClosedKind::OutOfReach, "out_of_reach"),
            (ClosedKind::ShuttingDown, "shutting_down"),
            (ClosedKind::Closed, "closed"),
            (ClosedKind::ThreadBound, "thread_bound"),
        ] {
            assert_eq!(serde_json::to_value(&kind).unwrap(), json!(token));
            assert_eq!(kind.as_str(), token);
        }
        let newer: ClosedKind = serde_json::from_value(json!("moved_away")).unwrap();
        assert_eq!(newer, ClosedKind::Unknown("moved_away".into()));

        let pending: BindPending =
            serde_json::from_value(json!({"held_by": "the dashboard"})).unwrap();
        assert_eq!(pending.held_by, "the dashboard");
        assert_eq!(serde_json::to_value(TakeOver {}).unwrap(), json!({}));

        assert_eq!(
            serde_json::to_value(TurnMode::PushToTalk).unwrap(),
            json!("push_to_talk")
        );
        let mode: TurnMode = serde_json::from_value(json!("automatic")).unwrap();
        assert_eq!(mode, TurnMode::Automatic);
        let newer: TurnMode = serde_json::from_value(json!("hybrid")).unwrap();
        assert_eq!(newer.as_str(), "hybrid");
    }
}
