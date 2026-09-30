//! The traits at the crate boundary (§3) plus deterministic fixtures.
//!
//! quickdoc-core never talks to a model itself. lmgw-core supplies the real
//! implementations later — an *in-process* embedder calling `embeddings_inner`
//! and an *HTTP* one against any OpenAI-compatible `/v1/embeddings`, plus a
//! reranker over the aux router's `/v1/rerank`. Everything here is `async` and
//! object-safe so those arrive as `Arc<dyn …>` with no signature churn.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{QuickdocError, Result};

/// The resolved model behind a set of vectors — not the alias that was asked
/// for. A corpus pins this at ingest and every query verifies it (§4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedIdentity {
    pub upstream: String,
    pub model: String,
    pub dims: usize,
}

impl EmbedIdentity {
    pub fn new(upstream: impl Into<String>, model: impl Into<String>, dims: usize) -> Self {
        Self {
            upstream: upstream.into(),
            model: model.into(),
            dims,
        }
    }
}

impl std::fmt::Display for EmbedIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{} ({}d)", self.upstream, self.model, self.dims)
    }
}

#[async_trait]
pub trait Embedder: Send + Sync {
    /// Who is actually answering. Resolved through the router, so an alias
    /// remap shows up here as a different identity.
    fn identity(&self) -> EmbedIdentity;

    /// One vector per input, in order. Vectors need not be normalised — the
    /// store does that before narrowing to f16.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// The optional cross-encoder stage (§6). The real implementation calls the aux
/// router's `/v1/rerank`; retrieval is defined to work without one.
#[async_trait]
pub trait Reranker: Send + Sync {
    fn model(&self) -> String;

    /// One score per document, in the order given. Higher is better.
    async fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<f32>>;

    /// [`rerank`](Self::rerank) for a reranker that may leave some documents
    /// **unscored** (a pair over its input limit): one entry per document,
    /// `None` for one it did not score. The pipeline keeps such a candidate's
    /// fused (RRF) score and marks it `rerank_skipped` — a score is never
    /// invented for it. The default scores every document.
    async fn rerank_scores(&self, query: &str, documents: &[String]) -> Result<Vec<Option<f32>>> {
        Ok(self
            .rerank(query, documents)
            .await?
            .into_iter()
            .map(Some)
            .collect())
    }
}

/// How `budget_tokens` is measured. lmgw-core substitutes a tiktoken-backed
/// counter; [`ApproxTokenCounter`] keeps this crate free of a tokenizer
/// dependency and names itself in the search trace so nobody mistakes an
/// estimate for the real count.
pub trait TokenCounter: Send + Sync {
    fn name(&self) -> String;
    fn count(&self, text: &str) -> usize;
}

/// Lowercased runs of alphanumerics — what FTS5's `unicode61` tokenizer does to
/// a string, so the BM25 query builder and the fixtures agree on what a term
/// is.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            cur.extend(ch.to_lowercase());
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------------------
// Fixtures — deterministic, offline, no model
// ---------------------------------------------------------------------------

#[inline]
fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn hash_term(term: &str, salt: u64) -> u64 {
    let mut h = salt;
    for b in term.as_bytes() {
        h = splitmix64(h ^ u64::from(*b));
    }
    h
}

/// A signed hashing-trick bag of words: each term lands on two dimensions, one
/// positive and one negative.
///
/// It is deterministic (the tests need that) *and* similarity-preserving —
/// texts that share vocabulary get a high cosine — which is what makes the KNN
/// stage testable offline at all. A pure hash-of-the-whole-string fixture would
/// make every chunk orthogonal and every retrieval test vacuous.
pub struct FixtureEmbedder {
    dims: usize,
    model: String,
}

impl FixtureEmbedder {
    pub fn new(dims: usize) -> Self {
        Self {
            dims,
            model: "fixture-bow".into(),
        }
    }

    /// Same vectors under a different reported identity — the fixture for
    /// §4's "alias remapped to another model of the same width" case.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dims];
        for term in tokenize(text) {
            let a = hash_term(&term, 0x9E37_79B9) as usize % self.dims;
            let b = hash_term(&term, 0xC2B2_AE3D) as usize % self.dims;
            v[a] += 1.0;
            v[b] -= 1.0;
        }
        crate::vector::l2_normalize(&mut v);
        v
    }
}

#[async_trait]
impl Embedder for FixtureEmbedder {
    fn identity(&self) -> EmbedIdentity {
        EmbedIdentity::new("fixture", self.model.clone(), self.dims)
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }
}

/// Keeps the order it was handed. Proves the rerank stage is wired without
/// pretending to judge anything.
pub struct NoopReranker;

#[async_trait]
impl Reranker for NoopReranker {
    fn model(&self) -> String {
        "noop".into()
    }

    async fn rerank(&self, _query: &str, documents: &[String]) -> Result<Vec<f32>> {
        Ok((0..documents.len())
            .map(|i| 1.0 / (1.0 + i as f32))
            .collect())
    }
}

/// Jaccard overlap between query terms and document terms. Deterministic, and
/// unlike [`NoopReranker`] it actually reorders, so a test can assert the stage
/// changed the ranking and shows up in the trace.
pub struct FixtureReranker;

#[async_trait]
impl Reranker for FixtureReranker {
    fn model(&self) -> String {
        "fixture-jaccard".into()
    }

    async fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<f32>> {
        let q: std::collections::BTreeSet<String> = tokenize(query).into_iter().collect();
        Ok(documents
            .iter()
            .map(|d| {
                let t: std::collections::BTreeSet<String> = tokenize(d).into_iter().collect();
                let inter = q.intersection(&t).count() as f32;
                let union = q.union(&t).count() as f32;
                if union == 0.0 {
                    0.0
                } else {
                    inter / union
                }
            })
            .collect())
    }
}

/// Characters divided by a visible ratio. Wrong by a few percent on English
/// prose and more on code — which is why `name()` says so and lmgw-core swaps
/// in the real tokenizer.
pub struct ApproxTokenCounter {
    pub chars_per_token: f32,
}

impl Default for ApproxTokenCounter {
    fn default() -> Self {
        Self {
            chars_per_token: 4.0,
        }
    }
}

impl TokenCounter for ApproxTokenCounter {
    fn name(&self) -> String {
        format!("approx({} chars/token)", self.chars_per_token)
    }

    fn count(&self, text: &str) -> usize {
        if self.chars_per_token <= 0.0 {
            return text.chars().count();
        }
        (text.chars().count() as f32 / self.chars_per_token).ceil() as usize
    }
}

/// Checks a vector an [`Embedder`] returned before it can reach a corpus.
///
/// The all-zeros case is §9a's live-verified footgun: `/v1/embeddings` against
/// a reranker section answers 200 with a zero vector, and a corpus embedded
/// from those is silently unsearchable.
pub fn validate_vector(model: &str, index: usize, v: &[f32], dims: usize) -> Result<()> {
    if v.len() != dims {
        return Err(QuickdocError::Embedder(format!(
            "{model} returned a {}-wide vector for input {index}, expected {dims}",
            v.len()
        )));
    }
    if crate::vector::is_zero(v) {
        return Err(QuickdocError::Embedder(format!(
            "{model} returned an all-zero vector for input {index} — \
             a reranker section answers /v1/embeddings that way"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_vectors_are_deterministic_and_similarity_preserving() {
        let e = FixtureEmbedder::new(64);
        let a = e.embed_one("axum routing get handler");
        let b = e.embed_one("axum routing get handler");
        let near = e.embed_one("axum routing post handler");
        let far = e.embed_one("tokio runtime worker threads");
        assert_eq!(a, b);
        let sim_near = crate::vector::dot_f32(&a, &near);
        let sim_far = crate::vector::dot_f32(&a, &far);
        assert!(sim_near > sim_far, "{sim_near} !> {sim_far}");
    }

    #[test]
    fn rejects_the_reranker_zero_vector() {
        assert!(validate_vector("m", 0, &[0.0, 0.0], 2).is_err());
        assert!(validate_vector("m", 0, &[0.0, 1.0], 2).is_ok());
        assert!(validate_vector("m", 0, &[1.0], 2).is_err());
    }
}
