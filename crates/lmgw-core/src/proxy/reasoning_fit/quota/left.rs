//! The attempt's row of a hand-over its client left during (openai egress
//! design §14.7). The row of a sent attempt that was handed on is written
//! once the walk handed the request on, so that a walk that ends writes the
//! hop's one error row instead; but the walk admits, evicts for and
//! cold-starts a local fallback in the handler, and a client that leaves
//! meanwhile cancels the handler there. [`AttemptGuard`] holds the row
//! until the walk decided: dropped before that, it writes the row in a task
//! of its own, saying where the request was handed and that the client
//! left.

use crate::error::GatewayError;
use crate::gate::FallbackReason;
use crate::ingress::ClientProto;
use crate::proxy::{KeyRef, RequestCtx};
use crate::state::SharedState;

use super::super::send::RowAs;
use super::Attempt;

/// [`RowAs`] owned, for a row written after the handler is gone.
enum OwnedRows {
    Public {
        proto: ClientProto,
        ctx: Box<RequestCtx>,
        alias: String,
        fallback: Option<FallbackReason>,
    },
    InProcess {
        key: KeyRef,
        ingress_proto: String,
        alias: String,
        fallback: Option<FallbackReason>,
    },
}

impl OwnedRows {
    fn of(rows: &RowAs<'_>) -> Self {
        match rows {
            RowAs::Public {
                proto,
                ctx,
                alias,
                fallback,
            } => Self::Public {
                proto: *proto,
                ctx: Box::new((*ctx).clone()),
                alias: alias.to_string(),
                fallback: *fallback,
            },
            RowAs::InProcess {
                key,
                ingress_proto,
                alias,
                fallback,
            } => Self::InProcess {
                key: key.clone(),
                ingress_proto: ingress_proto.to_string(),
                alias: alias.to_string(),
                fallback: *fallback,
            },
        }
    }

    fn rows(&self) -> RowAs<'_> {
        match self {
            Self::Public {
                proto,
                ctx,
                alias,
                fallback,
            } => RowAs::Public {
                proto: *proto,
                ctx,
                alias,
                fallback: *fallback,
            },
            Self::InProcess {
                key,
                ingress_proto,
                alias,
                fallback,
            } => RowAs::InProcess {
                key: key.clone(),
                ingress_proto,
                alias,
                fallback: *fallback,
            },
        }
    }
}

/// A sent attempt's row, held while its request is handed on (module doc).
pub(super) struct AttemptGuard(Option<Armed>);

struct Armed {
    state: SharedState,
    rows: OwnedRows,
    attempt: Attempt,
    error: GatewayError,
    reason: FallbackReason,
    /// The fallback the hop names: where the request was being handed.
    to: String,
}

impl AttemptGuard {
    /// Hold `attempt`'s row while the request is handed from it to `to`
    /// for `reason`.
    pub(super) fn arm(
        state: &SharedState,
        rows: &RowAs<'_>,
        attempt: Attempt,
        error: &GatewayError,
        reason: FallbackReason,
        to: String,
    ) -> Self {
        Self(Some(Armed {
            state: state.clone(),
            rows: OwnedRows::of(rows),
            attempt,
            error: error.clone(),
            reason,
            to,
        }))
    }

    /// The walk decided: the attempt, for the caller to write or not.
    pub(super) fn disarm(mut self) -> Attempt {
        self.0
            .take()
            .map(|a| a.attempt)
            .expect("an armed guard is disarmed once")
    }
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        let Some(a) = self.0.take() else {
            return;
        };
        tokio::spawn(async move {
            super::attempt_row(
                &a.state,
                &a.rows.rows(),
                &a.attempt,
                &a.error,
                a.reason,
                Some(a.to),
                true,
            )
            .await;
        });
    }
}
