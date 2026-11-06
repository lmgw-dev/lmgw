//! A decision row's fallback must answer System One (decision-models design
//! §2.4): checked when it is set, by the same predicate the gate runs on
//! every request ([`crate::proxy::decision_route_guard`]), so a fallback that
//! cannot answer is refused at the save rather than at the first request it
//! answers.

use crate::config::{AuxKind, Snapshot};
use crate::state::SharedState;

/// Refuse `fallback` for a row of `kind` when it is a decision row whose
/// fallback resolves to a route `/v1/systemone` would refuse: above all a
/// cloud model whose catalog states output modalities without `decisions`.
/// A catalog that states none, or does not list the model, passes, as it
/// does on a request. Every other row, and a row without a fallback, passes
/// untouched.
///
/// Runs after [`crate::ops::fallback::validate`], which already refused a
/// fallback that does not resolve or is not a decision model.
pub(super) async fn check_decision_fallback(
    state: &SharedState,
    snap: &Snapshot,
    kind: AuxKind,
    fallback: Option<&str>,
) -> Result<(), String> {
    let (AuxKind::Decision, Some(name)) = (kind, fallback) else {
        return Ok(());
    };
    // A disabled target has no route now (openai egress design §14.3,
    // decision 28): `validate` saved it with its note, and typed it from its
    // row, so a decision row's disabled fallback is a decision row; the
    // request that reaches it while it is disabled goes on to its own
    // fallback, which the gate checks for System One then.
    if snap.disabled_key(name).is_some() {
        return Ok(());
    }
    let route = snap
        .resolve(name)
        .map_err(|e| format!("fallback: '{name}' does not resolve ({e})"))?;
    crate::proxy::decision_route_guard(state, snap, &route, name)
        .await
        .map_err(|e| {
            format!("fallback: a decision model's fallback must answer /v1/systemone, and {e}")
        })
}
