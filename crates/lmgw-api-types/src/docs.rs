//! quickdoc — the docs plane (`/api/docs/…`, quickdoc §10 + §11)
//!
//! Tolerant mirrors of what `web/api_docs.rs` serializes (which is in turn
//! `quickdoc_core::store` / `retrieve` / `portability`). Same convention as
//! `downloads` and `jobs`: the server serializes its own structs, these stay
//! field-compatible, and every field is `default` so one side can grow first.

use serde::{Deserialize, Serialize};

/// The §6 stage defaults (`Settings.docs_search`) — where a request that
/// overrides nothing starts. None of these is a cap: every one is a
/// per-request parameter the playground and `docs__query` can override.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocsSearchDefaults {
    pub k_fts: u32,
    pub k_vec: u32,
    pub rrf_k: f32,
    /// BM25 column weights — a stage default like any other, and one the
    /// playground exposes, so it has to round-trip through a save.
    pub fts_weights: FtsWeightsView,
    /// `0` disables the rerank stage.
    pub k_rerank: u32,
    pub limit: u32,
    /// `0` means *no budget* — every one of `limit` chunks comes back.
    pub budget_tokens: u32,
    /// Depth an eval run measures hit@k at when the request names no `k`.
    pub eval_k: u32,
}

impl Default for DocsSearchDefaults {
    /// Mirrors `quickdoc_core::retrieve::SearchParams::default()`, so a UI that
    /// renders before `/api/settings-full` answers shows the real defaults
    /// rather than zeros.
    fn default() -> Self {
        Self {
            k_fts: 50,
            k_vec: 50,
            rrf_k: 60.0,
            fts_weights: FtsWeightsView::default(),
            k_rerank: 20,
            limit: 10,
            budget_tokens: 0,
            eval_k: 10,
        }
    }
}

/// `GET /api/docs/corpora` — the Docs tab's list, its badges, and the two
/// gateway-level facts it needs alongside them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocsOverview {
    pub corpora: Vec<CorpusView>,
    /// Pending `docs__request` rows — the tab badge.
    pub pending_requests: u64,
    /// Alias the rerank stage would call, absent when there is none.
    pub rerank_model: Option<String>,
    pub schema_version: i64,
}

/// One corpus as the list and the detail view both show it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CorpusView {
    pub id: i64,
    /// The client-facing `library@version`, as `docs__resolve` reports it.
    pub corpus_id: String,
    pub library: String,
    pub version: String,
    /// `ready | ingesting | re_embed_required | …` — free-form, a chip.
    pub status: String,
    pub chunk_count: i64,
    /// Chunks with no vector yet; non-zero means the KNN stage sees only part
    /// of the corpus.
    pub unembedded_chunks: i64,
    /// What serving this corpus costs once resident (§5): the visible price of
    /// the exact-KNN scan.
    pub resident_bytes: i64,
    pub embed_upstream: String,
    pub embed_model: String,
    pub embed_dims: i64,
    /// `upstream/model@dims`, the pinned identity in one string.
    pub embed_identity: String,
    pub ingest_model: String,
    pub ingest_prompt_version: String,
    pub crawl_date: String,
    pub source_kind: String,
    /// Latest measured hit@k; absent = never evaluated, which is not zero.
    pub eval_score: Option<f64>,
    pub eval_best: Option<f64>,
    pub eval_k: i64,
    pub eval_at: String,
    pub eval_regression: bool,
    /// `ok | re_embed_required` — the same flag `docs__resolve` reports.
    pub embed_status: String,
    /// `ok | regression | unmeasured`.
    pub eval_status: String,
    /// Machine-readable badges; `warnings` is one sentence per flag.
    pub flags: Vec<String>,
    pub warnings: Vec<String>,
    pub sources: Vec<CorpusSource>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CorpusSource {
    pub id: i64,
    pub corpus_id: i64,
    pub root: String,
    /// `llms_txt | markdown | rustdoc_json | html`.
    pub kind: String,
    /// Domains a fetch may touch; empty = the root's own host (§8).
    pub fence: Vec<String>,
    pub created_at: String,
}

/// `GET /api/docs/corpora/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CorpusDetail {
    pub corpus: CorpusView,
    pub documents: Vec<DocumentRow>,
    pub golden_queries: Vec<GoldenQueryRow>,
    pub eval_runs: Vec<EvalRunRow>,
}

/// `GET /api/docs/corpora/{id}/documents`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocumentsResponse {
    pub documents: Vec<DocumentRow>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocumentRow {
    pub id: i64,
    pub source_id: i64,
    pub url: String,
    /// Gates incremental re-ingest: unchanged pages never re-run the model.
    pub content_hash: String,
    pub fetched_at: String,
}

/// `GET /api/docs/chunks?document_id=`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChunksResponse {
    pub chunks: Vec<ChunkRow>,
}

/// One chunk. `payload` is a **verbatim** slice of the source document and is
/// rendered as such — never as markdown, since it carries its own fences.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChunkRow {
    pub id: String,
    pub document_id: i64,
    pub corpus_id: i64,
    pub heading_path: String,
    pub span_start: i64,
    pub span_end: i64,
    pub payload: String,
    /// LLM-derived — a label, never payload.
    pub derived_title: String,
    pub derived_summary: String,
}

/// `GET /api/docs/golden?corpus_id=`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GoldenResponse {
    pub golden_queries: Vec<GoldenQueryRow>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GoldenQueryRow {
    pub id: i64,
    pub corpus_id: i64,
    pub query: String,
    pub expected_chunk_ids: Vec<String>,
    /// `manual | synthetic`.
    pub origin: String,
}

/// `GET /api/docs/golden/candidates?corpus_id=[&status=]` — §11's curation
/// queue. `chunks` carries the sections the candidates were written from, keyed
/// by chunk id: deciding whether a question is a fair test means reading the
/// section it is supposed to find.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GoldenCandidatesResponse {
    pub candidates: Vec<GoldenCandidateRow>,
    pub chunks: std::collections::HashMap<String, ChunkRow>,
}

/// One generated query awaiting a decision. It is **not** a golden query: an
/// eval never scores it, and only an accept turns it into one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GoldenCandidateRow {
    pub id: i64,
    pub corpus_id: i64,
    pub query: String,
    /// The chunk it was written from — attached by code, never named by the
    /// model.
    pub expected_chunk_ids: Vec<String>,
    /// The model's sentence about why that chunk answers it.
    pub rationale: String,
    /// The model that wrote it — the corpus's pinned ingest model.
    pub model: String,
    /// `pending | accepted | rejected`.
    pub status: String,
    /// The golden query it became, when it was accepted.
    pub golden_query_id: Option<i64>,
    pub created_at: String,
    /// Empty while it is still pending.
    pub decided_at: String,
}

/// `GET /api/docs/eval?corpus_id=`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EvalHistory {
    pub eval_runs: Vec<EvalRunRow>,
}

/// One measured run. `params` is the §6 parameter set it was measured under —
/// two runs under different parameters are not comparable, so the numbers
/// never travel without them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EvalRunRow {
    pub id: i64,
    pub corpus_id: i64,
    pub k: i64,
    pub queries: i64,
    pub hit_at_k: f64,
    pub mrr: f64,
    /// Queries whose every expected chunk is gone — they score zero for a
    /// reason that is not "retrieval got worse".
    pub orphaned_queries: i64,
    pub regression: bool,
    pub params: serde_json::Value,
    /// Full per-query breakdown.
    pub report: serde_json::Value,
    pub created_at: String,
}

/// `GET /api/docs/requests[?status=]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocRequestsResponse {
    pub requests: Vec<DocRequestRow>,
}

/// One `docs__request` filing. `status` is `pending | fulfilled | dismissed`;
/// only the first two are settable by hand — `fulfilled` is what an ingest
/// job's auto-fulfil writes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocRequestRow {
    pub id: i64,
    pub library: String,
    /// Empty when the requester pinned no version.
    pub version: String,
    pub reason: Option<String>,
    /// MCP client name from `initialize`, when it gave one.
    pub client_name: Option<String>,
    pub count: i64,
    pub first_requested_at: String,
    pub last_requested_at: String,
    pub status: String,
}

/// `POST /api/docs/search` — both renderings of one answer: the trace for the
/// playground, and the exact markdown a `docs__query` caller would receive.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocsSearchResponse {
    pub corpus_id: String,
    pub hits: Vec<SearchHit>,
    pub trace: SearchTraceView,
    /// Chunk id → source document URL, for the deep links.
    pub urls: std::collections::HashMap<String, String>,
    pub status: CorpusStatusView,
    pub markdown: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SearchHit {
    pub chunk: ChunkRow,
    /// The score the final order is by: the reranker's when it ran, RRF's
    /// otherwise.
    pub score: f32,
    pub rrf_score: f32,
    pub fts_rank: Option<u32>,
    pub knn_rank: Option<u32>,
    pub knn_score: Option<f32>,
    pub rerank_score: Option<f32>,
    pub tokens: u32,
}

/// The two badges §7 and §11 both read. Neither blocks a query.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CorpusStatusView {
    pub embed_status: String,
    pub eval_status: String,
    pub flags: Vec<String>,
    pub warnings: Vec<String>,
}

/// Per-stage trace (§6, §10): what each stage saw, in the order it saw it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SearchTraceView {
    pub corpus_id: String,
    pub embed_model: String,
    pub params: SearchParamsView,
    /// The FTS5 MATCH expression actually issued — user text is never passed
    /// through raw.
    pub fts_query: String,
    pub fts: Vec<StageHitView>,
    pub knn: Vec<StageHitView>,
    pub fused: Vec<FusedHitView>,
    pub rerank_model: Option<String>,
    /// Why the rerank stage did not run, when `params.rerank` asked for it.
    pub rerank_skipped: Option<String>,
    pub rerank: Vec<StageHitView>,
    pub token_counter: String,
    pub budget_used_tokens: u32,
    /// Chunks the token budget dropped, in the order they would have come.
    pub budget_dropped: Vec<String>,
    pub resident_vectors: u32,
    pub resident_bytes: u64,
    /// Which widening kernel the KNN scan used.
    pub knn_kernel: String,
    pub timings: TimingsView,
}

/// One request's §6 stage parameters. Round-trips: this is both what the trace
/// reports and what the playground posts back as `params`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SearchParamsView {
    pub k_fts: u32,
    pub k_vec: u32,
    pub rrf_k: f32,
    pub fts_weights: FtsWeightsView,
    pub rerank: bool,
    pub k_rerank: u32,
    pub limit: u32,
    /// `None` returns all `limit` chunks — the budget is the caller's choice.
    pub budget_tokens: Option<u32>,
}

impl Default for SearchParamsView {
    fn default() -> Self {
        Self {
            k_fts: 50,
            k_vec: 50,
            rrf_k: 60.0,
            fts_weights: FtsWeightsView::default(),
            rerank: true,
            k_rerank: 20,
            limit: 10,
            budget_tokens: None,
        }
    }
}

impl SearchParamsView {
    /// The stage parameters the owner's defaults imply, as one request starts
    /// from them. `k_rerank == 0` is how Settings says "no rerank stage".
    pub fn from_defaults(d: &DocsSearchDefaults) -> Self {
        Self {
            k_fts: d.k_fts,
            k_vec: d.k_vec,
            rrf_k: d.rrf_k,
            fts_weights: d.fts_weights,
            rerank: d.k_rerank > 0,
            k_rerank: d.k_rerank,
            limit: d.limit,
            budget_tokens: (d.budget_tokens > 0).then_some(d.budget_tokens),
        }
    }
}

/// BM25 column weights, in `chunk_fts` column order.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FtsWeightsView {
    pub payload: f32,
    pub heading_path: f32,
    pub derived_title: f32,
    pub derived_summary: f32,
}

impl Default for FtsWeightsView {
    fn default() -> Self {
        Self {
            payload: 1.0,
            heading_path: 1.5,
            derived_title: 1.5,
            derived_summary: 1.0,
        }
    }
}

/// One candidate as a stage saw it. `score` is BM25 (negative, lower is
/// better), cosine, or the reranker's — read next to the stage it came from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StageHitView {
    pub chunk_id: String,
    /// 1-based within the stage.
    pub rank: u32,
    pub score: f32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FusedHitView {
    pub chunk_id: String,
    pub rrf_score: f32,
    pub fts_rank: Option<u32>,
    pub knn_rank: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TimingsView {
    pub embed_ms: f64,
    pub fts_ms: f64,
    pub knn_ms: f64,
    pub fuse_ms: f64,
    pub fetch_ms: f64,
    pub rerank_ms: f64,
    pub total_ms: f64,
}

/// `GET /api/docs/export/manifest[?corpus_id=]` — what that download would
/// contain, shown before committing to the transfer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExportManifest {
    pub schema_version: i64,
    pub exported_at: String,
    pub byte_size: u64,
    pub corpora: Vec<ManifestCorpus>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ManifestCorpus {
    pub corpus_id: String,
    pub library: String,
    pub version: String,
    pub status: String,
    pub embed_upstream: String,
    pub embed_model: String,
    pub embed_dims: i64,
    pub ingest_model: String,
    pub ingest_prompt_version: String,
    pub source_kind: String,
    pub crawl_date: String,
    pub chunk_count: i64,
    pub eval_score: Option<f64>,
    pub eval_regression: bool,
}

/// `POST /api/docs/import` — the verdict. `dry_run` is the `validate_only`
/// pass: every check ran, nothing was written.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImportReport {
    pub schema_version: i64,
    pub file_schema_version: i64,
    pub dry_run: bool,
    pub imported: Vec<ImportedCorpus>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImportedCorpus {
    pub corpus_id: String,
    /// The embedding identity the corpus is pinned to.
    pub embed_model: String,
    /// The name on *this* gateway that routes there, when one does.
    pub embed_alias: Option<String>,
    pub chunks: i64,
    pub documents: i64,
    pub replaced: bool,
}

/// What the ingest / re-embed / eval triggers answer: the job that is now
/// running, or the one that already was.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocsJobStarted {
    pub ok: bool,
    pub job_id: Option<i64>,
    pub already_running: bool,
    /// Present on corpus creation.
    pub corpus: Option<CorpusView>,
}
