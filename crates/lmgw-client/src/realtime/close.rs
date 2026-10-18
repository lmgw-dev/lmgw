//! What a realtime session's close means for a client: [`close_kind`] for
//! a WebSocket's close code and reason, [`call_close`] for a bound call's
//! last event, `lmgw.session.closed` (realtime calls design §8.2). One enum
//! whatever the transport.

use super::{
    ClosedKind, RevokeKind, SessionClosed, CLOSE_GOING_AWAY, CLOSE_OUT_OF_REACH, CLOSE_REVOKED,
    CLOSE_TAKEN_OVER,
};

/// Why lmgw closed a realtime session, read from the close's code and
/// reason ([`close_kind`]) or from a call's `lmgw.session.closed`
/// ([`call_close`]).
///
/// Non-exhaustive: a newer version of this crate may tell more closes
/// apart.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CloseKind {
    /// 4003: the key was revoked, and reconnecting with it is refused.
    /// `kind` says what to do: [`RevokeKind::KeyUnknown`] and
    /// [`RevokeKind::CertificateExpired`] are "pair the device again",
    /// [`RevokeKind::DeviceDisabled`] and [`RevokeKind::KeyExpired`] wait
    /// for the gateway's side.
    Revoked { kind: RevokeKind },
    /// 4004: the bound thread left this key's reach. The key is still
    /// good: ask for the current thread again.
    OutOfReach,
    /// 1001: lmgw is stopping or restarting. Reconnect once it is back.
    ShuttingDown,
    /// 4000: another client bound the thread; the reason names it (a
    /// call's `by`).
    TakenOver,
    /// A call's `thread_bound`: the thread is held by another session
    /// (`by`) — `takeover=never` when the channel opened, or the wait after
    /// `lmgw.session.bind_pending` ran out. The call never bound. No
    /// WebSocket close means it: the upgrade refuses such a bind with 409
    /// `chat_thread_bound` before its 101.
    ThreadBound,
    /// A close lmgw gives no meaning of its own: any code outside 4000–4999
    /// but 1001 (a normal close, a protocol, size or network error); or a
    /// call's `closed`, and a kind this build does not know, which carry no
    /// code (`None`) — a hangup, a dead or stalled link, a refusal at the
    /// open that an `error` just before it explains, a newer gateway's
    /// kind; the message says which.
    Other { code: Option<u16> },
    /// An application code (4000–4999) this build does not know: a newer
    /// gateway's.
    Unknown { code: u16 },
}

/// What the close `code` with `reason` means: the code decides, and a 4003's
/// reason says the [`RevokeKind`] in its leading token
/// (`device_disabled: device 'desktop' was disabled`). A 4003 whose reason
/// carries no token (a gateway from before the kinds) is an empty
/// [`RevokeKind::Unknown`].
pub fn close_kind(code: u16, reason: &str) -> CloseKind {
    match code {
        CLOSE_REVOKED => CloseKind::Revoked {
            kind: RevokeKind::of_close_reason(reason),
        },
        CLOSE_OUT_OF_REACH => CloseKind::OutOfReach,
        CLOSE_GOING_AWAY => CloseKind::ShuttingDown,
        CLOSE_TAKEN_OVER => CloseKind::TakenOver,
        _ if (4000..=4999).contains(&code) => CloseKind::Unknown { code },
        _ => CloseKind::Other { code: Some(code) },
    }
}

/// A bound call's end as its `lmgw.session.closed` says it
/// ([`call_close`]): the [`CloseKind`], who, and lmgw's sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallClose {
    pub kind: CloseKind,
    /// Who, named as the Chat feed's `voice.bound.by` names a client: the
    /// new binder for [`CloseKind::TakenOver`], the holder for
    /// [`CloseKind::ThreadBound`], who hung up for a hangup's
    /// [`CloseKind::Other`]; `None` otherwise.
    pub by: Option<String>,
    /// The sentence a WebSocket close's reason carries for the same end, to
    /// show as it is (a revocation's leads with its [`RevokeKind`] token).
    pub message: String,
}

/// What a bound call's `lmgw.session.closed {kind, by, message}` means, in
/// the terms a WebSocket's close gives ([`close_kind`]): `taken_over`,
/// `revoked` (its [`RevokeKind`] read from the message's leading token, as
/// a 4003's reason is read), `out_of_reach` and `shutting_down` are the
/// variants their close codes give; `thread_bound` is
/// [`CloseKind::ThreadBound`]; `closed`, and a kind this build does not
/// know, are [`CloseKind::Other`] without a code. The event's own `kind`
/// still names an unknown one, for the log.
pub fn call_close(closed: &SessionClosed) -> CallClose {
    let kind = match &closed.kind {
        ClosedKind::TakenOver => CloseKind::TakenOver,
        ClosedKind::Revoked => CloseKind::Revoked {
            kind: RevokeKind::of_close_reason(&closed.message),
        },
        ClosedKind::OutOfReach => CloseKind::OutOfReach,
        ClosedKind::ShuttingDown => CloseKind::ShuttingDown,
        ClosedKind::ThreadBound => CloseKind::ThreadBound,
        _ => CloseKind::Other { code: None },
    };
    CallClose {
        kind,
        by: closed.by.clone(),
        message: closed.message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_close_reads_as_its_kind() {
        for (reason, kind) in [
            (
                "device_disabled: device 'desktop' was disabled",
                RevokeKind::DeviceDisabled,
            ),
            (
                "key_expired: device 'desktop' expired",
                RevokeKind::KeyExpired,
            ),
            (
                "key_unknown: device 'desktop' was rotated — pair it again",
                RevokeKind::KeyUnknown,
            ),
            (
                "revoked: owner key 'owner:dashboard' was rotated",
                RevokeKind::Revoked,
            ),
            (
                "key_moved: device 'desktop' moved",
                RevokeKind::Unknown("key_moved".into()),
            ),
            (
                "key_v2: device 'desktop' moved",
                RevokeKind::Unknown("key_v2".into()),
            ),
            ("key_unknown", RevokeKind::KeyUnknown),
            (
                "device 'desktop' was disabled",
                RevokeKind::Unknown(String::new()),
            ),
            (
                "key 'owner:dashboard' was rotated",
                RevokeKind::Unknown(String::new()),
            ),
        ] {
            assert_eq!(
                close_kind(4003, reason),
                CloseKind::Revoked { kind },
                "{reason}"
            );
        }
        assert_eq!(
            close_kind(4004, "chat thread 7 is out of reach for this key"),
            CloseKind::OutOfReach
        );
        assert_eq!(
            close_kind(1001, crate::realtime::SHUTTING_DOWN),
            CloseKind::ShuttingDown
        );
        assert_eq!(
            close_kind(4000, "voice mode moved to device 'phone'"),
            CloseKind::TakenOver
        );
        assert_eq!(close_kind(1000, ""), CloseKind::Other { code: Some(1000) });
        assert_eq!(
            close_kind(1009, "too big"),
            CloseKind::Other { code: Some(1009) }
        );
        assert_eq!(close_kind(4010, "later"), CloseKind::Unknown { code: 4010 });
    }

    #[test]
    fn each_call_close_reads_as_the_kind_its_close_code_gives() {
        let read = |v: serde_json::Value| {
            call_close(&serde_json::from_value::<SessionClosed>(v).expect("reads"))
        };
        let c = read(
            serde_json::json!({"kind": "taken_over", "by": "device 'desktop'",
            "message": "voice mode moved to device 'desktop'"}),
        );
        assert_eq!(
            (c.kind, c.by.as_deref()),
            (CloseKind::TakenOver, Some("device 'desktop'"))
        );
        assert_eq!(c.message, "voice mode moved to device 'desktop'");
        // A revocation's kind from the message's token, as a 4003's.
        for (message, kind) in [
            (
                "key_unknown: device 'phone' was rotated — pair it again",
                RevokeKind::KeyUnknown,
            ),
            (
                "device_disabled: device 'phone' was disabled",
                RevokeKind::DeviceDisabled,
            ),
            // The device link's certificate expiry (device link D5, §5.1).
            (
                "certificate_expired: the certificate of device 'phone' expired — pair it again",
                RevokeKind::CertificateExpired,
            ),
            ("device 'phone' was disabled", RevokeKind::default()),
        ] {
            let c = read(serde_json::json!({"kind": "revoked", "by": null, "message": message}));
            assert_eq!(c.kind, CloseKind::Revoked { kind }, "{message}");
            assert_eq!(close_kind(4003, message), c.kind, "a 4003's reading");
            assert_eq!(c.by, None);
        }
        for (kind, want) in [
            ("out_of_reach", CloseKind::OutOfReach),
            ("shutting_down", CloseKind::ShuttingDown),
            ("thread_bound", CloseKind::ThreadBound),
            ("closed", CloseKind::Other { code: None }),
            ("moved_away", CloseKind::Other { code: None }),
        ] {
            let c = read(serde_json::json!({"kind": kind, "by": "the dashboard", "message": "m"}));
            assert_eq!(c.kind, want, "{kind}");
            assert_eq!(c.by.as_deref(), Some("the dashboard"), "{kind}");
        }
        assert_eq!(
            read(serde_json::json!({"kind": "shutting_down", "by": null,
                "message": crate::realtime::SHUTTING_DOWN}))
            .kind,
            close_kind(1001, crate::realtime::SHUTTING_DOWN)
        );
    }

    #[test]
    fn the_codes_are_the_ones_the_gateway_sends() {
        assert_eq!(
            (
                CLOSE_TAKEN_OVER,
                CLOSE_REVOKED,
                CLOSE_OUT_OF_REACH,
                CLOSE_GOING_AWAY
            ),
            (4000, 4003, 4004, 1001)
        );
    }
}
