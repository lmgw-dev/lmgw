//! The quickdoc serving plane (quickdoc design §7, §10): the `docs__*` MCP
//! toolset, the rerank stage, eval runs and the regression badge, corpus
//! export/import, and the dashboard endpoints behind all of it.
//!
//! No container and no network: the embedding model and the reranker are one
//! wiremock upstream, and the corpus is seeded directly rather than ingested
//! (`quickdoc_ingest.rs` owns that half).

use std::sync::Arc;

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::mcp::docs;
use lmgw_core::quickdoc::{eval as eval_job, golden as golden_job, portability};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewLocalModel, NewUpstream};
use quickdoc_core::embed::{EmbedIdentity, FixtureEmbedder};
use quickdoc_core::store::{self as qstore, NewChunk, NewCorpus};
use serde_json::{json, Map, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::common;
use common::serve;

const DIMS: usize = 32;

/// Three chunks with no vocabulary in common, so which one a query should find
/// is never in doubt.
const CHUNKS: [(&str, &str); 3] = [
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

// ---------------------------------------------------------------------------
// The mock world
// ---------------------------------------------------------------------------

struct FixtureEmbeddings;

impl Respond for FixtureEmbeddings {
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

/// A reranker that scores strictly by position — the *last* candidate wins.
///
/// Deliberately not a better ranker: a stage that agreed with fusion would be
/// indistinguishable from not running at all, and what these tests need to
/// prove is that the stage ran, that its scores reached the caller, and that
/// the order they imply is the order that comes back.
struct ReverseRerank;

impl Respond for ReverseRerank {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let n = body["documents"].as_array().map(Vec::len).unwrap_or(0);
        let results: Vec<Value> = (0..n)
            .map(|i| json!({"index": i, "relevance_score": i as f64}))
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({
            "model": "rerank-tgt", "object": "list", "results": results,
            "usage": {"prompt_tokens": 3, "total_tokens": 3},
        }))
    }
}

async fn mount(mock: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(FixtureEmbeddings)
        .mount(mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/rerank"))
        .respond_with(ReverseRerank)
        .mount(mock)
        .await;
}

/// The ingest model, scripted for a synthetic golden-query run: it answers the
/// section it is shown with a question about it — except for the first
/// `Routing` turn, where it copies a sentence straight back out of the section,
/// which is the small-model failure this contract exists to catch. Being driven
/// by the request body rather than by a turn counter is what lets it play both
/// the mistake and the correction.
struct ScriptedGenerator;

impl Respond for ScriptedGenerator {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let raw = String::from_utf8_lossy(&req.body).to_string();
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let turn = body["messages"]
            .as_array()
            .and_then(|m| m.iter().find(|m| m["role"] == "user"))
            .and_then(|m| m["content"].as_str())
            .unwrap_or_default()
            .to_string();
        let heading = turn
            .lines()
            .find_map(|l| l.strip_prefix("Section: "))
            .unwrap_or_default()
            .to_string();

        // Every question was accepted — say one sentence and stop.
        if raw.contains("Nothing left to correct") {
            return ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-2", "object": "chat.completion", "model": "ingest-tgt",
                "choices": [{"index": 0, "message":
                    {"role": "assistant", "content": "Wrote one question."},
                    "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 120, "completion_tokens": 6},
            }));
        }
        let copied = CHUNKS
            .iter()
            .find(|(h, _)| *h == heading)
            .map(|(_, p)| p.to_string())
            .unwrap_or_default();
        // The marker is the *rejection's* wording, not the system prompt's —
        // the prompt warns about copying too, so matching that would make the
        // script skip its own mistake.
        let query = if heading == "Routing" && !raw.contains("that is a copy of the text") {
            copied
        } else {
            format!("how do I use {} in axum?", heading.to_lowercase())
        };
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1", "object": "chat.completion", "model": "ingest-tgt",
            "choices": [{"index": 0, "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "c1", "type": "function", "function": {
                    "name": "emit_queries",
                    "arguments": json!({"queries": [
                        {"query": query, "rationale": format!("the {heading} section covers it")},
                    ]}).to_string(),
                }}],
            }, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 200, "completion_tokens": 30},
        }))
    }
}

async fn mount_generator(mock: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedGenerator)
        .mount(mock)
        .await;
}

/// A gateway whose `embed-model` and `rerank-model` aliases both point at the
/// mock. The rerank alias is a plain alias rather than an aux-router section,
/// so the model-kind gate is not in play here (`aux_router.rs` owns that).
async fn setup(mock: &MockServer) -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
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
    state.reload_snapshot().await.unwrap();
    state
}

async fn set_rerank_model(state: &SharedState, alias: &str) {
    let mut settings = state.snapshot().settings.clone();
    settings.docs_rerank_model = alias.into();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

fn identity() -> EmbedIdentity {
    EmbedIdentity::new("test-up", "embed-tgt", DIMS)
}

/// Seed `library@version` with [`CHUNKS`], already embedded.
async fn seed(state: &SharedState, library: &str, version: &str) -> i64 {
    seed_with_model(state, library, version, "ingest-model").await
}

/// [`seed`], with the corpus's pinned ingest model named — the GPU-hold test
/// needs one that really is local, and every other test needs the cloud-shaped
/// default.
async fn seed_with_model(
    state: &SharedState,
    library: &str,
    version: &str,
    ingest_model: &str,
) -> i64 {
    let cid = qstore::insert_corpus(
        &state.corpus,
        &NewCorpus {
            library: library.into(),
            version: version.into(),
            status: "ready".into(),
            embed: identity(),
            ingest_model: ingest_model.into(),
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
    let url = format!("{root}/guide");
    let (did, _) = qstore::upsert_document(&state.corpus, sid, &url, "hash")
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
            c.embedding = Some(fixture.embed_one(&format!("{heading} {payload}")));
            c
        })
        .collect();
    qstore::insert_chunks(&state.corpus, &url, DIMS, &chunks)
        .await
        .unwrap();
    cid
}

fn args(v: Value) -> Option<Map<String, Value>> {
    Some(v.as_object().unwrap().clone())
}

/// The text of a tool result, and whether it was an error.
async fn call(state: &SharedState, name: &str, v: Value) -> (String, bool) {
    let result = docs::call(state, name, args(v), None).await.unwrap();
    (
        result["content"][0]["text"].as_str().unwrap().to_string(),
        result["isError"].as_bool().unwrap_or(false),
    )
}

async fn call_json(state: &SharedState, name: &str, v: Value) -> Value {
    let (text, is_error) = call(state, name, v).await;
    assert!(!is_error, "{name} failed: {text}");
    serde_json::from_str(&text).unwrap()
}

// ---------------------------------------------------------------------------
// docs__resolve
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_reports_every_version_with_the_metadata_to_choose_between_them() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    seed(&state, "axum", "0.7").await;
    let newer = seed(&state, "axum", "0.8").await;
    qstore::set_corpus_eval_score(&state.corpus, newer, 0.9)
        .await
        .unwrap();

    let v = call_json(&state, "docs__resolve", json!({"library": "axum"})).await;
    let m = v["matches"].as_array().unwrap();
    assert_eq!(m.len(), 2);
    assert_eq!(m[0]["corpus_id"], "axum@0.8", "newest first: {v}");
    assert_eq!(m[0]["chunk_count"], 3);
    assert_eq!(m[0]["crawl_date"], "2026-08-29T10:00:00Z");
    assert_eq!(m[0]["source_kind"], "markdown");
    assert_eq!(m[0]["ingest_model"], "ingest-model");
    assert_eq!(m[0]["embed_model"], "test-up/embed-tgt (32d)");
    assert_eq!(m[0]["sources"][0], "https://docs.rs/axum");
    assert_eq!(m[0]["eval_score"], 0.9);
    assert_eq!(m[0]["embed_status"], "ok");
    // Never measured is a different statement from "scored zero", and says so.
    assert_eq!(m[1]["eval_status"], "unmeasured");
    assert_eq!(v["next_step"], "docs__query");
}

/// A resolve miss is where an agent would otherwise fall back to guessing, so
/// it is the one place the request path has to be impossible to miss (§7).
#[tokio::test]
async fn a_resolve_miss_names_docs_request_and_says_it_does_not_ingest() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    seed(&state, "axum", "0.8").await;

    let v = call_json(
        &state,
        "docs__resolve",
        json!({"library": "tower", "query": "how do I write a Layer"}),
    )
    .await;
    assert!(v["matches"].as_array().unwrap().is_empty());
    assert_eq!(v["next_step"], "docs__request");
    let hint = v["hint"].as_str().unwrap();
    assert!(hint.contains("docs__request(library=\"tower\""), "{hint}");
    assert!(hint.contains("how do I write a Layer"), "{hint}");
    assert!(
        hint.contains("does not start an ingest"),
        "the agent must not wait for a corpus: {hint}"
    );
    assert_eq!(v["libraries_available"][0], "axum");
}

/// The badge is reported and the corpus still answers — degraded-but-stated
/// beats refused (§7).
#[tokio::test]
async fn an_eval_regression_is_reported_but_never_blocks_a_query() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let cid = seed(&state, "axum", "0.8").await;
    record_score(&state, cid, 0.9, 10).await;
    record_score(&state, cid, 0.4, 10).await;

    let v = call_json(&state, "docs__resolve", json!({"library": "axum"})).await;
    assert_eq!(v["matches"][0]["eval_status"], "regression");
    assert_eq!(v["matches"][0]["flags"][0], "eval_regression");
    let warning = v["matches"][0]["warnings"][0].as_str().unwrap();
    assert!(
        warning.contains("0.40") && warning.contains("0.90"),
        "{warning}"
    );

    let (text, is_error) = call(
        &state,
        "docs__query",
        json!({"corpus_id": "axum@0.8", "query": "extractor"}),
    )
    .await;
    assert!(!is_error, "a regression must not block the query: {text}");
    assert!(text.contains("⚠ eval regression"), "{text}");
    assert!(text.contains("An extractor pulls typed data"), "{text}");
}

async fn record_score(state: &SharedState, corpus_id: i64, hit_at_k: f64, k: i64) {
    qstore::record_eval_run(
        &state.corpus,
        &qstore::NewEvalRun {
            corpus_id,
            k,
            queries: 10,
            hit_at_k,
            mrr: hit_at_k,
            orphaned_queries: 0,
            params: json!({}),
            report: json!({}),
        },
    )
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// docs__query
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_answers_in_markdown_with_deep_links_and_a_visible_budget() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    seed(&state, "axum", "0.8").await;

    let (text, _) = call(
        &state,
        "docs__query",
        json!({"corpus_id": "axum@0.8", "query": "extractor typed data"}),
    )
    .await;
    assert!(text.starts_with("# axum@0.8"), "{text}");
    assert!(text.contains("crawled 2026-08-29T10:00:00Z"), "{text}");
    assert!(
        text.contains("embedded with `test-up/embed-tgt (32d)`"),
        "{text}"
    );
    assert!(
        text.contains("source: https://docs.rs/axum/guide"),
        "every chunk needs its deep link: {text}"
    );
    assert!(text.contains("## 1. Extractors"), "{text}");
    assert!(
        text.contains("An extractor pulls typed data out of an incoming request."),
        "the payload is verbatim: {text}"
    );
    assert!(
        text.contains("no `budget_tokens` was set"),
        "the budget always reports itself: {text}"
    );
    assert!(
        text.contains("tiktoken o200k_base"),
        "the counter names itself: {text}"
    );

    // A budget that trims says how much it trimmed and how to see the rest.
    let (text, _) = call(
        &state,
        "docs__query",
        json!({"corpus_id": "axum@0.8", "query": "extractor", "budget_tokens": 20}),
    )
    .await;
    assert!(text.contains("of the 20-token budget used"), "{text}");
    assert!(text.contains("raise `budget_tokens`"), "{text}");
}

#[tokio::test]
async fn query_stage_overrides_apply_per_request_and_a_typo_is_named() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    seed(&state, "axum", "0.8").await;

    let (text, _) = call(
        &state,
        "docs__query",
        json!({
            "corpus_id": "axum@0.8", "query": "handler layer extractor",
            "params": {"limit": 1},
        }),
    )
    .await;
    assert_eq!(
        text.matches("\n## ").count(),
        1,
        "limit=1 means one: {text}"
    );

    let (text, is_error) = call(
        &state,
        "docs__query",
        json!({
            "corpus_id": "axum@0.8", "query": "x",
            "params": {"k_ftz": 5},
        }),
    )
    .await;
    assert!(is_error);
    assert!(
        text.contains("unknown search parameter 'k_ftz'") && text.contains("k_fts"),
        "a mistyped knob is named, not ignored: {text}"
    );

    let (text, is_error) = call(
        &state,
        "docs__query",
        json!({"corpus_id": "tower@0.5", "query": "x"}),
    )
    .await;
    assert!(is_error);
    assert!(text.contains("docs__resolve"), "{text}");
}

/// The stage lights up when a rerank model is configured, names itself in the
/// trace, and its scores decide the order (§6).
#[tokio::test]
async fn the_rerank_stage_runs_through_the_configured_model_and_decides_the_order() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let cid = seed(&state, "axum", "0.8").await;
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();

    // Without a rerank model the stage is skipped — with the reason, not in
    // silence.
    let r = lmgw_core::quickdoc::query::open_retriever(&state, &corpus)
        .await
        .unwrap();
    let params = lmgw_core::quickdoc::query::default_params(&state.snapshot());
    let fused = r.search("handler extractor layer", &params).await.unwrap();
    assert_eq!(
        fused.trace.rerank_skipped.as_deref(),
        Some("no rerank model is enabled on the aux router")
    );
    let fused_order: Vec<String> = fused.hits.iter().map(|h| h.chunk.id.clone()).collect();
    assert_eq!(fused_order.len(), 3);

    set_rerank_model(&state, "rerank-model").await;
    let r = lmgw_core::quickdoc::query::open_retriever(&state, &corpus)
        .await
        .unwrap();
    let ranked = r.search("handler extractor layer", &params).await.unwrap();
    assert_eq!(
        ranked.trace.rerank_model.as_deref(),
        Some("test-up/rerank-tgt")
    );
    assert!(ranked.trace.rerank_skipped.is_none());
    let ranked_order: Vec<String> = ranked.hits.iter().map(|h| h.chunk.id.clone()).collect();
    let mut reversed = fused_order.clone();
    reversed.reverse();
    assert_eq!(
        ranked_order, reversed,
        "the reranker's scores must decide the final order"
    );
    assert!(ranked.hits[0].rerank_score.is_some());

    // And a request may still turn the stage off for itself.
    let off = lmgw_core::quickdoc::query::apply_params(&params, &json!({"rerank": false})).unwrap();
    let plain = r.search("handler extractor layer", &off).await.unwrap();
    assert!(plain.trace.rerank.is_empty());
    assert_eq!(
        plain
            .hits
            .iter()
            .map(|h| h.chunk.id.clone())
            .collect::<Vec<_>>(),
        fused_order
    );
}

// ---------------------------------------------------------------------------
// docs__request
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_dedupes_by_counter_and_short_circuits_on_an_existing_corpus() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    seed(&state, "axum", "0.8").await;

    let v = call_json(
        &state,
        "docs__request",
        json!({"library": "tower", "reason": "need Layer docs"}),
    )
    .await;
    assert_eq!(v["filed"], true);
    assert_eq!(v["times_requested"], 1);
    assert_eq!(v["status"], "pending");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("no corpus will appear"),
        "{v}"
    );

    let v = call_json(&state, "docs__request", json!({"library": "tower"})).await;
    assert_eq!(
        v["times_requested"], 2,
        "a repeat bumps, it does not pile up"
    );
    assert_eq!(
        qstore::list_doc_requests(&state.corpus, "pending")
            .await
            .unwrap()
            .len(),
        1
    );

    // Already ingested: the answer is the corpus id, and nothing is filed.
    let v = call_json(&state, "docs__request", json!({"library": "axum"})).await;
    assert_eq!(v["filed"], false);
    assert_eq!(v["corpus_id"], "axum@0.8");
    assert_eq!(v["next_step"], "docs__query");
    assert_eq!(
        qstore::list_doc_requests(&state.corpus, "pending")
            .await
            .unwrap()
            .len(),
        1,
        "the short circuit must not file anything"
    );
}

/// A corpus that exists but cannot answer is not an answer. Only a `ready`
/// corpus with chunks short-circuits the request; a failed, half-finished or
/// empty one is filed like any other — with the corpus and its state named, so
/// the agent can say why asking again did not help and the owner can see that
/// somebody wanted the thing their ingest failed at.
#[tokio::test]
async fn a_corpus_that_cannot_answer_does_not_short_circuit_a_request() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let cid = seed(&state, "axum", "0.8").await;

    for status in ["failed", "ingesting"] {
        qstore::set_corpus_status(&state.corpus, cid, status)
            .await
            .unwrap();
        let v = call_json(&state, "docs__request", json!({"library": "axum"})).await;
        assert_eq!(v["filed"], true, "'{status}' is not an answer: {v}");
        assert_eq!(v["existing_corpus"]["corpus_id"], "axum@0.8");
        assert_eq!(v["existing_corpus"]["status"], status);
        let msg = v["message"].as_str().unwrap();
        assert!(
            msg.contains("axum@0.8") && msg.contains(status),
            "the agent is told what is there and why it is no use: {v}"
        );
    }

    // Two repeats, one row: the request was bumped, not piled up.
    let pending = qstore::list_doc_requests(&state.corpus, "pending")
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].count, 2);

    // An empty corpus is the same case, whatever its status says: `ready` with
    // no chunks is a row, not documentation.
    qstore::insert_corpus(
        &state.corpus,
        &NewCorpus {
            library: "hyper".into(),
            version: "1.0".into(),
            status: "ready".into(),
            embed: identity(),
            ingest_model: "ingest-model".into(),
            ingest_prompt_version: "v1".into(),
            crawl_date: "2026-08-29T10:00:00Z".into(),
            source_kind: "markdown".into(),
        },
    )
    .await
    .unwrap();
    let v = call_json(&state, "docs__request", json!({"library": "hyper"})).await;
    assert_eq!(v["filed"], true, "no chunks, no answer: {v}");
    assert_eq!(v["existing_corpus"]["chunk_count"], 0);
}

/// Dismissing is the owner saying no, and an agent asking again is not a vote
/// that overrides it. The counter still moves, so renewed demand is visible
/// where the owner looks for it — a request that reopened itself would climb
/// back to the top of the queue every time any agent asked.
#[tokio::test]
async fn a_dismissed_request_stays_dismissed_but_still_counts() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;

    let r = qstore::file_doc_request(&state.corpus, "tower", "", None, Some("agent-a"))
        .await
        .unwrap();
    qstore::set_doc_request_status(&state.corpus, r.id, "dismissed")
        .await
        .unwrap();

    let v = call_json(&state, "docs__request", json!({"library": "tower"})).await;
    assert_eq!(v["status"], "dismissed", "the owner's no holds: {v}");
    assert_eq!(v["times_requested"], 2);
    assert!(
        qstore::list_doc_requests(&state.corpus, "pending")
            .await
            .unwrap()
            .is_empty(),
        "and it does not climb back into the queue"
    );
    let dismissed = qstore::list_doc_requests(&state.corpus, "dismissed")
        .await
        .unwrap();
    assert_eq!(dismissed.len(), 1);
    assert_eq!(
        dismissed[0].count, 2,
        "renewed demand is still visible under the dismissed filter"
    );
}

/// A fulfilled request does reopen. `fulfilled` is only ever set by an ingest
/// finishing, so a fresh request against it means the corpus that answered it
/// is gone or unusable — the ready-corpus short circuit would have fired
/// otherwise and this call would never have happened.
#[tokio::test]
async fn a_fulfilled_request_reopens_when_it_is_asked_for_again() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;

    qstore::file_doc_request(&state.corpus, "tower", "", None, None)
        .await
        .unwrap();
    qstore::fulfill_doc_requests(&state.corpus, "tower", "")
        .await
        .unwrap();

    let v = call_json(&state, "docs__request", json!({"library": "tower"})).await;
    assert_eq!(v["status"], "pending");
    assert_eq!(
        qstore::list_doc_requests(&state.corpus, "pending")
            .await
            .unwrap()
            .len(),
        1,
        "the corpus that closed it is not there any more"
    );
}

/// The `re_embed_required` badge covers two states that mean opposite things to
/// a caller, and the advice attached to a resolve answer has to say which one
/// it is. Telling an agent "BM25 still works" about a corpus whose embedding
/// model is gone is simply false — `docs__query` hard-errors against it.
#[tokio::test]
async fn the_re_embed_hint_says_which_kind_of_re_embed_it_is() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let cid = seed(&state, "axum", "0.8").await;

    // Mid re-embed: the pin still resolves, so the corpus answers with BM25
    // carrying the part the vectors do not.
    qstore::set_corpus_status(&state.corpus, cid, "re_embed_required")
        .await
        .unwrap();
    let v = call_json(&state, "docs__resolve", json!({"library": "axum"})).await;
    let hint = v["hint"].as_str().unwrap();
    assert!(hint.contains("BM25"), "{hint}");
    assert!(!hint.contains("fails until"), "nothing here fails: {hint}");
    let (_, is_error) = call(
        &state,
        "docs__query",
        json!({"corpus_id": "axum@0.8", "query": "extractor"}),
    )
    .await;
    assert!(!is_error, "a mid-re-embed corpus still answers");

    // Pin lost: no model on this gateway resolves to it any more.
    let orphan = qstore::insert_corpus(
        &state.corpus,
        &NewCorpus {
            library: "tower".into(),
            version: "0.5".into(),
            status: "ready".into(),
            embed: EmbedIdentity::new("gone-upstream", "gone-model", DIMS),
            ingest_model: "ingest-model".into(),
            ingest_prompt_version: "v1".into(),
            crawl_date: "2026-08-29T10:00:00Z".into(),
            source_kind: "markdown".into(),
        },
    )
    .await
    .unwrap();
    assert!(orphan > 0);

    let v = call_json(&state, "docs__resolve", json!({"library": "tower"})).await;
    let hint = v["hint"].as_str().unwrap();
    assert!(
        hint.contains("fails until") && !hint.contains("BM25"),
        "an unqueryable corpus must not be advertised as searchable: {hint}"
    );
    assert_eq!(v["matches"][0]["embed_status"], "re_embed_required");
    let (msg, is_error) = call(
        &state,
        "docs__query",
        json!({"corpus_id": "tower@0.5", "query": "layer"}),
    )
    .await;
    assert!(
        is_error,
        "the hint has to match what the query actually does: {msg}"
    );
}

/// The whole surface over the real northbound plane, including the one thing
/// only the transport knows: who is asking.
#[tokio::test]
async fn the_docs_tools_are_served_on_mcp_and_record_the_client_that_asked() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    seed(&state, "axum", "0.8").await;
    let base = serve(state.clone()).await;
    let http = base.client();

    let resp = http
        .post(format!("{base}/mcp"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "clientInfo": {"name": "claude-code", "version": "9"},
            },
        }))
        .send()
        .await
        .unwrap();
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let body: Value = resp.json().await.unwrap();
    let instructions = body["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("docs__resolve"), "{instructions}");

    let post = |body: Value| {
        let http = http.clone();
        let base = base.clone();
        let sid = sid.clone();
        async move {
            http.post(format!("{base}/mcp"))
                .header("mcp-session-id", sid)
                .json(&body)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };

    let body = post(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).await;
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(names.contains(&"docs__resolve"), "{names:?}");
    assert!(names.contains(&"docs__query"), "{names:?}");
    assert!(names.contains(&"docs__request"), "{names:?}");
    assert!(
        !names.iter().any(|n| n.starts_with("lmgw__")),
        "the self-admin plane stays separate: {names:?}"
    );

    let body = post(json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "docs__request", "arguments": {"library": "tower"}},
    }))
    .await;
    assert_eq!(body["result"]["isError"], false, "{body}");

    let filed = &qstore::list_doc_requests(&state.corpus, "pending")
        .await
        .unwrap()[0];
    assert_eq!(filed.library, "tower");
    assert_eq!(
        filed.client_name.as_deref(),
        Some("claude-code"),
        "the initialize client name is what the owner's queue shows"
    );

    // The call is audited like any other tool call (§10).
    let logs = store::query_logs(
        &state.db,
        &store::LogFilter {
            alias: None,
            upstream_name: None,
            errors_only: false,
            limit: 10,
            before_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        logs.iter()
            .any(|r| r.mcp_tool.as_deref() == Some("docs__request")
                && r.upstream_name.as_deref() == Some("lmgw (docs)")),
        "{logs:?}"
    );
}

// ---------------------------------------------------------------------------
// eval runs and the badge
// ---------------------------------------------------------------------------

async fn add_golden(state: &SharedState, cid: i64, query: &str, expected: &[String]) -> i64 {
    qstore::insert_golden_query(&state.corpus, cid, query, expected, "manual")
        .await
        .unwrap()
}

async fn chunk_ids(state: &SharedState, cid: i64) -> Vec<String> {
    let docs = qstore::list_documents(&state.corpus, cid).await.unwrap();
    qstore::list_chunks(&state.corpus, docs[0].id)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect()
}

async fn run_eval(state: &SharedState, cid: i64, k: usize) -> Value {
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();
    let id = eval_job::start(state, &corpus, Some(k), None)
        .await
        .unwrap()
        .id();
    let row = wait_job(state, id).await;
    serde_json::from_str(row.result.as_deref().unwrap()).unwrap()
}

/// Wait for a job to end as `failed`, and hand back its row — the mirror of
/// [`wait_job`], for the runs that are *supposed* to be refused.
async fn wait_failed(state: &SharedState, id: i64) -> store::JobRow {
    for _ in 0..600 {
        let row = store::get_job(&state.db, id).await.unwrap().unwrap();
        match row.status.as_str() {
            "failed" => return row,
            "done" | "canceled" => panic!("job {id} ended as {}, not failed", row.status),
            _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    panic!("job {id} never finished");
}

/// Wait for a job to finish; anything but `done` is a failed test.
async fn wait_job(state: &SharedState, id: i64) -> store::JobRow {
    for _ in 0..600 {
        let row = store::get_job(&state.db, id).await.unwrap().unwrap();
        match row.status.as_str() {
            "done" => return row,
            "failed" | "canceled" => panic!("job {id} ended as {}: {:?}", row.status, row.error),
            _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    panic!("job {id} never finished");
}

#[tokio::test]
async fn an_eval_run_scores_the_corpus_and_lights_then_clears_the_badge() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let cid = seed(&state, "axum", "0.8").await;
    let ids = chunk_ids(&state, cid).await;
    let by_heading = |h: &str| {
        let idx = CHUNKS.iter().position(|(head, _)| *head == h).unwrap();
        // `insert_chunks` returns ids in input order, and `list_chunks` orders
        // by span, which is the same order.
        ids[idx].clone()
    };

    add_golden(
        &state,
        cid,
        "router route handler path",
        &[by_heading("Routing")],
    )
    .await;
    add_golden(
        &state,
        cid,
        "extractor typed data request",
        &[by_heading("Extractors")],
    )
    .await;

    let first = run_eval(&state, cid, 1).await;
    assert_eq!(first["queries"], 2);
    assert_eq!(first["hit_at_k"], 1.0, "{first}");
    assert_eq!(first["mrr"], 1.0);
    assert_eq!(first["regression"], false, "a first run has no baseline");

    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(corpus.eval_score, Some(1.0));
    assert_eq!(corpus.eval_best, Some(1.0));
    assert_eq!(corpus.eval_k, 1);
    assert!(!corpus.eval_regression);
    assert!(!corpus.eval_at.is_empty());

    // A query that can never hit at k=1 drags the score below the best.
    let unanswerable = add_golden(
        &state,
        cid,
        "layers wrap a service tracing",
        &[by_heading("Routing")],
    )
    .await;
    let second = run_eval(&state, cid, 1).await;
    assert!(
        (second["hit_at_k"].as_f64().unwrap() - 2.0 / 3.0).abs() < 1e-6,
        "{second}"
    );
    assert_eq!(second["regression"], true);
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();
    assert!(corpus.eval_regression);
    assert_eq!(
        corpus.eval_best,
        Some(1.0),
        "the best is what it regressed from"
    );

    // Recovering clears it by itself — nobody has to remember to reset a badge.
    qstore::delete_golden_query(&state.corpus, unanswerable)
        .await
        .unwrap();
    let third = run_eval(&state, cid, 1).await;
    assert_eq!(third["hit_at_k"], 1.0);
    assert_eq!(third["regression"], false);
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();
    assert!(!corpus.eval_regression);

    // The history is retained, newest first, with the parameters each run was
    // measured under — two runs under different settings are not comparable.
    let runs = qstore::list_eval_runs(&state.corpus, cid, 0).await.unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].hit_at_k, 1.0);
    assert_eq!(runs[0].params["limit"], 1);
    assert_eq!(runs[0].report["per_query"].as_array().unwrap().len(), 2);
}

/// hit@1 and hit@10 are different measurements, and so are two runs with
/// different retrieval settings. A badge that fires across those classes fires
/// for a change the owner made deliberately — which is how a badge becomes
/// something everyone ignores.
#[tokio::test]
async fn a_regression_is_only_measured_against_a_comparable_run() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let cid = seed(&state, "axum", "0.8").await;

    // A high score at k=10, then a lower one at k=1: not comparable, so not a
    // regression — and the badge quotes a best from the class it just measured.
    record_score(&state, cid, 0.9, 10).await;
    record_score(&state, cid, 0.6, 1).await;
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !corpus.eval_regression,
        "hit@1 0.6 is not a fall from hit@10 0.9, it is a different measurement"
    );
    assert_eq!(corpus.eval_best, Some(0.6), "the best at this k");
    assert_eq!(corpus.eval_k, 1);

    // Within the class the badge works exactly as before, and clears itself.
    record_score(&state, cid, 0.4, 1).await;
    assert!(
        qstore::get_corpus(&state.corpus, cid)
            .await
            .unwrap()
            .unwrap()
            .eval_regression,
        "0.4 is below the 0.6 measured at the same k with the same params"
    );
    record_score(&state, cid, 0.7, 1).await;
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();
    assert!(!corpus.eval_regression, "recovery clears it by itself");
    assert_eq!(corpus.eval_best, Some(0.7));

    // Different stage parameters at the same k are their own class too.
    qstore::record_eval_run(
        &state.corpus,
        &qstore::NewEvalRun {
            corpus_id: cid,
            k: 1,
            queries: 10,
            hit_at_k: 0.3,
            mrr: 0.3,
            orphaned_queries: 0,
            params: json!({"rerank": false}),
            report: json!({}),
        },
    )
    .await
    .unwrap();
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !corpus.eval_regression,
        "a run with the rerank stage off is not a fall from one with it on"
    );
    assert_eq!(corpus.eval_best, Some(0.3));

    // Every run is still in the history, whatever class it belongs to.
    assert_eq!(
        qstore::list_eval_runs(&state.corpus, cid, 0)
            .await
            .unwrap()
            .len(),
        5
    );
}

/// An eval with nothing to measure would write a 0.0 that reads as "retrieval
/// is broken". It is a missing input, and says so.
#[tokio::test]
async fn an_eval_with_no_golden_queries_refuses_instead_of_scoring_zero() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let cid = seed(&state, "axum", "0.8").await;
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();

    let id = eval_job::start(&state, &corpus, None, None)
        .await
        .unwrap()
        .id();
    let mut row = store::get_job(&state.db, id).await.unwrap().unwrap();
    for _ in 0..600 {
        if row.status == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        row = store::get_job(&state.db, id).await.unwrap().unwrap();
    }
    assert_eq!(row.status, "failed");
    assert!(
        row.error.as_deref().unwrap().contains("no golden queries"),
        "{row:?}"
    );
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        corpus.eval_score, None,
        "nothing was scored, so nothing was written"
    );
}

// ---------------------------------------------------------------------------
// Synthetic golden queries (§10) and their curation queue (§11)
// ---------------------------------------------------------------------------

/// The whole generator contract in one run: the model writes questions about
/// the chunks it is shown, a copied sentence is refused and corrected, code —
/// not the model — attaches the expectation, and **nothing** becomes a golden
/// query on the way.
#[tokio::test]
async fn a_generation_run_files_candidates_and_never_a_golden_query() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    mount_generator(&mock).await;
    let state = setup(&mock).await;
    let cid = seed(&state, "axum", "0.8").await;
    let ids = chunk_ids(&state, cid).await;
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();

    let job = golden_job::start(&state, &corpus, None, Some(1))
        .await
        .unwrap()
        .id();
    let row = wait_job(&state, job).await;
    let result: Value = serde_json::from_str(row.result.as_deref().unwrap()).unwrap();
    assert_eq!(
        result["sampled"], 3,
        "no sample size given means every chunk: {result}"
    );
    assert_eq!(result["candidates"], 3);
    assert_eq!(
        result["rejected"], 1,
        "the sentence copied out of the section must have been refused"
    );
    assert_eq!(result["duplicates"], 0);
    assert_eq!(result["status"], "pending curation");

    // Typed progress, in the kind's own unit.
    let progress: Value = serde_json::from_str(&row.progress).unwrap();
    assert_eq!(progress["done"], 3);
    assert_eq!(progress["total"], 3);
    assert_eq!(progress["detail"]["corpus"], "axum@0.8");
    assert_eq!(progress["detail"]["model"], "ingest-model");
    assert_eq!(progress["detail"]["candidates"], 3);
    assert_eq!(progress["detail"]["rejected"], 1);

    assert!(
        qstore::list_golden_queries(&state.corpus, cid)
            .await
            .unwrap()
            .is_empty(),
        "a generation run writes proposals, never golden queries"
    );
    let queue = qstore::list_golden_candidates(&state.corpus, cid, "pending")
        .await
        .unwrap();
    assert_eq!(queue.len(), 3);
    for c in &queue {
        assert_eq!(c.status, "pending");
        assert_eq!(c.model, "ingest-model", "attributable to a model");
        assert!(!c.rationale.is_empty());
        assert_eq!(
            c.expected_chunk_ids.len(),
            1,
            "code attaches the chunk it showed the model"
        );
        assert!(
            ids.contains(&c.expected_chunk_ids[0]),
            "an expectation cannot be hallucinated: {c:?}"
        );
        assert!(
            !CHUNKS.iter().any(|(_, payload)| c.query == *payload),
            "a sentence copied out of the section is not a question: {c:?}"
        );
    }
    assert!(queue.iter().any(|c| c.query.contains("routing")));
}

/// Golden generation is unattended batch work, so a GPU hold **refuses** it
/// and never re-routes it (gpu-hold design §2, §7.13) — the run is nobody's
/// interactive request, it can be re-run in a minute, and quietly spending a
/// cloud model's tokens on a background job is not a fallback anyone asked
/// for. `GenerationPlan::build` therefore keeps plain `resolve` and checks the
/// hold itself, before the first chunk.
///
/// The global fallback is configured on purpose: the point is that a batch job
/// ignores an available one, not merely that none exists to try. The generator
/// mock is mounted for the same reason — if the check were missing, the run
/// would succeed against it, and the request count is what says it did not.
#[tokio::test]
async fn golden_generation_is_refused_under_hold_and_never_reaches_the_cloud_mock() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    mount_generator(&mock).await;
    let state = setup(&mock).await;

    // A genuinely local generator: `setup`'s "ingest-model" is a cloud alias
    // and would sail through the hold untouched.
    store::insert_local_model(
        &state.db,
        &NewLocalModel {
            model_id: "local-gen".into(),
            gguf_path: "local-gen.gguf".into(),
            params: Default::default(),
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            ladder: vec![],
        },
    )
    .await
    .unwrap();
    let mut s = state.snapshot().settings.clone();
    s.hold.fallback_alias = Some("ingest-model".into());
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    lmgw_core::ops::hold_set(&state, true).await.unwrap();

    let cid = seed_with_model(&state, "axum", "0.8", "local-gen").await;
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();
    let requests_before = mock.received_requests().await.unwrap().len();

    let job = golden_job::start(&state, &corpus, None, Some(1))
        .await
        .unwrap()
        .id();
    let row = wait_failed(&state, job).await;
    let err = row.error.unwrap_or_default();
    assert!(
        err.contains("holding the GPU") || err.contains("gpu_hold"),
        "the job row carries the hold as its own error: {err}"
    );

    assert_eq!(
        mock.received_requests().await.unwrap().len(),
        requests_before,
        "a batch job under hold spends nothing — not one turn against the fallback"
    );
    assert!(
        qstore::list_golden_candidates(&state.corpus, cid, "pending")
            .await
            .unwrap()
            .is_empty(),
        "and files nothing"
    );
}

/// §11's curation flow: the queue is the only path from a candidate to a golden
/// query, a decision is made once, and a rejection is remembered rather than
/// deleted.
#[tokio::test]
async fn candidates_are_curated_over_the_api_and_decided_once() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    mount_generator(&mock).await;
    let state = setup(&mock).await;
    let cid = seed(&state, "axum", "0.8").await;
    let base = serve(state.clone()).await;
    let http = base.client();

    let started: Value = http
        .post(format!("{base}/api/docs/golden/generate"))
        .json(&json!({ "corpus_id": cid, "sample": 3, "per_chunk": 1 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(started["ok"], true, "{started}");
    wait_job(&state, started["job_id"].as_i64().unwrap()).await;

    let queue: Value = http
        .get(format!("{base}/api/docs/golden/candidates?corpus_id={cid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let candidates = queue["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 3, "pending is what the queue shows");
    let first = candidates[0]["id"].as_i64().unwrap();
    let second = candidates[1]["id"].as_i64().unwrap();
    let chunk_id = candidates[0]["expected_chunk_ids"][0].as_str().unwrap();
    assert!(
        queue["chunks"][chunk_id]["payload"]
            .as_str()
            .is_some_and(|p| !p.is_empty()),
        "the section travels with the proposal, so it is not curated blind: {queue}"
    );

    // ---- accept: the one path to a golden query, and it keeps its origin ----
    let accepted: Value = http
        .post(format!("{base}/api/docs/golden/candidates/{first}/accept"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gid = accepted["id"].as_i64().unwrap();
    let golden = qstore::get_golden_query(&state.corpus, gid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        golden.origin, "synthetic",
        "a curated query says where it came from"
    );
    assert_eq!(golden.expected_chunk_ids, vec![chunk_id.to_string()]);
    assert_eq!(golden.query, candidates[0]["query"].as_str().unwrap());

    let resp = http
        .post(format!("{base}/api/docs/golden/candidates/{first}/accept"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "a decision is made once");
    let err: Value = resp.json().await.unwrap();
    assert!(
        err["message"].as_str().unwrap().contains("accepted"),
        "{err}"
    );

    // ---- reject: discarded, but remembered ----
    http.post(format!("{base}/api/docs/golden/candidates/{second}/reject"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let queue: Value = http
        .get(format!("{base}/api/docs/golden/candidates?corpus_id={cid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        queue["candidates"].as_array().unwrap().len(),
        1,
        "a decided candidate leaves the queue"
    );
    let rejected: Value = http
        .get(format!(
            "{base}/api/docs/golden/candidates?corpus_id={cid}&status=rejected"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rejected["candidates"].as_array().unwrap().len(), 1);

    // ---- and only the accepted one is ever scored ----
    assert_eq!(
        qstore::list_golden_queries(&state.corpus, cid)
            .await
            .unwrap()
            .len(),
        1
    );
    let run = run_eval(&state, cid, 3).await;
    assert_eq!(
        run["queries"], 1,
        "an eval scores golden queries only: {run}"
    );

    // ---- an accept may carry the owner's edit, checked like any other ----
    let third = queue["candidates"][0]["id"].as_i64().unwrap();
    let resp = http
        .post(format!("{base}/api/docs/golden/candidates/{third}/accept"))
        .json(&json!({"expected_chunk_ids": ["deadbeef"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let err: Value = resp.json().await.unwrap();
    assert!(
        err["message"].as_str().unwrap().contains("deadbeef"),
        "{err}"
    );
    let still_pending = qstore::get_golden_candidate(&state.corpus, third)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        still_pending.status, "pending",
        "a refused accept leaves the candidate where it was"
    );

    http.post(format!("{base}/api/docs/golden/candidates/{third}/accept"))
        .json(&json!({"query": "how do I wrap a service in a layer?"}))
        .send()
        .await
        .unwrap();
    let golden = qstore::list_golden_queries(&state.corpus, cid)
        .await
        .unwrap();
    assert_eq!(golden.len(), 2);
    assert!(golden
        .iter()
        .any(|g| g.query == "how do I wrap a service in a layer?"));
}

/// A corpus with nothing in it is a missing input, not a run that proposes
/// questions about nothing.
#[tokio::test]
async fn generating_from_an_empty_corpus_refuses_instead_of_inventing_questions() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let cid = qstore::insert_corpus(
        &state.corpus,
        &NewCorpus {
            library: "tower".into(),
            version: "0.5".into(),
            status: "ready".into(),
            embed: identity(),
            ingest_model: "ingest-model".into(),
            ingest_prompt_version: "v1".into(),
            crawl_date: String::new(),
            source_kind: "markdown".into(),
        },
    )
    .await
    .unwrap();
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();

    let id = golden_job::start(&state, &corpus, None, None)
        .await
        .unwrap()
        .id();
    let mut row = store::get_job(&state.db, id).await.unwrap().unwrap();
    for _ in 0..600 {
        if row.status == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        row = store::get_job(&state.db, id).await.unwrap().unwrap();
    }
    assert_eq!(row.status, "failed");
    assert!(
        row.error.as_deref().unwrap().contains("no chunks"),
        "{row:?}"
    );
}

// ---------------------------------------------------------------------------
// Export / import
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_corpus_exports_to_a_file_and_imports_with_its_vectors_intact() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let source = setup(&mock).await;
    let cid = seed(&source, "axum", "0.8").await;
    add_golden(&source, cid, "extractor", &[]).await;
    record_score(&source, cid, 0.75, 10).await;

    let manifest = portability::manifest(&source, Some(cid)).await.unwrap();
    assert_eq!(manifest.schema_version, qstore::schema_version());
    assert_eq!(manifest.corpora.len(), 1);
    assert_eq!(manifest.corpora[0].corpus_id, "axum@0.8");
    assert_eq!(manifest.corpora[0].chunk_count, 3);
    assert_eq!(manifest.corpora[0].embed_model, "embed-tgt");
    assert!(manifest.byte_size > 0);

    let staged = portability::export(&source, Some(cid)).await.unwrap();

    // A second gateway with the same embedding model available.
    let target = setup(&mock).await;
    let report = portability::import(&target, &staged.path, false, true)
        .await
        .unwrap();
    assert!(report.dry_run);
    assert_eq!(
        report.imported[0].embed_alias.as_deref(),
        Some("embed-model")
    );
    assert!(
        qstore::list_corpora(&target.corpus)
            .await
            .unwrap()
            .is_empty(),
        "validate_only writes nothing"
    );

    let report = portability::import(&target, &staged.path, false, false)
        .await
        .unwrap();
    assert_eq!(report.file_schema_version, qstore::schema_version());
    assert_eq!(report.imported[0].corpus_id, "axum@0.8");
    assert_eq!(report.imported[0].chunks, 3);
    assert_eq!(report.imported[0].documents, 1);

    let imported = qstore::get_corpus_by_id(&target.corpus, "axum@0.8")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(imported.chunk_count, 3);
    assert_eq!(imported.eval_score, Some(0.75));
    assert_eq!(
        qstore::count_unembedded(&target.corpus, imported.id)
            .await
            .unwrap(),
        0,
        "the vectors travel with the chunks"
    );
    assert_eq!(
        qstore::list_golden_queries(&target.corpus, imported.id)
            .await
            .unwrap()
            .len(),
        1
    );

    // And it answers questions on the other side.
    let r = lmgw_core::quickdoc::query::open_retriever(&target, &imported)
        .await
        .unwrap();
    let params = lmgw_core::quickdoc::query::default_params(&target.snapshot());
    let found = r.search("extractor typed data", &params).await.unwrap();
    assert!(found.hits[0].chunk.payload.contains("An extractor pulls"));
    assert!(!found.trace.knn.is_empty(), "the KNN stage really ran");

    // Re-importing the same file needs `replace` — merging two corpora that
    // claim one id would produce one matching neither's metadata.
    let err = portability::import(&target, &staged.path, false, false)
        .await
        .unwrap_err();
    assert!(
        err.contains("already exists") && err.contains("replace"),
        "{err}"
    );
    portability::import(&target, &staged.path, true, false)
        .await
        .unwrap();
    assert_eq!(qstore::list_corpora(&target.corpus).await.unwrap().len(), 1);
}

/// The §10 check that matters most: a corpus whose pinned embedding model this
/// gateway cannot resolve would arrive unqueryable, so it is refused by name
/// with the model it wants — before anything is written.
#[tokio::test]
async fn an_import_is_refused_when_the_embedding_model_is_not_available_here() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let source = setup(&mock).await;
    let cid = seed(&source, "axum", "0.8").await;
    let staged = portability::export(&source, Some(cid)).await.unwrap();

    let bare = AppState::init_for_tests().await.unwrap();
    let err = portability::import(&bare, &staged.path, false, true)
        .await
        .unwrap_err();
    assert!(err.contains("axum@0.8"), "{err}");
    assert!(err.contains("test-up/embed-tgt (32d)"), "{err}");
    assert!(err.contains("unqueryable"), "{err}");
    assert!(qstore::list_corpora(&bare.corpus).await.unwrap().is_empty());
}

#[tokio::test]
async fn importing_something_that_is_not_a_corpus_file_says_so() {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nonsense.db");
    std::fs::write(&path, b"not a database").unwrap();
    let err = portability::import(&state, &path, false, true)
        .await
        .unwrap_err();
    assert!(
        err.contains("not a quickdoc corpus database") || err.contains("corpus db error"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// The dashboard plane (§10)
// ---------------------------------------------------------------------------

/// A score is only meaningful as a trend, so the history behind it may not be
/// quietly cut off at some constant nobody can see or raise. `?limit=` is the
/// caller's choice and absent means all of it — the same convention
/// `GET /api/docs/eval` already follows.
#[tokio::test]
async fn the_corpus_detail_returns_the_whole_eval_history_not_a_hidden_window() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let cid = seed(&state, "axum", "0.8").await;
    for i in 0..25 {
        record_score(&state, cid, 0.5 + f64::from(i) / 100.0, 10).await;
    }
    let base = serve(state.clone()).await;

    let body: Value = base
        .client()
        .get(format!("{base}/api/docs/corpora/{cid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["eval_runs"].as_array().unwrap().len(),
        25,
        "the detail view must not truncate the history it is the face of"
    );

    let body: Value = base
        .client()
        .get(format!("{base}/api/docs/corpora/{cid}?limit=5"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["eval_runs"].as_array().unwrap().len(),
        5,
        "a caller that wants a window asks for one"
    );
}

#[tokio::test]
async fn the_debug_endpoint_returns_every_stage_of_the_trace() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    set_rerank_model(&state, "rerank-model").await;
    let cid = seed(&state, "axum", "0.8").await;
    let base = serve(state.clone()).await;

    let body: Value = base
        .client()
        .post(format!("{base}/api/docs/search"))
        .json(&json!({
            "corpus_id": cid, "query": "extractor typed data",
            "params": {"k_fts": 5, "k_vec": 5},
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["corpus_id"], "axum@0.8");
    let trace = &body["trace"];
    assert_eq!(
        trace["params"]["k_fts"], 5,
        "the override reached the stage"
    );
    assert_eq!(trace["params"]["k_vec"], 5);
    assert!(!trace["fts"].as_array().unwrap().is_empty(), "{trace}");
    assert!(!trace["knn"].as_array().unwrap().is_empty(), "{trace}");
    assert!(!trace["fused"].as_array().unwrap().is_empty(), "{trace}");
    assert!(!trace["rerank"].as_array().unwrap().is_empty(), "{trace}");
    assert_eq!(trace["rerank_model"], "test-up/rerank-tgt");
    assert_eq!(trace["token_counter"], "tiktoken o200k_base");
    assert!(trace["fts_query"]
        .as_str()
        .unwrap()
        .contains("\"extractor\""));
    assert!(trace["resident_vectors"].as_u64().unwrap() >= 3);
    assert!(trace["timings"]["total_ms"].as_f64().is_some());
    // The same answer a docs__query caller would get, rendered alongside.
    assert!(body["markdown"].as_str().unwrap().starts_with("# axum@0.8"));
    assert!(!body["urls"].as_object().unwrap().is_empty());
}

#[tokio::test]
async fn corpora_golden_queries_and_the_request_queue_are_manageable_over_the_api() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let cid = seed(&state, "axum", "0.8").await;
    let ids = chunk_ids(&state, cid).await;
    qstore::file_doc_request(
        &state.corpus,
        "tower",
        "",
        Some("need Layer"),
        Some("agent"),
    )
    .await
    .unwrap();
    let base = serve(state.clone()).await;
    let http = base.client();

    // ---- corpus list, with the badges and the visible resident cost ----
    let body: Value = http
        .get(format!("{base}/api/docs/corpora"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let c = &body["corpora"][0];
    assert_eq!(c["corpus_id"], "axum@0.8");
    assert_eq!(c["chunk_count"], 3);
    assert_eq!(c["unembedded_chunks"], 0);
    assert_eq!(c["resident_bytes"], 3 * DIMS * 2);
    assert_eq!(c["embed_status"], "ok");
    assert_eq!(c["eval_status"], "unmeasured");
    assert_eq!(body["pending_requests"], 1);

    // ---- golden query CRUD ----
    let created: Value = http
        .post(format!("{base}/api/docs/golden"))
        .json(&json!({
            "corpus_id": cid, "query": "extractor", "expected_chunk_ids": [ids[1]],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gid = created["id"].as_i64().unwrap();

    let resp = http
        .post(format!("{base}/api/docs/golden"))
        .json(&json!({
            "corpus_id": cid, "query": "bad", "expected_chunk_ids": ["deadbeef"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let err: Value = resp.json().await.unwrap();
    assert!(
        err["message"].as_str().unwrap().contains("deadbeef"),
        "an expected id that is not in the corpus would score as a permanent \
         miss and read as a regression: {err}"
    );

    http.post(format!("{base}/api/docs/golden"))
        .json(&json!({"id": gid, "query": "extractor typed data", "expected_chunk_ids": [ids[1]]}))
        .send()
        .await
        .unwrap();
    let body: Value = http
        .get(format!("{base}/api/docs/golden?corpus_id={cid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["golden_queries"][0]["query"], "extractor typed data");

    // ---- the doc-request queue ----
    let body: Value = http
        .get(format!("{base}/api/docs/requests?status=pending"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rid = body["requests"][0]["id"].as_i64().unwrap();
    assert_eq!(body["requests"][0]["library"], "tower");
    assert_eq!(body["requests"][0]["client_name"], "agent");

    let resp = http
        .post(format!("{base}/api/docs/requests/{rid}/status"))
        .json(&json!({"status": "fulfilled"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        400,
        "only a finished ingest may call a request fulfilled"
    );

    http.post(format!("{base}/api/docs/requests/{rid}/status"))
        .json(&json!({"status": "dismissed"}))
        .send()
        .await
        .unwrap();
    let body: Value = http
        .get(format!("{base}/api/docs/requests?status=pending"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body["requests"].as_array().unwrap().is_empty());

    // ---- the corpus browser's two levels ----
    let body: Value = http
        .get(format!("{base}/api/docs/corpora/{cid}/documents"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let did = body["documents"][0]["id"].as_i64().unwrap();
    let body: Value = http
        .get(format!("{base}/api/docs/chunks?document_id={did}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["chunks"].as_array().unwrap().len(), 3);
    assert_eq!(body["chunks"][1]["derived_title"], "Extractors");

    // ---- and deleting the corpus takes its queries with it ----
    http.post(format!("{base}/api/docs/corpora/{cid}/delete"))
        .send()
        .await
        .unwrap();
    assert!(qstore::list_corpora(&state.corpus)
        .await
        .unwrap()
        .is_empty());
}

/// Creating a corpus pins the *resolved* embedding identity, not the alias that
/// was typed (§4), and refuses both models up front rather than in the job.
#[tokio::test]
async fn creating_a_corpus_pins_the_resolved_embedder_and_checks_both_models() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let base = serve(state.clone()).await;
    let http = base.client();

    let body: Value = http
        .post(format!("{base}/api/docs/corpora"))
        .json(&json!({
            "library": "tower", "version": "0.5",
            "embed_model": "embed-model", "ingest_model": "rerank-model",
            "sources": [{"root": "https://docs.rs/tower", "kind": "markdown"}],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["corpus"]["corpus_id"], "tower@0.5");
    assert_eq!(
        body["corpus"]["embed_identity"], "test-up/embed-tgt (32d)",
        "the pin is the resolved model, not the alias: {body}"
    );
    assert_eq!(body["corpus"]["embed_dims"], DIMS);
    assert_eq!(body["corpus"]["status"], "ingesting");
    assert_eq!(body["job_id"], Value::Null, "start was not requested");

    let resp = http
        .post(format!("{base}/api/docs/corpora"))
        .json(&json!({
            "library": "tower", "version": "0.5",
            "embed_model": "embed-model", "ingest_model": "rerank-model",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let resp = http
        .post(format!("{base}/api/docs/corpora"))
        .json(&json!({
            "library": "hyper", "version": "1.0",
            "embed_model": "embed-model", "ingest_model": "not-a-model",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let err: Value = resp.json().await.unwrap();
    assert!(
        err["message"].as_str().unwrap().contains("not-a-model"),
        "{err}"
    );
    assert_eq!(
        qstore::list_corpora(&state.corpus).await.unwrap().len(),
        1,
        "a refused create leaves no half-built corpus"
    );
}

/// The stage defaults are the owner's, and a request that overrides one keeps
/// the rest — a partial override must not silently fall back to the compiled-in
/// values.
#[tokio::test]
async fn stage_defaults_come_from_settings_and_partial_overrides_keep_the_rest() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let mut settings = state.snapshot().settings.clone();
    settings.docs_search.k_fts = 7;
    settings.docs_search.limit = 2;
    settings.docs_search.budget_tokens = 500;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let snap = state.snapshot();
    let base = lmgw_core::quickdoc::query::default_params(&snap);
    assert_eq!(base.k_fts, 7);
    assert_eq!(base.limit, 2);
    assert_eq!(base.budget_tokens, Some(500));

    let merged = lmgw_core::quickdoc::query::apply_params(&base, &json!({"limit": 3})).unwrap();
    assert_eq!(merged.limit, 3);
    assert_eq!(
        merged.k_fts, 7,
        "the untouched knob keeps the owner's value"
    );
    assert_eq!(merged.budget_tokens, Some(500));

    // Zero is how a caller says "no budget", and it is not the same as absent.
    seed(&state, "axum", "0.8").await;
    let (text, _) = call(
        &state,
        "docs__query",
        json!({"corpus_id": "axum@0.8", "query": "extractor", "budget_tokens": 0}),
    )
    .await;
    assert!(text.contains("no `budget_tokens` was set"), "{text}");
}

/// A reranker attached to the retriever must produce a score per document; a
/// short answer is an error, never zeros that would demote real hits.
#[tokio::test]
async fn a_reranker_that_scores_only_some_documents_is_an_error() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(FixtureEmbeddings)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "rerank-tgt", "object": "list",
            "results": [{"index": 0, "relevance_score": 1.0}],
        })))
        .mount(&mock)
        .await;
    let state = setup(&mock).await;
    set_rerank_model(&state, "rerank-model").await;
    let cid = seed(&state, "axum", "0.8").await;
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();

    let r = lmgw_core::quickdoc::query::open_retriever(&state, &corpus)
        .await
        .unwrap();
    let params = lmgw_core::quickdoc::query::default_params(&state.snapshot());
    let err = r
        .search("handler extractor layer", &params)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("1 scores for 3 documents"), "{err}");
}

/// The rerank stage is optional by design: a gateway with no rerank model still
/// answers, and the trace says why the stage did not run rather than leaving a
/// weaker ranking unexplained.
#[tokio::test]
async fn a_missing_rerank_model_is_explained_not_silent() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    set_rerank_model(&state, "").await;
    let cid = seed(&state, "axum", "0.8").await;
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();

    let r: Arc<_> = Arc::new(
        lmgw_core::quickdoc::query::open_retriever(&state, &corpus)
            .await
            .unwrap(),
    );
    let params = lmgw_core::quickdoc::query::default_params(&state.snapshot());
    let out = r.search("extractor", &params).await.unwrap();
    assert_eq!(
        out.trace.rerank_skipped.as_deref(),
        Some("no rerank model is enabled on the aux router")
    );
    assert!(!out.hits.is_empty(), "retrieval works without the stage");
}

// ---------------------------------------------------------------------------
// The shapes the Docs tab decodes (§11)
// ---------------------------------------------------------------------------

/// Every dashboard-plane body the Docs tab reads, decoded into the exact
/// `lmgw-api-types` DTOs the UI decodes them into.
///
/// The UI is a separate wasm bundle, so renaming a field on this side does not
/// break its build — it makes a tolerant DTO fall back to a default and the tab
/// renders a blank where the chunk count used to be. The tolerance is
/// deliberate (one side has to be able to grow first), so this is the gate
/// instead: parse each response as the page parses it and assert the values the
/// page actually shows survived the trip. It also sends the playground's own
/// `SearchParamsView` back as `params`, which is the other direction of the
/// same drift.
#[tokio::test]
async fn the_docs_tab_decodes_every_dashboard_response() {
    use lmgw_api_types as dto;
    use serde::de::DeserializeOwned;

    async fn get<T: DeserializeOwned>(http: &reqwest::Client, url: String) -> T {
        let resp = http.get(&url).send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        assert_eq!(status, 200, "GET {url}: {text}");
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("GET {url} does not decode as the UI's DTO ({e}): {text}"))
    }

    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    set_rerank_model(&state, "rerank-model").await;
    let cid = seed(&state, "axum", "0.8").await;
    let ids = chunk_ids(&state, cid).await;
    add_golden(&state, cid, "extractor typed data", &[ids[1].clone()]).await;
    run_eval(&state, cid, 2).await;
    qstore::insert_golden_candidate(
        &state.corpus,
        &qstore::NewGoldenCandidate {
            corpus_id: cid,
            query: "how do I use a middleware layer?".into(),
            expected_chunk_ids: vec![ids[2].clone()],
            rationale: "the Middleware section covers it".into(),
            model: "ingest-model".into(),
        },
    )
    .await
    .unwrap();
    qstore::file_doc_request(
        &state.corpus,
        "tower",
        "0.5",
        Some("need Layer"),
        Some("agent"),
    )
    .await
    .unwrap();
    let base = serve(state.clone()).await;
    let http = base.client();

    // ---- the list, its badges and the visible resident cost ----
    let overview: dto::DocsOverview = get(&http, format!("{base}/api/docs/corpora")).await;
    let c = &overview.corpora[0];
    assert_eq!(c.id, cid);
    assert_eq!(c.corpus_id, "axum@0.8");
    assert_eq!(c.chunk_count, 3);
    assert_eq!(c.unembedded_chunks, 0);
    assert_eq!(c.resident_bytes, 3 * DIMS as i64 * 2);
    assert_eq!(c.embed_identity, "test-up/embed-tgt (32d)");
    assert_eq!(c.embed_status, "ok");
    assert_eq!(c.eval_status, "ok");
    assert!(
        c.eval_score.is_some(),
        "a run happened, so there is a score"
    );
    assert_eq!(c.source_kind, "markdown");
    assert_eq!(c.sources.len(), 1);
    assert_eq!(c.sources[0].kind, "markdown");
    assert_eq!(c.crawl_date, "2026-08-29T10:00:00Z");
    assert_eq!(overview.pending_requests, 1, "the tab badge");
    assert_eq!(overview.rerank_model.as_deref(), Some("rerank-model"));
    assert!(overview.schema_version > 0);

    // ---- detail, and the corpus browser's two levels ----
    let detail: dto::CorpusDetail = get(&http, format!("{base}/api/docs/corpora/{cid}")).await;
    assert_eq!(detail.corpus.corpus_id, "axum@0.8");
    assert_eq!(detail.documents.len(), 1);
    assert_eq!(detail.golden_queries.len(), 1);
    assert_eq!(detail.eval_runs.len(), 1);

    let documents: dto::DocumentsResponse =
        get(&http, format!("{base}/api/docs/corpora/{cid}/documents")).await;
    let did = documents.documents[0].id;
    assert!(documents.documents[0].url.contains("docs.rs/axum"));
    assert!(!documents.documents[0].content_hash.is_empty());

    let chunks: dto::ChunksResponse =
        get(&http, format!("{base}/api/docs/chunks?document_id={did}")).await;
    assert_eq!(chunks.chunks.len(), 3);
    assert_eq!(chunks.chunks[1].derived_title, "Extractors");
    assert_eq!(
        chunks.chunks[1].payload, CHUNKS[1].1,
        "the browser shows the verbatim payload, byte for byte"
    );

    // ---- golden queries and the history behind the regression badge ----
    let golden: dto::GoldenResponse =
        get(&http, format!("{base}/api/docs/golden?corpus_id={cid}")).await;
    assert_eq!(
        golden.golden_queries[0].expected_chunk_ids,
        vec![ids[1].clone()]
    );
    assert_eq!(golden.golden_queries[0].origin, "manual");

    // ---- the curation queue, and the section it hands the owner to judge by ----
    let candidates: dto::GoldenCandidatesResponse = get(
        &http,
        format!("{base}/api/docs/golden/candidates?corpus_id={cid}"),
    )
    .await;
    let c = &candidates.candidates[0];
    assert_eq!(c.query, "how do I use a middleware layer?");
    assert_eq!(c.expected_chunk_ids, vec![ids[2].clone()]);
    assert_eq!(c.rationale, "the Middleware section covers it");
    assert_eq!(c.model, "ingest-model");
    assert_eq!(c.status, "pending");
    assert!(c.golden_query_id.is_none());
    assert!(c.decided_at.is_empty(), "it has not been decided");
    assert_eq!(
        candidates.chunks[&ids[2]].payload, CHUNKS[2].1,
        "the section travels with the proposal, verbatim"
    );

    let history: dto::EvalHistory =
        get(&http, format!("{base}/api/docs/eval?corpus_id={cid}")).await;
    let run = &history.eval_runs[0];
    assert_eq!(run.k, 2);
    assert_eq!(run.queries, 1);
    assert!(
        run.params.get("k_fts").is_some(),
        "a run travels with the parameters it was measured under: {}",
        run.params
    );
    assert!(
        !run.report.is_null(),
        "the per-query breakdown came through"
    );

    // ---- the request queue ----
    let requests: dto::DocRequestsResponse =
        get(&http, format!("{base}/api/docs/requests?status=pending")).await;
    let r = &requests.requests[0];
    assert_eq!(r.library, "tower");
    assert_eq!(r.version, "0.5");
    assert_eq!(r.reason.as_deref(), Some("need Layer"));
    assert_eq!(r.client_name.as_deref(), Some("agent"));
    assert_eq!(r.count, 1);
    assert_eq!(r.status, "pending");

    // ---- the playground: its own params object out, the full trace back ----
    let params = dto::SearchParamsView {
        k_fts: 5,
        k_vec: 5,
        ..Default::default()
    };
    let resp = http
        .post(format!("{base}/api/docs/search"))
        .json(&json!({ "corpus_id": cid, "query": "extractor typed data", "params": params }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(
        status, 200,
        "the UI's own params object is accepted: {text}"
    );
    let search: dto::DocsSearchResponse = serde_json::from_str(&text).unwrap();
    assert_eq!(search.corpus_id, "axum@0.8");
    assert_eq!(
        search.trace.params.k_fts, 5,
        "the override reached the stage"
    );
    assert_eq!(search.trace.params.rrf_k, 60.0);
    assert!(!search.trace.fts_query.is_empty());
    assert!(!search.trace.fts.is_empty());
    assert!(!search.trace.knn.is_empty());
    assert!(!search.trace.fused.is_empty());
    assert!(!search.trace.knn_kernel.is_empty());
    assert_eq!(
        search.trace.rerank_model.as_deref(),
        Some("test-up/rerank-tgt")
    );
    assert!(search.trace.rerank_skipped.is_none());
    assert!(!search.trace.rerank.is_empty());
    assert_eq!(search.trace.token_counter, "tiktoken o200k_base");
    assert!(search.trace.resident_vectors >= 3);
    assert!(search.trace.timings.total_ms >= 0.0);
    let hit = &search.hits[0];
    assert!(hit.rerank_score.is_some());
    assert!(search.urls.contains_key(&hit.chunk.id), "the deep link");
    assert!(search.markdown.starts_with("# axum@0.8"));
    assert_eq!(search.status.embed_status, "ok");

    // ---- export manifest, then the import dry run over the exported file ----
    // Per corpus: the whole-DB variant copies the on-disk `quickdoc.db`, which
    // a test state does not have — the artifact and the import path are the
    // same either way (`portability::export`).
    let manifest: dto::ExportManifest = get(
        &http,
        format!("{base}/api/docs/export/manifest?corpus_id={cid}"),
    )
    .await;
    assert_eq!(manifest.schema_version, overview.schema_version);
    assert!(manifest.byte_size > 0);
    assert_eq!(manifest.corpora[0].corpus_id, "axum@0.8");
    assert_eq!(manifest.corpora[0].chunk_count, 3);

    let file = http
        .get(format!("{base}/api/docs/export?corpus_id={cid}"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let resp = http
        .post(format!(
            "{base}/api/docs/import?replace=true&validate_only=true"
        ))
        .body(file)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, 200, "import dry run: {text}");
    let report: dto::ImportReport = serde_json::from_str(&text).unwrap();
    assert!(report.dry_run, "validate_only wrote nothing");
    assert_eq!(report.imported[0].corpus_id, "axum@0.8");
    assert_eq!(report.imported[0].chunks, 3);
    assert!(
        report.imported[0].embed_alias.is_some(),
        "the dry run names what the pinned model resolves as here"
    );

    // ---- what the wizard gets back when it creates a corpus ----
    let started: dto::DocsJobStarted = serde_json::from_str(
        &http
            .post(format!("{base}/api/docs/corpora"))
            .json(&json!({
                "library": "tower", "version": "0.5",
                "embed_model": "embed-model", "ingest_model": "rerank-model",
                "sources": [{"root": "https://docs.rs/tower", "kind": "markdown"}],
            }))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(started.ok);
    assert!(started.job_id.is_none(), "start was not requested");
    assert_eq!(started.corpus.unwrap().corpus_id, "tower@0.5");

    // ---- the tool inventory, which is where `docs__*` is finally visible ----
    //
    // Not a Docs-tab endpoint, but the same drift gate applies and the toolset
    // it reports is this one: the MCP page's per-tool switches and the Chat
    // picker's built-in toolsets both decode this body.
    let tools: dto::ToolInventory = get(&http, format!("{base}/api/tools")).await;
    let docs_tools: Vec<&str> = tools
        .tools
        .iter()
        .filter(|t| t.source_label == "docs")
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(
        docs_tools,
        vec!["docs__resolve", "docs__query", "docs__request"],
        "the quickdoc toolset as the dashboard sees it"
    );
    let docs_source = tools.sources.iter().find(|s| s.label == "docs").unwrap();
    assert_eq!(docs_source.kind, "builtin");
    assert_eq!(docs_source.plane, "/mcp");
    assert_eq!(docs_source.tool_count, 3);
    assert!(docs_source.available && docs_source.reason.is_none());

    // ---- the Settings fields, and both patches the UI posts ----
    let settings: dto::SettingsFull = get(&http, format!("{base}/api/settings-full")).await;
    let stored = state.snapshot().settings.clone();
    assert_eq!(
        settings.docs_ingest_reply_tokens,
        stored.docs_ingest_reply_tokens
    );
    assert_eq!(settings.docs_embed_batch, stored.docs_embed_batch);
    assert_eq!(settings.docs_fetch_delay_ms, stored.docs_fetch_delay_ms);
    assert_eq!(settings.docs_rerank_model, "rerank-model");
    assert_eq!(settings.docs_search.k_fts, stored.docs_search.k_fts);
    assert_eq!(settings.docs_search.eval_k, stored.docs_search.eval_k);

    let resp = http
        .post(format!("{base}/api/op/settings_set_full"))
        .json(&json!({
            "docs_ingest_reply_tokens": 2048, "docs_embed_batch": 8,
            "docs_fetch_delay_ms": 500, "docs_rerank_model": "rerank-model",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the Docs settings card's patch");
    // Every knob the playground puts on screen, exactly as it posts them: a
    // stage default somebody tuned and saved has to come back next time.
    let resp = http
        .post(format!("{base}/api/op/settings_set_full"))
        .json(&json!({
            "docs_search": {
                "k_fts": 11, "k_vec": 12, "rrf_k": 13.0,
                "fts_weights": {
                    "payload": 2.0, "heading_path": 0.5,
                    "derived_title": 3.0, "derived_summary": 0.0,
                },
                "k_rerank": 0, "limit": 4, "budget_tokens": 0, "eval_k": 3,
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the playground's 'save as defaults'");

    let settings: dto::SettingsFull = get(&http, format!("{base}/api/settings-full")).await;
    assert_eq!(settings.docs_ingest_reply_tokens, 2048);
    assert_eq!(settings.docs_embed_batch, 8);
    assert_eq!(settings.docs_fetch_delay_ms, 500);
    assert_eq!(settings.docs_search.k_fts, 11);
    assert_eq!(settings.docs_search.limit, 4);
    // The knobs the playground seeds itself with, read back out of them:
    // `k_rerank: 0` is how Settings spells "no rerank stage", and
    // `budget_tokens: 0` is "no budget", not a budget of zero.
    let seeded = dto::SearchParamsView::from_defaults(&settings.docs_search);
    assert!(!seeded.rerank);
    assert_eq!(seeded.budget_tokens, None);
    assert_eq!(seeded.limit, 4);
    assert_eq!(settings.docs_search.eval_k, 3, "eval_k round-trips");
    assert_eq!(seeded.fts_weights.payload, 2.0);
    assert_eq!(seeded.fts_weights.heading_path, 0.5);
    assert_eq!(seeded.fts_weights.derived_title, 3.0);
    assert_eq!(
        seeded.fts_weights.derived_summary, 0.0,
        "0 is a column switched off, and a legitimate thing to save"
    );
    // …and the saved weights are what a search actually runs with, not a
    // constant the settings pretended to hold.
    assert_eq!(
        state
            .snapshot()
            .settings
            .docs_search
            .params()
            .fts_weights
            .derived_title,
        3.0
    );

    // A weight below zero would invert what a column means, and is refused
    // rather than stored.
    let resp = http
        .post(format!("{base}/api/op/settings_set_full"))
        .json(&json!({"docs_search": {"fts_weights": {"payload": -1.0}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("payload"),
        "the refusal names the weight: {body}"
    );
}

// ---------------------------------------------------------------------------
// The owner's plane: `lmgw__docs_*` on /mcp/admin (§20)
// ---------------------------------------------------------------------------
//
// Same corpus, other audience. `docs__*` above is what an agent asks; these are
// what the owner does — created, ingested and deleted through the dashboard's
// own handlers, so there is exactly one implementation of "what a corpus is".

async fn set_self_admin(state: &SharedState, mode: lmgw_core::config::SelfAdmin) {
    let mut settings = state.snapshot().settings.clone();
    settings.self_admin = mode;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

async fn admin(state: &SharedState, name: &str, v: Value) -> (String, bool) {
    let result = lmgw_core::mcp::selfadmin::call(state, name, args(v))
        .await
        .unwrap();
    (
        result["content"][0]["text"].as_str().unwrap().to_string(),
        result["isError"].as_bool().unwrap_or(false),
    )
}

async fn admin_json(state: &SharedState, name: &str, v: Value) -> Value {
    let (text, is_error) = admin(state, name, v).await;
    assert!(!is_error, "{name} failed: {text}");
    serde_json::from_str(&text).unwrap()
}

/// The bulk-import path end to end, minus the crawl: create a corpus from an
/// alias pair and a root, see it in the listing under both selectors, and drop
/// it again. `start: false` is what keeps the network out of this test — the
/// ingest job itself is `quickdoc_ingest.rs`.
#[tokio::test]
async fn admin_plane_creates_and_deletes_a_corpus() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    set_self_admin(&state, lmgw_core::config::SelfAdmin::Full).await;

    let created = admin_json(
        &state,
        "lmgw__docs_corpus_set",
        json!({
            "action": "create",
            "library": "axum",
            "version": "0.8",
            "embed_model": "embed-model",
            "ingest_model": "ingest-model",
            "source_root": "https://docs.rs/axum/0.8/axum/\nhttps://docs.rs/axum/0.8/llms.txt",
            "source_kind": "markdown",
            "start": false,
        }),
    )
    .await;
    assert_eq!(created["corpus"]["corpus_id"], "axum@0.8");
    assert!(created["job_id"].is_null(), "start:false queues nothing");
    // Both roots were stored, and each fell back to its own host as its fence.
    assert_eq!(created["corpus"]["sources"].as_array().unwrap().len(), 2);
    // A corpus that is not running has to say how to run it.
    assert!(
        created["next_step"]
            .as_str()
            .unwrap()
            .contains("lmgw__docs_ingest"),
        "{created}"
    );

    // The embed model is pinned by resolved identity, not by the alias name:
    // the same pin `docs__query` later checks.
    assert_eq!(created["corpus"]["embed_identity"], identity().to_string());

    let id = created["corpus"]["id"].as_i64().unwrap();
    let listed = admin_json(&state, "lmgw__docs_corpora", json!({})).await;
    assert_eq!(listed["corpora"].as_array().unwrap().len(), 1);
    assert_eq!(listed["corpora"][0]["id"], id);

    // Either selector reaches the row — an agent reads `library@version` out of
    // `docs__resolve` and out of the request queue, never the numeric id.
    for sel in [json!(id.to_string()), json!("axum@0.8")] {
        let one = admin_json(&state, "lmgw__docs_corpora", json!({ "corpus": sel })).await;
        assert_eq!(one["corpus"]["id"], id);
        assert!(one["corpus"]["job"].is_null(), "nothing has run yet");
    }

    // Nothing is running, so cancel says so rather than reporting success.
    let (msg, is_error) = admin(
        &state,
        "lmgw__docs_ingest",
        json!({ "corpus": "axum@0.8", "action": "cancel" }),
    )
    .await;
    assert!(is_error, "{msg}");
    assert!(msg.contains("nothing to cancel"), "{msg}");

    let deleted = admin_json(
        &state,
        "lmgw__docs_corpus_set",
        json!({ "action": "delete", "corpus": "axum@0.8" }),
    )
    .await;
    assert_eq!(deleted["message"], "deleted axum@0.8");
    let listed = admin_json(&state, "lmgw__docs_corpora", json!({})).await;
    assert!(listed["corpora"].as_array().unwrap().is_empty());
}

/// A bad create fails at the call, naming everything that is wrong, rather than
/// inside a job the caller would have to go and read.
#[tokio::test]
async fn admin_plane_create_refuses_what_it_cannot_run() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    set_self_admin(&state, lmgw_core::config::SelfAdmin::Full).await;

    let (msg, is_error) = admin(
        &state,
        "lmgw__docs_corpus_set",
        json!({ "action": "create", "library": "axum" }),
    )
    .await;
    assert!(is_error);
    for missing in ["version", "embed_model", "ingest_model", "source_root"] {
        assert!(msg.contains(missing), "{missing} not named: {msg}");
    }

    let (msg, is_error) = admin(
        &state,
        "lmgw__docs_corpus_set",
        json!({
            "action": "create",
            "library": "axum",
            "version": "0.8",
            "embed_model": "embed-model",
            "ingest_model": "ingest-model",
            "source_root": "https://docs.rs/axum/",
            "source_kind": "pdf",
        }),
    )
    .await;
    assert!(is_error);
    assert!(msg.contains("rustdoc_json"), "the four kinds: {msg}");

    // An alias that does not resolve is caught by the same call, not by the job.
    let (msg, is_error) = admin(
        &state,
        "lmgw__docs_corpus_set",
        json!({
            "action": "create",
            "library": "axum",
            "version": "0.8",
            "embed_model": "embed-model",
            "ingest_model": "no-such-alias",
            "source_root": "https://docs.rs/axum/",
        }),
    )
    .await;
    assert!(is_error);
    assert!(msg.contains("no-such-alias"), "{msg}");

    let (msg, is_error) = admin(
        &state,
        "lmgw__docs_ingest",
        json!({ "corpus": "axum@0.8", "action": "start" }),
    )
    .await;
    assert!(is_error);
    assert!(msg.contains("no corpus"), "{msg}");
}

/// The loop the two planes make: an agent files what it missed, the owner's
/// plane reads that queue and answers it. Reading the queue needs no write
/// grant; clearing an entry does.
#[tokio::test]
async fn the_request_queue_crosses_both_planes() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    set_self_admin(&state, lmgw_core::config::SelfAdmin::ReadOnly).await;

    call_json(
        &state,
        "docs__request",
        json!({"library": "tower", "reason": "middleware questions"}),
    )
    .await;

    let queue = admin_json(&state, "lmgw__docs_requests", json!({})).await;
    assert_eq!(queue["requests"].as_array().unwrap().len(), 1);
    let req = &queue["requests"][0];
    assert_eq!(req["library"], "tower");
    assert_eq!(req["status"], "pending");
    let req_id = req["id"].as_i64().unwrap();

    // Read-only sees the worklist but may not answer it.
    let (msg, is_error) = admin(
        &state,
        "lmgw__docs_request_set",
        json!({ "id": req_id, "status": "dismissed" }),
    )
    .await;
    assert!(is_error);
    assert!(msg.contains("read_only"), "{msg}");

    set_self_admin(&state, lmgw_core::config::SelfAdmin::Full).await;
    admin_json(
        &state,
        "lmgw__docs_request_set",
        json!({ "id": req_id, "status": "dismissed" }),
    )
    .await;
    let pending = admin_json(&state, "lmgw__docs_requests", json!({"status": "pending"})).await;
    assert!(pending["requests"].as_array().unwrap().is_empty());
    let dismissed = admin_json(
        &state,
        "lmgw__docs_requests",
        json!({"status": "dismissed"}),
    )
    .await;
    assert_eq!(dismissed["requests"][0]["id"], req_id);

    // `fulfilled` is the ingest job's word, never the caller's.
    let (msg, is_error) = admin(
        &state,
        "lmgw__docs_request_set",
        json!({ "id": req_id, "status": "fulfilled" }),
    )
    .await;
    assert!(is_error);
    assert!(msg.contains("when an ingest for it completes"), "{msg}");
}

// ---------------------------------------------------------------------------
// Telemetry: the encoder halves log too (§10)
// ---------------------------------------------------------------------------

/// Every model call quickdoc makes reaches Traffic — including the two that
/// ride the in-process path rather than `/v1/*`.
///
/// They used to be the one kind of model traffic this gateway did not log: an
/// overnight ingest showed its extraction turns and nothing of the thousands of
/// vectors it wrote, and a query whose time went to the reranker looked like a
/// slow search. Both now write the same row a public request would, named apart
/// so Logs says which stage spent the time.
#[tokio::test]
async fn the_embed_and_rerank_calls_log_like_public_traffic() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    set_rerank_model(&state, "rerank-model").await;
    let cid = seed(&state, "axum", "0.8").await;
    let corpus = qstore::get_corpus(&state.corpus, cid)
        .await
        .unwrap()
        .unwrap();

    // One query: the vector half embeds the query text, the rerank stage scores
    // the fused window.
    let r = lmgw_core::quickdoc::query::open_retriever(&state, &corpus)
        .await
        .unwrap();
    let params = lmgw_core::quickdoc::query::default_params(&state.snapshot());
    r.search("handler extractor layer", &params).await.unwrap();

    let rows = store::query_logs(
        &state.db,
        &store::LogFilter {
            limit: 50,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let embed = rows
        .iter()
        .find(|r| r.ingress_proto == "quickdoc-embed")
        .unwrap_or_else(|| panic!("no embed row in {:?}", protos(&rows)));
    assert_eq!(embed.requested_alias, "embed-model");
    assert_eq!(embed.upstream_name.as_deref(), Some("test-up"));
    assert_eq!(embed.upstream_model.as_deref(), Some("embed-tgt"));
    assert_eq!(embed.status, 200);
    assert!(!embed.streamed, "an encoder call never streams");
    assert!(embed.total_ms.is_some(), "latency is measured, not omitted");
    // The upstream's own usage, not an estimate: an encoder reports prompt
    // tokens and no completion tokens, and both have to survive the row.
    assert_eq!(embed.prompt_tokens, Some(1));
    assert_eq!(embed.completion_tokens, None);

    let rerank = rows
        .iter()
        .find(|r| r.ingress_proto == "quickdoc-rerank")
        .unwrap_or_else(|| panic!("no rerank row in {:?}", protos(&rows)));
    assert_eq!(rerank.requested_alias, "rerank-model");
    assert_eq!(rerank.upstream_model.as_deref(), Some("rerank-tgt"));
    assert_eq!(rerank.status, 200);
    assert_eq!(rerank.prompt_tokens, Some(3));

    // Both are model traffic, so neither is excluded from the aggregates the
    // dashboard totals — the counters are the reason the proto is named at all.
    for proto in ["quickdoc-embed", "quickdoc-rerank"] {
        assert!(
            lmgw_core::telemetry::counts_in_token_stats(proto),
            "{proto} must count in the token stats"
        );
    }
}

/// A failure that never resolved an alias writes no row — there is no upstream
/// to key one to — and must not leave the in-flight gauge counting a request
/// that is over.
#[tokio::test]
async fn an_unresolvable_embed_alias_logs_nothing_and_balances_the_gauge() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;

    let err = lmgw_core::quickdoc::InProcessEmbedder::probe(state.clone(), "no-such-alias")
        .await
        .expect_err("an alias that does not exist cannot be probed");
    assert!(
        err.to_string().contains("no-such-alias"),
        "the refusal names it: {err}"
    );

    let rows = store::query_logs(&state.db, &store::LogFilter::default())
        .await
        .unwrap();
    assert!(
        !rows.iter().any(|r| r.ingress_proto == "quickdoc-embed"),
        "nothing answered, so nothing is logged: {:?}",
        protos(&rows)
    );
    assert_eq!(
        state.telemetry.stats().active_requests,
        0,
        "the started request was closed out"
    );
}

fn protos(rows: &[lmgw_core::store::RequestLogRow]) -> Vec<&str> {
    rows.iter().map(|r| r.ingress_proto.as_str()).collect()
}
