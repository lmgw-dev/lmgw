//! What a realtime session's close means for a client: [`close_kind`].

use super::{RevokeKind, CLOSE_GOING_AWAY, CLOSE_OUT_OF_REACH, CLOSE_REVOKED, CLOSE_TAKEN_OVER};

/// Why lmgw closed a realtime session, read from the close's code and
/// reason ([`close_kind`]).
///
/// Non-exhaustive: a newer version of this crate may tell more closes
/// apart.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CloseKind {
    /// 4003: the key was revoked, and reconnecting with it is refused.
    /// `kind` says what to do: [`RevokeKind::KeyUnknown`] is "pair the
    /// device again", [`RevokeKind::DeviceDisabled`] and
    /// [`RevokeKind::KeyExpired`] wait for the gateway's side.
    Revoked { kind: RevokeKind },
    /// 4004: the bound thread left this key's reach. The key is still
    /// good: ask for the current thread again.
    OutOfReach,
    /// 1001: lmgw is stopping or restarting. Reconnect once it is back.
    ShuttingDown,
    /// 4000: another client bound the thread; the reason names it.
    TakenOver,
    /// A close lmgw gives no meaning of its own, any code outside
    /// 4000–4999 but 1001: a normal close, a protocol, size or network
    /// error.
    Other { code: u16 },
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
        _ => CloseKind::Other { code },
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
        assert_eq!(close_kind(1000, ""), CloseKind::Other { code: 1000 });
        assert_eq!(close_kind(1009, "too big"), CloseKind::Other { code: 1009 });
        assert_eq!(close_kind(4010, "later"), CloseKind::Unknown { code: 4010 });
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
