//! What a response's speech sent and had answered, for its one row
//! (billable-units design §4.3, §4.5; realtime design §11).
//!
//! A clause counts when its upstream answered it with a 2xx: its characters
//! as sent, and one request. That is the moment the provider accepted the
//! work — a body that then breaks off, or a stop while it is read, does not
//! take it back. A clause the upstream refused is not counted, and leaves
//! the rest known.
//!
//! **Unknown when in doubt.** A clause that went out and got no answer — a
//! stop while the upstream had it, or a send that failed without an HTTP
//! answer — may or may not be in the provider's hands, so the response's
//! characters and request count are both unknown from then on: `None`,
//! never the sum of the clauses that were answered (§4.1).

use crate::error::GatewayError;
use crate::pricing::Quantities;

/// The answered clauses of one response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Tally {
    answered: u64,
    /// The characters of the answered clauses; `None` once one clause's
    /// text could not be counted.
    chars: Option<u64>,
    /// A clause went out and its answer never came.
    in_doubt: bool,
}

impl Tally {
    pub(super) fn new() -> Self {
        Self {
            answered: 0,
            chars: Some(0),
            in_doubt: false,
        }
    }

    /// A clause's answer was a 2xx; `chars` is the text it was sent
    /// (`proxy::audio::measure::speech_chars`).
    pub(super) fn answered(&mut self, chars: Option<u64>) {
        self.answered += 1;
        self.chars = self.chars.zip(chars).map(|(a, b)| a + b);
    }

    /// A clause went out and no answer came: whether the provider has it is
    /// unknown.
    pub(super) fn in_doubt(&mut self) {
        self.in_doubt = true;
    }

    /// Whether a clause was left in doubt.
    pub(super) fn is_in_doubt(&self) -> bool {
        self.in_doubt
    }

    /// The clauses answered so far, for the log line.
    pub(super) fn clauses(&self) -> u64 {
        self.answered
    }

    /// The characters the answered clauses were sent, for the log line.
    pub(super) fn chars(&self) -> Option<u64> {
        self.chars
    }

    /// The response's quantities for its row: its characters and its
    /// answered requests — a count the row writer takes as it stands, 0
    /// included — or neither when a clause was left in doubt. `requests`
    /// is then `None`, which the writer keeps unknown only on an error row:
    /// on a 2xx row without one it would default to 1 (`Synthesis::finish`
    /// states the contract that rules this out).
    pub(super) fn quantities(&self) -> Quantities {
        if self.in_doubt {
            return Quantities::default();
        }
        Quantities {
            chars_in: self.chars,
            requests: Some(self.answered),
            ..Default::default()
        }
    }
}

/// Whether a clause's failed send leaves it in doubt: no HTTP answer came
/// back — a transport failure or a timeout, after the request may have
/// reached the upstream. A status the upstream answered with is a refusal,
/// and certain.
pub(super) fn leaves_doubt(e: &GatewayError) -> bool {
    matches!(e, GatewayError::Transport(_) | GatewayError::Timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answered_clauses_add_their_characters_and_requests() {
        let mut t = Tally::new();
        assert_eq!(
            t.quantities(),
            Quantities {
                chars_in: Some(0),
                requests: Some(0),
                ..Default::default()
            },
            "nothing answered is a measured 0"
        );
        t.answered(Some(12));
        t.answered(Some(30));
        let q = t.quantities();
        assert_eq!((q.chars_in, q.requests), (Some(42), Some(2)));
        assert_eq!((q.audio_in_ms, q.images_out), (None, None));
        assert_eq!((t.clauses(), t.chars()), (2, Some(42)));
    }

    #[test]
    fn a_clause_in_doubt_leaves_both_unknown() {
        let mut t = Tally::new();
        t.answered(Some(12));
        assert!(!t.is_in_doubt());
        t.in_doubt();
        assert!(t.is_in_doubt());
        assert_eq!(
            t.quantities(),
            Quantities::default(),
            "never the answered part alone"
        );
        assert_eq!(t.clauses(), 1, "the log line still says what was answered");
    }

    #[test]
    fn an_uncounted_text_leaves_the_characters_unknown() {
        let mut t = Tally::new();
        t.answered(Some(12));
        t.answered(None);
        let q = t.quantities();
        assert_eq!((q.chars_in, q.requests), (None, Some(2)));
    }

    #[test]
    fn only_a_send_without_an_http_answer_is_in_doubt() {
        assert!(leaves_doubt(&GatewayError::Transport("reset".into())));
        assert!(leaves_doubt(&GatewayError::Timeout));
        let refused = GatewayError::Upstream {
            status: 500,
            provider_type: None,
            message: "engine fell over".into(),
        };
        assert!(!leaves_doubt(&refused));
        assert!(!leaves_doubt(&GatewayError::BadRequest("x".into())));
    }
}
