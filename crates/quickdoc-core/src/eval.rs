//! Golden-query scoring: hit@k and MRR (§2, §12 step 1).
//!
//! The number this produces is the `eval_score` `docs__resolve` reports and the
//! Docs tab badges, so retrieval tuning is measured rather than argued.

use serde::Serialize;

use crate::error::Result;
use crate::retrieve::{Retriever, SearchParams};
use crate::store::{self, GoldenQuery};

#[derive(Debug, Clone, Serialize)]
pub struct QueryScore {
    pub golden_query_id: i64,
    pub query: String,
    /// How many chunks the query declares as right answers.
    pub expected: usize,
    /// 1-based rank of the first expected chunk, when one was retrieved.
    pub first_hit_rank: Option<usize>,
    pub reciprocal_rank: f32,
    /// Expected ids the corpus no longer contains. §4: an orphan surfaces here
    /// instead of quietly dragging the score down.
    pub orphaned_chunk_ids: Vec<String>,
    pub retrieved: Vec<String>,
}

impl QueryScore {
    pub fn hit(&self) -> bool {
        self.first_hit_rank.is_some()
    }

    /// Every chunk this query expects is gone from the corpus. It scores zero,
    /// but for a reason the eval view can name and the owner can fix.
    pub fn fully_orphaned(&self) -> bool {
        self.expected > 0 && self.orphaned_chunk_ids.len() == self.expected
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub corpus_id: String,
    pub k: usize,
    pub queries: usize,
    /// Fraction of queries with an expected chunk in the top `k`.
    pub hit_at_k: f32,
    pub mrr: f32,
    /// Queries whose every expected chunk is gone — they score zero, but for a
    /// reason the eval view can name.
    pub orphaned_queries: usize,
    pub per_query: Vec<QueryScore>,
}

/// The stage settings one eval measures under: `params` with `limit` pinned to
/// `k`, because that is what hit@k means.
///
/// Split out so a caller driving the queries itself (the `eval_run` job, which
/// reports progress and honours cancel between them) measures under exactly the
/// same settings [`run`] would.
pub fn params_at_k(params: &SearchParams, k: usize) -> SearchParams {
    let mut params = params.clone();
    params.limit = k;
    if params.k_rerank > 0 {
        params.k_rerank = params.k_rerank.max(k);
    }
    params
}

/// Score one golden query. `params` must already be [`params_at_k`]-shaped.
pub async fn score_query(
    retriever: &Retriever,
    golden: &GoldenQuery,
    params: &SearchParams,
) -> Result<QueryScore> {
    let result = retriever.search(&golden.query, params).await?;
    let retrieved: Vec<String> = result.hits.iter().map(|h| h.chunk.id.clone()).collect();
    let present = store::get_chunks(retriever.pool(), &golden.expected_chunk_ids).await?;
    let orphaned_chunk_ids: Vec<String> = golden
        .expected_chunk_ids
        .iter()
        .filter(|id| !present.contains_key(*id))
        .cloned()
        .collect();
    let first_hit_rank = retrieved
        .iter()
        .position(|id| golden.expected_chunk_ids.contains(id))
        .map(|i| i + 1);
    Ok(QueryScore {
        golden_query_id: golden.id,
        query: golden.query.clone(),
        expected: golden.expected_chunk_ids.len(),
        first_hit_rank,
        reciprocal_rank: first_hit_rank.map_or(0.0, |r| 1.0 / r as f32),
        orphaned_chunk_ids,
        retrieved,
    })
}

/// Aggregate per-query scores into the report the corpus's badge is set from.
///
/// A query whose every expected chunk is gone scores zero either way; counting
/// it separately is what keeps "retrieval got worse" distinguishable from "the
/// golden queries went stale" (§4).
pub fn report(corpus_id: String, k: usize, per_query: Vec<QueryScore>) -> EvalReport {
    let n = per_query.len();
    let hits = per_query.iter().filter(|q| q.hit()).count();
    let rr_sum: f32 = per_query.iter().map(|q| q.reciprocal_rank).sum();
    let orphaned_queries = per_query.iter().filter(|q| q.fully_orphaned()).count();
    EvalReport {
        corpus_id,
        k,
        queries: n,
        hit_at_k: if n == 0 { 0.0 } else { hits as f32 / n as f32 },
        mrr: if n == 0 { 0.0 } else { rr_sum / n as f32 },
        orphaned_queries,
        per_query,
    }
}

/// Run every golden query of the retriever's corpus at depth `k`.
///
/// `params` carries the stage settings under test. Nothing else is touched, so
/// an optimisation agent can sweep `k_fts`/`k_vec`/`rrf_k` and compare like
/// with like.
pub async fn run(retriever: &Retriever, k: usize, params: &SearchParams) -> Result<EvalReport> {
    let corpus = retriever.corpus();
    let golden = store::list_golden_queries(retriever.pool(), corpus.id).await?;
    let params = params_at_k(params, k);
    let mut per_query = Vec::with_capacity(golden.len());
    for g in &golden {
        per_query.push(score_query(retriever, g, &params).await?);
    }
    Ok(report(corpus.corpus_id(), k, per_query))
}
