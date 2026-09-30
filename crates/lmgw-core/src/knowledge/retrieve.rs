//! Hybrid search over a set of knowledge bases (chat-complete design §9.3,
//! §9.4), through quickdoc-core's one pipeline
//! ([`hybrid_search`](quickdoc_core::retrieve::hybrid_search)) — this module
//! is only the [`SearchSource`] over `knowledge.db` and the assembly around
//! it.
//!
//! **One search per model pair.** Bases that share an embedding identity and
//! a rerank model are searched as one source: one BM25 over all of them, one
//! KNN (each base's resident matrix scanned, the best of all kept — exact, the
//! cosines are one space), one fusion, one rerank. Bases on different models
//! cannot share a vector space, so each group is searched on its own and the
//! group rankings are fused by rank (RRF, the owner's `rrf_k`), which needs no
//! score to be comparable across models.
//!
//! **The budget is applied once**, to the merged list, with quickdoc-core's
//! rule ([`apply_budget`]): excerpts in rank order while they fit, the first
//! one always, and what was dropped counted.
//!
//! **It never fails.** A base that is gone, still ingesting, mid re-embed, or
//! whose embedding model is held or no longer resolves is searched as far as
//! it can be — BM25 alone when the vector stage cannot run — and the reason is
//! a note on the result. The Chat sends its turn either way and shows why
//! (§9.3 "Empty or failing retrieval never blocks the turn").

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use quickdoc_core::embed::Embedder;
use quickdoc_core::retrieve::{
    apply_budget, fts_stage, knn_stage, rank_candidates, Candidates, FtsWeights, Hit, SearchModels,
    SearchParams, SearchSource, SearchTrace,
};
use quickdoc_core::vector::VectorMatrix;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use quickdoc_core::embed::Reranker;
use quickdoc_core::error::QuickdocError;

use super::retrieve_guard::{BoundedEmbedder, BoundedReranker, Notes};
use crate::quickdoc::embed::refuse_if_held;
use crate::quickdoc::query::{default_params, TiktokenCounter};
use crate::quickdoc::{InProcessEmbedder, InProcessReranker};
use crate::state::SharedState;

use super::store::{self, Kb, KbChunk};

/// The reason clause a held reranker refusal carries.
const RERANK_HOLD_WHY: &str = "a knowledge base ranks with the model it names, so it will not \
     rank through a fallback either";

/// One retrieved excerpt: what the Chat stores with a user message (its
/// `context`), numbers in the `<context>` block and cites as `[n]`, and what
/// `kb__search` returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Excerpt {
    pub kb_id: i64,
    /// The base's name.
    pub kb: String,
    pub file_id: i64,
    /// The file's name.
    pub file: String,
    /// 1-based PDF page; `None` for other files.
    pub page: Option<i64>,
    pub chunk_id: String,
    pub heading_path: String,
    /// The chunk's payload, verbatim.
    pub text: String,
    /// The final order's score within its search: the reranker's when it
    /// ran, RRF's otherwise — and RRF's for an excerpt the reranker did not
    /// score ([`Self::rerank_skipped`]).
    pub score: f32,
    /// The rerank stage ran but did not score this excerpt (its query + chunk
    /// pair was over the reranker's input limit): `score` is its fused score
    /// and it ranks behind the reranked ones.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rerank_skipped: bool,
    /// Counted over heading path + text, as the budget counted it.
    pub tokens: usize,
    /// Byte range in the file's extracted text — the source viewer's
    /// highlight ([`super::read::source`]).
    pub span_start: i64,
    pub span_end: i64,
    /// The sha256 of the file version the chunk was cut from. A citation
    /// stored with a Chat message keeps it, so the source viewer can say the
    /// document changed since ([`super::read::source`]).
    #[serde(default)]
    pub file_sha: String,
}

/// What a retrieval found and what it could not do.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Retrieval {
    pub excerpts: Vec<Excerpt>,
    /// Tokens the excerpts use (heading paths included).
    pub tokens: usize,
    /// Excerpts that ranked but did not fit the budget.
    pub dropped: usize,
    /// Why something was not searched, or not fully — one sentence each.
    pub notes: Vec<String>,
    pub ms: f64,
    /// The bases that were searched, by name.
    pub searched: Vec<String>,
    /// One per search (per model pair), for the playground.
    pub traces: Vec<SearchTrace>,
}

/// How to run one retrieval.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Excerpt budget in tokens; `None` returns every ranked excerpt up to
    /// the search's `limit`.
    pub budget_tokens: Option<usize>,
    /// Stage parameters. `None` starts from the owner's search defaults
    /// (`Settings.docs_search`); with a budget, `limit` is then raised to the
    /// whole fused depth so that the budget — the visible bound — is what
    /// decides how much comes back.
    pub params: Option<SearchParams>,
}

/// Search `kb_ids` for `query`. Never fails; see the module doc.
pub async fn retrieve(
    state: &SharedState,
    kb_ids: &[i64],
    query: &str,
    opts: &Options,
) -> Retrieval {
    let t0 = Instant::now();
    let mut out = Retrieval::default();
    let query = query.trim();
    if query.is_empty() {
        out.notes.push("the query is empty".into());
        return out;
    }
    let pool = &state.knowledge.pool;
    // Only the bases asked for, counted once.
    let counts = store::kb_counts_for(pool, kb_ids).await.unwrap_or_default();

    let mut params = match &opts.params {
        Some(p) => p.clone(),
        None => {
            let mut p = default_params(&state.snapshot());
            if opts.budget_tokens.is_some() {
                p.limit = p.limit.max(p.k_fts + p.k_vec);
            }
            p
        }
    };
    // Applied once, to the merged list.
    params.budget_tokens = None;

    // Group by (embedding identity, rerank alias).
    let mut groups: BTreeMap<(String, String), Vec<Kb>> = BTreeMap::new();
    let mut seen = std::collections::HashSet::new();
    for id in kb_ids {
        if !seen.insert(*id) {
            continue;
        }
        let kb = match store::get_kb(pool, *id).await {
            Ok(Some(kb)) => kb,
            Ok(None) => {
                out.notes
                    .push(format!("knowledge base {id} no longer exists"));
                continue;
            }
            Err(e) => {
                out.notes.push(format!("knowledge base {id}: {e}"));
                continue;
            }
        };
        let c = counts.get(&kb.id).cloned().unwrap_or_default();
        if c.chunks == 0 {
            out.notes.push(if c.pending + c.ingesting > 0 {
                format!(
                    "'{}' is still ingesting ({} file(s) waiting) — nothing to search in it yet",
                    kb.name,
                    c.pending + c.ingesting
                )
            } else {
                format!("'{}' has no documents yet", kb.name)
            });
            continue;
        }
        if c.pending + c.ingesting > 0 {
            out.notes.push(format!(
                "'{}': {} file(s) still ingesting — searched the {} that are ready",
                kb.name,
                c.pending + c.ingesting,
                c.ready
            ));
        }
        if c.embedded < c.chunks {
            out.notes.push(format!(
                "'{}' is being re-embedded: {} of {} chunks have a vector, so the vector \
                 search sees only those (keyword search sees all)",
                kb.name, c.embedded, c.chunks
            ));
        }
        out.searched.push(kb.name.clone());
        groups
            .entry((kb.embed_identity().to_string(), kb.rerank_alias.clone()))
            .or_default()
            .push(kb);
    }

    let mut ranked: Vec<Vec<Hit<KbChunk>>> = Vec::new();
    for kbs in groups.into_values() {
        if let Some((hits, trace)) =
            search_group(state, pool, kbs, query, &params, &mut out.notes).await
        {
            ranked.push(hits);
            out.traces.push(trace);
        }
    }

    let merged = merge(ranked, params.rrf_k);
    let (kept, used, dropped) = apply_budget(merged, opts.budget_tokens, |c| c.id.clone());
    out.tokens = used;
    out.dropped = dropped.len();
    out.excerpts = kept
        .into_iter()
        .map(|h| Excerpt {
            kb_id: h.chunk.kb_id,
            kb: h.chunk.kb_name.clone(),
            file_id: h.chunk.file_id,
            file: h.chunk.file_name.clone(),
            page: h.chunk.page,
            chunk_id: h.chunk.id.clone(),
            heading_path: h.chunk.heading_path.clone(),
            text: h.chunk.payload.clone(),
            score: h.score,
            rerank_skipped: h.rerank_skipped,
            tokens: h.tokens,
            span_start: h.chunk.span_start,
            span_end: h.chunk.span_end,
            file_sha: h.chunk.file_sha.clone(),
        })
        .collect();
    out.ms = t0.elapsed().as_secs_f64() * 1000.0;
    out
}

/// One group's search, with the degradations the module doc promises.
async fn search_group(
    state: &SharedState,
    pool: &SqlitePool,
    kbs: Vec<Kb>,
    query: &str,
    params: &SearchParams,
    notes: &mut Vec<String>,
) -> Option<(Vec<Hit<KbChunk>>, SearchTrace)> {
    let names = kbs
        .iter()
        .map(|k| format!("'{}'", k.name))
        .collect::<Vec<_>>()
        .join(", ");
    let first = kbs[0].clone();

    // Vectors: each base's resident matrix.
    let mut parts = Vec::with_capacity(kbs.len());
    for kb in kbs {
        match state.knowledge.matrix(&kb).await {
            Ok(m) => parts.push((kb, m)),
            Err(e) => {
                notes.push(format!(
                    "{}: its vectors could not be loaded ({e})",
                    kb.label()
                ));
                parts.push((kb.clone(), Arc::new(VectorMatrix::new(kb.dims()))));
            }
        }
    }
    let src = KbSource {
        pool: pool.clone(),
        dims: first.dims(),
        kbs: parts,
    };

    // The pinned embedder — or why the vector stage cannot run.
    let mut first_alias: Option<String> = None;
    let (embedder, embed_unavailable): (Option<Arc<dyn Embedder>>, Option<String>) =
        match InProcessEmbedder::for_identity(
            state.clone(),
            &first.embed_identity(),
            &first.label(),
            "change its embedding model",
        )
        .await
        {
            Ok(e) => match refuse_if_held(state, e.alias(), super::ingest::HOLD_WHY) {
                Ok(()) => {
                    first_alias = Some(e.alias().to_string());
                    (Some(e.into_arc()), None)
                }
                Err(held) => (None, Some(held.to_string())),
            },
            Err(e) => (None, Some(e.to_string())),
        };
    if let Some(why) = &embed_unavailable {
        notes.push(format!(
            "{names}: keyword search only — the vector search could not run: {why}"
        ));
    }

    // The base's rerank model, when it has one.
    let alias = first.rerank_alias.trim();
    let (reranker, rerank_unavailable) = if alias.is_empty() {
        (
            None,
            Some("no rerank model is set on this knowledge base".to_string()),
        )
    } else {
        match InProcessReranker::for_alias(state.clone(), alias) {
            // A held local reranker is refused up front, like the embedder:
            // its fallback would rank with another model.
            Ok(r) => match refuse_if_held(state, alias, RERANK_HOLD_WHY) {
                Ok(()) => (Some(r.into_arc()), None),
                Err(held) => {
                    notes.push(format!("{names}: not reranked — {held}"));
                    (None, Some(held.to_string()))
                }
            },
            Err(e) => {
                notes.push(format!("{names}: not reranked — {e}"));
                (None, Some(e.to_string()))
            }
        }
    };

    // The query and the rerank pairs are held to the models' per-input
    // limits; what that cuts is a note. Each model's tokenizer ratio is
    // measured on this base's chunks once per process, so a turn itself costs
    // no tokenizer call.
    if embedder.is_some() {
        if let Some(a) = &first_alias {
            super::fit::prime_ratio(state, &first, a).await;
        }
    }
    if reranker.is_some() {
        super::fit::prime_ratio(state, &first, alias).await;
    }
    let guard_notes: Notes = Notes::default();
    let embedder: Option<Arc<dyn Embedder>> = match (embedder, &first_alias) {
        (Some(e), Some(alias)) => Some(Arc::new(BoundedEmbedder {
            inner: e,
            state: state.clone(),
            alias: alias.clone(),
            notes: guard_notes.clone(),
        })),
        (e, _) => e,
    };
    let mut reranker: Option<Arc<dyn Reranker>> = reranker.map(|r| {
        Arc::new(BoundedReranker {
            inner: r,
            state: state.clone(),
            alias: alias.to_string(),
            notes: guard_notes.clone(),
        }) as Arc<dyn Reranker>
    });
    let mut embed_unavailable = embed_unavailable;
    let mut rerank_unavailable = rerank_unavailable;

    // Degrade one stage at a time, keeping every stage that still works:
    // full → without the reranker → without the vector stage → keywords. What
    // already ran is kept: BM25 runs once, the query is embedded once, and a
    // failing reranker sends the ranking back over the same candidates.
    let tokens = TiktokenCounter;
    let started = Instant::now();
    let (fts_query, fts, fts_ms) = match fts_stage(&src, query, params).await {
        Ok(r) => r,
        Err(e) => {
            notes.push(format!("{names}: not searched — {e}"));
            return None;
        }
    };
    let mut cand = Candidates::keywords(
        started,
        fts_query,
        fts,
        fts_ms,
        embed_unavailable
            .clone()
            .unwrap_or_else(|| "no embedder attached".into()),
    );
    if let Some(e) = embedder.as_deref() {
        match knn_stage(&src, e, query, params).await {
            Ok((knn, embed_ms, knn_ms)) => {
                cand = cand.with_vectors(e.identity().to_string(), knn, embed_ms, knn_ms);
            }
            Err(err) => {
                notes.push(format!(
                    "{names}: keyword search only — the vector search failed: {err}"
                ));
                embed_unavailable = Some(err.to_string());
                cand = Candidates::keywords(
                    started,
                    cand.fts_query.clone(),
                    cand.fts.clone(),
                    fts_ms,
                    err.to_string(),
                );
            }
        }
    }
    let result = loop {
        let models = SearchModels {
            embedder: None,
            embed_unavailable: embed_unavailable.clone(),
            reranker: reranker.as_deref(),
            rerank_unavailable: rerank_unavailable.clone(),
            tokens: &tokens,
        };
        match rank_candidates(&src, &models, query, params, &cand).await {
            Ok(r) => break Some(r),
            Err(e @ (QuickdocError::Reranker(_) | QuickdocError::RerankCount { .. }))
                if reranker.is_some() =>
            {
                notes.push(format!(
                    "{names}: reranker unavailable: {e} — the keyword and vector results are \
                     kept, in their fused order"
                ));
                rerank_unavailable = Some(e.to_string());
                reranker = None;
            }
            Err(e) => {
                notes.push(format!("{names}: not searched — {e}"));
                break None;
            }
        }
    };
    notes.extend(
        guard_notes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .map(|n| format!("{names}: {n}")),
    );
    result
}

/// Several groups' rankings as one: reciprocal-rank fusion over their ranks,
/// ties broken by chunk id. One group passes through in its own order.
fn merge(mut groups: Vec<Vec<Hit<KbChunk>>>, rrf_k: f32) -> Vec<Hit<KbChunk>> {
    if groups.len() <= 1 {
        return groups.pop().unwrap_or_default();
    }
    let mut scored: Vec<(f32, Hit<KbChunk>)> = Vec::new();
    for g in groups {
        for (i, h) in g.into_iter().enumerate() {
            scored.push((1.0 / (rrf_k + (i + 1) as f32), h));
        }
    }
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.chunk.id.cmp(&b.1.chunk.id))
    });
    scored.into_iter().map(|(_, h)| h).collect()
}

/// A set of bases with one embedding identity, as quickdoc-core's pipeline
/// sees them.
struct KbSource {
    pool: SqlitePool,
    dims: usize,
    kbs: Vec<(Kb, Arc<VectorMatrix>)>,
}

#[async_trait]
impl SearchSource for KbSource {
    type Doc = KbChunk;

    fn label(&self) -> String {
        self.kbs
            .iter()
            .map(|(k, _)| k.name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn dims(&self) -> usize {
        self.dims
    }

    fn resident_vectors(&self) -> usize {
        self.kbs.iter().map(|(_, m)| m.len()).sum()
    }

    fn resident_bytes(&self) -> usize {
        self.kbs.iter().map(|(_, m)| m.resident_bytes()).sum()
    }

    /// One FTS5 index holds every base, so BM25 scores of different bases are
    /// on one scale: the per-base lists merge by score.
    async fn fts(
        &self,
        expr: &str,
        w: &FtsWeights,
        k: usize,
    ) -> quickdoc_core::Result<Vec<(String, f32)>> {
        let mut all = Vec::new();
        for (kb, _) in &self.kbs {
            all.extend(store::fts(&self.pool, kb.id, expr, w, k).await?);
        }
        all.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        all.truncate(k);
        Ok(all)
    }

    /// Every base's matrix scanned, the best `k` of all of them kept: exact,
    /// because the cosines are one embedding space.
    fn knn(&self, query: &[f32], k: usize) -> quickdoc_core::Result<Vec<(String, f32)>> {
        let mut all = Vec::new();
        for (kb, m) in &self.kbs {
            if m.is_empty() {
                continue;
            }
            all.extend(m.search(&kb.label(), query, k)?);
        }
        all.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        all.truncate(k);
        Ok(all)
    }

    async fn fetch(&self, ids: &[String]) -> quickdoc_core::Result<HashMap<String, KbChunk>> {
        Ok(store::get_chunks(&self.pool, ids).await?)
    }

    fn doc_id(&self, doc: &KbChunk) -> String {
        doc.id.clone()
    }

    fn doc_text(&self, doc: &KbChunk) -> String {
        doc.text()
    }
}
