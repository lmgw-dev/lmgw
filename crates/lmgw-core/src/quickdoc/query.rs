//! Assembling one retrieval (§6): the pieces every caller of a corpus needs,
//! in one place so the MCP tool, the debug endpoint and the eval job cannot
//! drift into measuring three slightly different pipelines.
//!
//! What "assembling" means here: bind the corpus's **pinned** embedder, attach
//! the rerank stage (or the reason there is none), attach a token counter that
//! names itself, and start from the owner's stage defaults so a request that
//! overrides nothing still gets the settings it was configured with.

use std::sync::Arc;

use quickdoc_core::embed::TokenCounter;
use quickdoc_core::retrieve::{Retriever, SearchParams};
use quickdoc_core::store::Corpus;
use serde_json::Value;

use crate::config::Snapshot;
use crate::error::GatewayError;
use crate::state::SharedState;

use super::embed::{alias_for_identity, InProcessEmbedder};
use super::rerank::{rerank_alias, InProcessReranker};

/// The `budget_tokens` counter (§7). `o200k_base` rather than any one model's
/// encoding: an MCP caller's model is unknown here, and a counter that names
/// itself in the trace beats one that pretends to a precision it does not have.
pub struct TiktokenCounter;

/// tiktoken-rs's process-wide encoder, the one the gateway's own counter
/// (`egress::openai`) uses too, so the 200k-entry table is built once.
fn o200k() -> Option<&'static tiktoken_rs::CoreBPE> {
    tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase).ok()
}

impl TokenCounter for TiktokenCounter {
    fn name(&self) -> String {
        "tiktoken o200k_base".into()
    }

    fn count(&self, text: &str) -> usize {
        match o200k() {
            Some(bpe) => bpe.encode_ordinary(text).len(),
            // Never silently estimate under a name that claims otherwise.
            None => text.chars().count(),
        }
    }
}

/// The owner's stage defaults, as one request's starting parameters.
pub fn default_params(snap: &Snapshot) -> SearchParams {
    snap.settings.docs_search.params()
}

/// Overlay a caller's partial `params` object onto the defaults.
///
/// Field-wise rather than "deserialize the object": an override that mentions
/// only `k_rerank` must keep the owner's `k_fts`, not silently fall back to the
/// crate's compiled-in one.
pub fn apply_params(base: &SearchParams, overrides: &Value) -> Result<SearchParams, String> {
    let Some(obj) = overrides.as_object() else {
        return Err("'params' must be an object".into());
    };
    let mut merged =
        serde_json::to_value(base).map_err(|e| format!("encoding search params: {e}"))?;
    let target = merged
        .as_object_mut()
        .ok_or("search params did not encode as an object")?;
    for (k, v) in obj {
        if !target.contains_key(k) {
            return Err(format!(
                "unknown search parameter '{k}' (expected one of: {})",
                target.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        target.insert(k.clone(), v.clone());
    }
    serde_json::from_value(merged).map_err(|e| format!("invalid search params: {e}"))
}

/// Load a corpus with its models attached, ready to search.
///
/// The embedder is the corpus's *pinned* one — [`InProcessEmbedder::for_corpus`]
/// refuses to build if the pin no longer resolves — and the reranker is
/// whatever the gateway currently has, with its absence recorded as a reason
/// rather than as silence.
pub async fn open_retriever(
    state: &SharedState,
    corpus: &Corpus,
) -> Result<Retriever, GatewayError> {
    let embedder = InProcessEmbedder::for_corpus(state.clone(), corpus)
        .await?
        .into_arc();
    let retriever = Retriever::load(state.corpus.clone(), corpus.id, embedder)
        .await
        .map_err(|e| GatewayError::BadRequest(e.to_string()))?
        .with_token_counter(Arc::new(TiktokenCounter));

    let snap = state.snapshot();
    Ok(match rerank_alias(&snap) {
        Ok(alias) => match InProcessReranker::for_alias(state.clone(), &alias) {
            Ok(r) => retriever.with_reranker(r.into_arc()),
            Err(e) => retriever.with_rerank_unavailable(e.to_string()),
        },
        Err(reason) => retriever.with_rerank_unavailable(reason),
    })
}

/// The badges §7 and §11 both read: whether the corpus can still be queried
/// with its own embedding model, and whether its last eval regressed.
///
/// Neither blocks a query. Degraded-but-stated beats refused — a caller told
/// that a corpus scored below its best can caveat its answer or pick another
/// version, which it cannot do if the corpus simply refuses to answer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CorpusStatus {
    /// `ok` | `re_embed_required`
    pub embed_status: &'static str,
    /// Whether a model on this gateway still resolves to the corpus's pinned
    /// embedding identity.
    ///
    /// The `re_embed_required` badge covers two states that are not the same
    /// thing to a caller: a corpus mid re-embed still answers (the vector half
    /// is incomplete, BM25 carries the rest), while a corpus whose pin no
    /// longer resolves cannot be queried at all — opening its retriever is a
    /// hard error. Anything that phrases advice about the badge needs to know
    /// which one it is looking at.
    pub embed_model_resolvable: bool,
    /// `ok` | `regression` | `unmeasured`
    pub eval_status: &'static str,
    /// Short machine-readable badges, the same set the Docs tab renders.
    pub flags: Vec<String>,
    /// One sentence per flag, for the model and the human reading it.
    pub warnings: Vec<String>,
}

pub fn corpus_status(snap: &Snapshot, corpus: &Corpus) -> CorpusStatus {
    let mut flags = Vec::new();
    let mut warnings = Vec::new();

    let pinned = corpus.embed_identity();
    let resolvable = alias_for_identity(snap, &pinned).is_some();
    let embed_status = if resolvable && corpus.status != "re_embed_required" {
        "ok"
    } else {
        flags.push("re_embed_required".into());
        if resolvable {
            warnings.push(format!(
                "corpus {} is mid re-embed: some chunks have no vector yet, so the vector stage \
                 sees only part of it",
                corpus.corpus_id()
            ));
        } else {
            warnings.push(format!(
                "corpus {} is embedded with {pinned}, and no model on this gateway resolves to \
                 it any more — queries against it will fail until it is re-embedded",
                corpus.corpus_id()
            ));
        }
        "re_embed_required"
    };

    let eval_status = match (corpus.eval_score, corpus.eval_regression) {
        (None, _) => {
            flags.push("eval_unmeasured".into());
            "unmeasured"
        }
        (Some(score), true) => {
            flags.push("eval_regression".into());
            warnings.push(format!(
                "eval regression: the last run scored hit@{} {:.2}, below the best this corpus \
                 has reached at that k with those retrieval settings ({:.2}) — its answers may \
                 be worse than they were",
                corpus.eval_k.max(1),
                score,
                corpus.eval_best.unwrap_or(score),
            ));
            "regression"
        }
        (Some(_), false) => "ok",
    };

    if corpus.chunk_count == 0 {
        flags.push("empty".into());
        warnings.push(format!(
            "corpus {} has no chunks — it has not finished ingesting",
            corpus.corpus_id()
        ));
    }

    CorpusStatus {
        embed_status,
        embed_model_resolvable: resolvable,
        eval_status,
        flags,
        warnings,
    }
}
