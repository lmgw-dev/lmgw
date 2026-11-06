//! What the native passthrough's retries share (`refused_params.rs`,
//! `reasoning_off.rs`): the refused attempt's own row, and a stream whose
//! client left after a refusal.
//!
//! A stream's client that leaves after the upstream refused a send with a
//! `4xx` — while the error is read, or once it is read and before a retry
//! is decided — is sent no retry. A refusal ran nothing upstream and nothing
//! else was sent, so the request's one row is the `499` with no usage and no
//! local count: nothing for the budgets, as on the chat stream, whose prompt
//! counts only while a send that has not been refused is out
//! (`proxy/unanswered.rs`, `count_if_sent`). That row names the refusal
//! (its words, once read) and says nothing was retried; the refused attempt
//! writes no row of its own, which would say a retry went out. A `5xx` whose
//! body the client did not wait for may have been worked on: that is the
//! plain hang-up. A client that leaves while a send waits for its answer is
//! the plain [`Forward::unanswered`], whose prompt went out and is the
//! budgets' lower bound — a client that leaves while a decided retry's row
//! is written included, whose retry is then about to go out. A whole
//! answer's retry goes out whatever its client does: it reads on, as its
//! first send would have.

use std::time::Instant;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::{disconnected, Forward};
use crate::error::GatewayError;
use crate::pricing::Quantities;
use crate::proxy;
use crate::telemetry::RESPONSES_PROTO;

impl Forward {
    /// The row of a send made at `attempt` that the upstream refused with
    /// `status`, read as `e`, which is retried: `note` says why and with
    /// what. Its own in-flight count: the request's is closed by its final
    /// row.
    pub(super) async fn record_refused(
        &self,
        status: StatusCode,
        e: &GatewayError,
        attempt: Instant,
        note: String,
    ) {
        self.state.telemetry.request_started();
        proxy::record_passthrough(
            &self.state,
            &self.ctx,
            RESPONSES_PROTO,
            &self.alias,
            &self.route,
            self.fallback,
            attempt,
            None,
            self.streamed,
            status.as_u16(),
            Some((e.kind().to_string(), note)),
            self.degraded.clone(),
            &proxy::Reported::default(),
            Quantities::default(),
        )
        .await;
    }

    /// Whether a stream's client has left: no retry goes out for it.
    pub(super) fn left(&self) -> bool {
        self.streamed && *self.gone.borrow()
    }

    /// The row of a stream whose client left after the upstream refused a
    /// send with `status` (in `words`, once read), with nothing out since:
    /// the `499`, nothing charged, nothing retried (module doc).
    pub(super) async fn left_after_refusal(
        self,
        status: StatusCode,
        words: Option<String>,
    ) -> Response {
        let why = match words {
            Some(w) => format!(
                "client disconnected after the upstream refused the request ({w}); nothing was \
                 retried and nothing answered"
            ),
            None => format!(
                "client disconnected while the upstream's refusal ({}) was read; nothing was \
                 retried and nothing answered",
                status.as_u16()
            ),
        };
        proxy::record_passthrough(
            &self.state,
            &self.ctx,
            RESPONSES_PROTO,
            &self.alias,
            &self.route,
            self.fallback,
            self.started,
            None,
            self.streamed,
            proxy::CLIENT_CLOSED_STATUS,
            disconnected(&why),
            self.degraded.clone(),
            &proxy::Reported::default(),
            Quantities::default(),
        )
        .await;
        // Nobody reads it.
        StatusCode::from_u16(proxy::CLIENT_CLOSED_STATUS)
            .unwrap_or(StatusCode::BAD_REQUEST)
            .into_response()
    }
}
