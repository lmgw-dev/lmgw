//! **The quota and an unreachable upstream at the chat send** (openai
//! egress design §14.4): every chat send — `/v1/chat/completions`,
//! `/v1/messages`, the synthesized Responses loop's turns and every
//! in-process caller — goes through [`super::send_chat`], which asks here
//! before and after a cloud send.
//!
//! - **Before** ([`before`]): an upstream remembered out of quota
//!   ([`crate::upstream_quota`]) hands a hop with a fallback on to it, with
//!   no send and no row — the route was resolved before the state was set
//!   (an in-process run keeps its route for the whole run), or the state's
//!   re-check is someone else's.
//! - **After** ([`after`]): a `429` is read whole and classified as the send
//!   site would; only an exhausted quota is handled — a rate limit and
//!   flex's `429` go back as they came (decision 23). The quota is
//!   remembered; with a fallback on the hop, the attempt's own row is
//!   written ("…; handed to fallback 'B' (quota)") and the fallback is
//!   opened as the hop it is ([`crate::gate::chain::fall_back`]), so the
//!   sites serve it as they serve a candidate's re-pick
//!   ([`crate::gate::Sent::Rerouted`]). Without one, the quota error stands
//!   with a note naming where a fallback would be set. A connect that
//!   failed ([`GatewayError::Unreachable`]) is handed on the same way,
//!   reason `unreachable`, with its own `502` row. A `2xx` clears the
//!   remembered state.
//!
//! Nothing is retried once a byte reached the client: both come before the
//! answer's first byte (the stream checks its status before it relays).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::config::{HopKey, Route};
use crate::error::{GatewayError, UpstreamLimit};
use crate::gate::chain::{self, Chain, Next, Walk};
use crate::gate::{FallbackReason, RouteCheck, Sent};
use crate::ir::ChatRequest;
use crate::runtime::registry::Origin;
use crate::state::SharedState;
use crate::vram::LocalHold;

use super::send::{record_refused, RowAs};

mod left;

/// What a chat send carries to hand a request on at the send: the site's
/// route check, whose traffic it is, and the chain it was handed along so
/// far ([`crate::gate::GateHeaders::chain`]).
#[derive(Debug, Clone)]
pub(crate) struct Handover {
    pub(crate) check: RouteCheck,
    pub(crate) origin: Origin,
    pub(crate) chain: Chain,
    /// `Some` for a send that runs detached from its client (the unary
    /// chat read on a cloud row, which outlives a hang-up so its row and
    /// budget are written): the hand-over is decided at the send but not
    /// made there — no fallback is admitted, evicted for or started by a
    /// task nobody waits for. The send ends in the hop's own error and
    /// leaves the hand-over here; the handler, which a hang-up cancels,
    /// opens it ([`open_deferred`]).
    pub(crate) deferred: Option<Deferred>,
}

impl Handover {
    /// The hand-over of a request lmgw sends for its own callers (the
    /// dashboard's Chat, its tools): a chat model chosen by its route
    /// already (`RouteCheck::None`), as the owner's, with no chain behind
    /// it. A run that carries a client's request carries that request's
    /// instead (a background `/v1/responses` run is a guest).
    pub(crate) fn in_process() -> Self {
        Self {
            check: RouteCheck::None,
            origin: Origin::Owner,
            chain: Chain::default(),
            deferred: None,
        }
    }

    /// The hand-over a detached send left ([`Self::deferred`]), taken.
    pub(crate) fn take_deferred(&self) -> Option<HandOn> {
        self.deferred.as_ref().and_then(Deferred::take)
    }
}

/// Where a detached send leaves the hand-over it decided ([`Handover::deferred`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct Deferred(std::sync::Arc<std::sync::Mutex<Option<HandOn>>>);

impl Deferred {
    fn put(&self, h: HandOn) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
    }

    fn take(&self) -> Option<HandOn> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// A hand-over decided at the send and not made yet: the hop that cannot
/// answer, why, the error it answers without its fallback, and — when the
/// attempt was sent — what its own row needs.
#[derive(Debug)]
pub(crate) struct HandOn {
    name: String,
    key: HopKey,
    reason: FallbackReason,
    error: GatewayError,
    attempt: Option<Attempt>,
}

impl HandOn {
    /// The hop that cannot answer, quoted: `'gpt-5'`.
    pub(crate) fn hop(&self) -> String {
        format!("'{}'", self.name)
    }
}

/// A sent attempt's row (§14.7), written once the hand-over is made.
#[derive(Debug)]
struct Attempt {
    route: Route,
    started: Instant,
    stream: bool,
}

/// The hop a send on `route` is for: the fallback it is, or the name the
/// request asked for — and its identity, when it has one that can carry a
/// fallback.
fn hop<'a>(
    state: &SharedState,
    route: &'a Route,
    rows: &'a RowAs<'_>,
) -> (&'a str, Option<HopKey>) {
    let name = route.fallback.as_deref().unwrap_or(rows.alias());
    (name, state.snapshot().hop_key(name))
}

/// Whether a send on `route` may be handed on for an upstream reason: a
/// cloud route, no local hold, a request that falls back on one.
fn eligible(hold: Option<&LocalHold>, route: &Route, hand: Option<&Handover>) -> bool {
    hold.is_none()
        && crate::vram::classify(route).is_none()
        && hand.is_some_and(|h| h.check.quota_falls_back())
}

/// The re-check interval: the upstream's `Retry-After`, else the setting.
fn interval(state: &SharedState, retry_after: Option<Duration>) -> Duration {
    retry_after
        .unwrap_or_else(|| Duration::from_secs(state.snapshot().settings.quota_recheck_seconds))
}

/// **Before the send** (module doc): `Some` when the upstream is remembered
/// out of quota and the hop has a fallback — the fallback's answer.
pub(super) async fn before(
    state: &SharedState,
    hold: Option<&LocalHold>,
    route: &Route,
    hand: Option<&Handover>,
    rows: &RowAs<'_>,
) -> Option<Result<Sent, GatewayError>> {
    let hand = hand.filter(|_| eligible(hold, route, hand))?;
    let (name, key) = hop(state, route, rows);
    let key = key?;
    let snap = state.snapshot();
    snap.fallback_of(&key)?;
    let id = route.upstream.id;
    if !state.upstream_quota.skips(id, interval(state, None)) {
        return None;
    }
    let e = state.upstream_quota.get(id)?;
    let remembered = GatewayError::UpstreamLimited {
        limit: UpstreamLimit::Quota,
        code: e.code.map(Into::into),
        error_type: Some("insufficient_quota".into()),
        message: e.message,
        hints: Default::default(),
        note: None,
    };
    if let Some(d) = &hand.deferred {
        d.put(HandOn {
            name: name.to_string(),
            key,
            reason: FallbackReason::Quota,
            error: remembered.clone(),
            attempt: None,
        });
        return Some(Err(remembered));
    }
    Some(
        hand_on(state, hand, name, key, FallbackReason::Quota, remembered)
            .await
            .map(|(sent, _)| sent),
    )
}

/// Make the hand-over a detached send left ([`Handover::deferred`]), in the
/// handler: the fallback's answer as the send would have given it, and the
/// sent attempt's own row once the fallback is decided — or the hop's
/// error with the clause that says why its fallback could not take it.
pub(crate) async fn open_deferred(
    state: &SharedState,
    hand: &Handover,
    h: HandOn,
    rows: RowAs<'_>,
) -> Result<Sent, GatewayError> {
    let to = configured(state, &h.key);
    let guard = h
        .attempt
        .map(|a| left::AttemptGuard::arm(state, &rows, a, &h.error, h.reason, to));
    let handed = hand_on(state, hand, &h.name, h.key, h.reason, h.error.clone()).await;
    let attempt = guard.map(left::AttemptGuard::disarm);
    let (sent, to) = handed?;
    if let Some(a) = attempt {
        attempt_row(state, &rows, &a, &h.error, h.reason, to, false).await;
    }
    Ok(sent)
}

/// The fallback the hop `key` names: where its request is being handed.
fn configured(state: &SharedState, key: &HopKey) -> String {
    state
        .snapshot()
        .fallback_of(key)
        .unwrap_or_default()
        .to_string()
}

/// The attempt's own row (§14.7): lmgw's status for it, the provider's
/// words and where it went. No usage, no cost: nothing was charged.
/// `left`: its client left while the request was handed on
/// (`left::AttemptGuard`).
async fn attempt_row(
    state: &SharedState,
    rows: &RowAs<'_>,
    a: &Attempt,
    error: &GatewayError,
    reason: FallbackReason,
    to: Option<String>,
    left: bool,
) {
    let shown = match rows.anthropic() {
        true => error.anthropic_status(),
        false => error.http_status(),
    };
    let to = to.unwrap_or_default();
    record_refused(
        state,
        rows.again(),
        &a.route,
        a.started,
        a.stream,
        shown.as_u16(),
        (
            error.kind(),
            format!(
                "{}; handed to fallback '{to}' ({}){}",
                error.log_text(),
                reason.as_str(),
                if left {
                    "; the client left before it answered"
                } else {
                    ""
                }
            ),
        ),
    )
    .await;
}

/// **After the send** (module doc): `sent` as it came, or the fallback's
/// answer, or the quota or connect error with its clause.
#[allow(clippy::too_many_arguments)]
pub(super) async fn after(
    state: &SharedState,
    hold: Option<&LocalHold>,
    route: &Route,
    hand: Option<&Handover>,
    rows: &RowAs<'_>,
    ir: &ChatRequest,
    started: Instant,
    stream: bool,
    prompt_sent: Option<&AtomicBool>,
    sent: Result<Sent, GatewayError>,
) -> Result<Sent, GatewayError> {
    if hold.is_some() || crate::vram::classify(route).is_some() {
        return sent;
    }
    // What `on_error` answers ran nothing upstream: no prompt to bill.
    let unsent = || {
        if let Some(p) = prompt_sent {
            p.store(false, Ordering::Relaxed);
        }
    };
    let up = &route.upstream;
    let resp = match sent {
        Err(e @ GatewayError::Unreachable(_)) => {
            unsent();
            return on_error(state, hold, route, hand, rows, started, stream, e, None).await;
        }
        Ok(Sent::Upstream(resp)) => resp,
        other => return other,
    };
    let status = resp.status();
    if status.is_success() {
        state
            .upstream_quota
            .clear(up.id, &up.name, "a request to it succeeded");
        return Ok(Sent::Upstream(resp));
    }
    if status.as_u16() != 429 {
        return Ok(Sent::Upstream(resp));
    }
    let (version, headers) = (resp.version(), resp.headers().clone());
    let bytes = resp.bytes().await.map_err(GatewayError::from)?;
    let refused = crate::egress::chat_error(
        crate::egress::for_protocol(up.protocol),
        429,
        &headers,
        &bytes,
        ir.format_ask(),
    );
    let GatewayError::UpstreamLimited {
        limit: UpstreamLimit::Quota,
        code,
        message,
        hints,
        ..
    } = &refused
    else {
        // A rate limit, flex's missing capacity, anything else: as it came.
        return Ok(Sent::Upstream(crate::gate::rebuild(
            status, version, headers, bytes,
        )));
    };
    state.upstream_quota.set(
        up.id,
        &up.name,
        code.as_deref(),
        message,
        interval(state, hints.retry_after()),
    );
    if !eligible(hold, route, hand) {
        return Ok(Sent::Upstream(crate::gate::rebuild(
            status, version, headers, bytes,
        )));
    }
    unsent();
    on_error(
        state,
        hold,
        route,
        hand,
        rows,
        started,
        stream,
        refused,
        Some(status.as_u16()),
    )
    .await
}

/// A quota or a connect error on the hop (module doc): its fallback's
/// answer, with the attempt's own row, or the error with its clause.
/// `status` is the quota's upstream status (`None` for a connect error).
#[allow(clippy::too_many_arguments)]
async fn on_error(
    state: &SharedState,
    hold: Option<&LocalHold>,
    route: &Route,
    hand: Option<&Handover>,
    rows: &RowAs<'_>,
    started: Instant,
    stream: bool,
    error: GatewayError,
    status: Option<u16>,
) -> Result<Sent, GatewayError> {
    let reason = match status {
        Some(_) => FallbackReason::Quota,
        None => FallbackReason::Unreachable,
    };
    let (name, key) = hop(state, route, rows);
    let tried = hand.map(|h| h.chain.tried()).unwrap_or_default();
    let (Some(hand), Some(key)) = (hand.filter(|_| eligible(hold, route, hand)), key) else {
        return Err(no_fallback(error, name, reason, &tried));
    };
    if state.snapshot().fallback_of(&key).is_none() {
        return Err(no_fallback(error, name, reason, &tried));
    }
    let attempt = Attempt {
        route: route.clone(),
        started,
        stream,
    };
    if let Some(d) = &hand.deferred {
        d.put(HandOn {
            name: name.to_string(),
            key,
            reason,
            error: error.clone(),
            attempt: Some(attempt),
        });
        return Err(error);
    }
    let to = configured(state, &key);
    let guard = left::AttemptGuard::arm(state, rows, attempt, &error, reason, to);
    let handed = hand_on(state, hand, name, key, reason, error.clone()).await;
    let attempt = guard.disarm();
    let (sent, answered_by) = handed?;
    attempt_row(state, rows, &attempt, &error, reason, answered_by, false).await;
    Ok(sent)
}

/// The error a hop without a fallback answers for `reason`: a quota names
/// where a fallback would be set (§14.8) — nothing about privacy, a model
/// without one is how privacy is chosen; both name the chain.
fn no_fallback(
    error: GatewayError,
    name: &str,
    reason: FallbackReason,
    tried: &str,
) -> GatewayError {
    match reason {
        FallbackReason::Quota => {
            error.with_clause(&format!("'{name}' has no fallback configured{tried}"))
        }
        _ => error.with_clause(tried),
    }
}

/// Hand the request on from the hop `name` (`key`) for `reason`: the
/// fallback's answer and the name of the hop it went to first, or `error`
/// with the clause that says why the fallback could not take it.
async fn hand_on(
    state: &SharedState,
    hand: &Handover,
    name: &str,
    key: HopKey,
    reason: FallbackReason,
    error: GatewayError,
) -> Result<(Sent, Option<String>), GatewayError> {
    let walk = Walk {
        check: hand.check,
        origin: hand.origin,
    };
    let next = state.snapshot().next_hop(&key);
    let mut tried = hand.chain.clone();
    tried.leave(name, key.clone());
    let first = |c: &Chain| {
        c.hops()
            .get(hand.chain.hops().len())
            .map(|(n, _)| n.clone())
    };
    match chain::fall_back(state, walk, hand.chain.clone(), (name, key), next, reason).await {
        Next::Opened(o) => {
            let to = first(o.headers.chain());
            Ok((Sent::Rerouted(Ok(o)), to))
        }
        Next::Failed(f) => {
            let to = first(f.headers.chain());
            Ok((Sent::Rerouted(Err(f)), to))
        }
        Next::Ended(clause) => Err(error.with_clause(&format!("{clause}{}", tried.tried()))),
    }
}
