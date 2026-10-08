//! The quickdoc dashboard plane (§10): corpus CRUD, the debug search endpoint,
//! golden queries and their synthetic-candidate queue, eval runs, the
//! doc-request queue and corpus export/import.
//!
//! Mounted like the rest of `/api` — no auth middleware, no self-admin gate;
//! trust comes from `bind_addr`. What separates this from the `docs__*` MCP
//! tools is not authentication but *audience*: an agent asks a corpus
//! questions, the owner decides what a corpus is. Everything that creates,
//! re-embeds, evaluates, imports or deletes one lives here and nowhere else.
//!
//! Two conventions from `api.rs` carry over unchanged: every mutation returns
//! `Result<Value, String>` through [`ops_result`] so failures have one shape,
//! and long work is a job rather than a long request.
//!
//! The `_inner` halves are `pub(crate)` so [`crate::ops`] — and through it the
//! `lmgw__docs_*` self-admin tools (§20) — drives *these* paths rather than a
//! parallel implementation of corpus creation. The audience split above is
//! unchanged by that: the admin plane is the owner's, behind its own token.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use quickdoc_core::embed::EmbedIdentity;
use quickdoc_core::store::{self as qstore, Corpus, NewCorpus};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::quickdoc::{
    eval as eval_job, golden, ingest, portability, query, reembed, InProcessEmbedder,
};
use crate::state::SharedState;

use super::api::ops_result;

pub fn routes(state: &SharedState) -> Router<SharedState> {
    Router::new()
        .route("/api/docs/corpora", get(list_corpora).post(create_corpus))
        .route("/api/docs/corpora/{id}", get(corpus_detail))
        .route("/api/docs/corpora/{id}/delete", post(delete_corpus))
        .route("/api/docs/corpora/{id}/ingest", post(start_ingest))
        .route("/api/docs/corpora/{id}/re-embed", post(start_re_embed))
        .route("/api/docs/corpora/{id}/documents", get(list_documents))
        .route("/api/docs/chunks", get(list_chunks))
        .route("/api/docs/search", post(search))
        .route("/api/docs/eval", get(eval_history).post(start_eval))
        .route("/api/docs/golden", get(list_golden).post(set_golden))
        .route("/api/docs/golden/generate", post(generate_golden))
        .route("/api/docs/golden/candidates", get(list_candidates))
        .route(
            "/api/docs/golden/candidates/{id}/accept",
            post(accept_candidate),
        )
        .route(
            "/api/docs/golden/candidates/{id}/reject",
            post(reject_candidate),
        )
        .route("/api/docs/golden/{id}/delete", post(delete_golden))
        .route("/api/docs/requests", get(list_requests))
        .route("/api/docs/requests/{id}/status", post(set_request_status))
        .route("/api/docs/export", get(export))
        .route("/api/docs/export/manifest", get(export_manifest))
        // A corpus file is as big as it is; the JSON-route body limit does not
        // apply to it, exactly as it does not to an audio upload.
        .route(
            "/api/docs/import",
            post(import).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        // The whole plane is the owner's (principals §3.2): a corpus is
        // configuration, and ingesting into one spends money.
        .route_layer(crate::server::require(state, crate::principal::Cap::Admin))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub(crate) async fn corpus_or_err(st: &SharedState, id: i64) -> Result<Corpus, String> {
    qstore::get_corpus(&st.corpus, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no corpus {id}"))
}

/// Resolve what a caller *names* a corpus by. The dashboard always holds the
/// numeric id, but an agent reads `library@version` out of `docs__resolve` and
/// out of the request queue, so both have to land on the same row — a tool that
/// took only the id would make every call a two-step.
pub(crate) async fn corpus_by_selector(st: &SharedState, sel: &str) -> Result<Corpus, String> {
    let sel = sel.trim();
    if let Ok(id) = sel.parse::<i64>() {
        return corpus_or_err(st, id).await;
    }
    qstore::get_corpus_by_id(&st.corpus, sel)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!("no corpus '{sel}' — pass a numeric id or `library@version` as listed")
        })
}

/// One corpus as the Docs tab lists it: the row, its badges, and its sources.
pub(crate) async fn corpus_view(st: &SharedState, c: &Corpus) -> Result<Value, String> {
    let snap = st.snapshot();
    let status = query::corpus_status(&snap, c);
    let sources = qstore::list_sources(&st.corpus, c.id)
        .await
        .map_err(|e| e.to_string())?;
    let unembedded = qstore::count_unembedded(&st.corpus, c.id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "id": c.id,
        "corpus_id": c.corpus_id(),
        "library": c.library,
        "version": c.version,
        "status": c.status,
        "chunk_count": c.chunk_count,
        "unembedded_chunks": unembedded,
        // What this corpus costs to serve once loaded (§5): dims × 2 bytes per
        // embedded chunk. The visible price of the exact-KNN scan.
        "resident_bytes": (c.chunk_count - unembedded).max(0) * c.embed_dims * 2,
        "embed_upstream": c.embed_upstream,
        "embed_model": c.embed_model,
        "embed_dims": c.embed_dims,
        "embed_identity": c.embed_identity().to_string(),
        "ingest_model": c.ingest_model,
        "ingest_prompt_version": c.ingest_prompt_version,
        "crawl_date": c.crawl_date,
        "source_kind": c.source_kind,
        "eval_score": c.eval_score,
        "eval_best": c.eval_best,
        "eval_k": c.eval_k,
        "eval_at": c.eval_at,
        "eval_regression": c.eval_regression,
        "embed_status": status.embed_status,
        "eval_status": status.eval_status,
        "flags": status.flags,
        "warnings": status.warnings,
        "sources": sources,
        "created_at": c.created_at,
        "updated_at": c.updated_at,
    }))
}

// ---------------------------------------------------------------------------
// Corpus CRUD
// ---------------------------------------------------------------------------

async fn list_corpora(State(st): State<SharedState>) -> Response {
    ops_result(list_corpora_inner(&st).await)
}

pub(crate) async fn list_corpora_inner(st: &SharedState) -> Result<Value, String> {
    let rows = qstore::list_corpora(&st.corpus)
        .await
        .map_err(|e| e.to_string())?;
    let mut corpora = Vec::with_capacity(rows.len());
    for c in &rows {
        corpora.push(corpus_view(st, c).await?);
    }
    let pending = qstore::list_doc_requests(&st.corpus, "pending")
        .await
        .map_err(|e| e.to_string())?
        .len();
    Ok(json!({
        "corpora": corpora,
        "pending_requests": pending,
        "rerank_model": crate::quickdoc::rerank::rerank_alias(&st.snapshot()).ok(),
        "schema_version": qstore::schema_version(),
    }))
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct NewSource {
    pub(crate) root: String,
    /// `llms_txt` | `markdown` | `rustdoc_json` | `html`.
    pub(crate) kind: String,
    /// Domains a fetch may touch. Empty derives the fence from the root's own
    /// host, which is the safe default rather than "anywhere".
    #[serde(default)]
    pub(crate) fence: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct CreateCorpus {
    pub(crate) library: String,
    pub(crate) version: String,
    /// Model alias to embed with. Probed now, and the *resolved* identity is
    /// what the corpus pins.
    pub(crate) embed_model: String,
    /// Model alias that will drive extraction. Validated now so the wizard
    /// fails at the form rather than in the job.
    pub(crate) ingest_model: String,
    #[serde(default)]
    pub(crate) sources: Vec<NewSource>,
    /// Queue the ingest job immediately.
    #[serde(default)]
    pub(crate) start: bool,
}

async fn create_corpus(State(st): State<SharedState>, Json(p): Json<CreateCorpus>) -> Response {
    ops_result(create_corpus_inner(&st, p).await)
}

pub(crate) async fn create_corpus_inner(
    st: &SharedState,
    p: CreateCorpus,
) -> Result<Value, String> {
    let library = p.library.trim();
    let version = p.version.trim();
    if library.is_empty() || version.is_empty() {
        return Err(
            "library and version are both required — a corpus is one library at one \
                    version"
                .into(),
        );
    }
    if library.contains('@') {
        return Err(format!(
            "library '{library}' may not contain '@' — the corpus id is `library@version`"
        ));
    }
    let label = format!("{library}@{version}");
    if qstore::get_corpus_by_id(&st.corpus, &label)
        .await
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err(format!("corpus {label} already exists"));
    }

    // Both models are resolved before the row exists: a corpus pinned to a
    // model that is not there is one that can never be ingested or queried.
    let probe = InProcessEmbedder::probe(st.clone(), p.embed_model.trim())
        .await
        .map_err(|e| e.to_string())?;
    let identity: EmbedIdentity = quickdoc_core::embed::Embedder::identity(&probe);
    st.snapshot()
        .resolve(p.ingest_model.trim())
        .map_err(|e| format!("ingest model '{}': {e}", p.ingest_model.trim()))?;

    let source_kind = p
        .sources
        .first()
        .map(|s| s.kind.clone())
        .unwrap_or_default();
    let id = qstore::insert_corpus(
        &st.corpus,
        &NewCorpus {
            library: library.to_string(),
            version: version.to_string(),
            status: "ingesting".into(),
            embed: identity,
            ingest_model: p.ingest_model.trim().to_string(),
            ingest_prompt_version: quickdoc_core::ingest::prompt::CURRENT.to_string(),
            crawl_date: String::new(),
            source_kind,
        },
    )
    .await
    .map_err(|e| e.to_string())?;

    for s in &p.sources {
        // An empty fence is stored as empty on purpose: `Fence::new` reads it
        // as "this source's own host and nothing else", which is the safe
        // default the ingest job applies, not a fence that allows anything.
        qstore::insert_source(&st.corpus, id, s.root.trim(), s.kind.trim(), &s.fence)
            .await
            .map_err(|e| e.to_string())?;
    }

    let corpus = corpus_or_err(st, id).await?;
    let job = if p.start {
        Some(ingest::start(st, &corpus).await?.id())
    } else {
        None
    };
    Ok(json!({ "ok": true, "corpus": corpus_view(st, &corpus).await?, "job_id": job }))
}

/// `?limit=` on the detail view. Absent — and `0` — mean the **whole** eval
/// history: a score is read as a trend, and a series silently cut at some
/// constant is a series nobody can trust. Same convention as
/// `GET /api/docs/eval`.
#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct DetailQuery {
    #[serde(default)]
    limit: Option<i64>,
}

async fn corpus_detail(
    State(st): State<SharedState>,
    Path(id): Path<i64>,
    Query(q): Query<DetailQuery>,
) -> Response {
    ops_result(corpus_detail_inner(&st, id, q.limit.unwrap_or(0)).await)
}

async fn corpus_detail_inner(st: &SharedState, id: i64, limit: i64) -> Result<Value, String> {
    let c = corpus_or_err(st, id).await?;
    let documents = qstore::list_documents(&st.corpus, id)
        .await
        .map_err(|e| e.to_string())?;
    let golden = qstore::list_golden_queries(&st.corpus, id)
        .await
        .map_err(|e| e.to_string())?;
    let runs = qstore::list_eval_runs(&st.corpus, id, limit)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "corpus": corpus_view(st, &c).await?,
        "documents": documents,
        "golden_queries": golden,
        "eval_runs": runs,
    }))
}

async fn delete_corpus(State(st): State<SharedState>, Path(id): Path<i64>) -> Response {
    ops_result(delete_corpus_inner(&st, id).await)
}

pub(crate) async fn delete_corpus_inner(st: &SharedState, id: i64) -> Result<Value, String> {
    let c = corpus_or_err(st, id).await?;
    qstore::delete_corpus(&st.corpus, id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true, "message": format!("deleted {}", c.corpus_id()) }))
}

async fn start_ingest(State(st): State<SharedState>, Path(id): Path<i64>) -> Response {
    ops_result(
        async {
            let c = corpus_or_err(&st, id).await?;
            let spawn = ingest::start(&st, &c).await?;
            Ok(json!({ "ok": true, "job_id": spawn.id(),
                       "already_running": matches!(spawn, crate::jobs::Spawn::AlreadyRunning(_)) }))
        }
        .await,
    )
}

#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default)]
pub(crate) struct ReEmbedBody {
    /// Move the pin onto this alias. Omitted only fills in missing vectors.
    embed_model: Option<String>,
}

async fn start_re_embed(
    State(st): State<SharedState>,
    Path(id): Path<i64>,
    body: Option<Json<ReEmbedBody>>,
) -> Response {
    let p = body.map(|Json(b)| b).unwrap_or_default();
    ops_result(
        async {
            let c = corpus_or_err(&st, id).await?;
            let spawn = reembed::start(&st, &c, p.embed_model).await?;
            Ok(json!({ "ok": true, "job_id": spawn.id(),
                       "already_running": matches!(spawn, crate::jobs::Spawn::AlreadyRunning(_)) }))
        }
        .await,
    )
}

async fn list_documents(State(st): State<SharedState>, Path(id): Path<i64>) -> Response {
    ops_result(
        qstore::list_documents(&st.corpus, id)
            .await
            .map(|d| json!({ "documents": d }))
            .map_err(|e| e.to_string()),
    )
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct ChunksQuery {
    document_id: i64,
}

/// The corpus browser's leaf (§11): a document's chunks, verbatim payload
/// included, with the derived fields alongside rather than mixed into it.
async fn list_chunks(State(st): State<SharedState>, Query(q): Query<ChunksQuery>) -> Response {
    ops_result(
        qstore::list_chunks(&st.corpus, q.document_id)
            .await
            .map(|c| json!({ "chunks": c }))
            .map_err(|e| e.to_string()),
    )
}

// ---------------------------------------------------------------------------
// Debug search (§10) — the playground's and the optimisation agent's endpoint
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct SearchBody {
    /// Either the numeric row id or `library@version`.
    #[serde(default)]
    corpus_id: Option<i64>,
    #[serde(default)]
    corpus: Option<String>,
    query: String,
    /// Partial overrides of the search stage settings; anything absent keeps
    /// the configured default.
    #[serde(default)]
    params: Option<Value>,
    /// Convenience override, same precedence as the MCP tool's.
    #[serde(default)]
    budget_tokens: Option<usize>,
}

async fn search(State(st): State<SharedState>, Json(p): Json<SearchBody>) -> Response {
    ops_result(search_inner(&st, p).await)
}

async fn search_inner(st: &SharedState, p: SearchBody) -> Result<Value, String> {
    let corpus = match (p.corpus_id, p.corpus.as_deref()) {
        (Some(id), _) => corpus_or_err(st, id).await?,
        (None, Some(label)) => qstore::get_corpus_by_id(&st.corpus, label)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no corpus '{label}'"))?,
        (None, None) => return Err("corpus_id or corpus is required".into()),
    };

    let snap = st.snapshot();
    let mut params = query::default_params(&snap);
    if let Some(over) = p.params.filter(|v| !v.is_null()) {
        params = query::apply_params(&params, &over)?;
    }
    if let Some(b) = p.budget_tokens {
        params.budget_tokens = (b > 0).then_some(b);
    }

    let retriever = query::open_retriever(st, &corpus)
        .await
        .map_err(|e| e.to_string())?;
    let result = retriever
        .search(&p.query, &params)
        .await
        .map_err(|e| e.to_string())?;
    let ids: Vec<String> = result.hits.iter().map(|h| h.chunk.id.clone()).collect();
    let urls = qstore::document_urls(&st.corpus, &ids)
        .await
        .map_err(|e| e.to_string())?;
    let status = query::corpus_status(&snap, &corpus);
    // Both renderings of the same answer: the trace for the playground, the
    // markdown for seeing exactly what a `docs__query` caller would receive.
    let markdown =
        quickdoc_core::markdown::render(&corpus, &p.query, &result, &urls, &status.warnings);
    Ok(json!({
        "corpus_id": result.corpus_id,
        "hits": result.hits,
        "trace": result.trace,
        "urls": urls,
        "status": status,
        "markdown": markdown,
    }))
}

// ---------------------------------------------------------------------------
// Golden queries and eval runs
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct CorpusQuery {
    corpus_id: i64,
    #[serde(default)]
    limit: Option<i64>,
}

async fn list_golden(State(st): State<SharedState>, Query(q): Query<CorpusQuery>) -> Response {
    ops_result(
        qstore::list_golden_queries(&st.corpus, q.corpus_id)
            .await
            .map(|g| json!({ "golden_queries": g }))
            .map_err(|e| e.to_string()),
    )
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct GoldenBody {
    /// Present = edit that query; absent = create one.
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    corpus_id: Option<i64>,
    query: String,
    #[serde(default)]
    expected_chunk_ids: Vec<String>,
    /// `manual` (default) or `synthetic` — a curated candidate keeps saying
    /// where it came from.
    #[serde(default)]
    origin: Option<String>,
}

async fn set_golden(State(st): State<SharedState>, Json(p): Json<GoldenBody>) -> Response {
    ops_result(set_golden_inner(&st, p).await)
}

async fn set_golden_inner(st: &SharedState, p: GoldenBody) -> Result<Value, String> {
    let query = p.query.trim();
    if query.is_empty() {
        return Err("a golden query needs a query".into());
    }
    let origin = p.origin.as_deref().unwrap_or("manual").trim().to_string();
    if !matches!(origin.as_str(), "manual" | "synthetic") {
        return Err(format!("unknown origin '{origin}' (manual|synthetic)"));
    }
    // Expected ids that are not in the corpus would score as permanent misses
    // and read as a retrieval regression, so they are rejected on the way in
    // rather than surfaced as orphans later.
    let id = match p.id {
        Some(id) => {
            let existing = qstore::get_golden_query(&st.corpus, id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no golden query {id}"))?;
            check_expected(st, existing.corpus_id, &p.expected_chunk_ids).await?;
            qstore::update_golden_query(&st.corpus, id, query, &p.expected_chunk_ids, &origin)
                .await
                .map_err(|e| e.to_string())?;
            id
        }
        None => {
            let corpus_id = p
                .corpus_id
                .ok_or("corpus_id is required when creating a golden query")?;
            corpus_or_err(st, corpus_id).await?;
            check_expected(st, corpus_id, &p.expected_chunk_ids).await?;
            qstore::insert_golden_query(
                &st.corpus,
                corpus_id,
                query,
                &p.expected_chunk_ids,
                &origin,
            )
            .await
            .map_err(|e| e.to_string())?
        }
    };
    Ok(json!({ "ok": true, "id": id }))
}

async fn check_expected(st: &SharedState, corpus_id: i64, ids: &[String]) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }
    let present = qstore::get_chunks(&st.corpus, ids)
        .await
        .map_err(|e| e.to_string())?;
    let wrong: Vec<&String> = ids
        .iter()
        .filter(|id| present.get(*id).is_none_or(|c| c.corpus_id != corpus_id))
        .collect();
    if wrong.is_empty() {
        return Ok(());
    }
    Err(format!(
        "these expected chunk ids are not in this corpus: {}",
        wrong
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

async fn delete_golden(State(st): State<SharedState>, Path(id): Path<i64>) -> Response {
    ops_result(
        qstore::delete_golden_query(&st.corpus, id)
            .await
            .map(|_| json!({ "ok": true }))
            .map_err(|e| e.to_string()),
    )
}

// ---------------------------------------------------------------------------
// Synthetic golden queries (§10) and their curation queue (§11)
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct GenerateBody {
    corpus_id: i64,
    /// Chunks to sample. Absent or `0` samples every chunk in the corpus — the
    /// run is as big as the corpus unless a smaller sample is asked for.
    #[serde(default)]
    sample: Option<usize>,
    /// Questions to ask for per sampled chunk. Absent means one.
    #[serde(default)]
    per_chunk: Option<usize>,
}

/// Start a generation run. It writes **candidates** only; §11's queue is the
/// only path from one to a golden query, and this endpoint has no way to skip
/// it. The model is the corpus's pinned ingest model (§4) and is deliberately
/// not overridable here.
async fn generate_golden(State(st): State<SharedState>, Json(p): Json<GenerateBody>) -> Response {
    ops_result(
        async {
            let corpus = corpus_or_err(&st, p.corpus_id).await?;
            let spawn = golden::start(&st, &corpus, p.sample, p.per_chunk).await?;
            Ok(json!({ "ok": true, "job_id": spawn.id(),
                       "already_running": matches!(spawn, crate::jobs::Spawn::AlreadyRunning(_)) }))
        }
        .await,
    )
}

#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default)]
pub(crate) struct CandidatesQuery {
    corpus_id: i64,
    /// `pending` | `accepted` | `rejected`; empty lists every one of them.
    status: Option<String>,
}

/// The curation queue. The chunks the candidates were written from travel with
/// them: deciding whether a question is a fair test means reading the section it
/// is supposed to find, and a queue that made that a second round trip would be
/// curated blind.
async fn list_candidates(
    State(st): State<SharedState>,
    Query(q): Query<CandidatesQuery>,
) -> Response {
    ops_result(list_candidates_inner(&st, q).await)
}

async fn list_candidates_inner(st: &SharedState, q: CandidatesQuery) -> Result<Value, String> {
    let status = q.status.as_deref().unwrap_or("pending");
    let candidates = qstore::list_golden_candidates(&st.corpus, q.corpus_id, status)
        .await
        .map_err(|e| e.to_string())?;
    let ids: Vec<String> = candidates
        .iter()
        .flat_map(|c| c.expected_chunk_ids.clone())
        .collect();
    let chunks = qstore::get_chunks(&st.corpus, &ids)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "candidates": candidates, "chunks": chunks }))
}

#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default)]
pub(crate) struct AcceptBody {
    /// The query as edited before accepting; absent accepts it as proposed.
    query: Option<String>,
    /// The expectation as edited before accepting; absent keeps the chunk the
    /// candidate was written from.
    expected_chunk_ids: Option<Vec<String>>,
}

/// Promote a candidate to a real golden query, keeping `synthetic` as its
/// origin so a curated query never stops saying where it came from.
async fn accept_candidate(
    State(st): State<SharedState>,
    Path(id): Path<i64>,
    body: Option<Json<AcceptBody>>,
) -> Response {
    let p = body.map(|Json(b)| b).unwrap_or_default();
    ops_result(accept_candidate_inner(&st, id, p).await)
}

async fn accept_candidate_inner(st: &SharedState, id: i64, p: AcceptBody) -> Result<Value, String> {
    let c = candidate_or_err(st, id).await?;
    if c.status != "pending" {
        return Err(format!(
            "candidate {id} was already {} — a decision is made once",
            c.status
        ));
    }
    let query = p.query.unwrap_or_else(|| c.query.clone());
    let query = query.trim();
    if query.is_empty() {
        return Err("a golden query needs a query".into());
    }
    let expected = p
        .expected_chunk_ids
        .unwrap_or_else(|| c.expected_chunk_ids.clone());
    check_expected(st, c.corpus_id, &expected).await?;
    let golden_id =
        qstore::insert_golden_query(&st.corpus, c.corpus_id, query, &expected, "synthetic")
            .await
            .map_err(|e| e.to_string())?;
    qstore::decide_golden_candidate(&st.corpus, id, "accepted", Some(golden_id))
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true, "id": golden_id }))
}

/// Discard a candidate. The row stays with `rejected` on it rather than being
/// deleted, which is what stops the next generation run from proposing the same
/// question again.
async fn reject_candidate(State(st): State<SharedState>, Path(id): Path<i64>) -> Response {
    ops_result(
        async {
            let c = candidate_or_err(&st, id).await?;
            if c.status != "pending" {
                return Err(format!(
                    "candidate {id} was already {} — a decision is made once",
                    c.status
                ));
            }
            qstore::decide_golden_candidate(&st.corpus, id, "rejected", None)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({ "ok": true }))
        }
        .await,
    )
}

async fn candidate_or_err(st: &SharedState, id: i64) -> Result<qstore::GoldenCandidate, String> {
    qstore::get_golden_candidate(&st.corpus, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no golden-query candidate {id}"))
}

async fn eval_history(State(st): State<SharedState>, Query(q): Query<CorpusQuery>) -> Response {
    ops_result(
        qstore::list_eval_runs(&st.corpus, q.corpus_id, q.limit.unwrap_or(0))
            .await
            .map(|runs| json!({ "eval_runs": runs }))
            .map_err(|e| e.to_string()),
    )
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct EvalBody {
    corpus_id: i64,
    /// hit@k depth; absent takes the configured default.
    #[serde(default)]
    k: Option<usize>,
    /// Stage overrides to measure under; absent measures under the defaults
    /// `docs__query` itself runs with.
    #[serde(default)]
    params: Option<Value>,
}

async fn start_eval(State(st): State<SharedState>, Json(p): Json<EvalBody>) -> Response {
    ops_result(start_eval_inner(&st, p).await)
}

async fn start_eval_inner(st: &SharedState, p: EvalBody) -> Result<Value, String> {
    let corpus = corpus_or_err(st, p.corpus_id).await?;
    let params = match p.params.filter(|v| !v.is_null()) {
        Some(over) => Some(query::apply_params(
            &query::default_params(&st.snapshot()),
            &over,
        )?),
        None => None,
    };
    let spawn = eval_job::start(st, &corpus, p.k, params).await?;
    Ok(json!({
        "ok": true,
        "job_id": spawn.id(),
        "already_running": matches!(spawn, crate::jobs::Spawn::AlreadyRunning(_)),
    }))
}

// ---------------------------------------------------------------------------
// The doc-request queue (§7, §10)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default)]
pub(crate) struct RequestsQuery {
    /// `pending` | `fulfilled` | `dismissed`; empty lists everything.
    status: Option<String>,
}

async fn list_requests(State(st): State<SharedState>, Query(q): Query<RequestsQuery>) -> Response {
    ops_result(list_requests_inner(&st, q.status.as_deref().unwrap_or_default()).await)
}

/// `status` empty means every request, whatever its state.
pub(crate) async fn list_requests_inner(st: &SharedState, status: &str) -> Result<Value, String> {
    qstore::list_doc_requests(&st.corpus, status)
        .await
        .map(|r| json!({ "requests": r }))
        .map_err(|e| e.to_string())
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct RequestStatusBody {
    /// `dismissed` to clear it, `pending` to put it back.
    status: String,
}

async fn set_request_status(
    State(st): State<SharedState>,
    Path(id): Path<i64>,
    Json(p): Json<RequestStatusBody>,
) -> Response {
    ops_result(set_request_status_inner(&st, id, &p.status).await)
}

pub(crate) async fn set_request_status_inner(
    st: &SharedState,
    id: i64,
    status: &str,
) -> Result<Value, String> {
    // `fulfilled` is not settable by hand: it means "an ingest finished
    // for this library@version", which only the ingest job can know.
    if !matches!(status, "pending" | "dismissed") {
        return Err(format!(
            "status '{status}' cannot be set by hand (pending|dismissed) — a request becomes \
             'fulfilled' when an ingest for it completes"
        ));
    }
    qstore::set_doc_request_status(&st.corpus, id, status)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// Export / import (§10)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default)]
pub(crate) struct ExportQuery {
    /// One corpus instead of the whole file.
    corpus_id: Option<i64>,
}

/// The file itself. Deliberately a plain SQLite database and not an archive:
/// §3's promise is that one file *is* the backup, and wrapping it would break
/// "rsync it and you are done".
async fn export(State(st): State<SharedState>, Query(q): Query<ExportQuery>) -> Response {
    let staged = match portability::export(&st, q.corpus_id).await {
        Ok(s) => s,
        Err(e) => return api_error(e),
    };
    let name = staged
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "quickdoc.db".into());
    match staged.bytes() {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/vnd.sqlite3".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{name}\""),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => api_error(format!("reading the export: {e}")),
    }
}

/// What that download would contain. The manifest travels beside the file
/// rather than inside it (see [`portability`]), so this is the half a UI shows
/// before committing to the transfer, and the half an import re-derives to
/// check against.
async fn export_manifest(State(st): State<SharedState>, Query(q): Query<ExportQuery>) -> Response {
    ops_result(
        portability::manifest(&st, q.corpus_id)
            .await
            .and_then(|m| serde_json::to_value(m).map_err(|e| e.to_string())),
    )
}

#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default)]
pub(crate) struct ImportQuery {
    /// Overwrite a `library@version` that already exists here.
    replace: bool,
    /// Run the checks and report, writing nothing.
    validate_only: bool,
}

async fn import(
    State(st): State<SharedState>,
    Query(q): Query<ImportQuery>,
    body: Bytes,
) -> Response {
    ops_result(import_inner(&st, q, body).await)
}

async fn import_inner(st: &SharedState, q: ImportQuery, body: Bytes) -> Result<Value, String> {
    if body.is_empty() {
        return Err("no corpus file in the request body".into());
    }
    let dir = tempfile::tempdir().map_err(|e| format!("staging the upload: {e}"))?;
    let path = dir.path().join("upload.db");
    std::fs::write(&path, &body).map_err(|e| format!("writing the upload: {e}"))?;
    let report = portability::import(st, &path, q.replace, q.validate_only).await?;
    serde_json::to_value(report).map_err(|e| e.to_string())
}

fn api_error(message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(lmgw_api_types::ApiError {
            code: "op_failed".into(),
            message,
        }),
    )
        .into_response()
}
