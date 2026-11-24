//! A conversion's `request_logs` row (document-conversion design §7.5): one
//! per request, labelled `documents`, the class of a row that is no model
//! call (no tokens, no price), with its status and total time, and beside it
//! the request's `convert_calls` record (§7.8): its route, file count,
//! target and options with their outcomes. A request whose client went away
//! is a 499 row, its tools stopped with it.
//!
//! An async request's row is written when it is accepted (202); its
//! conversion's outcome is the job's state.

use std::time::Instant;

use crate::extract::convert::calls::Record;
use crate::proxy::{RequestCtx, CLIENT_CLOSED_STATUS, CLIENT_DISCONNECTED};
use crate::state::SharedState;
use crate::store::NewRequestLog;
use crate::telemetry::RequestClass;

/// The row's `ingress_proto`.
pub const PROTO: &str = "documents";

/// The native route's `requested_alias`: the route, as a conversion has no
/// model. A docling route's is `docling` and its path
/// (`docling/v1/convert/file`), so Usage can filter them.
pub const ALIAS: &str = "documents/convert";

/// A row's error kind for an answer whose client did not read it within
/// its write deadline ([`Row::when_sent`]).
pub const WRITE_DEADLINE_KIND: &str = "write_deadline";

/// The open row: written by [`Row::finish`], or as a 499 when dropped
/// before it.
pub struct Row {
    state: SharedState,
    ctx: RequestCtx,
    alias: String,
    started: Instant,
    open: bool,
    record: Option<Record>,
}

impl Row {
    /// Open the native route's row and the in-flight gauge.
    pub fn open(state: &SharedState, ctx: &RequestCtx) -> Self {
        Self::open_as(state, ctx, ALIAS.to_string())
    }

    /// Open a row labelled `alias`.
    pub fn open_as(state: &SharedState, ctx: &RequestCtx, alias: String) -> Self {
        state.telemetry.request_started();
        Self {
            state: state.clone(),
            ctx: ctx.clone(),
            alias,
            started: Instant::now(),
            open: true,
            record: None,
        }
    }

    /// What the request sent, written beside the row.
    pub fn record(&mut self, r: Record) {
        self.record = Some(r);
    }

    fn row(&self, status: u16, error: Option<(String, String)>) -> NewRequestLog {
        let snap = self.state.snapshot();
        NewRequestLog {
            client_key: self.ctx.current_key_name(&snap),
            ingress_proto: PROTO.to_string(),
            requested_alias: self.alias.clone(),
            status: i64::from(status),
            total_ms: Some(self.started.elapsed().as_millis() as i64),
            streamed: false,
            error_kind: error.as_ref().map(|(k, _)| k.clone()),
            error_msg: error.map(|(_, m)| m),
            key_id: self.ctx.principal.key_id(),
            // No model call: no tokens, no price, kept out of the model
            // classes' averages and the unpriced remainder.
            class: RequestClass::Tool,
            via: self.ctx.via,
            ..Default::default()
        }
    }

    /// The row written once the answer's body has ended
    /// ([`super::send::To::then`], verification N-b): `status` and `error`
    /// for a body handed over whole; a client that left midway or missed
    /// the write deadline is a `499` saying how much of the answer was
    /// handed over (codes and counts only).
    pub fn when_sent(self, status: u16, error: Option<(String, String)>) -> super::send::OnEnd {
        use super::send::Ended;
        use lmgw_api_types::units::fmt_bytes;
        Box::new(move |how| {
            Box::pin(async move {
                let (status, error) = match how {
                    Ended::Whole => (status, error),
                    Ended::Left { sent, len } => (
                        CLIENT_CLOSED_STATUS,
                        Some((
                            CLIENT_DISCONNECTED.to_string(),
                            format!(
                                "the client disconnected while its answer was sent: {} of {} \
                                 handed over (the answer was {status})",
                                fmt_bytes(sent),
                                fmt_bytes(len)
                            ),
                        )),
                    ),
                    Ended::Late { sent, len } => (
                        CLIENT_CLOSED_STATUS,
                        Some((
                            WRITE_DEADLINE_KIND.to_string(),
                            format!(
                                "the client did not read its answer within its write deadline \
                                 (ANSWER_WRITE_GRACE_S, ANSWER_MIN_RATE_BYTES_PER_S): {} of {} \
                                 handed over, then its connection was ended (the answer was \
                                 {status})",
                                fmt_bytes(sent),
                                fmt_bytes(len)
                            ),
                        )),
                    ),
                    Ended::Unwritable => (
                        500,
                        Some((
                            "internal".to_string(),
                            "the answer could not be written as JSON".to_string(),
                        )),
                    ),
                };
                self.finish(status, error).await;
            })
        })
    }

    /// Write the row: `status`, and the error's kind and message for a
    /// request that converted nothing.
    pub async fn finish(mut self, status: u16, error: Option<(String, String)>) {
        self.open = false;
        let row = self.row(status, error);
        let record = self.record.take();
        write(&self.state, row, status, record).await;
    }
}

/// The row, then its record beside it, in one task the handler waits for.
async fn write(state: &SharedState, row: NewRequestLog, status: u16, record: Option<Record>) {
    let st = state.clone();
    let task = tokio::spawn(async move {
        let id = crate::proxy::write_row(&st, row, status).await;
        if let (Some(r), true) = (record, id > 0) {
            if let Err(e) = crate::store::insert_convert_call(&st.db, id, &r.row()).await {
                tracing::warn!("recording what a conversion request sent: {e}");
            }
        }
    });
    let _ = task.await;
}

impl Drop for Row {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            self.state.telemetry.request_abandoned();
            return;
        };
        let row = self.row(
            CLIENT_CLOSED_STATUS,
            Some((
                CLIENT_DISCONNECTED.to_string(),
                "the client disconnected while its documents were converted; the tools still \
                 running were stopped"
                    .to_string(),
            )),
        );
        let state = self.state.clone();
        let record = self.record.take();
        rt.spawn(async move {
            write(&state, row, CLIENT_CLOSED_STATUS, record).await;
        });
    }
}

#[cfg(test)]
mod tests;
