//! The in-process [`Embedder`] (quickdoc §3): quickdoc's vectors, produced by
//! lmgw's own embedding path with no HTTP loopback.
//!
//! Three things make this more than a convenience wrapper around
//! `proxy::embed_once`:
//!
//! * **The pin is enforced here.** A corpus stores the *resolved* upstream and
//!   model it was embedded with (§4). This binds to that pin, refuses to open
//!   if the alias now resolves somewhere else, and checks every returned vector
//!   against the pinned width. There is no truncate-or-pad path: a corpus whose
//!   vectors are the wrong shape is unsearchable, and the only honest answer is
//!   to say so before anything is written.
//! * **The rerank gate holds.** `/v1/embeddings` against a reranker section
//!   answers 200 with an all-zero vector (§9a, verified live). The refusal lives
//!   in `proxy::embed_in_process`, which this rides, *and* is repeated at
//!   construction so a misconfigured corpus fails when the job starts rather
//!   than after it has embedded half a library.
//! * **It logs.** Every call rides `proxy::embed_once`, which writes the same
//!   `request_logs` row a public `/v1/embeddings` request would, under
//!   [`telemetry::EMBED_PROTO`]. An ingest writes thousands of vectors, and
//!   before this they were the one kind of model traffic Traffic never showed.

use std::sync::Arc;

use async_trait::async_trait;
use quickdoc_core::embed::{validate_vector, EmbedIdentity, Embedder};
use quickdoc_core::error::{QuickdocError, Result as QResult};
use quickdoc_core::store::Corpus;

use crate::config::{AuxKind, Snapshot};
use crate::error::GatewayError;
use crate::proxy;
use crate::state::SharedState;
use crate::telemetry;

pub struct InProcessEmbedder {
    state: SharedState,
    /// The client-facing name that routes to [`identity`](Self::identity).
    alias: String,
    identity: EmbedIdentity,
}

impl std::fmt::Debug for InProcessEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessEmbedder")
            .field("alias", &self.alias)
            .field("identity", &self.identity.to_string())
            .finish()
    }
}

impl InProcessEmbedder {
    /// Resolve `alias` and learn its width by embedding a probe — how a *new*
    /// corpus obtains the identity it will pin.
    pub async fn probe(state: SharedState, alias: &str) -> Result<Self, GatewayError> {
        let route = gate(&state, alias)?;
        budget_gate(&state, alias).await?;
        refuse_if_held(&state, alias, CORPUS_PIN)?;
        let (_, headers, resp) = proxy::embed_once(
            &state,
            alias,
            vec!["quickdoc probe".into()],
            telemetry::EMBED_PROTO,
        )
        .await
        .map_err(|(_, _, e)| e)?;
        if let Some(fb) = headers.fallback() {
            return Err(refuse_fallback(&state, alias, fb));
        }
        let dims = resp
            .embeddings
            .first()
            .map(Vec::len)
            .filter(|d| *d > 0)
            .ok_or_else(|| {
                GatewayError::Internal(format!("'{alias}' answered a probe with no vector"))
            })?;
        Ok(Self {
            state,
            alias: alias.to_string(),
            identity: EmbedIdentity::new(route.upstream.name, route.upstream_model, dims),
        })
    }

    /// Bind to a corpus's pinned identity (§4).
    pub async fn for_corpus(state: SharedState, corpus: &Corpus) -> Result<Self, GatewayError> {
        Self::for_identity(
            state,
            &corpus.embed_identity(),
            &format!("corpus {}", corpus.corpus_id()),
            "re-embed the corpus",
        )
        .await
    }

    /// Bind to a pinned identity — a corpus's, or a knowledge base's
    /// (chat-complete design §9.1). `owner` names what pinned it in the errors
    /// ("corpus axum@0.8", "knowledge base 'Taxes'"), `remedy` is the fix
    /// they offer besides restoring the upstream.
    ///
    /// The pin is `(upstream, model, dims)`, not the alias that was typed, so
    /// this looks up a name that still routes there. Failing to find one, or
    /// finding one that resolves elsewhere, is a hard error naming both sides —
    /// an alias silently remapped to another model of the same width is the
    /// exact failure the pin exists to catch.
    pub async fn for_identity(
        state: SharedState,
        pinned: &EmbedIdentity,
        owner: &str,
        remedy: &str,
    ) -> Result<Self, GatewayError> {
        let snap = state.snapshot();
        let alias = alias_for_identity(&snap, pinned).ok_or_else(|| {
            GatewayError::BadRequest(format!(
                "{owner} is embedded with {pinned}, and no model on this gateway resolves \
                 to it any more — restore that upstream or {remedy}"
            ))
        })?;
        let route = gate(&state, &alias)?;
        budget_gate(&state, &alias).await?;
        let got = EmbedIdentity::new(route.upstream.name, route.upstream_model, pinned.dims);
        if got.upstream != pinned.upstream || got.model != pinned.model {
            // `QuickdocError::EmbedMismatch`'s wording, with the owner named
            // by the caller rather than always as a corpus.
            return Err(GatewayError::BadRequest(format!(
                "{owner} is embedded with {pinned}, but the embedder resolves to {got} — \
                 {remedy} or query it with its pinned model"
            )));
        }
        Ok(Self {
            state,
            alias,
            identity: pinned.clone(),
        })
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }

    pub fn into_arc(self) -> Arc<dyn Embedder> {
        Arc::new(self)
    }
}

#[async_trait]
impl Embedder for InProcessEmbedder {
    fn identity(&self) -> EmbedIdentity {
        self.identity.clone()
    }

    async fn embed(&self, texts: &[String]) -> QResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // The one thing the request path does that a corpus cannot survive: a
        // GPU hold re-routes `self.alias` to its row's fallback and answers
        // with *that* model's vectors (gpu-hold design §2/§4). They would pass
        // every check below — the width can match by coincidence — and land in
        // a corpus pinned to `self.identity`, one silently unsearchable half
        // and one honest half. So the hold is refused here, before the call,
        // and again below on what the call returned.
        refuse_if_held(&self.state, &self.alias, CORPUS_PIN)
            .map_err(|e| QuickdocError::Embedder(e.to_string()))?;
        let (_, headers, resp) = proxy::embed_once(
            &self.state,
            &self.alias,
            texts.to_vec(),
            telemetry::EMBED_PROTO,
        )
        .await
        .map_err(|(_, _, e)| QuickdocError::Embedder(format!("{}: {e}", self.alias)))?;
        if let Some(fb) = headers.fallback() {
            return Err(QuickdocError::Embedder(format!(
                "'{}' was answered by the GPU-hold fallback '{fb}' — this corpus is pinned to \
                 {}, so vectors from another model are refused rather than mixed in. Release \
                 the hold and run this again.",
                self.alias, self.identity
            )));
        }
        if resp.embeddings.len() != texts.len() {
            return Err(QuickdocError::EmbedCount {
                want: texts.len(),
                got: resp.embeddings.len(),
            });
        }
        // Width and the all-zero reranker vector, per input. A mismatch is an
        // error, never a reshape.
        for (i, v) in resp.embeddings.iter().enumerate() {
            validate_vector(&self.identity.model, i, v, self.identity.dims)?;
        }
        Ok(resp.embeddings)
    }
}

/// Resolve `alias` and refuse it if it lands on a reranker.
///
/// `proxy::embed_in_process` refuses the same thing on every call; this is the
/// same gate applied once, up front, so an ingest job fails at its first step
/// with a message about the *model* rather than mid-run with one about a vector.
///
/// Plain `resolve`, never `resolve_for_request` — the identity a corpus pins is
/// the model it is *really* embedded with, and a GPU hold must not be able to
/// move it (gpu-hold design §2/§4). The request path's fallback is refused
/// where it would otherwise arrive, in [`InProcessEmbedder::embed`] and
/// [`InProcessEmbedder::probe`]; this resolve keeps the pin honest so those
/// refusals can name the real model.
/// Refuse an ingest or re-embed that the owner's budget cannot pay for
/// (usage-analytics §4.4), applied **once, up front** for the same reason
/// [`gate`] is: an unattended job that stops at its first step with a message
/// about the budget is far better than one that discovers the ceiling four
/// thousand vectors in, having already spent past it.
///
/// A corpus that embeds locally costs nothing and is never refused here — the
/// check prices the alias, and a local one prices free.
async fn budget_gate(state: &SharedState, alias: &str) -> Result<(), GatewayError> {
    crate::policy::check_internal(
        &state.policy,
        &state.db,
        &state.snapshot(),
        telemetry::INGEST_PROTO,
        alias,
    )
    .await
}

fn gate(state: &SharedState, alias: &str) -> Result<crate::config::Route, GatewayError> {
    let snap = state.snapshot();
    let route = snap.resolve(alias)?;
    if let Some(m) = snap.aux_model_for(&route) {
        if m.kind == AuxKind::Rerank {
            return Err(GatewayError::BadRequest(format!(
                "'{alias}' resolves to the rerank model '{}' — rerankers cannot produce \
                 embeddings (llama-server would answer with an all-zero vector). Pick an \
                 embedding model for this corpus.",
                m.model_id
            )));
        }
    }
    Ok(route)
}

/// The reason clause [`refuse_if_held`] puts in an embedding refusal.
pub(super) const CORPUS_PIN: &str =
    "a corpus is pinned to the one model it was embedded with, so quickdoc will not embed \
     through a fallback either";

/// Refuse a held local model **before** the call, not after (gpu-hold design
/// §2: batch and index work is refused, never re-routed).
///
/// The post-call fallback checks in [`InProcessEmbedder::embed`] and
/// [`super::rerank::InProcessReranker::rerank`] are the backstop that cannot
/// be bypassed — they see what the request path actually did. This is the same
/// refusal one step earlier, and the step matters: without it a held reembed
/// pays a cloud provider for a batch of vectors that are then thrown away
/// unused, which is precisely the "quietly spending a cloud model's tokens"
/// that §2 refuses batch work to avoid.
///
/// `Ok` when the alias does not resolve at all: the call that follows reports
/// that far better than a hold check can.
pub(crate) fn refuse_if_held(
    state: &SharedState,
    alias: &str,
    why: &str,
) -> Result<(), GatewayError> {
    let snap = state.snapshot();
    // The hold, or a benchmark's lease (benchmark design §3.2).
    let Some(block) = snap.gpu_block() else {
        return Ok(());
    };
    let Ok(route) = snap.resolve(alias) else {
        return Ok(());
    };
    match crate::vram::classify(&route) {
        Some(target) => Err(block.refusal(target.model_id, format!(" ({why})"))),
        None => Ok(()),
    }
}

/// Why quickdoc will not take a GPU-hold fallback's vectors (gpu-hold design
/// §2): a corpus is pinned to one `(upstream, model, dims)`, and an index half
/// filled by another model is unsearchable in a way no later check can detect.
/// Named as a hold rather than as a routing error because that is what it is,
/// and because the fix is to release the hold and re-run — not to reconfigure
/// anything.
///
/// A benchmark's lease answers the same, as `gpu_benchmark` (benchmark design
/// §3.2): the fallback answered because of it.
fn refuse_fallback(state: &SharedState, alias: &str, fallback: &str) -> GatewayError {
    state
        .snapshot()
        .gpu_block()
        .unwrap_or(crate::bench::lease::GpuBlock::Hold)
        .refusal(
            alias,
            format!(
                " (a corpus is pinned to the one model it was embedded with, so quickdoc will \
                 not embed through the fallback '{fallback}')"
            ),
        )
}

/// A client-routable name that resolves to exactly `(upstream, model)`.
///
/// A corpus pins the resolved identity rather than the alias it was created
/// with, which keeps it honest when an alias is repointed — but something has to
/// route again at query and re-embed time. This is that reverse lookup, over the
/// three ways a name reaches a model: an explicit alias, an aux-router model's
/// public name, and a public local model's.
pub fn alias_for_identity(snap: &Snapshot, id: &EmbedIdentity) -> Option<String> {
    let mut aliases: Vec<&crate::config::ModelAlias> = snap
        .aliases
        .values()
        .filter(|a| a.enabled && a.upstream_model_id == id.model)
        .collect();
    aliases.sort_by(|a, b| a.alias.cmp(&b.alias));
    for a in aliases {
        if snap
            .upstreams
            .get(&a.upstream_id)
            .is_some_and(|u| u.enabled && u.name == id.upstream)
        {
            return Some(a.alias.clone());
        }
    }
    if id.upstream == crate::config::AUX_UPSTREAM_NAME {
        if let Some(m) = snap
            .aux_models
            .iter()
            .find(|m| m.enabled && m.model_id == id.model)
        {
            return Some(snap.aux_public_name(&m.model_id));
        }
    }
    let router = snap.router_upstream();
    if id.upstream == router.name {
        if let Some(m) = snap
            .local_models
            .iter()
            .find(|m| m.public && m.enabled && m.model_id == id.model)
        {
            return Some(snap.local_public_name(&m.model_id));
        }
    }
    None
}
