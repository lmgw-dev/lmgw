//! A gateway with a corpus store to test the docs plane against: the embedding
//! model and the reranker are one wiremock upstream, a corpus is seeded
//! directly rather than ingested, and every job kind a docs call can start has
//! a fake executor, so no call reaches the network and no job races its
//! answer.

use std::sync::Arc;
use std::time::Duration;

use lmgw_core::config::{Protocol, SelfAdmin, UpstreamKind};
use lmgw_core::erased::Erased;
use lmgw_core::jobs::{JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use quickdoc_core::embed::{EmbedIdentity, FixtureEmbedder};
use quickdoc_core::store::{self as qstore, NewChunk, NewCorpus};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

pub const DIMS: usize = 32;

/// Three chunks with no vocabulary in common.
pub const CHUNKS: [(&str, &str); 3] = [
    (
        "Routing",
        "Router::new().route(\"/\", get(root)) attaches a handler to a path.",
    ),
    (
        "Extractors",
        "An extractor pulls typed data out of an incoming request.",
    ),
    (
        "Middleware",
        "Layers wrap a service so that tracing runs around every call.",
    ),
];

struct Embeddings;

impl Respond for Embeddings {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let inputs: Vec<String> = match &body["input"] {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect(),
            _ => Vec::new(),
        };
        let fixture = FixtureEmbedder::new(DIMS);
        let data: Vec<Value> = inputs
            .iter()
            .enumerate()
            .map(|(i, t)| json!({"object": "embedding", "index": i, "embedding": fixture.embed_one(t)}))
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "data": data, "model": "embed-tgt",
            "usage": {"prompt_tokens": 1, "total_tokens": 1},
        }))
    }
}

/// Any job kind: reports `done` of `total` once (its input's, else 3 of
/// unknown), then ends as its input's `mode` says: `hold` (the default) until
/// cancelled, `done`, or `fail`.
struct Scripted {
    kind: JobKind,
}

impl JobExecutor for Scripted {
    fn kind(&self) -> JobKind {
        self.kind
    }

    fn run<'a>(&'a self, ctx: JobCtx, input: Value) -> Erased<'a, Result<JobOutcome, String>> {
        Box::pin(async move {
            let done = input["done"].as_u64().unwrap_or(3);
            ctx.progress(JobProgress {
                done,
                total: input["total"].as_u64(),
                stage: "working".into(),
                detail: json!({}),
            })
            .await;
            match input["mode"].as_str().unwrap_or("hold") {
                "done" => Ok(JobOutcome::Done(json!({"steps": done}))),
                "fail" => Err("executor said no".into()),
                _ => loop {
                    if ctx.canceled() {
                        return Ok(JobOutcome::Canceled);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                },
            }
        })
    }
}

/// A gateway whose `embed-model`, `rerank-model` and `ingest-model` aliases
/// all point at `mock`, with self-admin at `full` and the fake executors.
pub async fn setup(mock: &MockServer) -> SharedState {
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(Embeddings)
        .mount(mock)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    for kind in [
        JobKind::Ingest,
        JobKind::ReEmbed,
        JobKind::EvalRun,
        JobKind::GoldenGen,
    ] {
        state.jobs.register(Arc::new(Scripted { kind }));
    }
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    for (alias, target) in [
        ("embed-model", "embed-tgt"),
        ("rerank-model", "rerank-tgt"),
        ("ingest-model", "ingest-tgt"),
    ] {
        store::insert_alias(
            &state.db,
            &NewAlias {
                alias: alias.into(),
                upstream_id: up,
                upstream_model_id: target.into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: None,
            },
        )
        .await
        .unwrap();
    }
    let mut settings = state.snapshot().settings.clone();
    settings.self_admin = SelfAdmin::Full;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    state
}

pub async fn set_rerank_model(state: &SharedState, alias: &str) {
    let mut settings = state.snapshot().settings.clone();
    settings.docs_rerank_model = alias.into();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

/// The embedding identity `embed-model` resolves to.
pub fn identity() -> EmbedIdentity {
    EmbedIdentity::new("test-up", "embed-tgt", DIMS)
}

/// `library@version` with [`CHUNKS`] in one document of two (the second is
/// empty), `embedded` of the chunks with a vector, and two sources (the
/// second with a fence). Returns the corpus id.
pub async fn seed(
    state: &SharedState,
    library: &str,
    version: &str,
    status: &str,
    embedded: usize,
) -> i64 {
    let cid = qstore::insert_corpus(
        &state.corpus,
        &NewCorpus {
            library: library.into(),
            version: version.into(),
            status: status.into(),
            embed: identity(),
            ingest_model: "ingest-model".into(),
            ingest_prompt_version: "v1".into(),
            crawl_date: "2026-08-29T10:00:00Z".into(),
            source_kind: "markdown".into(),
        },
    )
    .await
    .unwrap();
    let root = format!("https://docs.rs/{library}");
    let sid = qstore::insert_source(&state.corpus, cid, &root, "markdown", &[])
        .await
        .unwrap();
    qstore::insert_source(
        &state.corpus,
        cid,
        &format!("{root}/llms.txt"),
        "llms_txt",
        &["docs.rs".to_string()],
    )
    .await
    .unwrap();
    let url = format!("{root}/guide");
    let (did, _) = qstore::upsert_document(&state.corpus, sid, &url, "hash")
        .await
        .unwrap();
    qstore::upsert_document(&state.corpus, sid, &format!("{root}/empty"), "hash-2")
        .await
        .unwrap();
    let fixture = FixtureEmbedder::new(DIMS);
    let chunks: Vec<NewChunk> = CHUNKS
        .iter()
        .enumerate()
        .map(|(i, (heading, payload))| {
            let mut c = NewChunk::new(cid, did, (i as i64 * 100, i as i64 * 100 + 80), *payload);
            c.heading_path = (*heading).into();
            c.derived_title = (*heading).into();
            c.derived_summary = format!("about {heading}");
            if i < embedded {
                c.embedding = Some(fixture.embed_one(&format!("{heading} {payload}")));
            }
            c
        })
        .collect();
    qstore::insert_chunks(&state.corpus, &url, DIMS, &chunks)
        .await
        .unwrap();
    cid
}

/// The ids of a corpus's first document's chunks, in span order.
pub async fn chunk_ids(state: &SharedState, cid: i64) -> Vec<String> {
    let docs = qstore::list_documents(&state.corpus, cid).await.unwrap();
    let first = docs
        .iter()
        .find(|d| d.url.ends_with("/guide"))
        .expect("a seeded corpus has its guide");
    qstore::list_chunks(&state.corpus, first.id)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect()
}

/// A finished eval run, with `hit_at_k` and `mrr` as the floats they are.
pub async fn record_run(state: &SharedState, cid: i64, hit_at_k: f64, mrr: f64, k: i64) {
    qstore::record_eval_run(
        &state.corpus,
        &qstore::NewEvalRun {
            corpus_id: cid,
            k,
            queries: 4,
            hit_at_k,
            mrr,
            orphaned_queries: 1,
            params: json!({"k_fts": 50, "rrf_k": 60.0}),
            report: json!({"per_query": [{"q": "x", "rank": 1, "score": 0.75}]}),
        },
    )
    .await
    .unwrap();
}
