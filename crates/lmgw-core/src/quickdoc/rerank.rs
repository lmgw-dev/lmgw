//! The in-process [`Reranker`] (quickdoc §6, §9a): the retrieval pipeline's
//! cross-encoder stage, riding lmgw's own `/v1/rerank` path with no HTTP
//! loopback.
//!
//! The symmetry with [`super::embed::InProcessEmbedder`] is deliberate and is
//! the point: both bind to a resolved model, both enforce the model-kind gate
//! at construction *and* on every call (`proxy::rerank_in_process` repeats it),
//! and neither will quietly produce numbers from the wrong kind of section. An
//! embedding section asked to rank answers with its pooling output — a
//! plausible float that is not a relevance score — which would reorder every
//! answer a corpus gives with no symptom at all.

use std::sync::Arc;

use async_trait::async_trait;
use quickdoc_core::embed::Reranker;
use quickdoc_core::error::{QuickdocError, Result as QResult};

use crate::config::{AuxKind, Snapshot};
use crate::error::GatewayError;
use crate::proxy::{self, RequestCtx};
use crate::state::SharedState;
use crate::telemetry;

/// The reason clause [`super::embed::refuse_if_held`] puts in a rerank refusal.
const TRACE_PIN: &str =
    "a search trace names the model that ranked its answers, so quickdoc will not rank through \
     a fallback either";

pub struct InProcessReranker {
    state: SharedState,
    alias: String,
    /// The resolved `upstream/model` behind [`alias`](Self::alias), which is
    /// what the search trace names — an alias is not an identity.
    resolved: String,
    /// Whose request it ranks for, as the embedder's
    /// ([`super::embed::InProcessEmbedder::charged_to`]).
    caller: Option<RequestCtx>,
}

impl std::fmt::Debug for InProcessReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessReranker")
            .field("alias", &self.alias)
            .field("resolved", &self.resolved)
            .finish()
    }
}

impl InProcessReranker {
    /// Bind to `alias`, refusing it up front if it is not something that can
    /// rank.
    ///
    /// Plain `resolve`, never `resolve_for_request`: [`Self::model`] is what a
    /// search trace names as the ranker, and a GPU hold must not be able to
    /// quietly make that a different model (gpu-hold design §2/§4). The
    /// request path's fallback is refused where it would arrive, in
    /// [`Self::rerank`].
    pub fn for_alias(state: SharedState, alias: &str) -> Result<Self, GatewayError> {
        let snap = state.snapshot();
        let route = snap.resolve(alias)?;
        if let Some(m) = snap.aux_model_for(&route) {
            if m.kind == AuxKind::Embed {
                return Err(GatewayError::BadRequest(format!(
                    "'{alias}' resolves to the embedding model '{}' — it has no cross-encoder \
                     head and cannot rank. Configure a rerank model on the aux router, or clear \
                     the docs rerank model setting to run retrieval without the stage.",
                    m.model_id
                )));
            }
        }
        let resolved = format!("{}/{}", route.upstream.name, route.upstream_model);
        Ok(Self {
            state,
            alias: alias.to_string(),
            resolved,
            caller: None,
        })
    }

    /// Rank for `caller`'s request (client-apps design L4): each call checked
    /// against its key, its row the key's. `None` keeps the gateway's own.
    pub fn charged_to(mut self, caller: Option<RequestCtx>) -> Self {
        self.caller = caller;
        self
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }

    pub fn into_arc(self) -> Arc<dyn Reranker> {
        Arc::new(self)
    }
}

#[async_trait]
impl Reranker for InProcessReranker {
    fn model(&self) -> String {
        self.resolved.clone()
    }

    async fn rerank(&self, query: &str, documents: &[String]) -> QResult<Vec<f32>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        // No `top_n`: the pipeline needs a score for every candidate it sent,
        // because it maps the scores back onto its own fused order. Asking the
        // upstream to truncate would silently drop the tail instead.
        // A GPU hold re-routes `self.alias` to its row's fallback and answers
        // with that model's scores (gpu-hold design §2/§4). Accepting them
        // would reorder a corpus's answers with a cross-encoder the trace does
        // not name and the corpus never agreed to — the same silent-reordering
        // failure the embed/rerank kind gate exists to prevent, arriving
        // through a different door. Refused before the call and again on what
        // the call returned, exactly as the embedder does it.
        super::embed::refuse_if_held(&self.state, &self.alias, TRACE_PIN)
            .map_err(|e| QuickdocError::Reranker(e.to_string()))?;
        let key = super::embed::charge(&self.state, self.caller.as_ref(), &self.alias)
            .await
            .map_err(|e| QuickdocError::Reranker(e.to_string()))?;
        let (_, headers, resp) = proxy::rerank_once(
            &self.state,
            &self.alias,
            query,
            documents.to_vec(),
            None,
            telemetry::RERANK_PROTO,
            key,
        )
        .await
        .map_err(|(_, _, e)| QuickdocError::Reranker(format!("{}: {e}", self.alias)))?;
        if let Some(fb) = headers.fallback() {
            return Err(QuickdocError::Reranker(format!(
                "'{}' was answered by the GPU-hold fallback '{fb}' — quickdoc will not rank with \
                 a model other than the {} its trace names. Release the hold, or clear the docs \
                 rerank model to run retrieval without the stage.",
                self.alias, self.resolved
            )));
        }
        resp.in_request_order(documents.len())
            .ok_or(QuickdocError::RerankCount {
                want: documents.len(),
                got: resp.results.len(),
            })
    }
}

/// Which model the rerank stage should use, or why there is none.
///
/// `Err` is not a failure — it is the sentence the search trace prints in
/// `rerank_skipped`, so "no reranker" always comes with the reason it is
/// missing rather than as a silently weaker ranking.
pub fn rerank_alias(snap: &Snapshot) -> Result<String, String> {
    let configured = snap.settings.docs_rerank_model.trim();
    if !configured.is_empty() {
        return match snap.resolve(configured) {
            Ok(_) => Ok(configured.to_string()),
            Err(e) => Err(format!(
                "the configured docs rerank model '{configured}' does not resolve: {e}"
            )),
        };
    }
    // Nothing configured: use the aux router's rerank model when it has exactly
    // one, which is the ordinary single-reranker setup. Two of them is a choice
    // only the owner can make, so it is reported rather than guessed.
    let mut enabled: Vec<&crate::config::AuxModel> = snap
        .aux_models
        .iter()
        .filter(|m| m.enabled && m.kind == AuxKind::Rerank)
        .collect();
    enabled.sort_by(|a, b| a.model_id.cmp(&b.model_id));
    match enabled.as_slice() {
        [] => Err("no rerank model is enabled on the aux router".into()),
        [m] => Ok(snap.aux_public_name(&m.model_id)),
        many => Err(format!(
            "{} rerank models are enabled ({}) — pick one as the rerank model under Settings → \
             Docs",
            many.len(),
            many.iter()
                .map(|m| snap.aux_public_name(&m.model_id))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}
