//! A unary in-process call whose future is dropped before it wrote its row
//! ([`super::sample_once_noting`]): the in-flight gauge it opened is closed,
//! and, for a caller that asks for it ([`Dropped::Row`]), the call still
//! leaves its `request_logs` row.
//!
//! Every in-process call opens the gauge (`request_started`) and closes it
//! with its row. A caller that drops the call where it stands (an agent
//! run's cancel, a `/v1/responses` request whose client went away, a
//! document conversion's page call when its client left, its
//! `document_timeout` passed or its job was cancelled) runs nothing after
//! the await, so until 2026-11-19 the gauge went on counting a call that
//! was over. [`Open`] is the guard that balances it, as
//! `InFlight` does for the streamed sibling.
//!
//! **The row** ([`Dropped::Row`]) is what the public routes write for a
//! client that went away (`proxy/unanswered.rs`): status `499`,
//! `client_disconnected`, written from a task of its own, with no tokens
//! (nothing was answered; the prompt is not counted locally yet). A call
//! sent to a cloud model and cut mid-answer is then in the usage log under
//! the caller's key instead of leaving no trace.

use std::time::Instant;

use super::{record_in_process, InProcessLog};
use crate::config::Route;
use crate::gate::FallbackReason;
use crate::ir::Usage;
use crate::proxy::{KeyRef, CLIENT_CLOSED_STATUS, CLIENT_DISCONNECTED};
use crate::state::SharedState;
use crate::telemetry::RequestClass;

/// What a dropped call leaves behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dropped {
    /// The gauge closed, no row: every caller until a caller asks for more.
    Gauge,
    /// The gauge closed by a `499` row under the caller's key.
    Row,
}

/// What the `499` row says.
const DROPPED_MESSAGE: &str = "the caller ended the call before the model answered (its client \
                               went away, its time limit passed or its work was cancelled); \
                               nothing was answered";

/// What the row of a dropped call is written from.
pub(super) struct Row<'a> {
    pub key: KeyRef,
    pub ingress_proto: &'a str,
    pub alias: &'a str,
    pub route: &'a Route,
    pub started: Instant,
    pub fallback: Option<FallbackReason>,
}

enum Stage<'a> {
    /// The call runs: a drop here closes the gauge, with the row when one
    /// is asked for.
    Running(Option<Row<'a>>),
    /// The call's own row is being written: a drop here closes the gauge
    /// only (the write may have happened).
    Recording,
    /// The row was written, or the turn was handed on: nothing is left.
    Closed,
}

/// One call's open gauge (module doc).
pub(super) struct Open<'a> {
    state: &'a SharedState,
    stage: Stage<'a>,
}

impl<'a> Open<'a> {
    /// The gauge was just opened; `row` is what a dropped call's row would
    /// say, for a caller that asked for one.
    pub(super) fn running(state: &'a SharedState, row: Option<Row<'a>>) -> Self {
        Self {
            state,
            stage: Stage::Running(row),
        }
    }

    /// The call's own row is being written now.
    pub(super) fn recording(&mut self) {
        self.stage = Stage::Recording;
    }

    /// The row is written and closed the gauge.
    pub(super) fn recorded(&mut self) {
        self.stage = Stage::Closed;
    }

    /// The turn goes on as another call, which opens its own: this one sent
    /// nothing and logs nothing.
    pub(super) fn handed_on(&mut self) {
        self.stage = Stage::Closed;
        self.state.telemetry.request_abandoned();
    }
}

impl Drop for Open<'_> {
    fn drop(&mut self) {
        let row = match std::mem::replace(&mut self.stage, Stage::Closed) {
            Stage::Closed => return,
            Stage::Recording | Stage::Running(None) => None,
            Stage::Running(Some(row)) => Some(row),
        };
        // Outside a runtime (teardown) there is no database write to make.
        let (Some(row), Ok(rt)) = (row, tokio::runtime::Handle::try_current()) else {
            self.state.telemetry.request_abandoned();
            return;
        };
        let state = self.state.clone();
        let (key, started, fallback) = (row.key, row.started, row.fallback);
        let (proto, alias) = (row.ingress_proto.to_string(), row.alias.to_string());
        let route = row.route.clone();
        rt.spawn(async move {
            record_in_process(
                InProcessLog {
                    key,
                    ingress_proto: &proto,
                    alias: &alias,
                    route: &route,
                    started,
                    streamed: false,
                    class: RequestClass::Chat,
                    timings: None,
                    max_tokens_clamped: None,
                    fallback,
                    rung: None,
                    degraded: None,
                    quantities: Default::default(),
                    service_tier: Default::default(),
                },
                CLIENT_CLOSED_STATUS,
                None,
                Usage::default(),
                Some((CLIENT_DISCONNECTED, DROPPED_MESSAGE.to_string())),
                &state,
            )
            .await;
        });
    }
}
