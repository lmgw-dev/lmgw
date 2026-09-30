//! **The candidate pick** (candidate-aliases design §4.1–§4.3, §4.6, §5
//! stage 2; §12 entries 45–48): which of a candidate alias's local models
//! answers a request — or its one fallback, or a refusal.
//!
//! A candidate alias is not a model name, so it is answered before the name
//! would be resolved, in both halves of the per-request gate
//! ([`super::open`]):
//!
//! - **At resolve** ([`walk::at_resolve`]): under the GPU hold the alias
//!   fallback answers at once (reason `hold`), or `gpu_hold`. Otherwise the
//!   route handed back is a **local chat route** — the first routable
//!   candidate's — so every site that reads the route between the halves
//!   (`/v1/responses`' native-or-synthesized choice, the chats) sees what it
//!   would for a local model. Nothing is claimed or started there; which
//!   candidate answers is admission's decision, like §4.7's swap.
//! - **At admission** ([`walk::walk`]), the owner's order, verbatim:
//!   - *without background:* "load primary and evict if not on hold, if
//!     candidate is loaded use that instead, otherwise use fallback" — the
//!     primary if loaded, else the first loaded alternate, else the primary
//!     through normal admission, whose outside-VRAM verdict answers with the
//!     alias fallback (§4.7); a primary that cannot be used at all (missing,
//!     disabled, lacking an enabled facet) with nothing else loaded is the
//!     alias fallback too, reason `unavailable` (§12 entry 70);
//!   - *background:* "load primary if possible, if any candidate is loaded
//!     use that, otherwise use fallback" — the primary if loaded and not
//!     draining for the owner, else started into room that disturbs nobody
//!     ([`crate::vram::start_background`]), else the first loaded
//!     alternate, else the alias fallback (reason `background`), else `503
//!     gpu_hold` logged `gpu_deferred` — or, when the primary's start failed
//!     or it can never fit, that error (§12 entry 74).
//!
//!   In both modes a primary whose joined start failed, or was stopped, is
//!   never started again by the same walk (§12 entry 62).
//!
//!   The alias only ever **starts its primary**; alternates are joined
//!   ([`crate::vram::join`]), never started, and come back never
//!   ([`crate::vram::Restart::No`]). Candidates the alias lists but may not
//!   route to (a row that lost an enabled facet, §4.6) are skipped. The
//!   candidates' own row fallbacks are never read — only the alias's.
//! - **At the send** ([`walk::repick`], entry 45): a candidate that turns out
//!   unable to take the request once it is known — a guest over a candidate's
//!   context (a guarded pool's per-request limit, llama-server's own refusal
//!   before any work, a ladder's top rung), a guest's climb that is denied
//!   or fails ([`repicks_climb`], §12 entry 92), or a candidate that stopped
//!   under the request and may not be restarted for it
//!   ([`GatewayError::CandidateLost`], both modes) — is excluded, and the
//!   walk runs again. Nothing had reached the client yet. The excluded
//!   list travels on the hold's [`crate::gate::AdmissionPolicy`], so the
//!   walk is bounded by the candidate list and needs no cap of its own.
//!
//! [`uses`] is the other half of §4.6's contract: which facets a request
//! uses, so one the alias does not enable is refused with a `400` before
//! anything starts ([`crate::gate::Routed::using`]).

mod uses;
mod walk;

pub use uses::{legacy_facets, request_facets};
pub(crate) use walk::{at_resolve, repick, repick_failed_climb, walk};

use std::sync::Arc;

use crate::candidates::FacetSet;
use crate::config::{CandidateAlias, FallbackRoute, Snapshot};
use crate::error::GatewayError;
use crate::runtime::registry::Origin;
use crate::vram::{LocalHold, Target};

/// What a candidate alias's request carries from `resolve` to admission
/// ([`crate::gate::Routed`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct CandidateReq {
    /// The alias's enabled facets — what [`crate::gate::Routed::using`]
    /// holds a request to.
    pub(crate) enabled: FacetSet,
}

/// What a candidate's hold remembers about the request it serves, on its
/// [`crate::gate::AdmissionPolicy`]: enough for a send that finds the
/// candidate unable to take the request to run the walk again without it
/// ([`walk::repick`], §12 entry 45).
///
/// It also carries the alias itself as it was when the request was picked
/// ([`Self::row`], §12 entry 86): a request that started on an alias keeps
/// that alias's fallback — and its enabled facets, which the fallback is
/// held to — even if the alias is disabled or deleted while the request is
/// in flight, instead of falling through to a candidate's row fallback,
/// which the alias never uses (§4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateCtx {
    /// The candidate alias as the client named it.
    pub alias: String,
    /// Candidates this request already found could not take it, in the order
    /// it found them. Each re-pick adds one, so the walk ends with the list.
    pub excluded: Vec<String>,
    /// The alias as the walk that picked this request read it.
    pub row: Arc<CandidateAlias>,
}

impl CandidateCtx {
    /// The alias this request is judged by: as it is now while it is still
    /// an enabled candidate alias — an edit made meanwhile applies — else as
    /// it was when the request was picked.
    pub fn alias_row<'s>(&'s self, snap: &'s Snapshot) -> &'s CandidateAlias {
        snap.candidate_alias(&self.alias).unwrap_or(&self.row)
    }

    /// The facets [`Self::alias_row`] enables — what its fallback must
    /// support (§4.6).
    pub fn enabled(&self, snap: &Snapshot) -> FacetSet {
        FacetSet::from_names(&self.alias_row(snap).capabilities_enabled).unwrap_or_default()
    }
}

/// The fallback of the request `hold` serves, wherever one is looked up
/// once the request holds a model (a climb's outside-VRAM verdict, the GPU
/// hold during a climb): a candidate alias's own ([`CandidateCtx::alias_row`]),
/// never its candidate's row fallback (§4.1) — even when the alias was
/// disabled or deleted meanwhile. Every other hold reads
/// [`Snapshot::request_fallback`], exactly as before.
pub(crate) fn request_fallback(snap: &Snapshot, hold: &LocalHold) -> FallbackRoute {
    match ctx(hold) {
        Some(c) => snap.alias_fallback(c.alias_row(snap)),
        None => snap.request_fallback(
            hold.alias(),
            &Target {
                class: hold.class(),
                model_id: hold.model_id().to_string(),
            },
        ),
    }
}

/// The facets the fallback of the request `hold` serves must support: a
/// candidate alias's enabled set, `None` for every other request
/// ([`crate::gate::open::fallback_serves`]).
pub(crate) fn fallback_facets(snap: &Snapshot, hold: &LocalHold) -> Option<FacetSet> {
    ctx(hold).map(|c| c.enabled(snap))
}

/// The `400` for a request that uses a facet the alias does not enable
/// (§4.6 "Requests", §9): named, and answered before anything starts.
pub(crate) fn facet_refusal(alias: &str, missing: FacetSet) -> GatewayError {
    let names: Vec<&str> = missing.iter().map(|f| f.label()).collect();
    GatewayError::Unsupported(format!(
        "'{alias}' does not enable {} — this request uses it, and a candidate alias only takes \
         requests every one of its models (and its fallback) is promised to serve; switch the \
         capability on for the alias, or send the request without it",
        names.join(", ")
    ))
}

/// The candidate context of the request `hold` serves, if it is a candidate
/// alias's.
fn ctx(hold: &LocalHold) -> Option<&CandidateCtx> {
    hold.policy().and_then(|p| p.candidate.as_deref())
}

/// Whether `error`, from a send on `hold`, means "this candidate cannot take
/// the request — pick again" (§12 entry 45) rather than the request's
/// answer:
/// - [`GatewayError::CandidateLost`], in both modes: the model stopped under
///   the request and is not restarted for it;
/// - [`GatewayError::ContextExceeded`] for a **guest**: a prompt above this
///   candidate's context (a guarded pool's per-request limit, a ladder's top
///   rung). An owner request gets that `400`, as a direct request would —
///   only a request above the alias's published context, the minimum of its
///   candidates, can meet it.
///
/// Never for a hold that is not a candidate alias's: direct names and plain
/// aliases are byte-for-byte unchanged (§7 item 14).
pub(crate) fn repicks(hold: Option<&LocalHold>, error: &GatewayError) -> bool {
    let Some(hold) = hold.filter(|h| ctx(h).is_some()) else {
        return false;
    };
    match error {
        GatewayError::CandidateLost { .. } => true,
        GatewayError::ContextExceeded { .. } => hold.origin() == Origin::Background,
        _ => false,
    }
}

/// Whether the request `hold` serves came through a candidate alias, so the
/// gate may send it elsewhere before anything is sent (a site that rewrites
/// its body for the fit keeps the client's copy for that).
pub(crate) fn reroutable(hold: Option<&LocalHold>) -> bool {
    hold.is_some_and(|h| ctx(h).is_some())
}

/// Whether a guest's ladder climb that ended in `error` is answered by
/// picking again ([`walk::repick_failed_climb`], §12 entry 92) rather than
/// with that error: the climb's own failures — a rung no card could hold
/// (`vram_too_large`, a configuration problem, logged as one), a running
/// rung that never went quiet (`LadderDrainTimeout`), and its 502s (a rung
/// that would not start, a joined climb whose rung failed, a stop that won
/// against the climb). The model could not take the request, as a guest's
/// primary that cannot be brought up could not (entry 74), so the walk goes
/// on without it. A denial is [`crate::vram::Climbed::Denied`], not an
/// error. Owner requests get the error, exactly as before.
pub(crate) fn repicks_climb(hold: &LocalHold, error: &GatewayError) -> bool {
    holds_back_refusals(Some(hold))
        && matches!(
            error,
            GatewayError::VramTooLarge { .. }
                | GatewayError::LadderDrainTimeout { .. }
                | GatewayError::Upstream { status: 502, .. }
        )
}

/// Whether a send on `hold` must hold back llama-server's context refusal
/// (`exceed_context_size_error`, §12 entry 12's backstop) so the gate can
/// pick again: a guest's candidate only (§12 entry 45) — on a ladder, the
/// backstop's second refusal, after the climb its first one made (entry
/// 92).
pub(crate) fn holds_back_refusals(hold: Option<&LocalHold>) -> bool {
    hold.is_some_and(|h| ctx(h).is_some() && h.origin() == Origin::Background)
}
