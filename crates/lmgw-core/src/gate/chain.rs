//! **The chain** (openai egress design §14.3): a request that reaches a
//! fallback is served on it as if the client had named it, and the hop is
//! checked like any request — can it answer now? If it cannot, its own
//! fallback applies, and the reason is recorded for that hop. One walk for
//! every reason, replacing the one-hop lookups.
//!
//! - [`Chain`] is what a request carries along it: the hops it was handed
//!   to, each with its reason (`x-lmgw-fallback-chain`, [`Chain::header`]),
//!   and the identities it visited, so a loop the save check could not see
//!   (a hand edit, two writes racing) ends the walk instead of looping.
//! - [`walk_static`] is the part a snapshot decides alone, at resolve: a
//!   local hop on the GPU under the hold or a benchmark's lease, or a CPU
//!   row whose container still runs on the GPU under the hold, goes on to
//!   its fallback before anything is started.
//! - [`fall_back`] and [`take`] serve the rest, wherever a hop is found
//!   unable to answer later — at admission (VRAM outside lmgw, a guest
//!   deferred, the hold coming on) and at the send (a climb, a quota): the
//!   next hop is entered as a request naming it would be, a cloud hop
//!   answering at once and a local one admitted, with every rule above.
//!
//! A hop ends the walk ([`Next::Ended`]) when its fallback is none, does not
//! resolve, is a candidate alias, was already visited, or is one this
//! endpoint cannot use (the site's [`RouteCheck`]): the walk does not skip
//! past such a hop. The last hop that could not answer then gives its own
//! error, with a clause naming the chain.

use axum::http::HeaderValue;

use super::open::{admit_route, AtAdmission, FallbackReason, GateHeaders, OpenFailed, Opened};
use super::RouteCheck;
use crate::bench::lease::GpuBlock;
use crate::config::{already_tried, tried_clause, FallbackRoute, HopKey, Route, Snapshot};
use crate::error::GatewayError;
use crate::runtime::registry::Origin;
use crate::state::SharedState;

/// The hops a request was handed to, with each one's reason, and the
/// identities it visited (module doc).
///
/// Boxed and absent until the first fallback: it rides in every
/// [`GateHeaders`], which ride in the gate's refusal shapes, and
/// `clippy.toml` sizes those against `large-error-threshold`; a request no
/// fallback served (nearly all of them) carries one pointer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chain(Option<Box<Hops>>);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Hops {
    /// The name the walk started from: the name the request named.
    start: Option<String>,
    visited: Vec<HopKey>,
    hops: Vec<(String, FallbackReason)>,
}

impl Chain {
    fn inner(&mut self) -> &mut Hops {
        self.0.get_or_insert_with(Default::default)
    }

    /// Each hop after the one asked for, in order, with its reason.
    pub fn hops(&self) -> &[(String, FallbackReason)] {
        self.0.as_deref().map_or(&[], |h| h.hops.as_slice())
    }

    /// The hop that answers, and why it does.
    pub fn last(&self) -> Option<(&str, FallbackReason)> {
        self.hops().last().map(|(n, r)| (n.as_str(), *r))
    }

    /// The hop `name` (identity `key`) cannot answer: the walk goes on from
    /// it. The first such hop is the one the request named.
    pub(crate) fn leave(&mut self, name: &str, key: HopKey) {
        let h = self.inner();
        if h.start.is_none() {
            h.start = Some(name.to_string());
        }
        if !h.visited.contains(&key) {
            h.visited.push(key);
        }
    }

    /// Whether the walk was at `key` already.
    pub(crate) fn seen(&self, key: &HopKey) -> bool {
        self.0.as_deref().is_some_and(|h| h.visited.contains(key))
    }

    /// The request is handed to `name`, for `reason`.
    pub(crate) fn push(&mut self, name: String, key: HopKey, reason: FallbackReason) {
        let h = self.inner();
        if !h.visited.contains(&key) {
            h.visited.push(key);
        }
        h.hops.push((name, reason));
    }

    fn names(&self) -> Vec<String> {
        self.hops().iter().map(|(n, _)| n.clone()).collect()
    }

    fn start(&self) -> &str {
        self.0
            .as_deref()
            .and_then(|h| h.start.as_deref())
            .unwrap_or_default()
    }

    /// " (tried: A → B → C)", or nothing before the first fallback.
    pub(crate) fn tried(&self) -> String {
        tried_clause(self.start(), &self.names())
    }

    /// The clause of `fb`, met again: the request answers an error and
    /// never loops.
    fn already(&self, fb: &str) -> String {
        already_tried(fb, self.start(), &self.names())
    }

    /// `x-lmgw-fallback-chain` (§14.5): each hop as an RFC 8941 list member,
    /// `"B";reason=quota, "C";reason=hold`. A name with a byte outside
    /// 0x20–0x7E cannot be an sf-string; its member is the reason alone
    /// (`"";reason=quota`), so the list keeps its length. `None` before
    /// the first fallback.
    pub fn header(&self) -> Option<HeaderValue> {
        if self.hops().is_empty() {
            return None;
        }
        HeaderValue::from_str(&sf_list(self.hops())).ok()
    }
}

/// The hops as an RFC 8941 list (sf-strings with a `reason` token).
pub fn sf_list(hops: &[(String, FallbackReason)]) -> String {
    hops.iter()
        .map(|(name, reason)| {
            let printable = name.bytes().all(|b| (0x20..=0x7e).contains(&b));
            let s = if printable {
                name.replace('\\', "\\\\").replace('"', "\\\"")
            } else {
                String::new()
            };
            format!("\"{s}\";reason={}", reason.as_str())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What the request carries to every hop: the site's route check and whose
/// traffic it is. Policy (scope and budget) is not checked again per hop
/// (§14.9).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Walk {
    pub(crate) check: RouteCheck,
    pub(crate) origin: Origin,
}

/// Where a walk from a hop that cannot answer went.
// One per walk, matched at once.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Next {
    /// A hop answers (a cloud hop), or was admitted (a local one).
    Opened(Opened),
    /// A later hop could not answer either and gave its own error, the
    /// chain named in it.
    Failed(OpenFailed),
    /// The fallback could not be tried: `""` for none, else the clause that
    /// says why, for the hop that falls back to give with its own error.
    Ended(String),
}

/// Why a hop cannot answer now, by what the snapshot says (§14.3, `hold`,
/// `benchmark`, `disabled`): the reason its fallback answers for, and the
/// error it gives without one.
pub(crate) struct Stop {
    pub(crate) reason: FallbackReason,
    pub(crate) key: HopKey,
    kind: StopKind,
}

enum StopKind {
    Block(GpuBlock),
    /// A row switched to the CPU while its container still runs on the GPU
    /// under the hold: its own refusal (`vram::gpu_container_under_hold`).
    HeldContainer(String),
    /// The name the request named is a disabled row or alias (§14.3,
    /// decision 28): today's answer for it.
    Disabled(GatewayError),
    /// A cloud hop whose upstream is remembered out of quota (§14.6): the
    /// quota error it would answer, from what was remembered.
    Quota(crate::upstream_quota::Exhausted),
    /// A GPU hop the last machine check says this computer cannot start
    /// (machine-check design §2.5, [`super::machine_setup::stop`]): that
    /// item's plain sentence, with what to do.
    GpuSetup(String),
}

impl Stop {
    /// A block that holds the card for `key` (a candidate alias under the
    /// hold, whose candidates all are).
    pub(crate) fn block(block: GpuBlock, key: HopKey) -> Self {
        Self {
            reason: block.fallback_reason(),
            key,
            kind: StopKind::Block(block),
        }
    }

    /// The disabled model or alias `key` that a request named, whose
    /// answer without a fallback is `error` (today's: unknown model, or the
    /// alias's upstream error).
    pub(crate) fn disabled(error: GatewayError, key: HopKey) -> Self {
        Self {
            reason: FallbackReason::Disabled,
            key,
            kind: StopKind::Disabled(error),
        }
    }

    /// The GPU hop `key` the last machine check says cannot start
    /// (`problem`, its plain sentence): its fallback answers for
    /// `gpu_setup`.
    pub(crate) fn gpu_setup(problem: String, key: HopKey) -> Self {
        Self {
            reason: FallbackReason::GpuSetup,
            key,
            kind: StopKind::GpuSetup(problem),
        }
    }

    /// Whether the hop itself is tried when its fallback cannot answer
    /// (none, unusable, met already, or not for this endpoint), rather than
    /// its refusal given: only a machine check's verdict, which may be out
    /// of date — a start that fails checks again (machine-check design
    /// §2.5). Every other stop is a fact of the snapshot.
    pub(crate) fn tries_hop(&self) -> bool {
        matches!(self.kind, StopKind::GpuSetup(_))
    }

    /// The error the hop `name` gives when no fallback answers for it;
    /// `clause` says why its fallback did not.
    pub(crate) fn refusal(self, name: &str, clause: String) -> GatewayError {
        match self.kind {
            StopKind::GpuSetup(problem) => GatewayError::GpuSetup {
                model: name.to_string(),
                problem,
                detail: clause,
            },
            StopKind::Block(b) => b.refusal(name, clause),
            StopKind::HeldContainer(detail) => GatewayError::GpuHold {
                model: name.to_string(),
                detail: format!("{detail}{clause}"),
            },
            StopKind::Disabled(e) => e.with_clause(&clause),
            StopKind::Quota(e) => GatewayError::UpstreamLimited {
                limit: crate::error::UpstreamLimit::Quota,
                code: e.code.map(Into::into),
                error_type: Some("insufficient_quota".into()),
                message: e.message,
                hints: Default::default(),
                note: None,
            }
            .with_clause(&clause),
        }
    }
}

/// `next` with the disabled hops at its front passed (§14.3, `disabled`,
/// [`Snapshot::past_disabled`]): the request is handed to each on `reason`
/// and, as it cannot answer, on to its own fallback with reason
/// `disabled`. The first hop that is not disabled, and the reason it is
/// entered on.
pub(crate) fn past_disabled(
    snap: &Snapshot,
    chain: &mut Chain,
    next: FallbackRoute,
    reason: FallbackReason,
) -> (FallbackRoute, FallbackReason) {
    let (passed, next) = snap.past_disabled(next, |k| chain.seen(k));
    let mut reason = reason;
    for (name, key) in passed {
        chain.push(name, key, reason);
        reason = FallbackReason::Disabled;
    }
    (next, reason)
}

/// The static stop of the hop `key` (whose route is `route`), if any
/// (module doc): a local hop held, a CPU row's GPU container under the
/// hold, a cloud hop with a fallback whose upstream is remembered out of
/// quota (§14.6) on a request whose `check` falls back on a quota, or — for
/// a caller that may fall back (`at`) — a GPU hop with a fallback the last
/// machine check says this computer cannot start (machine-check design
/// §2.5, [`super::machine_setup::stop`]). A pinned caller is never swapped
/// for that verdict (D97): it tries the start, whose failure answers it
/// `gpu_setup`, rather than a fallback it would refuse.
pub(super) fn stop_at(
    state: &SharedState,
    snap: &Snapshot,
    route: &Route,
    key: &HopKey,
    check: RouteCheck,
    at: AtAdmission,
) -> Option<Stop> {
    if let Some((block, key)) = snap.blocked_at_resolve(route) {
        return Some(Stop::block(block, key));
    }
    let Some(target) = crate::vram::classify(route) else {
        return quota_stop(state, snap, route, key, check);
    };
    match crate::vram::gpu_container_under_hold(state, snap, &target) {
        Some(GatewayError::GpuHold { detail, .. }) => Some(Stop {
            reason: FallbackReason::Hold,
            key: HopKey::Local(target.class, target.model_id),
            kind: StopKind::HeldContainer(detail),
        }),
        Some(_) => None,
        None if at == AtAdmission::MayFallBack => super::machine_setup::stop(state, snap, &target),
        None => None,
    }
}

/// A cloud hop skipped for its upstream's remembered quota (§14.6): only
/// with a fallback to skip to — without one the request goes to the
/// upstream, so its client gets the provider's own answer and the memory
/// learns when the credit is back.
fn quota_stop(
    state: &SharedState,
    snap: &Snapshot,
    route: &Route,
    key: &HopKey,
    check: RouteCheck,
) -> Option<Stop> {
    if !check.quota_falls_back() || snap.fallback_of(key).is_none() {
        return None;
    }
    // A look that takes nothing: a re-check that is due is the send's to
    // take (`QuotaMemory::skips`, at the chat send and the native forward).
    let id = route.upstream.id;
    if !state.upstream_quota.holds(id) {
        return None;
    }
    Some(Stop {
        reason: FallbackReason::Quota,
        key: key.clone(),
        kind: StopKind::Quota(state.upstream_quota.get(id)?),
    })
}

/// May `route` (the hop `name`) take this request: the site's check, judged
/// as if the client had named it. `Err` is the clause that says why not —
/// the check's own refusal, so the reader learns what the hop lacks. Cheap
/// for every check but the image routes', which read the hop upstream's
/// catalog.
pub(crate) async fn serves(
    state: &SharedState,
    route: &Route,
    name: &str,
    check: RouteCheck,
) -> Result<(), String> {
    match super::open::fallback_refusal(state, name, route, name, check).await {
        None => Ok(()),
        Some(why) => Err(format!(
            " (fallback '{name}' cannot serve this endpoint: {})",
            why.trim_end_matches('.')
        )),
    }
}

/// **The static walk, at resolve** (module doc): from the hop `from` that
/// cannot answer for `stop`, along each hop's own fallback, while the hop it
/// reaches cannot answer either for a reason the snapshot decides. `Ok` is
/// the first hop that can be tried — its name and route — and `chain` the
/// hops on the way; `Err` the error the last hop gives, the chain named.
/// Nothing is started.
///
/// `from_route` is `from`'s own route, when it has one (not a disabled
/// name): a stop that lets its hop be tried when its fallback cannot answer
/// ([`Stop::tries_hop`]) ends the walk at that hop, with the chain as it
/// stood on reaching it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn walk_static(
    state: &SharedState,
    snap: &Snapshot,
    check: RouteCheck,
    chain: &mut Chain,
    from: String,
    from_route: Option<Route>,
    stop: Stop,
    next: FallbackRoute,
) -> Result<(String, Route), GatewayError> {
    let (mut name, mut here, mut stop, mut next) = (from, from_route, stop, next);
    loop {
        // The chain on reaching this hop, for a stop that lets its hop be
        // tried.
        let reached = stop.tries_hop().then(|| chain.clone());
        // The fallback cannot answer: the hop's refusal, or the hop itself
        // for a stop that lets it be tried.
        let ended = |stop: Stop,
                     name: String,
                     here: Option<Route>,
                     chain: &mut Chain,
                     clause: String|
         -> Result<(String, Route), GatewayError> {
            match (here, &reached) {
                (Some(route), Some(reached)) => {
                    tracing::info!(
                        "'{name}': the last machine check says it cannot start on the graphics \
                         card, and its fallback cannot answer{clause} — trying it anyway"
                    );
                    *chain = reached.clone();
                    Ok((name, route))
                }
                _ => Err(stop.refusal(&name, clause)),
            }
        };
        chain.leave(&name, stop.key.clone());
        let (next_now, reason) = past_disabled(snap, chain, next, stop.reason);
        let (fb, key, route) = match next_now {
            FallbackRoute::None => {
                let clause = chain.tried();
                return ended(stop, name, here, chain, clause);
            }
            FallbackRoute::Unusable { alias, why, .. } => {
                let clause = format!(" (fallback '{alias}' {why}){}", chain.tried());
                return ended(stop, name, here, chain, clause);
            }
            FallbackRoute::Usable { alias, key, route } => (alias, key, route),
        };
        if chain.seen(&key) {
            let clause = chain.already(&fb);
            return ended(stop, name, here, chain, clause);
        }
        if let Err(clause) = serves(state, &route, &fb, check).await {
            let clause = format!("{clause}{}", chain.tried());
            return ended(stop, name, here, chain, clause);
        }
        let key_now = key.clone();
        chain.push(fb.clone(), key, reason);
        match stop_at(
            state,
            snap,
            &route,
            &key_now,
            check,
            AtAdmission::MayFallBack,
        ) {
            Some(s) => {
                next = snap.next_hop(&s.key);
                name = fb;
                here = Some(route);
                stop = s;
            }
            None => return Ok((fb, route)),
        }
    }
}

/// **A hop cannot answer for `reason`** (module doc): try its fallback
/// `next` — checked (visited, the site's route check), then entered
/// ([`take`]). `from` is the hop that falls back: its name and identity.
pub(crate) fn fall_back<'a>(
    state: &'a SharedState,
    walk: Walk,
    mut chain: Chain,
    from: (&'a str, HopKey),
    next: FallbackRoute,
    reason: FallbackReason,
) -> crate::erased::Erased<'a, Next> {
    futures::FutureExt::boxed(async move {
        chain.leave(from.0, from.1);
        let snap = state.snapshot();
        let (next, reason) = past_disabled(&snap, &mut chain, next, reason);
        let (name, key, route) = match next {
            FallbackRoute::None => return Next::Ended(String::new()),
            FallbackRoute::Unusable { alias, why, .. } => {
                return Next::Ended(format!(" (fallback '{alias}' {why})"))
            }
            FallbackRoute::Usable { alias, key, route } => (alias, key, route),
        };
        if chain.seen(&key) {
            return Next::Ended(chain.already(&name));
        }
        if let Err(clause) = serves(state, &route, &name, walk.check).await {
            return Next::Ended(clause);
        }
        take(state, walk, chain, name, key, route, reason).await
    })
}

/// Hand the request to the hop `name` (checked by the caller) for `reason`
/// and serve it there: a hop the snapshot stops goes on to its own
/// fallback; a cloud hop answers; a local one is admitted as a request
/// naming it would be — a guest's as a guest's — which may hand it on again
/// (VRAM outside lmgw, a deferral) or wait for room, as a request naming it
/// would.
pub(crate) fn take<'a>(
    state: &'a SharedState,
    walk: Walk,
    mut chain: Chain,
    name: String,
    key: HopKey,
    route: Route,
    reason: FallbackReason,
) -> crate::erased::Erased<'a, Next> {
    futures::FutureExt::boxed(async move {
        chain.push(name.clone(), key.clone(), reason);
        let snap = state.snapshot();
        if let Some(stop) = stop_at(
            state,
            &snap,
            &route,
            &key,
            walk.check,
            AtAdmission::MayFallBack,
        ) {
            let next = snap.next_hop(&stop.key);
            let tried = chain.clone();
            match fall_back(
                state,
                walk,
                chain.clone(),
                (&name, stop.key.clone()),
                next,
                stop.reason,
            )
            .await
            {
                // A stop that lets its hop be tried (a machine check's
                // verdict, `Stop::tries_hop`): admitted below, as a request
                // naming it would be.
                Next::Ended(clause) if stop.tries_hop() => tracing::info!(
                    "'{name}': the last machine check says it cannot start on the graphics card, \
                     and its fallback cannot answer{clause} — trying it anyway"
                ),
                // The hop that gave the error: its row is on its route
                // (review B6), not on the route the request left.
                Next::Ended(clause) => {
                    return Next::Failed(OpenFailed {
                        route: Some(Box::new(route)),
                        headers: GateHeaders::along(tried.clone()),
                        error: stop.refusal(&name, format!("{clause}{}", tried.tried())),
                    })
                }
                other => return other,
            }
        }
        let headers = GateHeaders::along(chain);
        if crate::vram::classify(&route).is_none() {
            return Next::Opened(Opened {
                route,
                hold: None,
                headers,
            });
        }
        let admitted = match walk.origin {
            Origin::Background => {
                super::guest::admit(
                    state,
                    route,
                    headers,
                    &name,
                    walk.check,
                    AtAdmission::MayFallBack,
                )
                .await
            }
            _ => {
                admit_route(
                    state,
                    route,
                    headers,
                    &name,
                    walk.check,
                    AtAdmission::MayFallBack,
                    None,
                )
                .await
            }
        };
        match admitted {
            Ok(opened) => Next::Opened(opened),
            Err(failed) => Next::Failed(failed),
        }
    })
}

/// A send found the model `hold` is on cannot take the request, and its
/// fallback `next` answers for `reason` instead (a climb that could not
/// happen, the hold coming on mid-request): the chain the hold was admitted
/// with goes on from it. `ended` is the error for a fallback that could not
/// be tried after all (the snapshot changed since the caller looked).
pub(crate) async fn from_hold(
    state: &SharedState,
    hold: &crate::vram::LocalHold,
    next: FallbackRoute,
    reason: FallbackReason,
    ended: impl FnOnce(String) -> GatewayError,
) -> Result<Opened, OpenFailed> {
    let (chain, check) = match hold.policy() {
        Some(p) => (p.chain.clone(), p.check),
        None => (Chain::default(), RouteCheck::None),
    };
    let key = match hold.policy().and_then(|p| p.candidate.as_deref()) {
        Some(c) => HopKey::Candidate(c.row.id),
        None => HopKey::Local(hold.class(), hold.model_id().to_string()),
    };
    let walk = Walk {
        check,
        origin: hold.origin(),
    };
    let tried = {
        let mut c = chain.clone();
        c.leave(hold.alias(), key.clone());
        c
    };
    // The request leaves this model and keeps its claim: a local fallback
    // that fits only once this model goes may evict it when that claim is
    // its only user (`vram::leaving`), and is not queued behind it.
    let walked = crate::vram::leaving(
        hold,
        fall_back(state, walk, chain, (hold.alias(), key), next, reason),
    );
    match walked.await {
        Next::Opened(o) => Ok(o),
        Next::Failed(f) => Err(f),
        Next::Ended(clause) => Err(OpenFailed {
            route: None,
            headers: GateHeaders::along(tried.clone()),
            error: ended(format!("{clause}{}", tried.tried())),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chain_header_is_an_rfc_8941_list_that_keeps_its_length() {
        let hops = vec![
            ("B".to_string(), FallbackReason::Hold),
            ("say \"hi\"\\".to_string(), FallbackReason::ExternalVram),
            ("modèle".to_string(), FallbackReason::Background),
        ];
        assert_eq!(
            sf_list(&hops),
            "\"B\";reason=hold, \"say \\\"hi\\\"\\\\\";reason=external_vram, \"\";reason=background"
        );
    }

    #[test]
    fn the_tried_clause_and_the_loop_clause_name_the_chain() {
        let mut c = Chain::default();
        assert_eq!(c.tried(), "");
        c.leave("A", HopKey::Alias(1));
        c.push("B".into(), HopKey::Alias(2), FallbackReason::Hold);
        c.push("C".into(), HopKey::Alias(3), FallbackReason::Hold);
        assert_eq!(c.tried(), " (tried: A → B → C)");
        assert!(c.seen(&HopKey::Alias(1)) && c.seen(&HopKey::Alias(3)));
        assert_eq!(
            c.already("A"),
            " (fallback 'A' was already tried in this request: A → B → C → A)"
        );
        assert_eq!(c.last(), Some(("C", FallbackReason::Hold)));
    }
}
