//! The early refusal of a structured output the model is known not to
//! produce (structured-output design §7): the gate's stage between
//! [`super::Routed::using`] and admission, so nothing is started for a request
//! that would be refused.
//!
//! **Only when lmgw knows.** The model's published flag for the asked type —
//! `capabilities.structured_output.json_schema` or `.json_object` on
//! `GET /v1/models`, the owner's override merged in — must say `false`. An
//! absent flag is unknown, and the upstream decides. The override is the
//! escape hatch for a wrong `false`.
//!
//! **Only what is at hand** (§11.21). Every upstream catalog is read from the
//! cache ([`crate::catalog::cached_only`]), a stale entry too: a request never
//! waits on a provider's model list for this check. A catalog not read yet
//! (or a stale one) is read in the background for the next request, and this
//! one goes as unknown — the egress backstop and the upstream answer it. A
//! candidate alias reads its cached pick
//! ([`crate::candidates::derive::cached_pick`]), the one its walk reads
//! anyway, instead of deriving the alias again per request.

use crate::candidates::Facet;
use crate::config::{CandidateAlias, Snapshot};
use crate::error::GatewayError;
use crate::ir::FormatKind;
use crate::state::SharedState;

/// The refusal for asking `name` for `kind`, or `None` when its flag does not
/// say `false`. `name` is what would answer at this point: the hold's
/// fallback, else the name the client sent ([`super::Routed::serves_format`],
/// §11.7).
pub(crate) async fn refusal(
    state: &SharedState,
    name: &str,
    kind: FormatKind,
) -> Option<GatewayError> {
    let snap = state.snapshot();
    let flag = match snap.candidate_alias(name) {
        Some(ca) => candidate_flag(state, &snap, ca, kind).await,
        None => published_flag(state, name, kind).await,
    };
    (flag == Some(false)).then(|| unsupported(name, kind))
}

/// The flag `GET /v1/models` publishes for `name`, from cached catalogs only.
async fn published_flag(state: &SharedState, name: &str, kind: FormatKind) -> Option<bool> {
    let (entry, _) =
        crate::catalog::cached_only(crate::capabilities::exposed::exposed_entry(state, name)).await;
    let entry = entry?;
    let flags = entry.capabilities?.structured_output?;
    match kind {
        FormatKind::JsonSchema => flags.json_schema,
        FormatKind::JsonObject => flags.json_object,
    }
}

/// A candidate alias's flag, as its `/v1/models` entry publishes it
/// (`capabilities::exposed`'s candidate arm): `json_schema` is the enabled
/// facet, `json_object` what every routable candidate says — absent when
/// nothing is routable (§11.20).
async fn candidate_flag(
    state: &SharedState,
    snap: &std::sync::Arc<Snapshot>,
    ca: &CandidateAlias,
    kind: FormatKind,
) -> Option<bool> {
    let (pick, _) =
        crate::catalog::cached_only(crate::candidates::derive::cached_pick(state, snap, ca)).await;
    let on = pick.enabled.contains(Facet::StructuredOutput);
    match kind {
        FormatKind::JsonSchema => Some(on),
        FormatKind::JsonObject if on => pick.json_object,
        FormatKind::JsonObject => Some(false),
    }
}

#[cfg(test)]
mod tests;

/// [`refusal`]'s error: which model, which type, why lmgw knows, and the two
/// ways on.
pub(crate) fn unsupported(name: &str, kind: FormatKind) -> GatewayError {
    let k = kind.as_str();
    GatewayError::ResponseFormatUnsupported {
        message: format!(
            "'{name}' does not produce {k} structured output (its capabilities say \
             structured_output.{k} is false) — ask for another type or none; if that flag is \
             wrong, the model's capabilities override corrects it"
        ),
    }
}
