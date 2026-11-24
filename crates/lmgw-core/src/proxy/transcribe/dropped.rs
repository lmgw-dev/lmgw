//! A transcription's open gauge, and the row of one dropped before it wrote
//! its own (document-conversion WP7 review round): what
//! `proxy/in_process/dropped.rs` is for a unary chat call, for the
//! multipart path.
//!
//! Every transcription opens the gauge (`request_started`) and closes it
//! with its row. A caller that drops the call where it stands runs nothing
//! after the await: without a row asked for, the guard abandons the
//! request, as `InFlight` does; with one ([`Unrecorded::running`]'s `row`,
//! a document conversion's transcription whose client left, whose
//! `document_timeout` passed or whose job was cancelled), it writes the
//! `499` `client_disconnected` row the public routes write for a client
//! that went away, under the caller's key, from a task of its own, with
//! the route once the gate had opened one. A call sent to a cloud model and
//! cut mid-upload is then in the usage log, and in the key's budget,
//! instead of leaving no trace.

use std::time::Instant;

use super::{log, record};
use crate::config::Route;
use crate::gate::FallbackReason;
use crate::ingress::ClientProto;
use crate::ir::Usage;
use crate::proxy::{RequestCtx, CLIENT_CLOSED_STATUS, CLIENT_DISCONNECTED};
use crate::state::SharedState;

/// What the `499` row says.
const DROPPED_MESSAGE: &str = "the caller ended the transcription before the model answered \
                               (its client went away, its time limit passed or its work was \
                               cancelled); nothing was answered";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// The call runs: a drop here closes the gauge, with the row when one
    /// is asked for.
    Running,
    /// The call's own row is being written: a drop closes the gauge only.
    Recording,
    /// The row was written.
    Closed,
}

/// One transcription's open gauge (module doc).
pub(super) struct Unrecorded<'a> {
    state: &'a SharedState,
    ctx: &'a RequestCtx,
    proto: ClientProto,
    alias: &'a str,
    started: Instant,
    /// The route the gate opened, once it has.
    route: Option<Route>,
    fallback: Option<FallbackReason>,
    row: bool,
    stage: Stage,
}

impl<'a> Unrecorded<'a> {
    /// Open the gauge; `row`: a drop writes the `499` row.
    pub(super) fn running(
        state: &'a SharedState,
        ctx: &'a RequestCtx,
        proto: ClientProto,
        alias: &'a str,
        started: Instant,
        row: bool,
    ) -> Self {
        state.telemetry.request_started();
        Self {
            state,
            ctx,
            proto,
            alias,
            started,
            route: None,
            fallback: None,
            row,
            stage: Stage::Running,
        }
    }

    /// The gate opened `route` (a fallback's, for `fallback`).
    pub(super) fn routed(&mut self, route: &Route, fallback: Option<FallbackReason>) {
        self.route = Some(route.clone());
        self.fallback = fallback;
    }

    /// The call's own row is being written now.
    pub(super) fn recording(&mut self) {
        self.stage = Stage::Recording;
    }

    /// The row is written and closed the gauge.
    pub(super) fn recorded(&mut self) {
        self.stage = Stage::Closed;
    }
}

impl Drop for Unrecorded<'_> {
    fn drop(&mut self) {
        match self.stage {
            Stage::Closed => return,
            Stage::Recording => {
                self.state.telemetry.request_abandoned();
                return;
            }
            Stage::Running => {}
        }
        // Outside a runtime (teardown) there is no database write to make.
        let rt = tokio::runtime::Handle::try_current();
        let (true, Ok(rt)) = (self.row, rt) else {
            self.state.telemetry.request_abandoned();
            return;
        };
        let state = self.state.clone();
        let ctx = self.ctx.clone();
        let (proto, alias, started) = (self.proto, self.alias.to_string(), self.started);
        let (route, fallback) = (self.route.take(), self.fallback);
        rt.spawn(async move {
            let p = log(
                &state,
                &ctx,
                proto,
                &alias,
                started,
                route.as_ref(),
                fallback,
            );
            record(
                p,
                CLIENT_CLOSED_STATUS,
                None,
                Usage::default(),
                Some((CLIENT_DISCONNECTED, DROPPED_MESSAGE.to_string())),
            )
            .await;
        });
    }
}
