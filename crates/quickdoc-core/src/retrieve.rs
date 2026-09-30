//! The hybrid retrieval pipeline (§6).
//!
//! ```text
//! query ─→ FTS5 BM25 (top K_f) ─┐
//!       └→ embed → exact KNN (top K_v) ─┴→ RRF fusion → [rerank top K_r] → budgeted response
//! ```
//!
//! Every stage parameter is a per-request [`SearchParams`] field with a default
//! that is written down and overridable — nothing here caps anything silently.
//! Each stage's candidates and scores land in [`SearchTrace`], which is what the
//! debug endpoint (§10) and the playground render.
//!
//! The pipeline is written once, in [`hybrid_search`], over the
//! [`SearchSource`] trait: a docs corpus ([`Retriever`]) is one source, the
//! gateway's knowledge bases (chat-complete design §9.1) are another, with a
//! schema of their own. Neither carries a copy of the ranking.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::embed::{tokenize, ApproxTokenCounter, EmbedIdentity, Embedder, Reranker, TokenCounter};
use crate::error::{QuickdocError, Result};
use crate::store::{self, Chunk, Corpus};
use crate::vector::{self, VectorMatrix};

/// BM25 column weights, in `chunk_fts` column order. Headings and derived
/// titles are short and topical, so a hit there says more than one buried in a
/// long payload — hence the default tilt.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct FtsWeights {
    pub payload: f32,
    pub heading_path: f32,
    pub derived_title: f32,
    pub derived_summary: f32,
}

impl Default for FtsWeights {
    fn default() -> Self {
        Self {
            payload: 1.0,
            heading_path: 1.5,
            derived_title: 1.5,
            derived_summary: 1.0,
        }
    }
}

/// Per-request stage parameters (§6). lmgw-core seeds the defaults from
/// Settings; an external agent may override any of them for one call, and only
/// the owner commits new defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchParams {
    /// `K_f` — BM25 candidates. Fusion only needs enough depth for a document
    /// that one stage ranks poorly to still be rescued by the other.
    pub k_fts: usize,
    /// `K_v` — exact-KNN candidates.
    pub k_vec: usize,
    /// RRF's rank-damping constant. 60 is the Cormack et al. value and the one
    /// every published RRF baseline uses; lower sharpens the top ranks.
    pub rrf_k: f32,
    pub fts_weights: FtsWeights,
    /// Run the cross-encoder stage when a reranker is attached. Asking for it
    /// without one is not an error — the trace records that it was skipped.
    pub rerank: bool,
    /// `K_r` — how deep into the fused list the reranker looks. Cross-encoders
    /// are the expensive stage, so this is the knob that costs latency.
    pub k_rerank: usize,
    /// Ranked chunks returned. The token budget may trim further; nothing else
    /// does.
    pub limit: usize,
    /// Response budget in tokens, counted over what a caller actually receives.
    /// `None` returns all `limit` chunks — the budget is the caller's choice,
    /// never a server-side default nobody can see.
    pub budget_tokens: Option<usize>,
}

impl Default for SearchParams {
    fn default() -> Self {
        Self {
            k_fts: 50,
            k_vec: 50,
            rrf_k: 60.0,
            fts_weights: FtsWeights::default(),
            rerank: true,
            k_rerank: 20,
            limit: 10,
            budget_tokens: None,
        }
    }
}

/// One candidate as a stage saw it.
#[derive(Debug, Clone, Serialize)]
pub struct StageHit {
    pub chunk_id: String,
    /// 1-based within the stage.
    pub rank: usize,
    /// BM25 (negative, lower is better), cosine, or reranker score depending on
    /// the stage — the trace is read next to the stage it came from. For a
    /// `skipped` rerank entry it is the candidate's fused (RRF) score: the
    /// reranker never scored it.
    pub score: f32,
    /// Rerank stage only: the reranker did not score this candidate (its pair
    /// was over the model's input limit). It ranks behind the scored ones, by
    /// fused score.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FusedHit {
    pub chunk_id: String,
    pub rrf_score: f32,
    pub fts_rank: Option<usize>,
    pub knn_rank: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Timings {
    pub embed_ms: f64,
    pub fts_ms: f64,
    pub knn_ms: f64,
    pub fuse_ms: f64,
    pub fetch_ms: f64,
    pub rerank_ms: f64,
    pub total_ms: f64,
}

/// Per-stage trace: BM25 hits and scores, KNN hits and distances, the fusion
/// table, rerank scores, and what the budget trimmed (§6, §10).
#[derive(Debug, Clone, Serialize)]
pub struct SearchTrace {
    pub corpus_id: String,
    pub embed_model: String,
    pub params: SearchParams,
    /// The FTS5 MATCH expression actually issued — user text is never passed
    /// through raw, so the playground shows what was really asked.
    pub fts_query: String,
    pub fts: Vec<StageHit>,
    /// Why the vector stage did not run — BM25 alone answered. Absent when
    /// it ran, which is every docs corpus search.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knn_skipped: Option<String>,
    pub knn: Vec<StageHit>,
    pub fused: Vec<FusedHit>,
    pub rerank_model: Option<String>,
    /// Why the rerank stage did not run, when `params.rerank` asked for it.
    pub rerank_skipped: Option<String>,
    pub rerank: Vec<StageHit>,
    pub token_counter: String,
    pub budget_used_tokens: usize,
    /// Chunks dropped by the token budget, in the order they would have come.
    pub budget_dropped: Vec<String>,
    pub resident_vectors: usize,
    pub resident_bytes: usize,
    /// Which widening kernel the KNN scan used — the difference between a
    /// millisecond scan and a 20× slower one, so it is not left to guesswork.
    pub knn_kernel: &'static str,
    pub timings: Timings,
}

/// One returned document with the provenance of its rank.
///
/// Generic over what a hit carries so the pipeline below serves more than one
/// store (see [`SearchSource`]); a corpus search is `Hit<Chunk>`, which is what
/// plain `Hit` means. The field keeps the name `chunk` for that reason — every
/// store this runs over is a store of chunks.
#[derive(Debug, Clone, Serialize)]
pub struct Hit<D = Chunk> {
    pub chunk: D,
    /// The score the final order is by: the reranker's when it ran, RRF's
    /// otherwise.
    pub score: f32,
    pub rrf_score: f32,
    pub fts_rank: Option<usize>,
    pub knn_rank: Option<usize>,
    pub knn_score: Option<f32>,
    pub rerank_score: Option<f32>,
    /// The rerank stage ran but did not score this candidate; `score` is then
    /// its fused (RRF) score, and it ranks behind the reranked ones.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rerank_skipped: bool,
    pub tokens: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub corpus_id: String,
    pub hits: Vec<Hit>,
    pub trace: SearchTrace,
}

// ---------------------------------------------------------------------------
// The table-agnostic core
// ---------------------------------------------------------------------------

/// What the hybrid pipeline needs from a store: a BM25 stage, a resident
/// vector stage, and the documents behind the ids those two return.
///
/// The pipeline itself — fusion, rerank, limit, token budget, trace — is
/// [`hybrid_search`], written once. A docs corpus ([`Retriever`]) and the
/// gateway's knowledge bases are two implementations of this trait over two
/// schemas, so neither grows a second copy of the ranking.
#[async_trait]
pub trait SearchSource: Send + Sync {
    /// What a hit carries back to the caller.
    type Doc: Clone + Send + Sync;

    /// Names what is searched, in the trace and in errors (`axum@0.8`).
    fn label(&self) -> String;
    /// The width every stored vector has; the query vector must match it.
    fn dims(&self) -> usize;
    fn resident_vectors(&self) -> usize;
    fn resident_bytes(&self) -> usize;
    /// Stage 1: `(id, bm25)` best first (lower BM25 is better), at most `k`.
    /// `expr` is already a safe MATCH expression ([`fts_match_expression`]).
    async fn fts(&self, expr: &str, weights: &FtsWeights, k: usize) -> Result<Vec<(String, f32)>>;
    /// Stage 2: exact KNN, `(id, cosine)` best first, at most `k`. `query` is
    /// L2-normalised and exactly [`Self::dims`] wide.
    fn knn(&self, query: &[f32], k: usize) -> Result<Vec<(String, f32)>>;
    /// The documents behind `ids`. An id the store no longer has is simply
    /// absent from the map.
    async fn fetch(&self, ids: &[String]) -> Result<HashMap<String, Self::Doc>>;
    /// The document's id, as the stages returned it.
    fn doc_id(&self, doc: &Self::Doc) -> String;
    /// What the reranker scores and the budget counts: what a caller actually
    /// receives of this document.
    fn doc_text(&self, doc: &Self::Doc) -> String;
}

/// The models one [`hybrid_search`] runs with.
pub struct SearchModels<'a> {
    /// `None` skips the vector stage — BM25 alone still answers — and
    /// [`Self::embed_unavailable`] is what the trace says about it.
    pub embedder: Option<&'a dyn Embedder>,
    pub embed_unavailable: Option<String>,
    pub reranker: Option<&'a dyn Reranker>,
    /// Why there is no reranker, when the host knows more than "there isn't
    /// one".
    pub rerank_unavailable: Option<String>,
    pub tokens: &'a dyn TokenCounter,
}

/// What stages 1–2 (BM25, embed + KNN) found. The pipeline is split here so a
/// caller that degrades a *later* stage (the reranker failing) resumes from
/// these results instead of embedding the query and searching again — see
/// [`rank_candidates`].
#[derive(Debug, Clone)]
pub struct Candidates {
    pub fts_query: String,
    pub fts: Vec<StageHit>,
    pub knn: Vec<StageHit>,
    /// Why the vector stage did not run.
    pub knn_skipped: Option<String>,
    /// The embedder's identity, when the vector stage ran.
    pub embed_model: String,
    timings: Timings,
    started: Instant,
}

/// Stage 1: BM25. Returns the FTS5 expression issued, the hits and the time.
pub async fn fts_stage<S>(
    src: &S,
    query: &str,
    params: &SearchParams,
) -> Result<(String, Vec<StageHit>, f64)>
where
    S: SearchSource + ?Sized,
{
    let t0 = Instant::now();
    let fts_query = fts_match_expression(query);
    let fts: Vec<StageHit> = if fts_query.is_empty() || params.k_fts == 0 {
        Vec::new()
    } else {
        ranked(
            src.fts(&fts_query, &params.fts_weights, params.k_fts)
                .await?,
        )
    };
    Ok((fts_query, fts, ms(t0)))
}

/// Stage 2: embed the query and scan the vectors. Returns the hits and the
/// embed and KNN times.
pub async fn knn_stage<S>(
    src: &S,
    embedder: &dyn Embedder,
    query: &str,
    params: &SearchParams,
) -> Result<(Vec<StageHit>, f64, f64)>
where
    S: SearchSource + ?Sized,
{
    let t0 = Instant::now();
    let inputs = vec![query.to_string()];
    let mut qvec = embedder
        .embed(&inputs)
        .await?
        .into_iter()
        .next()
        .ok_or(QuickdocError::EmbedCount { want: 1, got: 0 })?;
    let embed_ms = ms(t0);
    if qvec.len() != src.dims() {
        return Err(QuickdocError::DimsMismatch {
            corpus: src.label(),
            expected: src.dims(),
            got: qvec.len(),
        });
    }
    vector::l2_normalize(&mut qvec);
    let t0 = Instant::now();
    let knn = ranked(src.knn(&qvec, params.k_vec)?);
    Ok((knn, embed_ms, ms(t0)))
}

impl Candidates {
    /// Stage 1 only (the vector stage is skipped for `skipped`).
    pub fn keywords(
        started: Instant,
        fts_query: String,
        fts: Vec<StageHit>,
        fts_ms: f64,
        skipped: impl Into<String>,
    ) -> Self {
        Self {
            fts_query,
            fts,
            knn: Vec::new(),
            knn_skipped: Some(skipped.into()),
            embed_model: String::new(),
            timings: Timings {
                fts_ms,
                ..Default::default()
            },
            started,
        }
    }

    /// Both stages ran.
    pub fn with_vectors(
        mut self,
        embed_model: String,
        knn: Vec<StageHit>,
        embed_ms: f64,
        knn_ms: f64,
    ) -> Self {
        self.knn = knn;
        self.knn_skipped = None;
        self.embed_model = embed_model;
        self.timings.embed_ms = embed_ms;
        self.timings.knn_ms = knn_ms;
        self
    }
}

/// The whole pipeline over any [`SearchSource`]: BM25 + KNN → RRF → optional
/// rerank → `limit` → token budget, with every stage in the trace.
pub async fn hybrid_search<S>(
    src: &S,
    models: &SearchModels<'_>,
    query: &str,
    params: &SearchParams,
) -> Result<(Vec<Hit<S::Doc>>, SearchTrace)>
where
    S: SearchSource + ?Sized,
{
    let started = Instant::now();
    let (fts_query, fts, fts_ms) = fts_stage(src, query, params).await?;
    let cand = match models.embedder {
        Some(embedder) => {
            let (knn, embed_ms, knn_ms) = knn_stage(src, embedder, query, params).await?;
            Candidates::keywords(started, fts_query, fts, fts_ms, "").with_vectors(
                embedder.identity().to_string(),
                knn,
                embed_ms,
                knn_ms,
            )
        }
        None => Candidates::keywords(
            started,
            fts_query,
            fts,
            fts_ms,
            models
                .embed_unavailable
                .clone()
                .unwrap_or_else(|| "no embedder attached".into()),
        ),
    };
    rank_candidates(src, models, query, params, &cand).await
}

/// Stages 3–5 over stages 1–2's [`Candidates`]: RRF fusion → optional rerank →
/// `limit` → token budget. Nothing is searched or embedded again, so a caller
/// whose reranker failed calls this again without it and pays only for the
/// stages that ran after the failure.
pub async fn rank_candidates<S>(
    src: &S,
    models: &SearchModels<'_>,
    query: &str,
    params: &SearchParams,
    cand: &Candidates,
) -> Result<(Vec<Hit<S::Doc>>, SearchTrace)>
where
    S: SearchSource + ?Sized,
{
    let label = src.label();
    let mut timings = cand.timings.clone();
    let (fts, knn) = (&cand.fts, &cand.knn);

    // ---- stage 3: RRF fusion ----
    let t0 = Instant::now();
    let fused = rrf_fuse(fts, knn, params.rrf_k);
    timings.fuse_ms = ms(t0);

    // Everything past fusion needs payloads: the reranker scores them and
    // the caller receives them.
    let depth = if params.rerank {
        params.k_rerank.max(params.limit)
    } else {
        params.limit
    };
    let head: Vec<&FusedHit> = fused.iter().take(depth).collect();
    let t0 = Instant::now();
    let ids: Vec<String> = head.iter().map(|f| f.chunk_id.clone()).collect();
    let docs = src.fetch(&ids).await?;
    timings.fetch_ms = ms(t0);

    let knn_scores: HashMap<&str, f32> =
        knn.iter().map(|h| (h.chunk_id.as_str(), h.score)).collect();
    let mut hits: Vec<Hit<S::Doc>> = head
        .iter()
        .filter_map(|f| {
            docs.get(&f.chunk_id).map(|d| Hit {
                chunk: d.clone(),
                score: f.rrf_score,
                rrf_score: f.rrf_score,
                fts_rank: f.fts_rank,
                knn_rank: f.knn_rank,
                knn_score: knn_scores.get(f.chunk_id.as_str()).copied(),
                rerank_score: None,
                rerank_skipped: false,
                tokens: 0,
            })
        })
        .collect();

    // ---- stage 4: optional cross-encoder rerank ----
    let mut rerank_trace = Vec::new();
    let mut rerank_model = None;
    let mut rerank_skipped = None;
    if params.rerank {
        match (models.reranker, params.k_rerank) {
            (None, _) => {
                rerank_skipped = Some(
                    models
                        .rerank_unavailable
                        .clone()
                        .unwrap_or_else(|| "no reranker attached".into()),
                )
            }
            (Some(_), 0) => rerank_skipped = Some("k_rerank is 0".into()),
            (Some(r), k) => {
                let t0 = Instant::now();
                rerank_model = Some(r.model());
                let window = k.min(hits.len());
                let texts: Vec<String> = hits[..window]
                    .iter()
                    .map(|h| src.doc_text(&h.chunk))
                    .collect();
                let scores = r.rerank_scores(query, &texts).await?;
                if scores.len() != texts.len() {
                    return Err(QuickdocError::RerankCount {
                        want: texts.len(),
                        got: scores.len(),
                    });
                }
                for (h, s) in hits[..window].iter_mut().zip(&scores) {
                    match s {
                        Some(s) => {
                            h.rerank_score = Some(*s);
                            h.score = *s;
                        }
                        // Not scored: it keeps its fused score, flagged.
                        None => h.rerank_skipped = true,
                    }
                }
                // Only the reranked window reorders — scored candidates by
                // their rerank score, then the unscored by fused score; the
                // tail keeps its fused order behind it.
                hits[..window].sort_by(|a, b| {
                    a.rerank_skipped
                        .cmp(&b.rerank_skipped)
                        .then_with(|| {
                            b.score
                                .partial_cmp(&a.score)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .then_with(|| src.doc_id(&a.chunk).cmp(&src.doc_id(&b.chunk)))
                });
                rerank_trace = hits[..window]
                    .iter()
                    .enumerate()
                    .map(|(i, h)| StageHit {
                        chunk_id: src.doc_id(&h.chunk),
                        rank: i + 1,
                        score: h.rerank_score.unwrap_or(h.rrf_score),
                        skipped: h.rerank_skipped,
                    })
                    .collect();
                timings.rerank_ms = ms(t0);
            }
        }
    }

    hits.truncate(params.limit);

    // ---- stage 5: token budget ----
    for h in &mut hits {
        h.tokens = models.tokens.count(&src.doc_text(&h.chunk));
    }
    let (kept, used, dropped) = apply_budget(hits, params.budget_tokens, |d| src.doc_id(d));

    timings.total_ms = ms(cand.started);
    let trace = SearchTrace {
        corpus_id: label,
        embed_model: cand.embed_model.clone(),
        params: params.clone(),
        fts_query: cand.fts_query.clone(),
        fts: cand.fts.clone(),
        knn_skipped: cand.knn_skipped.clone(),
        knn: cand.knn.clone(),
        fused,
        rerank_model,
        rerank_skipped,
        rerank: rerank_trace,
        token_counter: models.tokens.name(),
        budget_used_tokens: used,
        budget_dropped: dropped,
        resident_vectors: src.resident_vectors(),
        resident_bytes: src.resident_bytes(),
        knn_kernel: vector::knn_kernel(),
        timings,
    };
    Ok((kept, trace))
}

/// Keep hits in order while they fit `budget` tokens; `None` keeps all of them.
/// Each hit's `tokens` must already be counted. Returns the kept hits, the
/// tokens they use, and the ids of the ones dropped, in the order they would
/// have come.
///
/// A single top hit bigger than the whole budget still goes out, and the used
/// count says so: an answer that states its overrun beats no answer at all.
/// Public because a caller that merges several searches (knowledge bases
/// across embedding models) budgets the merged list once, with this same
/// rule.
pub fn apply_budget<D>(
    hits: Vec<Hit<D>>,
    budget: Option<usize>,
    id: impl Fn(&D) -> String,
) -> (Vec<Hit<D>>, usize, Vec<String>) {
    let mut used = 0usize;
    let mut dropped = Vec::new();
    let mut kept = Vec::with_capacity(hits.len());
    for h in hits {
        let overruns = budget.is_some_and(|b| used + h.tokens > b);
        if overruns && !kept.is_empty() {
            dropped.push(id(&h.chunk));
            continue;
        }
        used += h.tokens;
        kept.push(h);
    }
    (kept, used, dropped)
}

fn ranked(pairs: Vec<(String, f32)>) -> Vec<StageHit> {
    pairs
        .into_iter()
        .enumerate()
        .map(|(i, (chunk_id, score))| StageHit {
            chunk_id,
            rank: i + 1,
            score,
            skipped: false,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// A docs corpus
// ---------------------------------------------------------------------------

/// A corpus with its vectors resident and its models attached.
///
/// Construction is where §4's query-time verification happens: the embedder's
/// resolved identity must be the one the corpus pinned, or the corpus refuses
/// to open at all.
pub struct Retriever {
    pool: SqlitePool,
    corpus: Corpus,
    matrix: VectorMatrix,
    embedder: Arc<dyn Embedder>,
    reranker: Option<Arc<dyn Reranker>>,
    /// Why there is no reranker, when the host knows more than "there isn't
    /// one" — "no rerank model is enabled on the aux router" is actionable in a
    /// way the default message is not.
    rerank_unavailable: Option<String>,
    tokens: Arc<dyn TokenCounter>,
}

impl std::fmt::Debug for Retriever {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Retriever")
            .field("corpus", &self.corpus.corpus_id())
            .field("embedder", &self.embedder.identity().to_string())
            .field("reranker", &self.reranker.as_ref().map(|r| r.model()))
            .field("resident_vectors", &self.matrix.len())
            .field("resident_bytes", &self.matrix.resident_bytes())
            .finish()
    }
}

impl Retriever {
    pub async fn load(
        pool: SqlitePool,
        corpus_id: i64,
        embedder: Arc<dyn Embedder>,
    ) -> Result<Self> {
        let corpus = store::get_corpus(&pool, corpus_id)
            .await?
            .ok_or_else(|| QuickdocError::CorpusNotFound(corpus_id.to_string()))?;
        Self::from_corpus(pool, corpus, embedder).await
    }

    /// Same, addressed the way clients do: `library@version`.
    pub async fn load_by_id(
        pool: SqlitePool,
        corpus_id: &str,
        embedder: Arc<dyn Embedder>,
    ) -> Result<Self> {
        let corpus = store::get_corpus_by_id(&pool, corpus_id)
            .await?
            .ok_or_else(|| QuickdocError::CorpusNotFound(corpus_id.to_string()))?;
        Self::from_corpus(pool, corpus, embedder).await
    }

    async fn from_corpus(
        pool: SqlitePool,
        corpus: Corpus,
        embedder: Arc<dyn Embedder>,
    ) -> Result<Self> {
        verify_embedder(&corpus, &embedder.identity())?;
        let matrix = store::load_matrix(&pool, &corpus).await?;
        Ok(Self {
            pool,
            corpus,
            matrix,
            embedder,
            reranker: None,
            rerank_unavailable: None,
            tokens: Arc::new(ApproxTokenCounter::default()),
        })
    }

    pub fn with_reranker(mut self, reranker: Arc<dyn Reranker>) -> Self {
        self.reranker = Some(reranker);
        self.rerank_unavailable = None;
        self
    }

    /// Attach the *reason* there is no reranker, which the trace reports
    /// instead of a bare "no reranker attached".
    pub fn with_rerank_unavailable(mut self, reason: impl Into<String>) -> Self {
        self.reranker = None;
        self.rerank_unavailable = Some(reason.into());
        self
    }

    pub fn with_token_counter(mut self, tokens: Arc<dyn TokenCounter>) -> Self {
        self.tokens = tokens;
        self
    }

    pub fn corpus(&self) -> &Corpus {
        &self.corpus
    }

    /// The attached reranker's model, if any — what the trace would report,
    /// without having to run a search to find out.
    pub fn rerank_model(&self) -> Option<String> {
        self.reranker.as_ref().map(|r| r.model())
    }

    /// Why there is none, when the host said.
    pub fn rerank_unavailable(&self) -> Option<&str> {
        self.rerank_unavailable.as_deref()
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// What this corpus costs to serve, for the Docs tab's per-corpus figure.
    pub fn resident_bytes(&self) -> usize {
        self.matrix.resident_bytes()
    }

    pub fn resident_vectors(&self) -> usize {
        self.matrix.len()
    }

    /// Re-slurp the matrix after an ingest or re-embed wrote new vectors.
    pub async fn reload_vectors(&mut self) -> Result<()> {
        self.corpus = store::get_corpus(&self.pool, self.corpus.id)
            .await?
            .ok_or_else(|| QuickdocError::CorpusNotFound(self.corpus.corpus_id()))?;
        self.matrix = store::load_matrix(&self.pool, &self.corpus).await?;
        Ok(())
    }

    pub async fn search(&self, query: &str, params: &SearchParams) -> Result<SearchResult> {
        let models = SearchModels {
            embedder: Some(self.embedder.as_ref()),
            embed_unavailable: None,
            reranker: self.reranker.as_deref(),
            rerank_unavailable: self.rerank_unavailable.clone(),
            tokens: self.tokens.as_ref(),
        };
        let (hits, trace) = hybrid_search(self, &models, query, params).await?;
        Ok(SearchResult {
            corpus_id: self.corpus.corpus_id(),
            hits,
            trace,
        })
    }
}

#[async_trait]
impl SearchSource for Retriever {
    type Doc = Chunk;

    fn label(&self) -> String {
        self.corpus.corpus_id()
    }

    fn dims(&self) -> usize {
        self.corpus.dims()
    }

    fn resident_vectors(&self) -> usize {
        self.matrix.len()
    }

    fn resident_bytes(&self) -> usize {
        self.matrix.resident_bytes()
    }

    async fn fts(&self, expr: &str, w: &FtsWeights, k: usize) -> Result<Vec<(String, f32)>> {
        let rows = sqlx::query(
            "SELECT c.id AS id, bm25(chunk_fts, ?1, ?2, ?3, ?4) AS score
             FROM chunk_fts JOIN chunk c ON c.rowid = chunk_fts.rowid
             WHERE chunk_fts MATCH ?5 AND c.corpus_id = ?6
             ORDER BY score ASC LIMIT ?7",
        )
        .bind(w.payload as f64)
        .bind(w.heading_path as f64)
        .bind(w.derived_title as f64)
        .bind(w.derived_summary as f64)
        .bind(expr)
        .bind(self.corpus.id)
        .bind(k as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get("id"), r.get::<f64, _>("score") as f32))
            .collect())
    }

    fn knn(&self, query: &[f32], k: usize) -> Result<Vec<(String, f32)>> {
        self.matrix.search(&self.corpus.corpus_id(), query, k)
    }

    async fn fetch(&self, ids: &[String]) -> Result<HashMap<String, Chunk>> {
        store::get_chunks(&self.pool, ids).await
    }

    fn doc_id(&self, doc: &Chunk) -> String {
        doc.id.clone()
    }

    fn doc_text(&self, doc: &Chunk) -> String {
        rerank_document(doc)
    }
}

/// §4's hard stop. Dimensions alone are not enough: two models of the same
/// width produce incomparable spaces, and the corpus would answer plausible
/// nonsense forever.
pub fn verify_embedder(corpus: &Corpus, got: &EmbedIdentity) -> Result<()> {
    let pinned = corpus.embed_identity();
    if &pinned != got {
        return Err(QuickdocError::EmbedMismatch {
            corpus: corpus.corpus_id(),
            pinned: pinned.to_string(),
            got: got.to_string(),
        });
    }
    Ok(())
}

/// Build the FTS5 MATCH expression from free text: every term quoted, joined by
/// `OR`.
///
/// User text is never handed to FTS5 raw — a bare `-`, `"` or `NEAR` would
/// either error or silently mean something the caller never asked for. `OR`
/// rather than `AND` because BM25 is what does the discriminating; requiring
/// every term would drop the long-tail queries fusion exists to rescue.
pub fn fts_match_expression(query: &str) -> String {
    tokenize(query)
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Reciprocal Rank Fusion: `Σ 1/(k + rank)` over the stages a chunk appears in.
/// Rank-based, so BM25's negative log-scale scores and cosine similarities
/// never need to be made commensurable.
pub fn rrf_fuse(fts: &[StageHit], knn: &[StageHit], rrf_k: f32) -> Vec<FusedHit> {
    let mut acc: HashMap<&str, (f32, Option<usize>, Option<usize>)> = HashMap::new();
    for h in fts {
        let e = acc.entry(h.chunk_id.as_str()).or_insert((0.0, None, None));
        e.0 += 1.0 / (rrf_k + h.rank as f32);
        e.1 = Some(h.rank);
    }
    for h in knn {
        let e = acc.entry(h.chunk_id.as_str()).or_insert((0.0, None, None));
        e.0 += 1.0 / (rrf_k + h.rank as f32);
        e.2 = Some(h.rank);
    }
    let mut out: Vec<FusedHit> = acc
        .into_iter()
        .map(|(id, (score, fr, kr))| FusedHit {
            chunk_id: id.to_string(),
            rrf_score: score,
            fts_rank: fr,
            knn_rank: kr,
        })
        .collect();
    // Ties broken by id so a fused list is reproducible across runs.
    out.sort_by(|a, b| {
        b.rrf_score
            .partial_cmp(&a.rrf_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.chunk_id.cmp(&b.chunk_id))
    });
    out
}

/// What the reranker scores and the budget counts: the heading path plus the
/// verbatim payload, which is what a caller actually receives.
fn rerank_document(c: &Chunk) -> String {
    if c.heading_path.is_empty() {
        c.payload.clone()
    } else {
        format!("{}\n{}", c.heading_path, c.payload)
    }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: &str, rank: usize) -> StageHit {
        StageHit {
            chunk_id: id.into(),
            rank,
            score: 0.0,
            skipped: false,
        }
    }

    #[test]
    fn quotes_every_term_and_survives_punctuation() {
        assert_eq!(
            fts_match_expression("axum -NEAR \"router\""),
            "\"axum\" OR \"near\" OR \"router\""
        );
        assert_eq!(fts_match_expression("!!!"), "");
    }

    #[test]
    fn fusion_rewards_agreement_between_the_stages() {
        let fts = vec![hit("a", 1), hit("b", 2)];
        let knn = vec![hit("b", 1), hit("c", 2)];
        let fused = rrf_fuse(&fts, &knn, 60.0);
        assert_eq!(fused[0].chunk_id, "b", "in both lists beats first in one");
        assert_eq!(fused[0].fts_rank, Some(2));
        assert_eq!(fused[0].knn_rank, Some(1));
        assert_eq!(fused.len(), 3);
        assert!(fused[1].rrf_score > fused[2].rrf_score);
    }

    /// A store that is not the corpus schema at all: two documents in a
    /// vector, BM25 faked by term overlap. What the core needs from a source is
    /// the trait, nothing about tables.
    struct TwoDocs {
        docs: Vec<(String, String)>,
        matrix: VectorMatrix,
    }

    impl TwoDocs {
        fn new() -> Self {
            let e = crate::embed::FixtureEmbedder::new(16);
            let docs = vec![
                ("a".to_string(), "invoice total for march".to_string()),
                ("b".to_string(), "holiday plan for august".to_string()),
            ];
            let mut matrix = VectorMatrix::new(16);
            for (id, text) in &docs {
                matrix
                    .push_vector("two", id.clone(), &e.embed_one(text))
                    .unwrap();
            }
            Self { docs, matrix }
        }
    }

    #[async_trait]
    impl SearchSource for TwoDocs {
        type Doc = (String, String);
        fn label(&self) -> String {
            "two".into()
        }
        fn dims(&self) -> usize {
            16
        }
        fn resident_vectors(&self) -> usize {
            self.matrix.len()
        }
        fn resident_bytes(&self) -> usize {
            self.matrix.resident_bytes()
        }
        async fn fts(&self, expr: &str, _: &FtsWeights, k: usize) -> Result<Vec<(String, f32)>> {
            let terms: Vec<String> = tokenize(expr);
            let mut out: Vec<(String, f32)> = self
                .docs
                .iter()
                .filter_map(|(id, t)| {
                    let n = tokenize(t).iter().filter(|w| terms.contains(w)).count();
                    (n > 0).then(|| (id.clone(), -(n as f32)))
                })
                .collect();
            out.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            out.truncate(k);
            Ok(out)
        }
        fn knn(&self, query: &[f32], k: usize) -> Result<Vec<(String, f32)>> {
            self.matrix.search("two", query, k)
        }
        async fn fetch(&self, ids: &[String]) -> Result<HashMap<String, Self::Doc>> {
            Ok(self
                .docs
                .iter()
                .filter(|(id, _)| ids.contains(id))
                .map(|d| (d.0.clone(), d.clone()))
                .collect())
        }
        fn doc_id(&self, doc: &Self::Doc) -> String {
            doc.0.clone()
        }
        fn doc_text(&self, doc: &Self::Doc) -> String {
            doc.1.clone()
        }
    }

    #[tokio::test]
    async fn the_core_runs_over_any_source_and_says_why_a_stage_was_skipped() {
        let src = TwoDocs::new();
        let tokens = ApproxTokenCounter::default();
        let embedder = crate::embed::FixtureEmbedder::new(16);
        let params = SearchParams {
            rerank: false,
            ..Default::default()
        };
        let models = SearchModels {
            embedder: Some(&embedder),
            embed_unavailable: None,
            reranker: None,
            rerank_unavailable: None,
            tokens: &tokens,
        };
        let (hits, trace) = hybrid_search(&src, &models, "march invoice", &params)
            .await
            .unwrap();
        assert_eq!(hits[0].chunk.0, "a");
        assert!(hits[0].fts_rank.is_some() && hits[0].knn_rank.is_some());
        assert!(trace.knn_skipped.is_none());

        // No embedder: BM25 alone answers, and the trace names the reason.
        let models = SearchModels {
            embedder: None,
            embed_unavailable: Some("the GPU hold is on".into()),
            ..models
        };
        let (hits, trace) = hybrid_search(&src, &models, "august", &params)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk.0, "b");
        assert!(hits[0].knn_rank.is_none());
        assert!(trace.knn.is_empty());
        assert_eq!(trace.knn_skipped.as_deref(), Some("the GPU hold is on"));
        assert_eq!(trace.embed_model, "");
    }

    /// Scores the first document only; the second is "over its limit".
    struct Partial;

    #[async_trait]
    impl Reranker for Partial {
        fn model(&self) -> String {
            "partial".into()
        }
        async fn rerank(&self, _q: &str, _d: &[String]) -> Result<Vec<f32>> {
            unreachable!("the pipeline asks for rerank_scores")
        }
        async fn rerank_scores(&self, _q: &str, d: &[String]) -> Result<Vec<Option<f32>>> {
            Ok(d.iter()
                .enumerate()
                .map(|(i, _)| (i == 0).then_some(-5.0))
                .collect())
        }
    }

    /// A candidate the reranker did not score keeps its fused score, is
    /// marked, and ranks behind the scored ones — even behind a scored one
    /// whose (real) score is lower than the fused score.
    #[tokio::test]
    async fn an_unscored_candidate_keeps_its_fused_score_and_is_flagged() {
        let src = TwoDocs::new();
        let tokens = ApproxTokenCounter::default();
        let embedder = crate::embed::FixtureEmbedder::new(16);
        let params = SearchParams::default();
        let models = SearchModels {
            embedder: Some(&embedder),
            embed_unavailable: None,
            reranker: Some(&Partial),
            rerank_unavailable: None,
            tokens: &tokens,
        };
        let (hits, trace) = hybrid_search(&src, &models, "march invoice august", &params)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(!hits[0].rerank_skipped);
        assert_eq!(hits[0].rerank_score, Some(-5.0));
        assert!(hits[1].rerank_skipped);
        assert_eq!(hits[1].rerank_score, None);
        assert_eq!(hits[1].score, hits[1].rrf_score, "no invented score");
        let skipped: Vec<bool> = trace.rerank.iter().map(|h| h.skipped).collect();
        assert_eq!(skipped, [false, true]);
        assert_eq!(trace.rerank[1].score, hits[1].rrf_score);
    }

    /// The stages after a failure resume from stage 1–2's results.
    #[tokio::test]
    async fn ranking_resumes_from_the_candidates_without_searching_again() {
        let src = TwoDocs::new();
        let tokens = ApproxTokenCounter::default();
        let embedder = crate::embed::FixtureEmbedder::new(16);
        let params = SearchParams::default();
        let (fts_query, fts, fts_ms) = fts_stage(&src, "march invoice", &params).await.unwrap();
        let (knn, e_ms, k_ms) = knn_stage(&src, &embedder, "march invoice", &params)
            .await
            .unwrap();
        let cand = Candidates::keywords(Instant::now(), fts_query, fts, fts_ms, "").with_vectors(
            embedder.identity().to_string(),
            knn,
            e_ms,
            k_ms,
        );
        let models = SearchModels {
            embedder: None,
            embed_unavailable: None,
            reranker: None,
            rerank_unavailable: Some("the reranker failed".into()),
            tokens: &tokens,
        };
        let (hits, trace) = rank_candidates(&src, &models, "march invoice", &params, &cand)
            .await
            .unwrap();
        assert_eq!(hits[0].chunk.0, "a");
        assert!(trace.knn_skipped.is_none() && !trace.knn.is_empty());
        assert_eq!(trace.rerank_skipped.as_deref(), Some("the reranker failed"));
        assert_eq!(trace.embed_model, embedder.identity().to_string());
    }

    #[test]
    fn the_budget_keeps_an_oversized_first_hit_and_names_what_it_dropped() {
        let hit = |id: &str, tokens| Hit {
            chunk: id.to_string(),
            score: 0.0,
            rrf_score: 0.0,
            fts_rank: None,
            knn_rank: None,
            knn_score: None,
            rerank_score: None,
            rerank_skipped: false,
            tokens,
        };
        let (kept, used, dropped) = apply_budget(
            vec![hit("a", 50), hit("b", 5), hit("c", 1)],
            Some(10),
            |d| d.clone(),
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(used, 50);
        assert_eq!(dropped, ["b", "c"]);
        let (kept, used, _) = apply_budget(vec![hit("a", 4), hit("b", 5)], None, |d| d.clone());
        assert_eq!((kept.len(), used), (2, 9));
    }

    #[test]
    fn a_stage_that_found_nothing_leaves_the_other_intact() {
        let knn = vec![hit("a", 1), hit("b", 2)];
        let fused = rrf_fuse(&[], &knn, 60.0);
        assert_eq!(fused[0].chunk_id, "a");
        assert!(fused[0].fts_rank.is_none());
    }
}
