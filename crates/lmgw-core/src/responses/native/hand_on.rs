//! A native forward's hop handed on (openai egress design §14.4): an
//! upstream remembered out of quota, an exhausted quota answered, a connect
//! that failed. With a fallback on the hop the forward ends in
//! [`Native::HandOn`] — carrying its own row when something was sent — and
//! the handler serves the fallback, writing that row once the walk handed
//! the request on; without one the error stands, a quota's note naming
//! where a fallback would be set and the chain it was tried along.

use std::time::{Duration, Instant};

use axum::http::StatusCode;

use super::{Forward, Native};
use crate::config::Route;
use crate::error::{GatewayError, UpstreamLimit};
use crate::gate::FallbackReason;
use crate::pricing::Quantities;
use crate::proxy::{self, RequestCtx};
use crate::state::SharedState;
use crate::telemetry::RESPONSES_PROTO;

/// A sent attempt's own row (§14.7), written by the handler once the
/// request was handed on ([`Self::write`]): lmgw's status for it, the
/// provider's words and where it went. No usage, no cost.
pub(in crate::responses) struct HandedRow {
    state: SharedState,
    ctx: RequestCtx,
    alias: String,
    route: Route,
    fallback: Option<FallbackReason>,
    started: Instant,
    streamed: bool,
    degraded: Option<String>,
    status: StatusCode,
    kind: String,
    words: String,
}

impl HandedRow {
    /// Write it: the request went to `to` for `reason`.
    pub(in crate::responses) async fn write(self, to: &str, reason: FallbackReason) {
        self.write_as(to, reason, false).await;
    }

    /// [`Self::write`]; `left`: its client left while the request was
    /// handed on ([`HeldRow`]).
    async fn write_as(self, to: &str, reason: FallbackReason, left: bool) {
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
            self.status.as_u16(),
            Some((
                self.kind,
                format!(
                    "{}; handed to fallback '{to}' ({}){}",
                    self.words,
                    reason.as_str(),
                    if left {
                        "; the client left before it answered"
                    } else {
                        ""
                    }
                ),
            )),
            self.degraded,
            &proxy::Reported::default(),
            Quantities::default(),
        )
        .await;
    }
}

/// A [`HandedRow`] held while the walk decides (§14.7): the walk admits,
/// evicts for and cold-starts a local fallback in the handler, and a client
/// that leaves meanwhile cancels it there. Dropped still holding the row,
/// it writes it in a task of its own, naming `to`, the fallback the hop
/// names, and that the client left. A walk that ends takes the row and
/// writes none: the hop's error is the request's one row.
pub(in crate::responses) struct HeldRow(Option<(Box<HandedRow>, String, FallbackReason)>);

impl HeldRow {
    pub(in crate::responses) fn arm(
        row: Option<Box<HandedRow>>,
        to: String,
        reason: FallbackReason,
    ) -> Self {
        Self(row.map(|r| (r, to, reason)))
    }

    /// The row, for the handler to write or not.
    pub(in crate::responses) fn take(&mut self) -> Option<Box<HandedRow>> {
        self.0.take().map(|(r, ..)| r)
    }
}

impl Drop for HeldRow {
    fn drop(&mut self) {
        if let Some((row, to, reason)) = self.0.take() {
            tokio::spawn(async move { row.write_as(&to, reason, true).await });
        }
    }
}

impl Forward {
    /// The re-check interval: the upstream's `Retry-After`, else the setting.
    fn interval(&self, retry_after: Option<Duration>) -> Duration {
        retry_after.unwrap_or_else(|| {
            Duration::from_secs(self.state.snapshot().settings.quota_recheck_seconds)
        })
    }

    /// A hop with a fallback on an upstream remembered out of quota: handed
    /// on with no send (§14.6). A due re-check is this forward's to take.
    pub(super) fn remembered(&self) -> Option<Native> {
        self.hop_fallback.as_ref()?;
        let id = self.route.upstream.id;
        if !self.state.upstream_quota.skips(id, self.interval(None)) {
            return None;
        }
        let e = self.state.upstream_quota.get(id)?;
        Some(Native::HandOn {
            error: GatewayError::UpstreamLimited {
                limit: UpstreamLimit::Quota,
                code: e.code.map(Into::into),
                error_type: Some("insufficient_quota".into()),
                message: e.message,
                hints: Default::default(),
                note: None,
            },
            reason: FallbackReason::Quota,
            row: None,
        })
    }

    /// The send failed before an answer: a connect that failed is
    /// `unreachable`, handed on when the hop has a fallback.
    pub(super) async fn send_failed(self, e: reqwest::Error) -> Native {
        if !(e.is_connect() && !e.is_timeout()) {
            return Native::answered(self.failed(GatewayError::from(e)).await);
        }
        let e = GatewayError::Unreachable(e.to_string());
        if self.hop_fallback.is_none() {
            let tried = self.tried.clone();
            return Native::answered(self.failed(e.with_clause(&tried)).await);
        }
        let row = self.handed(&e, StatusCode::BAD_GATEWAY);
        Native::HandOn {
            error: e,
            reason: FallbackReason::Unreachable,
            row: Some(row),
        }
    }

    /// The upstream refused with `status`, read as `e`: an exhausted quota
    /// is remembered and handed on when the hop has a fallback.
    pub(super) async fn refused(self, status: StatusCode, e: GatewayError) -> Native {
        let GatewayError::UpstreamLimited {
            limit: UpstreamLimit::Quota,
            code,
            message,
            hints,
            ..
        } = &e
        else {
            return Native::answered(self.failed(e).await);
        };
        let up = &self.route.upstream;
        self.state.upstream_quota.set(
            up.id,
            &up.name,
            code.as_deref(),
            message,
            self.interval(hints.retry_after()),
        );
        if self.hop_fallback.is_none() {
            // §14.8: where a fallback would be set, and the chain it was
            // tried along ("'C' has no fallback configured (tried: A → B → C)").
            let note = format!("'{}' has no fallback configured{}", self.hop, self.tried);
            return Native::answered(self.failed(e.with_clause(&note)).await);
        }
        let row = self.handed(&e, status);
        Native::HandOn {
            error: e,
            reason: FallbackReason::Quota,
            row: Some(row),
        }
    }

    /// The attempt's own row, for the handler to write once it handed the
    /// request on.
    fn handed(&self, e: &GatewayError, status: StatusCode) -> Box<HandedRow> {
        Box::new(HandedRow {
            state: self.state.clone(),
            ctx: self.ctx.clone(),
            alias: self.alias.clone(),
            route: self.route.clone(),
            fallback: self.fallback,
            started: self.started,
            streamed: self.streamed,
            degraded: self.degraded.clone(),
            status,
            kind: e.kind().to_string(),
            words: e.log_text(),
        })
    }
}
