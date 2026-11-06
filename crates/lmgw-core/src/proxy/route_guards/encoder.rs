//! A local **encoder** — an embedding or rerank row — on a route that
//! generates (`/v1/chat/completions`, `/v1/messages`, `/v1/responses`,
//! legacy `/v1/completions`, the in-process chat callers): refused by name
//! before its container starts, as a decision row is
//! ([`super::refuse_decision_row`]). llama-server in embedding or reranking
//! mode has no head to generate with.
//!
//! What a client names directly met llama-server's own error before; what
//! makes it a guard of its own is the chain (openai egress design §14.3): a
//! fallback of another type, which no save takes but a hand edit can leave,
//! ends the walk as one "this endpoint cannot serve" instead of starting an
//! encoder for a chat turn.

use crate::config::{AuxKind, Route, Snapshot};
use crate::error::GatewayError;

pub(crate) fn refuse_encoder_for_generation(
    snap: &Snapshot,
    route: &Route,
    alias: &str,
    endpoint: &str,
) -> Result<(), GatewayError> {
    let Some(m) = snap.aux_model_for(route) else {
        return Ok(());
    };
    let (what, answers) = match m.kind {
        AuxKind::Embed => ("embedding", "/v1/embeddings"),
        AuxKind::Rerank => ("rerank", "/v1/rerank"),
        // The decision guard's own refusal covers it.
        AuxKind::Decision => return Ok(()),
    };
    Err(GatewayError::Unsupported(format!(
        "model '{alias}' is the {what} model '{}', which cannot generate: {endpoint} needs a \
         model that does, and this one answers {answers} only. (Refused here so its container \
         is not started for a request it cannot answer.)",
        m.model_id
    )))
}
