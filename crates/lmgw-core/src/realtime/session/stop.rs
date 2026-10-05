//! Why a session ends before the client's close reaches the reader loop,
//! and the close that says so (realtime design §10.4; package A review 1,
//! A2 review 1).

use super::super::handshake::{Limits, CLOSE_TOO_BIG};
use super::super::liveness::{End, CLOSE_NO_PONG};

/// Why the session ends early.
pub(super) enum Stop {
    /// A ping had no pong for a whole interval (`liveness`): close with this
    /// reason.
    NoPong(String),
    /// The client's read side ended — a close, a FIN, a frame over the size
    /// limit — while a ping went unanswered: nothing is left to wait for
    /// (`liveness`, package A review #1), and how it ended decides the close.
    Gone(End),
}

impl Stop {
    /// The close frame to send, if any — the one the reader loop sends for
    /// the same frame, without its flush (the output is what waits) — and
    /// the log line, worded for what happened: a failed read is not "the
    /// client closed" (A2 review 1).
    pub fn close(self, limits: &Limits) -> (Option<(u16, String)>, String) {
        match self {
            Self::NoPong(reason) => (
                Some((CLOSE_NO_PONG, reason.clone())),
                format!("closed — {reason}"),
            ),
            Self::Gone(End::Closed) => (
                None,
                "the client's read side ended while a ping was unanswered and its output \
                 waited; the session ends without waiting for it to read"
                    .into(),
            ),
            Self::Gone(End::Failed(e)) => match limits.close_reason(e) {
                Some(reason) => (
                    Some((CLOSE_TOO_BIG, reason.clone())),
                    format!("closed — {reason}"),
                ),
                None => (
                    None,
                    "a read failed while a ping was unanswered and its output waited; the \
                     session ends"
                        .into(),
                ),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RealtimeSettings;
    use tokio_tungstenite::tungstenite;

    #[test]
    fn a_failed_read_closes_as_the_reader_loop_would() {
        let limits = Limits::from_settings(&RealtimeSettings {
            max_message_mb: 9,
            max_frame_mb: 9,
            ..Default::default()
        });
        let too_long =
            tungstenite::Error::Capacity(tungstenite::error::CapacityError::MessageTooLong {
                size: 10 << 20,
                max_size: limits.max_message,
            });
        let (close, log) = Stop::Gone(End::Failed(axum::Error::new(too_long))).close(&limits);
        let (code, reason) = close.expect("a 1009 close");
        assert_eq!(code, 1009);
        assert!(reason.contains("realtime.max_message_mb"), "{reason}");
        assert_eq!(log, format!("closed — {reason}"));
        // Any other failed read: no close of its own, and no word of a
        // client that closed.
        let other = axum::Error::new(std::io::Error::other("reset"));
        let (close, log) = Stop::Gone(End::Failed(other)).close(&limits);
        assert!(close.is_none());
        assert!(log.starts_with("a read failed"), "{log}");
        let (close, log) = Stop::Gone(End::Closed).close(&limits);
        assert!(close.is_none() && log.contains("read side ended"), "{log}");
        let (close, _) = Stop::NoPong("no pong".into()).close(&limits);
        assert_eq!(close, Some((1011, "no pong".into())));
    }
}
