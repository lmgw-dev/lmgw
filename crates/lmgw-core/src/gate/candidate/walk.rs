//! The walk over a candidate alias's list (candidate-aliases design
//! §4.2–§4.3; §12 entries 45–48) — see [`super`]'s module doc for the order
//! and why each half of the gate does what it does.

use std::sync::Arc;

use super::{CandidateCtx, CandidateReq};
use crate::bench::lease::GpuBlock;
use crate::candidates::derive::{cached_pick, derive, fallback_supports, CandidatePick};
use crate::config::{CandidateAlias, FallbackRoute, Route, Snapshot};
use crate::error::GatewayError;
use crate::gate::open::{
    admit_route, fallback_serves, AdmissionPolicy, AtAdmission, FallbackReason, GateHeaders,
    OpenFailed, Opened, RouteCheck,
};
use crate::runtime::registry::Origin;
use crate::state::SharedState;
use crate::vram::{self, BackgroundStart, LocalHold, Restart};

/// What [`at_resolve`] settled on: the route the rest of `resolve` checks,
/// the headers so far, and what admission needs to finish the pick.
pub(crate) struct AtResolve {
    pub(crate) route: Route,
    pub(crate) headers: GateHeaders,
    pub(crate) req: CandidateReq,
}

/// The candidate alias `ca`'s part of `resolve` (module doc of [`super`]).
///
/// - **The GPU hold:** the alias fallback, reason `hold`, for both alias
///   kinds (§4.2–§4.3 "Hold active") — never a candidate's row fallback
///   (§4.1). A fallback that is none, does not resolve, is local, or lacks a
///   facet the alias enables (§4.6 "treated as none") is `gpu_hold`, naming
///   why — the hold's refusal, logged `gpu_hold`, not a deferral. A
///   benchmark's lease is answered the same way, with reason `benchmark` and
///   `gpu_benchmark` (benchmark design §3.2).
/// - **Otherwise** a local chat route: the first routable candidate's, else
///   the primary's. Every candidate is a local chat model, so the site's
///   route check answers the same for each of them, and a site reading the
///   route before admission sees a local model, as it would for a direct
///   name. Which candidate answers is admission's decision ([`walk`]).
pub(crate) async fn at_resolve(
    state: &SharedState,
    snap: &Arc<Snapshot>,
    ca: &CandidateAlias,
) -> Result<AtResolve, GatewayError> {
    let pick = cached_pick(state, snap, ca).await;
    let req = CandidateReq {
        enabled: pick.enabled,
    };
    if let Some(block) = snap.gpu_block() {
        let (fb, route) = hold_fallback(state, snap, ca, &pick, &block).await?;
        let mut headers = GateHeaders::default();
        headers.fall_back(fb, block.fallback_reason());
        return Ok(AtResolve {
            route,
            headers,
            req,
        });
    }
    let id = pick
        .routable
        .first()
        .map(String::as_str)
        .or(ca.primary())
        .ok_or_else(|| {
            GatewayError::Internal(format!(
                "candidate alias '{}' lists no candidates",
                ca.alias
            ))
        })?;
    Ok(AtResolve {
        route: snap.chat_local_route(id),
        headers: GateHeaders::default(),
        req,
    })
}

/// The alias fallback under the GPU hold (or a benchmark's lease), or the
/// `gpu_hold` / `gpu_benchmark` refusal naming why there is none. The site's
/// route check is the caller's.
async fn hold_fallback(
    state: &SharedState,
    snap: &Snapshot,
    ca: &CandidateAlias,
    pick: &CandidatePick,
    block: &GpuBlock,
) -> Result<(String, Route), GatewayError> {
    let refused = |detail: String| block.refusal(ca.alias.clone(), detail);
    match snap.alias_fallback(ca) {
        FallbackRoute::None => Err(refused(String::new())),
        FallbackRoute::Unusable { alias, why } => {
            Err(refused(format!(" (fallback '{alias}' {why})")))
        }
        FallbackRoute::Usable { alias, route } => {
            match fallback_supports(state, &alias, pick.enabled).await {
                Ok(()) => Ok((alias, route)),
                Err(f) => Err(refused(format!(
                    " (fallback '{alias}' lacks {}, which the alias enables, so it counts as none)",
                    f.as_str()
                ))),
            }
        }
    }
}

/// Admission for a candidate alias: the walk (module doc of [`super`]).
///
/// `excluded` are candidates this request already found could not take it
/// ([`repick`]); `because` is why the last of them could not, for the
/// deferral message. The snapshot is read afresh — the alias may have been
/// edited since `resolve`, and the hold switched on.
///
/// `carried` is the alias as a re-pick's request was first picked
/// ([`CandidateCtx::row`]): when the alias has since been disabled or
/// deleted, the walk goes on with it, so the request keeps its alias's
/// candidates and fallback to the end (§12 entry 86). `None` for a
/// request's first walk, which needs the alias to exist now.
///
/// Every candidate hold it returns carries the request's policy with a
/// [`CandidateCtx`], so a send that finds the candidate unable to take the
/// request can run it again, and `x-lmgw-candidate` names the model.
pub(crate) async fn walk(
    state: &SharedState,
    alias: &str,
    carried: Option<Arc<CandidateAlias>>,
    check: RouteCheck,
    images: bool,
    excluded: Vec<String>,
    because: Option<String>,
) -> Result<Opened, OpenFailed> {
    let snap = state.snapshot();
    let (row, pick) = match (snap.candidate_alias(alias), carried) {
        (Some(ca), _) => (Arc::new(ca.clone()), cached_pick(state, &snap, ca).await),
        // Not in this snapshot, so not the pick cache's to keep: derived
        // once, for this walk only.
        (None, Some(row)) => {
            let pick = CandidatePick::from(&derive(state, &snap, &row).await);
            (row, pick)
        }
        (None, None) => return Err(refused(GatewayError::UnknownAlias(alias.to_string()))),
    };
    let w = Walk {
        state,
        snap: &snap,
        ca: &row,
        pick: &pick,
        alias,
        check,
        images,
        excluded,
    };
    if snap.gpu_block().is_some() {
        return w.held().await;
    }
    if row.background {
        w.guest(because).await
    } else {
        w.owner().await
    }
}

/// A send found the candidate `hold` is on unable to take the request
/// (`cause`, [`super::repicks`]): nothing reached the client, so the walk
/// runs again with that candidate excluded (§12 entry 45) — the next loaded
/// candidate, fit-checked by the send that follows the same way, then the
/// alias fallback. Bounded by the list: every pass excludes one more.
///
/// A hold that is not a candidate alias's has nothing to pick from, and
/// `cause` is the answer.
pub(crate) async fn repick(
    state: &SharedState,
    hold: &LocalHold,
    cause: GatewayError,
) -> Result<Opened, OpenFailed> {
    repick_as(state, hold, cause, false).await
}

/// [`repick`] for a guest's ladder climb that failed
/// ([`super::repicks_climb`], §12 entry 92): the walk goes on without the
/// model, as entry 74 answers a guest's primary that could not be brought
/// up — and when nothing else answers either (no other candidate loaded, no
/// fallback that can take the request), the climb's own error is the answer
/// rather than a deferral: it says more, and a rung no card could hold is a
/// configuration problem, not traffic declined on purpose.
pub(crate) async fn repick_failed_climb(
    state: &SharedState,
    hold: &LocalHold,
    cause: GatewayError,
) -> Result<Opened, OpenFailed> {
    repick_as(state, hold, cause, true).await
}

async fn repick_as(
    state: &SharedState,
    hold: &LocalHold,
    cause: GatewayError,
    own_error_last: bool,
) -> Result<Opened, OpenFailed> {
    let Some(policy) = hold.policy() else {
        return Err(refused(cause));
    };
    let Some(ctx) = policy.candidate.as_deref() else {
        return Err(refused(cause));
    };
    let model = hold.model_id();
    let why = skip_reason(&cause);
    if matches!(cause, GatewayError::VramTooLarge { .. }) {
        tracing::warn!(
            alias = %ctx.alias,
            model,
            "'{model}' can never climb to the rung this request for '{}' needs on this card \
             ({why}) — a configuration problem with the row; picking again without it",
            ctx.alias
        );
    } else {
        tracing::info!(
            alias = %ctx.alias,
            model,
            "'{model}' cannot take this request for '{}' ({why}) — picking again without it",
            ctx.alias
        );
    }
    let mut excluded = ctx.excluded.clone();
    if !excluded.iter().any(|m| m == model) {
        excluded.push(model.to_string());
    }
    let picked = walk(
        state,
        &ctx.alias,
        Some(Arc::clone(&ctx.row)),
        policy.check,
        policy.images,
        excluded,
        Some(format!("'{model}' {why}")),
    )
    .await;
    match picked {
        Err(OpenFailed {
            error: GatewayError::GpuDeferred { .. },
            ..
        }) if own_error_last => {
            let mut headers = GateHeaders::default();
            headers.chose(model.to_string());
            Err(OpenFailed {
                route: Some(state.snapshot().chat_local_route(model)),
                headers,
                error: cause,
            })
        }
        picked => picked,
    }
}

/// Why a candidate was excluded, as a clause after its name.
fn skip_reason(cause: &GatewayError) -> String {
    match cause {
        GatewayError::CandidateLost { detail, .. } => {
            format!("stopped under this request and is not restarted for it ({detail})")
        }
        GatewayError::ContextExceeded { .. } => format!("cannot hold this request ({cause})"),
        GatewayError::GpuDeferred { detail, .. } => format!("may not climb for it ({detail})"),
        other => format!("cannot take this request ({other})"),
    }
}

/// A refusal before any candidate was settled on: no route, no headers.
fn refused(error: GatewayError) -> OpenFailed {
    OpenFailed {
        route: None,
        headers: GateHeaders::default(),
        error,
    }
}

/// What one join came to.
///
/// `#[allow(large_enum_variant)]` like [`crate::gate::Sent`]: the admission
/// is the point of the answer, and there is one of these per candidate
/// tried.
#[allow(clippy::large_enum_variant)]
enum Joined {
    /// Loaded, and now claimed for this request.
    Got(Opened),
    /// Not loaded for this claim — the walk moves on.
    Not,
    /// It was coming up, and the start this claim joined did not: it failed,
    /// or a stop landed on it (§12 entries 62, 74). Not loaded, so the walk
    /// moves on — but for the primary this is also the answer the walk ends
    /// with when nothing else takes the request, and it is never started
    /// again by the same walk: that would pay the same failed load twice, or
    /// bring back a model somebody had just stopped.
    Failed(GatewayError),
    /// The GPU hold came on meanwhile: the alias fallback answers.
    Held,
}

/// One request's walk: everything the steps share.
struct Walk<'a> {
    state: &'a SharedState,
    snap: &'a Snapshot,
    /// The alias as this walk reads it — shared with every hold it hands
    /// out ([`CandidateCtx::row`]).
    ca: &'a Arc<CandidateAlias>,
    pick: &'a CandidatePick,
    alias: &'a str,
    check: RouteCheck,
    images: bool,
    excluded: Vec<String>,
}

impl Walk<'_> {
    /// A candidate this walk may route to: routable (§4.6) and not excluded
    /// by this request.
    fn usable(&self, id: &str) -> bool {
        self.pick.routable.iter().any(|c| c == id) && !self.excluded.iter().any(|c| c == id)
    }

    fn primary(&self) -> Option<&str> {
        self.ca.primary()
    }

    /// §4.2, without background: a loaded candidate in list order — the
    /// primary first — instead of evicting anything; else the primary
    /// through normal admission, which may evict and queue, and whose
    /// outside-VRAM verdict answers with the alias fallback (§4.7:
    /// `admit_route` looks the fallback up by the requested name, which is
    /// this alias's own). `vram_too_large` and `vram_queue_timeout` come back
    /// as for a direct request, naming the primary.
    ///
    /// A start of the primary this walk joined and saw fail, or be stopped,
    /// is not followed by a second start (§12 entries 62, 74): a loaded
    /// alternate still answers, and otherwise that start's error is the
    /// answer, as for a direct request that had joined it.
    async fn owner(&self) -> Result<Opened, OpenFailed> {
        let primary = self.primary();
        let mut primary_failed: Option<GatewayError> = None;
        for id in &self.pick.routable {
            if !self.usable(id) {
                continue;
            }
            // The owner's primary comes back through admission, as any model
            // they asked for; an alternate is never started by its alias.
            let is_primary = Some(id.as_str()) == primary;
            let restart = if is_primary {
                Restart::Admit
            } else {
                Restart::No
            };
            match self.join(id, Origin::Owner, restart).await {
                Joined::Got(opened) => return Ok(opened),
                Joined::Held => return self.held().await,
                Joined::Failed(e) if is_primary => primary_failed = Some(e),
                Joined::Failed(_) | Joined::Not => {}
            }
        }
        let Some(primary) = primary else {
            return Err(refused(GatewayError::Internal(format!(
                "candidate alias '{}' lists no candidates",
                self.alias
            ))));
        };
        if let Some(error) = primary_failed {
            return Err(self.primary_error(primary, error));
        }
        if let Some((p, reason)) = self
            .pick
            .primary_skipped
            .as_ref()
            .filter(|(p, _)| p == primary)
        {
            let detail = format!(
                "its primary '{p}' {}, and no other candidate is loaded (the alias only ever \
                 starts its primary)",
                reason.describe()
            );
            return self.unavailable(detail).await;
        }
        if !self.usable(primary) {
            // Only a re-pick excludes, and an owner's primary is restarted
            // by admission rather than lost — so this is a primary that
            // could not take the request after all.
            return Err(refused(GatewayError::CandidateUnavailable {
                alias: self.alias.to_string(),
                detail: format!(
                    "its primary '{primary}' cannot take this request, and no other candidate is \
                     loaded"
                ),
            }));
        }
        let mut headers = GateHeaders::default();
        headers.chose(primary.to_string());
        admit_route(
            self.state,
            self.snap.chat_local_route(primary),
            headers,
            self.alias,
            self.check,
            self.images,
            AtAdmission::MayFallBack,
            Some(self.ctx()),
        )
        .await
    }

    /// §4.3, background: the primary if loaded and not draining for the
    /// owner; else started into room that disturbs nobody (never waits,
    /// evicts only idle guests); else the first loaded, non-draining
    /// alternate; else the alias fallback (reason `background`); else `503
    /// gpu_hold`, logged `gpu_deferred`, saying what is in the way.
    ///
    /// A primary that cannot be brought up — its start fails, a start it
    /// joined fails or is stopped, or it cannot fit even on an empty card
    /// (`vram_too_large`, a configuration problem, logged as one) — is "not
    /// possible" in the owner's order, so the walk goes on to the loaded
    /// alternates and then the alias fallback (§12 entry 74). Only when no
    /// fallback can answer is that error the answer, named for the primary:
    /// it says more than "deferred" would.
    async fn guest(&self, because: Option<String>) -> Result<Opened, OpenFailed> {
        let mut blocked: Option<String> = None;
        let mut primary_failed: Option<OpenFailed> = None;
        if let Some(p) = self.primary().filter(|p| self.usable(p)) {
            match self.join(p, Origin::Background, Restart::Background).await {
                Joined::Got(opened) => return Ok(opened),
                Joined::Held => return self.held().await,
                Joined::Failed(e) => primary_failed = Some(self.primary_error(p, e)),
                Joined::Not => {}
            }
            if primary_failed.is_none() {
                let route = self.snap.chat_local_route(p);
                match vram::start_background(self.state, &route, self.alias).await {
                    Ok(BackgroundStart::Started(hold)) => return Ok(self.chosen(route, hold, p)),
                    Ok(BackgroundStart::Blocked(why)) => blocked = Some(why),
                    Err(GatewayError::GpuHold { .. } | GatewayError::GpuBenchmark { .. }) => {
                        return self.held().await
                    }
                    Err(error) => {
                        self.log_start_failure(p, &error);
                        primary_failed = Some(self.primary_error(p, error));
                    }
                }
            }
        }
        let primary = self.primary();
        for id in &self.pick.routable {
            if Some(id.as_str()) == primary || !self.usable(id) {
                continue;
            }
            match self.join(id, Origin::Background, Restart::No).await {
                Joined::Got(opened) => return Ok(opened),
                Joined::Held => return self.held().await,
                Joined::Failed(_) | Joined::Not => {}
            }
        }
        let detail = self.deferral(blocked, because, primary_failed.as_ref().map(|f| &f.error));
        match (
            self.fallback_answer(FallbackReason::Background).await,
            primary_failed,
        ) {
            (Ok(opened), _) => {
                tracing::info!(
                    alias = %self.alias,
                    "background traffic for '{}': {detail} — its fallback answers",
                    self.alias
                );
                Ok(opened)
            }
            (Err(_), Some(failed)) => Err(failed),
            (Err(why), None) => Err(refused(GatewayError::GpuDeferred {
                model: self.alias.to_string(),
                detail: format!("{detail}{why}"),
            })),
        }
    }

    /// An owner alias with nothing it may load (§12 entry 70): its primary
    /// cannot be used at all — `detail` says why — and no other candidate is
    /// loaded. the owner's order ends "otherwise use fallback", so the alias
    /// fallback answers (reason `unavailable`), and the drift is logged so it
    /// is fixed rather than silently lived with. With no fallback that can
    /// answer it is `candidate_unavailable`, naming the alias, the primary
    /// and why.
    async fn unavailable(&self, detail: String) -> Result<Opened, OpenFailed> {
        match self.fallback_answer(FallbackReason::Unavailable).await {
            Ok(opened) => {
                tracing::warn!(
                    alias = %self.alias,
                    "'{}' cannot use any of its models: {detail} — its fallback answers; fix the \
                     row or the alias",
                    self.alias
                );
                Ok(opened)
            }
            Err(why) => Err(refused(GatewayError::CandidateUnavailable {
                alias: self.alias.to_string(),
                detail: format!("{detail}{why}"),
            })),
        }
    }

    /// A guest's start of its primary failed: a warning either way, and a
    /// primary no card could hold named as the configuration problem it is.
    fn log_start_failure(&self, primary: &str, error: &GatewayError) {
        if matches!(error, GatewayError::VramTooLarge { .. }) {
            tracing::warn!(
                alias = %self.alias,
                "'{}': its primary '{primary}' can never be loaded on this card ({error}) — a \
                 configuration problem with the alias or the row; background traffic goes on to \
                 its loaded alternates and its fallback",
                self.alias
            );
        } else {
            tracing::warn!(
                alias = %self.alias,
                "'{}': starting its primary '{primary}' for background traffic failed ({error}) \
                 — trying its loaded alternates, then its fallback",
                self.alias
            );
        }
    }

    /// The refusal about the primary: its route and `x-lmgw-candidate`, as
    /// for an admission error about it.
    fn primary_error(&self, primary: &str, error: GatewayError) -> OpenFailed {
        let mut headers = GateHeaders::default();
        headers.chose(primary.to_string());
        OpenFailed {
            route: Some(self.snap.chat_local_route(primary)),
            headers,
            error,
        }
    }

    /// The alias fallback, answering for `reason` — or, as `Err`, why it
    /// cannot: empty for none, else a clause naming the fallback. A fallback
    /// that does not resolve, is local, or does not take this request (the
    /// site's route check, the enabled facets, §4.6) counts as none.
    async fn fallback_answer(&self, reason: FallbackReason) -> Result<Opened, String> {
        match self.snap.alias_fallback(self.ca) {
            FallbackRoute::None => Err(String::new()),
            FallbackRoute::Unusable { alias: fb, why } => Err(format!(" (fallback '{fb}' {why})")),
            FallbackRoute::Usable { alias: fb, route } => {
                let facets = Some(self.pick.enabled);
                if !fallback_serves(
                    self.state,
                    &fb,
                    &route,
                    self.alias,
                    self.check,
                    self.images,
                    facets,
                )
                .await
                {
                    return Err(format!(" (fallback '{fb}' cannot take this request)"));
                }
                let mut headers = GateHeaders::default();
                headers.fall_back(fb, reason);
                Ok(Opened {
                    route,
                    hold: None,
                    headers,
                })
            }
        }
    }

    /// Why nothing local took a guest's request, for the deferral message
    /// and the fallback's log line: what blocked the primary's start (it
    /// reads after "GPU in use by "), why its start failed, why this request
    /// excluded a candidate, or why the primary is not routable at all.
    fn deferral(
        &self,
        blocked: Option<String>,
        because: Option<String>,
        failed: Option<&GatewayError>,
    ) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(b) = blocked {
            parts.push(format!("GPU in use by {b}"));
        }
        if let (Some(e), Some(p)) = (failed, self.primary()) {
            parts.push(format!("its primary '{p}' could not be started ({e})"));
        }
        if let Some(w) = because {
            parts.push(w);
        }
        if let Some((p, reason)) = &self.pick.primary_skipped {
            parts.push(format!("its primary '{p}' {}", reason.describe()));
        }
        if parts.is_empty() {
            parts.push("its primary could not be loaded".to_string());
        }
        format!(
            "{}, and no other candidate of '{}' is loaded",
            parts.join("; "),
            self.alias
        )
    }

    /// The GPU hold (or a benchmark's lease) is on at admission (it came on
    /// after `resolve`, or while joining): the alias fallback, as
    /// [`at_resolve`] answers — judged by the site's check, since `resolve`
    /// judged a local route. What blocks is read now, since it may have come
    /// on after this walk's snapshot; the hold when it has gone again.
    async fn held(&self) -> Result<Opened, OpenFailed> {
        let block = self
            .state
            .snapshot()
            .gpu_block()
            .or_else(|| self.snap.gpu_block())
            .unwrap_or(GpuBlock::Hold);
        let (fb, route) = hold_fallback(self.state, self.snap, self.ca, self.pick, &block)
            .await
            .map_err(refused)?;
        let facets = Some(self.pick.enabled);
        if !fallback_serves(
            self.state, &fb, &route, self.alias, self.check, false, facets,
        )
        .await
        {
            return Err(refused(block.refusal(
                self.alias,
                format!(" (fallback '{fb}' cannot serve this endpoint)"),
            )));
        }
        let mut headers = GateHeaders::default();
        headers.fall_back(fb, block.fallback_reason());
        Ok(Opened {
            route,
            hold: None,
            headers,
        })
    }

    /// Claim `id` if it is loaded ([`vram::join`]), never starting it. A
    /// join that fails — a start it joined that did not come up, or that a
    /// stop landed on — is logged and counts as not loaded, as §4.1 "loaded"
    /// asks ([`Joined::Failed`]).
    async fn join(&self, id: &str, origin: Origin, restart: Restart) -> Joined {
        let route = self.snap.chat_local_route(id);
        match vram::join(self.state, &route, self.alias, origin, restart).await {
            Ok(Some(hold)) => Joined::Got(self.chosen(route, hold, id)),
            Ok(None) => Joined::Not,
            Err(GatewayError::GpuHold { .. } | GatewayError::GpuBenchmark { .. }) => Joined::Held,
            Err(e) => {
                tracing::warn!(
                    "'{}': candidate '{id}' was coming up and did not ({e}) — trying the next one",
                    self.alias
                );
                Joined::Failed(e)
            }
        }
    }

    /// The request is `id`'s: its route on the claimed container, the policy
    /// a later send or climb reads, and `x-lmgw-candidate`.
    fn chosen(&self, mut route: Route, mut hold: LocalHold, id: &str) -> Opened {
        route.upstream.base_url = hold.endpoint();
        hold.set_policy(AdmissionPolicy {
            check: self.check,
            images: self.images,
            candidate: Some(self.ctx()),
        });
        let mut headers = GateHeaders::default();
        headers.chose(id.to_string());
        Opened {
            route,
            hold: Some(hold),
            headers,
        }
    }

    fn ctx(&self) -> Arc<CandidateCtx> {
        Arc::new(CandidateCtx {
            alias: self.alias.to_string(),
            excluded: self.excluded.clone(),
            row: Arc::clone(self.ca),
        })
    }
}
