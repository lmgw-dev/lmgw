//! What a request-log row processed besides tokens (billable-units design
//! §4), as its three writers — [`super::record`],
//! [`record_in_process`](crate::proxy::record_in_process) and
//! `record_free_form` — record and price it.
//!
//! **How a measurement reaches a row.** The route that measured builds a
//! [`Quantities`] and hands it over in `LogParams::quantities` (or
//! `InProcessLog::quantities`); `Default::default()` is "nothing measured".
//! The writer then fills `requests` ([`with_requests_default`]), prices the
//! row with [`crate::pricing::price_request`] against the answering alias's
//! [`Sheet`](crate::pricing::Sheet), and writes the three quantity columns
//! ([`quantity_column`]) and the cost's unit part and rates.
//!
//! **The `requests` audit (§4.5).** A stop before the answer writes a 200
//! `canceled` row, so the default leaves `requests` unknown on every
//! `canceled` row, and a relay that knows its upstream answered says so with
//! [`Quantities::answered`]. Every writer of a `canceled` row, and what it
//! knows:
//!
//! - `proxy/chat_stream.rs`, the public chat relay (`/v1/chat/completions`,
//!   `/v1/messages` streams): its task starts only after the upstream's 2xx
//!   headers, so its row — a client gone mid-stream included — is answered.
//! - `proxy/legacy.rs`, the streamed `/v1/completions` passthrough relay:
//!   likewise after the 2xx; answered.
//! - `proxy/in_process.rs`, `stream_once_on` (agent runs, `/v1/responses`
//!   turns, the realtime responder): answered once the turn's response passed
//!   its status check, a consumer's stop or a failure mid-stream included; a
//!   stop before that (`stop::unanswered_usage`) stays unknown.
//! - `web/chat.rs`, the dashboard Chat's own relay: answered once its stream
//!   began, a stop or a failure mid-stream included; a stop while the
//!   upstream had the request (`unanswered_usage`) stays unknown.
//! - `proxy/audio.rs`, `finish_media`: its 2xx headers arrived, so its
//!   upstream answered, a client gone later included.
//! - `proxy/transcribe.rs` and `proxy/synthesize.rs`: a stop is a `canceled`
//!   row whatever the upstream had answered; a synthesis row's `requests` is
//!   its answered clause count (§4.3).
//!
//! A `499` `client_disconnected` row (`proxy/unanswered.rs`) is no 2xx and
//! stays unknown by the default, as it should: nothing was answered.

use crate::pricing::Quantities;

/// The `error_kind` of a call its consumer or client stopped
/// ([`crate::proxy::canceled`]): a 200 row whatever the upstream had done.
const CANCELED: &str = "canceled";

/// `q` with `requests` filled where the caller left it `None` (§4.5): one
/// answered upstream request for a row that has a route, a 2xx status and an
/// `error_kind` other than `canceled`, `None` otherwise. A caller's own
/// count, explicit `Some`, always stands.
pub(in crate::proxy) fn with_requests_default(
    q: Quantities,
    routed: bool,
    status: u16,
    error_kind: Option<&str>,
) -> Quantities {
    if q.requests.is_some() {
        return q;
    }
    let answered = routed && (200..300).contains(&status) && error_kind != Some(CANCELED);
    Quantities {
        requests: answered.then_some(1),
        ..q
    }
}

/// A quantity as its `request_logs` column (`INTEGER`): `None` stays NULL,
/// never 0.
pub(in crate::proxy) fn quantity_column(v: Option<u64>) -> Option<i64> {
    v.map(|v| i64::try_from(v).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_default_to_one_only_for_an_answered_routed_row() {
        let none = Quantities::default();
        let d = |routed, status, kind| with_requests_default(none, routed, status, kind).requests;
        assert_eq!(d(true, 200, None), Some(1));
        assert_eq!(d(true, 204, None), Some(1));
        assert_eq!(
            d(true, 200, Some("transport")),
            Some(1),
            "a relay that broke after its 2xx was still answered"
        );
        assert_eq!(
            d(true, 200, Some("canceled")),
            None,
            "a stop may precede the answer"
        );
        assert_eq!(d(false, 200, None), None, "no route, no upstream");
        assert_eq!(d(true, 502, Some("upstream")), None);
        assert_eq!(d(true, 429, Some("key_rate")), None);
        assert_eq!(d(true, 499, Some("client_disconnected")), None);
    }

    #[test]
    fn a_callers_own_count_stands() {
        let three = Quantities {
            requests: Some(3),
            chars_in: Some(10),
            ..Default::default()
        };
        assert_eq!(
            with_requests_default(three, true, 200, Some("canceled")),
            three
        );
        assert_eq!(with_requests_default(three, false, 500, None), three);
        let measured = Quantities {
            chars_in: Some(10),
            ..Default::default()
        };
        assert_eq!(
            with_requests_default(measured, true, 200, None),
            Quantities {
                requests: Some(1),
                ..measured
            },
            "the other quantities pass through"
        );
    }

    #[test]
    fn a_column_is_null_for_unknown_never_zero() {
        assert_eq!(quantity_column(None), None);
        assert_eq!(quantity_column(Some(0)), Some(0));
        assert_eq!(quantity_column(Some(27_400)), Some(27_400));
        assert_eq!(quantity_column(Some(u64::MAX)), Some(i64::MAX));
    }
}
