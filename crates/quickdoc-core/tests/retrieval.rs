//! The §6 pipeline end to end against a real corpus DB and the fixture
//! embedder: both stages, fusion, the optional rerank, the token budget, the
//! trace, corpus scoping, §4's embed-identity gate, and the eval harness.
//!
//! Queries here are written in content words. The fixture embedder is a plain
//! bag of words with no notion of term rarity, so a stopword-heavy phrase
//! ("how do I …") measures the fixture rather than the pipeline. What is under
//! test is the wiring and the ranking arithmetic; retrieval *quality* is what
//! the eval harness measures, against a real model.

use std::sync::Arc;

use quickdoc_core::embed::{
    ApproxTokenCounter, EmbedIdentity, Embedder, FixtureEmbedder, FixtureReranker, NoopReranker,
};
use quickdoc_core::retrieve::{Retriever, SearchParams};
use quickdoc_core::store::{self, NewChunk, NewCorpus};
use quickdoc_core::{eval, QuickdocError};
use sqlx::SqlitePool;

const DIMS: usize = 256;

/// (heading path, verbatim payload) — small but shaped like real docs: prose
/// sections next to code, several libraries, overlapping vocabulary.
const AXUM_DOCS: &[(&str, &str)] = &[
    (
        "axum > routing",
        "Router::new().route(\"/users\", get(list_users).post(create_user)) registers one path with a method router.",
    ),
    (
        "axum > extractors",
        "An extractor implements FromRequestParts. Path, Query and Json are built in and run in argument order.",
    ),
    (
        "axum > middleware",
        "Add a tower layer to the whole router with .layer(TraceLayer::new_for_http()).",
    ),
    (
        "axum > state",
        "Share application state with the State extractor and Router::with_state.",
    ),
    (
        "axum > error handling",
        "Return a type implementing IntoResponse. A handler that returns Result maps its error through IntoResponse too.",
    ),
    (
        "tokio > runtime",
        "The multi thread runtime spawns worker threads. Annotate main with tokio::main to start it.",
    ),
    (
        "tokio > tasks",
        "tokio::spawn returns a JoinHandle. Tasks are cooperatively scheduled and must yield at await points.",
    ),
    (
        "serde > derive",
        "Derive Serialize and Deserialize on a struct to map it to and from JSON.",
    ),
];

async fn seed(pool: &SqlitePool, library: &str, docs: &[(&str, &str)]) -> (i64, Vec<String>) {
    let embedder = FixtureEmbedder::new(DIMS);
    let corpus_id =
        store::insert_corpus(pool, &NewCorpus::new(library, "0.8", embedder.identity()))
            .await
            .unwrap();
    let source_id = store::insert_source(
        pool,
        corpus_id,
        &format!("https://docs.rs/{library}"),
        "markdown",
        &["docs.rs".to_string()],
    )
    .await
    .unwrap();
    let url = format!("https://docs.rs/{library}/guide");
    let (document_id, _) = store::upsert_document(pool, source_id, &url, "hash-1")
        .await
        .unwrap();

    let mut offset = 0i64;
    let mut chunks = Vec::new();
    for (heading, payload) in docs {
        let end = offset + payload.len() as i64;
        let mut c = NewChunk::new(corpus_id, document_id, (offset, end), *payload);
        c.heading_path = (*heading).into();
        // Derived fields are embedded for recall and shown only as labels.
        c.derived_title = heading.rsplit(" > ").next().unwrap_or_default().into();
        c.embedding = Some(embedder.embed_one(&format!("{heading}\n{payload}")));
        chunks.push(c);
        offset = end;
    }
    let ids = store::insert_chunks(pool, &url, DIMS, &chunks)
        .await
        .unwrap();
    store::set_corpus_status(pool, corpus_id, "ready")
        .await
        .unwrap();
    (corpus_id, ids)
}

async fn retriever(pool: &SqlitePool, corpus_id: i64) -> Retriever {
    Retriever::load(
        pool.clone(),
        corpus_id,
        Arc::new(FixtureEmbedder::new(DIMS)),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn both_stages_contribute_and_the_trace_shows_their_work() {
    let pool = store::open_in_memory().await.unwrap();
    let (corpus_id, ids) = seed(&pool, "axum", AXUM_DOCS).await;
    let r = retriever(&pool, corpus_id).await;

    let res = r
        .search("route registers path method", &SearchParams::default())
        .await
        .unwrap();

    assert_eq!(res.corpus_id, "axum@0.8");
    assert_eq!(res.hits[0].chunk.id, ids[0], "routing is the answer");
    assert_eq!(res.hits[0].chunk.heading_path, "axum > routing");
    assert_eq!(res.hits[0].fts_rank, Some(1));
    assert_eq!(res.hits[0].knn_rank, Some(1));

    let t = &res.trace;
    assert_eq!(
        t.fts_query,
        "\"route\" OR \"registers\" OR \"path\" OR \"method\""
    );
    assert!(!t.fts.is_empty(), "BM25 stage found nothing");
    assert_eq!(t.knn.len(), AXUM_DOCS.len(), "exact KNN sees every vector");
    assert!(!t.fused.is_empty());
    assert!(t
        .fused
        .iter()
        .any(|f| f.fts_rank.is_some() && f.knn_rank.is_some()));
    assert_eq!(t.resident_vectors, AXUM_DOCS.len());
    assert_eq!(t.resident_bytes, AXUM_DOCS.len() * DIMS * 2, "f16, not f32");
    assert!(res.hits[0].knn_score.is_some());
    assert!(t.timings.total_ms >= 0.0);

    // Payloads come back verbatim — never a derived rewrite (§8).
    assert_eq!(res.hits[0].chunk.payload, AXUM_DOCS[0].1);
}

/// The point of fusion: a chunk only one stage likes still surfaces.
#[tokio::test]
async fn a_chunk_found_by_one_stage_alone_still_ranks() {
    let pool = store::open_in_memory().await.unwrap();
    let (corpus_id, _) = seed(&pool, "axum", AXUM_DOCS).await;
    let r = retriever(&pool, corpus_id).await;

    let res = r
        .search("JoinHandle", &SearchParams::default())
        .await
        .unwrap();
    let fused = res
        .trace
        .fused
        .iter()
        .find(|f| f.fts_rank == Some(1))
        .expect("BM25 matched the literal identifier");
    assert!(res.hits.iter().any(|h| h.chunk.id == fused.chunk_id));
}

#[tokio::test]
async fn asking_for_rerank_without_one_is_recorded_not_an_error() {
    let pool = store::open_in_memory().await.unwrap();
    let (corpus_id, _) = seed(&pool, "axum", AXUM_DOCS).await;
    let r = retriever(&pool, corpus_id).await;

    let res = r
        .search("extractors", &SearchParams::default())
        .await
        .unwrap();
    assert!(!res.hits.is_empty());
    assert_eq!(
        res.trace.rerank_skipped.as_deref(),
        Some("no reranker attached")
    );
    assert!(res.trace.rerank.is_empty());
    assert!(res.hits[0].rerank_score.is_none());
    assert_eq!(res.hits[0].score, res.hits[0].rrf_score);
}

#[tokio::test]
async fn the_rerank_stage_rescores_the_window_it_was_given() {
    let pool = store::open_in_memory().await.unwrap();
    let (corpus_id, _) = seed(&pool, "axum", AXUM_DOCS).await;
    let r = retriever(&pool, corpus_id)
        .await
        .with_reranker(Arc::new(FixtureReranker));

    let params = SearchParams {
        k_rerank: 4,
        limit: 4,
        ..Default::default()
    };
    let res = r
        .search("tokio spawn task JoinHandle", &params)
        .await
        .unwrap();
    assert_eq!(res.trace.rerank_model.as_deref(), Some("fixture-jaccard"));
    assert!(res.trace.rerank_skipped.is_none());
    assert_eq!(res.trace.rerank.len(), 4);
    assert_eq!(res.hits[0].chunk.heading_path, "tokio > tasks");
    for h in &res.hits {
        assert!(h.rerank_score.is_some());
        assert_eq!(Some(h.score), h.rerank_score);
    }
    // Scores descend after the stage reordered the window.
    for w in res.hits.windows(2) {
        assert!(w[0].score >= w[1].score);
    }
}

#[tokio::test]
async fn a_noop_reranker_leaves_the_fused_order_alone() {
    let pool = store::open_in_memory().await.unwrap();
    let (corpus_id, _) = seed(&pool, "axum", AXUM_DOCS).await;
    let plain = retriever(&pool, corpus_id).await;
    let noop = retriever(&pool, corpus_id)
        .await
        .with_reranker(Arc::new(NoopReranker));

    let params = SearchParams::default();
    let a = plain.search("router state", &params).await.unwrap();
    let b = noop.search("router state", &params).await.unwrap();
    let ids = |r: &quickdoc_core::retrieve::SearchResult| {
        r.hits
            .iter()
            .map(|h| h.chunk.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&a), ids(&b));
}

#[tokio::test]
async fn the_token_budget_trims_visibly() {
    let pool = store::open_in_memory().await.unwrap();
    let (corpus_id, _) = seed(&pool, "axum", AXUM_DOCS).await;
    let r = retriever(&pool, corpus_id)
        .await
        .with_token_counter(Arc::new(ApproxTokenCounter::default()));

    let unbudgeted = r.search("router", &SearchParams::default()).await.unwrap();
    assert_eq!(unbudgeted.hits.len(), 8, "limit 10 over an 8-chunk corpus");
    assert!(unbudgeted.trace.budget_dropped.is_empty());

    let params = SearchParams {
        budget_tokens: Some(60),
        ..Default::default()
    };
    let budgeted = r.search("router", &params).await.unwrap();
    assert!(budgeted.hits.len() < unbudgeted.hits.len());
    assert!(budgeted.trace.budget_used_tokens <= 60);
    assert_eq!(
        budgeted.hits.len() + budgeted.trace.budget_dropped.len(),
        unbudgeted.hits.len(),
        "every dropped chunk is named"
    );
    assert!(budgeted.trace.token_counter.starts_with("approx("));

    // A budget too small for even the first hit still answers, and says so.
    let tiny = SearchParams {
        budget_tokens: Some(1),
        ..Default::default()
    };
    let res = r.search("router", &tiny).await.unwrap();
    assert_eq!(res.hits.len(), 1);
    assert!(res.trace.budget_used_tokens > 1, "the overrun is visible");
}

#[tokio::test]
async fn stage_parameters_are_honoured_per_request() {
    let pool = store::open_in_memory().await.unwrap();
    let (corpus_id, _) = seed(&pool, "axum", AXUM_DOCS).await;
    let r = retriever(&pool, corpus_id).await;

    let params = SearchParams {
        k_fts: 2,
        k_vec: 3,
        limit: 2,
        rerank: false,
        ..Default::default()
    };
    let res = r.search("router state extractor", &params).await.unwrap();
    assert!(res.trace.fts.len() <= 2);
    assert_eq!(res.trace.knn.len(), 3);
    assert_eq!(res.hits.len(), 2);

    // k_fts = 0 turns the BM25 stage off entirely: pure vector search.
    let vec_only = SearchParams {
        k_fts: 0,
        ..params.clone()
    };
    let res = r.search("router state extractor", &vec_only).await.unwrap();
    assert!(res.trace.fts.is_empty());
    assert!(res.trace.fused.iter().all(|f| f.fts_rank.is_none()));
}

#[tokio::test]
async fn retrieval_never_leaks_across_corpora() {
    let pool = store::open_in_memory().await.unwrap();
    let (axum_id, axum_chunks) = seed(&pool, "axum", AXUM_DOCS).await;
    let (_, other_chunks) = seed(&pool, "actix", AXUM_DOCS).await;
    let r = retriever(&pool, axum_id).await;

    let res = r
        .search("register a route", &SearchParams::default())
        .await
        .unwrap();
    assert!(res.hits.iter().all(|h| axum_chunks.contains(&h.chunk.id)));
    assert!(res.hits.iter().all(|h| !other_chunks.contains(&h.chunk.id)));
    assert_eq!(res.trace.resident_vectors, AXUM_DOCS.len());
}

/// §4: an alias remapped to another model of the same width is exactly the
/// silent failure this gate exists for.
#[tokio::test]
async fn a_different_embed_model_refuses_to_open_the_corpus() {
    let pool = store::open_in_memory().await.unwrap();
    let (corpus_id, _) = seed(&pool, "axum", AXUM_DOCS).await;

    let other = Arc::new(FixtureEmbedder::new(DIMS).with_model("qwen3-embed"));
    let err = Retriever::load(pool.clone(), corpus_id, other)
        .await
        .unwrap_err();
    match err {
        QuickdocError::EmbedMismatch {
            corpus,
            pinned,
            got,
        } => {
            assert_eq!(corpus, "axum@0.8");
            assert!(pinned.contains("fixture-bow"), "{pinned}");
            assert!(got.contains("qwen3-embed"), "{got}");
        }
        other => panic!("expected an embed mismatch, got {other}"),
    }

    // Same model, different width: also refused.
    let narrow = Arc::new(FixtureEmbedder::new(DIMS / 2));
    assert!(Retriever::load(pool, corpus_id, narrow).await.is_err());
}

#[tokio::test]
async fn reload_picks_up_vectors_written_after_load() {
    let pool = store::open_in_memory().await.unwrap();
    let embedder = FixtureEmbedder::new(DIMS);
    let corpus_id =
        store::insert_corpus(&pool, &NewCorpus::new("tower", "0.5", embedder.identity()))
            .await
            .unwrap();
    let source_id =
        store::insert_source(&pool, corpus_id, "https://docs.rs/tower", "markdown", &[])
            .await
            .unwrap();
    let (document_id, _) = store::upsert_document(&pool, source_id, "https://docs.rs/tower/g", "h")
        .await
        .unwrap();
    let ids = store::insert_chunks(
        &pool,
        "https://docs.rs/tower/g",
        DIMS,
        &[NewChunk::new(
            corpus_id,
            document_id,
            (0, 20),
            "Service and Layer traits",
        )],
    )
    .await
    .unwrap();

    let mut r = retriever(&pool, corpus_id).await;
    assert_eq!(store::count_unembedded(&pool, corpus_id).await.unwrap(), 1);
    assert_eq!(r.resident_vectors(), 0);
    // BM25 alone still answers while the corpus waits to be embedded.
    let res = r.search("Layer", &SearchParams::default()).await.unwrap();
    assert_eq!(res.hits.len(), 1);
    assert!(res.trace.knn.is_empty());

    store::set_chunk_embedding(
        &pool,
        &ids[0],
        DIMS,
        &embedder.embed_one("Service and Layer traits"),
    )
    .await
    .unwrap();
    r.reload_vectors().await.unwrap();
    assert_eq!(r.resident_vectors(), 1);
    assert_eq!(store::count_unembedded(&pool, corpus_id).await.unwrap(), 0);
    let res = r.search("Layer", &SearchParams::default()).await.unwrap();
    assert_eq!(res.trace.knn.len(), 1);
}

#[tokio::test]
async fn eval_scores_golden_queries_and_names_orphans() {
    let pool = store::open_in_memory().await.unwrap();
    let (corpus_id, ids) = seed(&pool, "axum", AXUM_DOCS).await;

    store::insert_golden_query(
        &pool,
        corpus_id,
        "route registers path method",
        &[ids[0].clone()],
        "manual",
    )
    .await
    .unwrap();
    store::insert_golden_query(
        &pool,
        corpus_id,
        "share application state",
        &[ids[3].clone()],
        "manual",
    )
    .await
    .unwrap();
    // An expected chunk that a re-ingest removed.
    store::insert_golden_query(
        &pool,
        corpus_id,
        "websocket upgrade",
        &["deadbeef".to_string()],
        "synthetic",
    )
    .await
    .unwrap();

    let r = retriever(&pool, corpus_id).await;
    let report = eval::run(&r, 5, &SearchParams::default()).await.unwrap();
    assert_eq!(report.corpus_id, "axum@0.8");
    assert_eq!(report.queries, 3);
    assert_eq!(report.orphaned_queries, 1);
    // Two of three answerable, both at rank 1; the orphan can only score 0.
    assert!(
        (report.hit_at_k - 2.0 / 3.0).abs() < 1e-6,
        "hit@5 = {}",
        report.hit_at_k
    );
    assert!(
        (report.mrr - 2.0 / 3.0).abs() < 1e-6,
        "mrr = {}",
        report.mrr
    );

    let orphan = report
        .per_query
        .iter()
        .find(|q| q.query == "websocket upgrade")
        .unwrap();
    assert!(!orphan.hit());
    assert_eq!(orphan.orphaned_chunk_ids, vec!["deadbeef".to_string()]);

    store::set_corpus_eval_score(&pool, corpus_id, report.hit_at_k as f64)
        .await
        .unwrap();
    let c = store::get_corpus(&pool, corpus_id).await.unwrap().unwrap();
    assert_eq!(c.eval_score, Some(report.hit_at_k as f64));
}

#[tokio::test]
async fn a_corpus_db_is_a_plain_file_that_survives_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quickdoc.db");
    let pool = store::open(&path).await.unwrap();
    let (corpus_id, ids) = seed(&pool, "axum", AXUM_DOCS).await;
    store::checkpoint(&pool).await.unwrap();
    pool.close().await;

    let pool = store::open(&path).await.unwrap();
    let r = retriever(&pool, corpus_id).await;
    let res = r
        .search("route registers path method", &SearchParams::default())
        .await
        .unwrap();
    assert_eq!(res.hits[0].chunk.id, ids[0]);
    assert_eq!(r.resident_vectors(), AXUM_DOCS.len());
}

#[tokio::test]
async fn an_unknown_corpus_is_named_in_the_error() {
    let pool = store::open_in_memory().await.unwrap();
    let e = Retriever::load_by_id(pool, "axum@0.8", Arc::new(FixtureEmbedder::new(DIMS)))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("axum@0.8"), "{e}");
}

#[test]
fn embed_identity_prints_both_models_for_the_mismatch_error() {
    let id = EmbedIdentity::new("embed", "bge-m3", 1024);
    assert_eq!(id.to_string(), "embed/bge-m3 (1024d)");
}
