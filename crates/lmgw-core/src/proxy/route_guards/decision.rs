//! The decision kind's guards (decision-models design §2.2), both run by the
//! gate before admission, so no container starts for a request it cannot
//! answer: what `/v1/systemone` refuses ([`decision_route_guard`],
//! [`crate::gate::RouteCheck::Decision`]), and the reverse — a decision row
//! on a generation, embedding or rerank route ([`refuse_decision_row`]).

use crate::config::{AuxKind, AuxModel, Protocol, Route, Snapshot, UpstreamKind};
use crate::error::GatewayError;
use crate::state::SharedState;

/// The route a decision model answers on.
const SYSTEMONE: &str = "/v1/systemone";

/// A refusal of §2.2: **501** `not_supported_error`, the status llama.cpp
/// answers the same refusal with (Appendix A.7), in lmgw's code. Its status
/// is its own because the handler only sees the [`GatewayError`] the gate
/// hands back, and [`GatewayError::Unsupported`] is a 400.
fn not_supported(message: String) -> GatewayError {
    GatewayError::Refused {
        status: 501,
        code: "not_supported",
        message,
    }
}

/// The route check of `/v1/systemone` ([`crate::gate::RouteCheck::Decision`]):
/// can the route the gate settled on answer System One?
///
/// - A **local aux row** must be of kind `decision`; an embedder or reranker
///   is refused by name and kind, in llama.cpp's words ("not a decision
///   model") — the mirror of [`super::refuse_embedder_for_rerank`].
/// - Any **other local row** (chat, audio, image) is refused by name: none is
///   a decision model once the header classifies them (§3.2).
/// - A **cloud route** on an `openai` or `llama_cpp` upstream goes through,
///   as `/v1/rerank`'s does (§4.1), unless the upstream's catalog states
///   output modalities for the model and `decisions` is not among them. The
///   raw list is read, as [`super::image_route_guard`]'s cloud half reads it,
///   never the parsed task: a stated list says what the model is, and no
///   list means unknown, which forwards. An `anthropic` or `gemini` upstream
///   has no such route, and neither has an owner's own sd-server or
///   audio.cpp.
///
/// The gate runs it again on a fallback it swaps to (§2.4), so a fallback
/// that changed since it was set fails here by name.
pub(crate) async fn decision_route_guard(
    state: &SharedState,
    snap: &Snapshot,
    route: &Route,
    alias: &str,
) -> Result<(), GatewayError> {
    if let Some(m) = snap.aux_model_for(route) {
        return match m.kind {
            AuxKind::Decision => Ok(()),
            AuxKind::Embed | AuxKind::Rerank => Err(not_supported(format!(
                "model '{alias}' is not a decision model: it is the {} model '{}', served on \
                 {}. {SYSTEMONE} answers aux rows of kind decision (a GGUF whose header \
                 declares decision.type).",
                m.kind.as_str(),
                m.model_id,
                m.kind.endpoint()
            ))),
        };
    }
    if let Some(target) = crate::vram::classify(route) {
        return Err(not_supported(format!(
            "model '{alias}' is the {} model '{}', not a decision model. {SYSTEMONE} answers \
             aux rows of kind decision (a GGUF whose header declares decision.type) and models \
             of openai or llama_cpp upstreams.",
            target.class, target.model_id
        )));
    }
    match route.upstream.protocol {
        Protocol::Openai | Protocol::LlamaCpp => {}
        Protocol::Anthropic | Protocol::Gemini => {
            return Err(not_supported(format!(
                "'{alias}' resolves to '{}' on {} upstream '{}', which has no System One \
                 route; {SYSTEMONE} forwards to openai and llama_cpp upstreams",
                route.upstream_model,
                route.upstream.protocol.as_str(),
                route.upstream.name
            )))
        }
    }
    match route.upstream.kind {
        UpstreamKind::Generic | UpstreamKind::LlamaServer => {}
        UpstreamKind::AudioCpp | UpstreamKind::SdCpp => {
            return Err(not_supported(format!(
                "'{alias}' resolves to '{}' on upstream '{}', an {} server, which has no \
                 System One route",
                route.upstream_model,
                route.upstream.name,
                route.upstream.kind.as_str()
            )))
        }
    }
    cloud_decision_guard(state, route, alias).await
}

/// The cloud half of [`decision_route_guard`]: refuse what the upstream's
/// catalog positively says is no decision model. A catalog that cannot be
/// read, that does not list the model, or that states no output modalities
/// for it forwards the request; the upstream's own 404 covers the rest.
///
/// The cloud refusals say what `alias` *resolves to*, because under the hold
/// that is its fallback (§2.4): the requested decision row is fine, the
/// route the gate swapped to is not.
async fn cloud_decision_guard(
    state: &SharedState,
    route: &Route,
    alias: &str,
) -> Result<(), GatewayError> {
    let Some(info) = crate::catalog::upstream_models(state, &route.upstream)
        .await
        .ok()
        .and_then(|list| list.into_iter().find(|m| m.id == route.upstream_model))
    else {
        return Ok(());
    };
    match info.output_modalities {
        Some(out) if !out.iter().any(|m| m == "decisions") => Err(not_supported(format!(
            "'{alias}' resolves to '{}' on upstream '{}', which is not a decision model: its \
             catalog says it outputs {}, and {SYSTEMONE} needs one that outputs decisions",
            route.upstream_model,
            route.upstream.name,
            out.join(", ")
        ))),
        _ => Ok(()),
    }
}

/// The reverse guard (§2.2): a local **decision** row is refused by name on
/// `endpoint`, a generation, embedding or rerank route, before its container
/// starts. llama-server cannot answer there: chat is a 500 ("does not support
/// logits computation"), embeddings a 400, rerank a 501 (Appendix A.7).
/// The status is those routes' own, as [`super::refuse_media_route`]'s: a
/// `400` (`Unsupported`). llama.cpp publishes its label-logit types as
/// `decisions` only, and lmgw follows it. Every other row passes.
///
/// `endpoint` is `None` for an in-process caller that names no route — the
/// dashboard's chats, an agent run, MCP sampling — whose model was chosen as
/// a chat model's ([`crate::gate::RouteCheck::None`]).
pub(crate) fn refuse_decision_row(
    snap: &Snapshot,
    route: &Route,
    alias: &str,
    endpoint: Option<&str>,
) -> Result<(), GatewayError> {
    match snap.aux_model_for(route) {
        Some(m) if m.kind == AuxKind::Decision => Err(decision_row_refusal(m, alias, endpoint)),
        _ => Ok(()),
    }
}

/// [`refuse_decision_row`]'s refusal, for the aux section guards that already
/// hold the row.
pub(crate) fn decision_row_refusal(
    m: &AuxModel,
    alias: &str,
    endpoint: Option<&str>,
) -> GatewayError {
    let lead = match endpoint {
        Some(endpoint) => format!(
            "model '{alias}' is the decision model '{}', and {endpoint} does not serve one — \
             send it to {SYSTEMONE}. A decision model cannot generate, embed or rerank.",
            m.model_id
        ),
        None => format!(
            "model '{alias}' is the decision model '{}', which cannot generate, embed or rerank: \
             it answers System One questions on {SYSTEMONE} only.",
            m.model_id
        ),
    };
    GatewayError::Unsupported(format!(
        "{lead} (Refused here so its container is not started for a request llama-server \
         cannot answer.)"
    ))
}
