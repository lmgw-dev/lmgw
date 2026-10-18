//! A decision row's GPU-hold fallback must answer System One
//! (decision-models design §2.4): checked when it is set, by the same
//! predicate the gate runs on every request
//! ([`crate::proxy::decision_route_guard`]), so a fallback that cannot answer
//! is refused at the save rather than at the first held request.

use crate::config::{AuxKind, HoldFallbackMode, Snapshot};
use crate::state::SharedState;

/// Refuse `fallback` for a row of `kind` when it is a decision row whose
/// fallback (mode `alias`) resolves to a route `/v1/systemone` would refuse:
/// above all a cloud model whose catalog states output modalities without
/// `decisions`. A catalog that states none, or does not list the model,
/// passes, as it does on a request. Every other row, and a row without an
/// alias fallback, passes untouched.
///
/// Runs after [`crate::ops::validate_fallback_alias`], which already refused
/// a fallback that does not resolve or is itself local.
pub(super) async fn check_decision_fallback(
    state: &SharedState,
    snap: &Snapshot,
    kind: AuxKind,
    mode: HoldFallbackMode,
    fallback: Option<&str>,
) -> Result<(), String> {
    let (AuxKind::Decision, HoldFallbackMode::Alias, Some(alias)) = (kind, mode, fallback) else {
        return Ok(());
    };
    let route = snap
        .usable_fallback(alias)
        .map_err(|why| format!("hold_fallback: fallback alias '{alias}' {why}"))?;
    crate::proxy::decision_route_guard(state, snap, &route, alias)
        .await
        .map_err(|e| {
            format!("hold_fallback: a decision model's fallback must answer /v1/systemone, and {e}")
        })
}
