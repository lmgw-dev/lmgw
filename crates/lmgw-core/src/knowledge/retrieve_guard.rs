//! Per-input limits on what a search sends to its models (review R2,
//! finding 3; R3, finding 4).
//!
//! A chunk is sized to its base's embedder at ingest, but the *query* is
//! whatever the Chat or a tool sends — the current message plus the previous
//! one — and a rerank input is query + chunk together. llama-server refuses an
//! input over its ubatch, and one refused input fails the whole request. So
//! both stages are wrapped:
//!
//! - [`BoundedEmbedder`] embeds the **tail** of a query that exceeds the
//!   embedding model's per-input limit. The newest words matter most (the Chat
//!   puts the previous message first), so the front goes.
//! - [`BoundedReranker`] scores only the query + chunk pairs that fit the
//!   reranker's limit; the rest are not scored — they keep their fused score,
//!   are marked `rerank_skipped`, and rank behind the scored ones. When no
//!   pair fits, the stage is unavailable and says why.
//!
//! **What a turn costs.** Neither wrapper calls a tokenizer per turn. The
//! model's tokenizer ratio to tiktoken is measured once per model identity per
//! process ([`limit::cached_counter`], primed from the base's own chunks by
//! [`super::fit::prime_ratio`]); every count here is tiktoken (a local
//! computation — the same number stored with each chunk) times that ratio.
//! Only a query whose estimate reaches [`limit::EXACT_ABOVE_PERCENT`] of the
//! limit is counted exactly by the model, since only then does it decide
//! anything.
//!
//! Every cut is a note on the retrieval, never silent. The limits and the
//! token counts are [`super::limit`]'s: the same per-input rule that sizes a
//! base's chunks.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use quickdoc_core::embed::{EmbedIdentity, Embedder, Reranker, TokenCounter};
use quickdoc_core::error::{QuickdocError, Result as QResult};

use crate::state::SharedState;

use super::limit;

/// Notes a wrapped stage leaves for the retrieval to publish.
pub type Notes = Arc<Mutex<Vec<String>>>;

fn note(notes: &Notes, text: String) {
    let mut n = notes.lock().unwrap_or_else(|e| e.into_inner());
    if !n.contains(&text) {
        n.push(text);
    }
}

/// An embedder that never sends the model more than one input holds.
pub struct BoundedEmbedder {
    pub inner: Arc<dyn Embedder>,
    pub state: SharedState,
    pub alias: String,
    pub notes: Notes,
}

#[async_trait]
impl Embedder for BoundedEmbedder {
    fn identity(&self) -> EmbedIdentity {
        self.inner.identity()
    }

    async fn embed(&self, texts: &[String]) -> QResult<Vec<Vec<f32>>> {
        let input = limit::input_limit(&self.state, &self.alias).await;
        let Some(max) = input.tokens else {
            return self.inner.embed(texts).await;
        };
        let mut shortened = Vec::with_capacity(texts.len());
        for t in texts {
            let (counter, _) = limit::cached_counter(&self.state, &self.alias, t).await;
            let counter = if limit::near_limit(counter.count(t) as u64, max) {
                limit::calibrate(&self.state, &self.alias, t).await.0
            } else {
                shortened.push(t.clone());
                continue;
            };
            let (tail, cut) = limit::fit_tail(t, max as usize, &*counter);
            if cut {
                note(
                    &self.notes,
                    format!(
                        "query shortened to the embedding model's {max}-token input ({}): the \
                         search used its last {} of {} tokens",
                        input.source,
                        counter.count(tail),
                        counter.count(t)
                    ),
                );
            }
            shortened.push(tail.to_string());
        }
        self.inner.embed(&shortened).await
    }
}

/// A reranker that scores only the pairs that fit its per-input limit.
pub struct BoundedReranker {
    pub inner: Arc<dyn Reranker>,
    pub state: SharedState,
    pub alias: String,
    pub notes: Notes,
}

#[async_trait]
impl Reranker for BoundedReranker {
    fn model(&self) -> String {
        self.inner.model()
    }

    /// Every pair must fit: for callers that cannot take an unscored
    /// candidate. The pipeline calls [`rerank_scores`](Self::rerank_scores).
    async fn rerank(&self, query: &str, documents: &[String]) -> QResult<Vec<f32>> {
        let scores = self.rerank_scores(query, documents).await?;
        let skipped = scores.iter().filter(|s| s.is_none()).count();
        if skipped > 0 {
            return Err(QuickdocError::Reranker(format!(
                "{skipped} of {} pairs are over the reranker's input limit and were not scored",
                documents.len()
            )));
        }
        Ok(scores.into_iter().flatten().collect())
    }

    async fn rerank_scores(&self, query: &str, documents: &[String]) -> QResult<Vec<Option<f32>>> {
        let input = limit::input_limit(&self.state, &self.alias).await;
        let Some(max) = input.tokens else {
            return self.inner.rerank_scores(query, documents).await;
        };
        let (counter, _) = limit::cached_counter(&self.state, &self.alias, query).await;
        // `count` carries two special tokens, so `count(q) + count(d)` is the
        // pair with the four a query + document pair is wrapped in. The
        // documents are counted by tiktoken (local; what each chunk stores)
        // times the remembered ratio — no tokenizer round-trip per document.
        // The query is counted exactly when it is near the limit.
        let mut q = counter.count(query) as u64;
        if limit::near_limit(q, max) {
            let (exact, _) = limit::calibrate(&self.state, &self.alias, query).await;
            q = exact.count(query) as u64;
        }
        let fits: Vec<bool> = documents
            .iter()
            .map(|d| q + counter.count(d) as u64 <= max)
            .collect();
        let sendable: Vec<String> = documents
            .iter()
            .zip(&fits)
            .filter(|(_, ok)| **ok)
            .map(|(d, _)| d.clone())
            .collect();
        let skipped = documents.len() - sendable.len();
        if sendable.is_empty() {
            return Err(QuickdocError::Reranker(format!(
                "none of the {} candidates fits {}'s {max}-token limit for a query + chunk pair \
                 ({}); the query alone takes {q} tokens",
                documents.len(),
                self.alias,
                input.source
            )));
        }
        let scores = self.inner.rerank(query, &sendable).await?;
        if scores.len() != sendable.len() {
            return Err(QuickdocError::RerankCount {
                want: sendable.len(),
                got: scores.len(),
            });
        }
        if skipped > 0 {
            note(
                &self.notes,
                format!(
                    "{skipped} of {} candidates were too long for '{}' ({max} tokens per query + \
                     chunk pair: {}) and were not reranked — they keep their search score, are \
                     marked rerank_skipped, and rank behind the reranked ones",
                    documents.len(),
                    self.alias,
                    input.source
                ),
            );
        }
        let mut real = scores.into_iter();
        Ok(fits
            .iter()
            .map(|ok| if *ok { real.next() } else { None })
            .collect())
    }
}
