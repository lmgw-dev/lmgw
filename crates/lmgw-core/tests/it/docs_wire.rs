//! The docs plane's HTTP answers (`/api/docs/*`) are built from the types of
//! `lmgw-api-types`: each live answer is read into its type and written back
//! equal, checked against the built OpenAPI document, and its key set pinned,
//! so a key the server adds, drops, renames, turns from absent into `null` or
//! from an integer into a float shows up here.
//!
//! Every job a route starts is a fake executor (`support::docs_world`), so no
//! call reaches the network and no job races its answer.

use lmgw_api_types as dto;
use quickdoc_core::store as qstore;
use serde_json::{json, Value};
use wiremock::MockServer;

use crate::common::{self, round_trips, serve, Gw};
use crate::support::docs_world::{chunk_ids, record_run, seed, set_rerank_model, setup, CHUNKS};

struct World {
    state: lmgw_core::state::SharedState,
    gw: Gw,
    _mock: MockServer,
}

/// Two corpora: `axum@0.8` evaluated, with golden queries, candidates of every
/// state, a document of two and every chunk embedded; `tower@0.5` never
/// evaluated, with one chunk missing its vector. Doc requests of every state.
async fn world() -> World {
    let mock = MockServer::start().await;
    let state = setup(&mock).await;
    let a = seed(&state, "axum", "0.8", "ready", 3).await;
    seed(&state, "tower", "0.5", "ingesting", 2).await;
    let ids = chunk_ids(&state, a).await;
    record_run(&state, a, 1.0, 0.75, 2).await;
    record_run(&state, a, 0.5, 0.25, 2).await;
    qstore::insert_golden_query(&state.corpus, a, "extractors", &[ids[1].clone()], "manual")
        .await
        .unwrap();
    qstore::insert_golden_query(&state.corpus, a, "layers", &[], "synthetic")
        .await
        .unwrap();
    for (q, chunk) in [("pending one", 0), ("accepted one", 1), ("rejected one", 2)] {
        qstore::insert_golden_candidate(
            &state.corpus,
            &qstore::NewGoldenCandidate {
                corpus_id: a,
                query: q.into(),
                expected_chunk_ids: vec![ids[chunk].clone()],
                rationale: format!("the {} section covers it", CHUNKS[chunk].0),
                model: "ingest-model".into(),
            },
        )
        .await
        .unwrap();
    }
    qstore::decide_golden_candidate(&state.corpus, 2, "accepted", Some(2))
        .await
        .unwrap();
    qstore::decide_golden_candidate(&state.corpus, 3, "rejected", None)
        .await
        .unwrap();
    qstore::file_doc_request(
        &state.corpus,
        "hyper",
        "1.0",
        Some("need Service"),
        Some("agent"),
    )
    .await
    .unwrap();
    qstore::file_doc_request(&state.corpus, "reqwest", "", None, None)
        .await
        .unwrap();
    let gw = serve(state.clone()).await;
    World {
        state,
        gw,
        _mock: mock,
    }
}

async fn send(gw: &Gw, method: &str, p: &str, body: Option<Value>) -> (u16, Value) {
    let client = gw.client();
    let url = format!("{gw}{p}");
    let mut req = match method {
        "GET" => client.get(url),
        _ => client.post(url),
    };
    if let Some(b) = &body {
        req = req.json(b);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    let json = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{method} {p} is not JSON ({e}): {text}"));
    (status, json)
}

async fn get(gw: &Gw, p: &str) -> Value {
    let (status, body) = send(gw, "GET", p, None).await;
    assert_eq!(status, 200, "GET {p}: {body}");
    body
}

async fn post(gw: &Gw, p: &str, body: Option<Value>) -> Value {
    let (status, answer) = send(gw, "POST", p, body.clone()).await;
    assert_eq!(status, 200, "POST {p} {body:?}: {answer}");
    answer
}

/// The keys of `live`, to be exactly `want` (in any order).
fn assert_keys(live: &Value, want: &[&str]) {
    let mut got: Vec<&str> = live
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {live}"))
        .keys()
        .map(String::as_str)
        .collect();
    got.sort_unstable();
    let mut want = want.to_vec();
    want.sort_unstable();
    assert_eq!(got, want, "{live}");
}

fn whole(v: &Value) -> bool {
    v.is_u64() || v.is_i64()
}

const CORPUS_KEYS: [&str; 28] = [
    "id",
    "corpus_id",
    "library",
    "version",
    "status",
    "chunk_count",
    "unembedded_chunks",
    "resident_bytes",
    "embed_upstream",
    "embed_model",
    "embed_dims",
    "embed_identity",
    "ingest_model",
    "ingest_prompt_version",
    "crawl_date",
    "source_kind",
    "eval_score",
    "eval_best",
    "eval_k",
    "eval_at",
    "eval_regression",
    "embed_status",
    "eval_status",
    "flags",
    "warnings",
    "sources",
    "created_at",
    "updated_at",
];

/// `CORPUS_KEYS` of a corpus never evaluated: the four eval keys are absent.
fn unmeasured_keys() -> Vec<&'static str> {
    CORPUS_KEYS
        .iter()
        .copied()
        .filter(|k| !["eval_score", "eval_best", "eval_k", "eval_at"].contains(k))
        .collect()
}

fn required(name: &str) -> Vec<String> {
    let doc = lmgw_core::openapi::admin_doc();
    let schemas = common::documented_schemas(doc, name);
    assert_eq!(schemas.len(), 1, "{name}: {schemas:?}");
    let mut r: Vec<String> = schemas[0]["required"]
        .as_array()
        .unwrap_or_else(|| panic!("no required list in {}", schemas[0]))
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    r.sort();
    r
}

#[tokio::test]
async fn the_corpus_list_and_detail_round_trip_with_every_key() {
    let w = world().await;

    let live = get(&w.gw, "/api/docs/corpora").await;
    assert_keys(
        &live,
        &[
            "corpora",
            "pending_requests",
            "rerank_model",
            "schema_version",
        ],
    );
    // No rerank alias is set: the key is there, `null`.
    assert!(live["rerank_model"].is_null(), "{live}");
    assert!(whole(&live["pending_requests"]) && live["pending_requests"] == 2);
    let overview: dto::DocsOverview = round_trips("GET /api/docs/corpora", &live);
    assert_eq!(overview.corpora.len(), 2);

    let evaluated = &live["corpora"][0];
    assert_keys(evaluated, &CORPUS_KEYS);
    assert_eq!(evaluated["corpus_id"], "axum@0.8");
    // The latest run was 0.5, the best 1.0: floats, though 1.0 is whole.
    assert!(evaluated["eval_score"].is_f64() && evaluated["eval_score"] == 0.5);
    assert!(evaluated["eval_best"].is_f64() && evaluated["eval_best"] == 1.0);
    assert_eq!(evaluated["eval_regression"], true);
    for k in [
        "id",
        "chunk_count",
        "unembedded_chunks",
        "resident_bytes",
        "embed_dims",
        "eval_k",
    ] {
        assert!(whole(&evaluated[k]), "{k}: {evaluated}");
    }
    // 3 chunks of 32 dims, 2 bytes each.
    assert_eq!(evaluated["resident_bytes"], 3 * 32 * 2);
    assert_eq!(evaluated["sources"].as_array().unwrap().len(), 2);
    assert_keys(
        &evaluated["sources"][1],
        &["id", "corpus_id", "root", "kind", "fence", "created_at"],
    );
    assert_eq!(evaluated["sources"][1]["fence"], json!(["docs.rs"]));

    // Never evaluated leaves the four eval keys out: not `null`, not zero, not
    // an empty string. A chunk without its vector counts and costs nothing
    // resident.
    let fresh = &live["corpora"][1];
    assert_keys(fresh, &unmeasured_keys());
    round_trips::<dto::CorpusView>("a corpus never evaluated", fresh);
    assert_eq!(fresh["unembedded_chunks"], 1);
    assert_eq!(fresh["resident_bytes"], 2 * 32 * 2);

    // With a rerank alias, the key is its name.
    set_rerank_model(&w.state, "rerank-model").await;
    let live = get(&w.gw, "/api/docs/corpora").await;
    assert_eq!(live["rerank_model"], "rerank-model");
    round_trips::<dto::DocsOverview>("GET /api/docs/corpora with a rerank alias", &live);

    let id = overview.corpora[0].id;
    let live = get(&w.gw, &format!("/api/docs/corpora/{id}")).await;
    assert_keys(
        &live,
        &["corpus", "documents", "eval_runs", "golden_queries"],
    );
    assert_keys(&live["corpus"], &CORPUS_KEYS);
    assert_keys(
        &live["documents"][0],
        &["id", "source_id", "url", "content_hash", "fetched_at"],
    );
    assert_keys(
        &live["golden_queries"][0],
        &["id", "corpus_id", "query", "expected_chunk_ids", "origin"],
    );
    assert_keys(
        &live["eval_runs"][0],
        &[
            "id",
            "corpus_id",
            "k",
            "queries",
            "hit_at_k",
            "mrr",
            "orphaned_queries",
            "regression",
            "params",
            "report",
            "created_at",
        ],
    );
    let detail: dto::CorpusDetail = round_trips("GET /api/docs/corpora/{id}", &live);
    assert_eq!(detail.documents.len(), 2);
    assert_eq!(detail.golden_queries.len(), 2);
    assert_eq!(detail.eval_runs.len(), 2);
    // A whole-number score is still a float on the wire; counts are not.
    let run = &live["eval_runs"][1];
    assert!(run["hit_at_k"].is_f64() && run["hit_at_k"] == 1.0, "{run}");
    assert!(run["mrr"].is_f64() && run["mrr"] == 0.75, "{run}");
    assert!(whole(&run["queries"]) && whole(&run["orphaned_queries"]));
    // The limit cuts the history, newest first.
    let live = get(&w.gw, &format!("/api/docs/corpora/{id}?limit=1")).await;
    assert_eq!(live["eval_runs"].as_array().unwrap().len(), 1);
    assert_eq!(live["eval_runs"][0]["hit_at_k"], 0.5);
    round_trips::<dto::CorpusDetail>("GET /api/docs/corpora/{id}?limit=1", &live);
}

#[tokio::test]
async fn the_documents_chunks_golden_candidates_eval_and_requests_round_trip() {
    let w = world().await;
    let a = get(&w.gw, "/api/docs/corpora").await["corpora"][0]["id"]
        .as_i64()
        .unwrap();

    let live = get(&w.gw, &format!("/api/docs/corpora/{a}/documents")).await;
    assert_keys(&live, &["documents"]);
    let documents: dto::DocumentsResponse = round_trips("GET documents", &live);
    let guide = documents
        .documents
        .iter()
        .find(|d| d.url.ends_with("/guide"))
        .unwrap();

    let live = get(&w.gw, &format!("/api/docs/chunks?document_id={}", guide.id)).await;
    assert_keys(&live, &["chunks"]);
    assert_keys(
        &live["chunks"][0],
        &[
            "id",
            "document_id",
            "corpus_id",
            "heading_path",
            "span_start",
            "span_end",
            "payload",
            "derived_title",
            "derived_summary",
        ],
    );
    let chunks: dto::ChunksResponse = round_trips("GET chunks", &live);
    assert_eq!(chunks.chunks.len(), 3);
    assert_eq!(chunks.chunks[1].payload, CHUNKS[1].1);
    // The document with nothing in it has an empty list, not a missing one.
    let empty = documents
        .documents
        .iter()
        .find(|d| d.id != guide.id)
        .unwrap();
    let live = get(&w.gw, &format!("/api/docs/chunks?document_id={}", empty.id)).await;
    assert_eq!(live, json!({"chunks": []}));
    round_trips::<dto::ChunksResponse>("GET chunks of an empty document", &live);

    let live = get(&w.gw, &format!("/api/docs/golden?corpus_id={a}")).await;
    assert_keys(&live, &["golden_queries"]);
    let golden: dto::GoldenResponse = round_trips("GET golden", &live);
    assert_eq!(golden.golden_queries[1].expected_chunk_ids.len(), 0);
    assert_eq!(live["golden_queries"][1]["expected_chunk_ids"], json!([]));

    // The queue defaults to the pending ones; the chunks they were written
    // from are keyed by id.
    let live = get(&w.gw, &format!("/api/docs/golden/candidates?corpus_id={a}")).await;
    assert_keys(&live, &["candidates", "chunks"]);
    let queue: dto::GoldenCandidatesResponse = round_trips("GET candidates", &live);
    assert_eq!(queue.candidates.len(), 1);
    assert_eq!(queue.candidates[0].query, "pending one");
    assert_eq!(queue.chunks.len(), 1);
    assert_keys(
        &live["candidates"][0],
        &[
            "id",
            "corpus_id",
            "query",
            "expected_chunk_ids",
            "rationale",
            "model",
            "status",
            "golden_query_id",
            "created_at",
            "decided_at",
        ],
    );
    // Pending: no golden query yet (`null`, key there), nothing decided.
    assert!(live["candidates"][0]["golden_query_id"].is_null());
    assert_eq!(live["candidates"][0]["decided_at"], "");
    // Accepted: the golden query it became.
    let live = get(
        &w.gw,
        &format!("/api/docs/golden/candidates?corpus_id={a}&status=accepted"),
    )
    .await;
    let queue: dto::GoldenCandidatesResponse = round_trips("GET accepted candidates", &live);
    assert_eq!(queue.candidates[0].golden_query_id, Some(2));
    // Every state at once; a corpus with none has two empty collections.
    let live = get(
        &w.gw,
        &format!("/api/docs/golden/candidates?corpus_id={a}&status="),
    )
    .await;
    assert_eq!(live["candidates"].as_array().unwrap().len(), 3);
    assert_eq!(live["chunks"].as_object().unwrap().len(), 3);
    // A status that names no state is a refusal naming the valid ones, not an
    // empty queue.
    let (status, body) = send(
        &w.gw,
        "GET",
        &format!("/api/docs/golden/candidates?corpus_id={a}&status=bogus"),
        None,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "op_failed");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("pending|accepted|rejected"),
        "{body}"
    );

    let live = get(&w.gw, &format!("/api/docs/eval?corpus_id={a}")).await;
    assert_keys(&live, &["eval_runs"]);
    let history: dto::EvalHistory = round_trips("GET eval", &live);
    assert_eq!(history.eval_runs.len(), 2);
    assert_eq!(history.eval_runs[0].hit_at_k, 0.5, "newest first");
    let live = get(&w.gw, &format!("/api/docs/eval?corpus_id={a}&limit=1")).await;
    assert_eq!(live["eval_runs"].as_array().unwrap().len(), 1);

    let live = get(&w.gw, "/api/docs/requests").await;
    assert_keys(&live, &["requests"]);
    assert_keys(
        &live["requests"][0],
        &[
            "id",
            "library",
            "version",
            "reason",
            "client_name",
            "count",
            "first_requested_at",
            "last_requested_at",
            "status",
        ],
    );
    let queue: dto::DocRequestsResponse = round_trips("GET requests", &live);
    assert_eq!(queue.requests.len(), 2);
    // Neither a reason nor a client name is `null`, key there.
    let bare = live["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["library"] == "reqwest")
        .unwrap();
    assert!(
        bare["reason"].is_null() && bare["client_name"].is_null(),
        "{bare}"
    );
    assert_eq!(bare["version"], "");
    assert!(whole(&bare["count"]));
    let live = get(&w.gw, "/api/docs/requests?status=dismissed").await;
    assert_eq!(live, json!({"requests": []}));
    let (status, body) = send(&w.gw, "GET", "/api/docs/requests?status=bogus", None).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "op_failed");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("pending|fulfilled|dismissed"),
        "{body}"
    );
}

#[tokio::test]
async fn an_eval_runs_params_and_report_are_absent_when_the_stored_text_does_not_parse() {
    let w = world().await;
    let a = get(&w.gw, "/api/docs/corpora").await["corpora"][0]["id"]
        .as_i64()
        .unwrap();
    // The newest run (id 2) has a mangled report and a mangled params column.
    sqlx::query("UPDATE eval_run SET report = 'not json', params = '{' WHERE id = 2")
        .execute(&w.state.corpus)
        .await
        .unwrap();
    let live = get(&w.gw, &format!("/api/docs/eval?corpus_id={a}")).await;
    let runs = live["eval_runs"].as_array().unwrap();
    let broken = runs.iter().find(|r| r["id"] == 2).unwrap();
    let intact = runs.iter().find(|r| r["id"] == 1).unwrap();
    assert!(
        broken.get("params").is_none() && broken.get("report").is_none(),
        "{broken}"
    );
    assert!(intact.get("params").is_some() && intact.get("report").is_some());
    round_trips::<dto::EvalHistory>("GET eval with an unreadable run", &live);

    // A stored JSON `null` is no value either: absent, not `null`.
    sqlx::query("UPDATE eval_run SET report = 'null', params = 'null' WHERE id = 1")
        .execute(&w.state.corpus)
        .await
        .unwrap();
    let live = get(&w.gw, &format!("/api/docs/eval?corpus_id={a}")).await;
    let nulled = live["eval_runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == 1)
        .unwrap();
    assert!(
        nulled.get("params").is_none() && nulled.get("report").is_none(),
        "{nulled}"
    );
}

#[tokio::test]
async fn a_corpus_evaluated_at_zero_keeps_its_score_and_depth() {
    let w = world().await;
    let b = get(&w.gw, "/api/docs/corpora").await["corpora"][1]["id"]
        .as_i64()
        .unwrap();
    record_run(&w.state, b, 0.0, 0.0, 3).await;
    let live = get(&w.gw, "/api/docs/corpora").await;
    let view = live["corpora"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == b)
        .unwrap();
    // Zero is a measurement, not "never evaluated".
    assert_eq!(view["eval_score"], 0.0, "{view}");
    assert_eq!(view["eval_k"], 3, "{view}");
    assert!(view["eval_at"].is_string(), "{view}");
}

#[tokio::test]
async fn every_route_that_takes_a_corpus_id_answers_404_not_found_for_one_nothing_has() {
    let w = world().await;
    for (method, path, body) in [
        ("GET", "/api/docs/corpora/999", None),
        ("GET", "/api/docs/corpora/999/documents", None),
        ("GET", "/api/docs/golden?corpus_id=999", None),
        ("GET", "/api/docs/golden/candidates?corpus_id=999", None),
        ("GET", "/api/docs/eval?corpus_id=999", None),
        ("GET", "/api/docs/export?corpus_id=999", None),
        ("GET", "/api/docs/export/manifest?corpus_id=999", None),
        (
            "POST",
            "/api/docs/search",
            Some(json!({"corpus_id": 999, "query": "x"})),
        ),
        (
            "POST",
            "/api/docs/golden",
            Some(json!({"corpus_id": 999, "query": "x"})),
        ),
        ("POST", "/api/docs/corpora/999/ingest", None),
        ("POST", "/api/docs/corpora/999/re-embed", Some(json!({}))),
        ("POST", "/api/docs/corpora/999/delete", None),
        ("POST", "/api/docs/eval", Some(json!({"corpus_id": 999}))),
        (
            "POST",
            "/api/docs/golden/generate",
            Some(json!({"corpus_id": 999})),
        ),
    ] {
        let (status, live) = send(&w.gw, method, path, body).await;
        assert_eq!(status, 404, "{method} {path}: {live}");
        assert_keys(&live, &["code", "message"]);
        assert_eq!(live["code"], "not_found", "{path}: {live}");
        assert_eq!(live["message"], "no corpus 999", "{path}: {live}");
    }
}

#[tokio::test]
async fn the_job_triggers_answer_the_job_and_whether_it_was_already_running() {
    let w = world().await;
    let a = get(&w.gw, "/api/docs/corpora").await["corpora"][0]["id"]
        .as_i64()
        .unwrap();

    for (what, path, body) in [
        ("ingest", format!("/api/docs/corpora/{a}/ingest"), None),
        (
            "re-embed",
            format!("/api/docs/corpora/{a}/re-embed"),
            Some(json!({})),
        ),
        (
            "eval",
            "/api/docs/eval".to_string(),
            Some(json!({"corpus_id": a, "k": 3})),
        ),
        (
            "golden generate",
            "/api/docs/golden/generate".to_string(),
            Some(json!({"corpus_id": a, "sample": 2})),
        ),
    ] {
        // Each kind has its own job key, so each starts fresh, and asking
        // again finds the same job still running.
        let first = post(&w.gw, &path, body.clone()).await;
        assert_keys(&first, &["already_running", "job_id", "ok"]);
        assert_eq!(first["already_running"], false, "{what}");
        assert!(whole(&first["job_id"]), "{what}: {first}");
        let started: dto::DocsJobStarted = round_trips(&format!("POST {what}"), &first);
        assert!(started.ok && !started.already_running);

        let again = post(&w.gw, &path, body).await;
        assert_keys(&again, &["already_running", "job_id", "ok"]);
        assert_eq!(again["already_running"], true, "{what}");
        assert_eq!(again["job_id"], first["job_id"], "{what}");
        round_trips::<dto::DocsJobStarted>(&format!("POST {what} again"), &again);
    }

    // A corpus that is not there is the not-found answer, not a job.
    let (status, body) = send(&w.gw, "POST", "/api/docs/corpora/999/ingest", None).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["code"], "not_found");
    assert_eq!(body["message"], "no corpus 999");
}

#[tokio::test]
async fn creating_a_corpus_answers_the_corpus_and_the_job_it_started() {
    let w = world().await;
    let body = |library: &str, start: bool| {
        json!({
            "library": library, "version": "1", "embed_model": "embed-model",
            "ingest_model": "ingest-model", "start": start,
            "sources": [{"root": format!("https://docs.rs/{library}"), "kind": "markdown",
                         "fence": ["docs.rs"]}],
        })
    };

    // Asked not to start: the corpus, and a `null` job.
    let live = post(&w.gw, "/api/docs/corpora", Some(body("bytes", false))).await;
    assert_keys(&live, &["corpus", "job_id", "ok"]);
    assert!(live["job_id"].is_null(), "{live}");
    assert_keys(&live["corpus"], &unmeasured_keys());
    assert_eq!(live["corpus"]["status"], "ingesting");
    assert_eq!(live["corpus"]["sources"][0]["fence"], json!(["docs.rs"]));
    let created: dto::CorpusCreated = round_trips("POST /api/docs/corpora", &live);
    assert_eq!(created.corpus.corpus_id, "bytes@1");
    assert!(created.job_id.is_none());

    // Started: the job's id.
    let live = post(&w.gw, "/api/docs/corpora", Some(body("http", true))).await;
    assert_keys(&live, &["corpus", "job_id", "ok"]);
    assert!(whole(&live["job_id"]), "{live}");
    let created: dto::CorpusCreated = round_trips("POST /api/docs/corpora with start", &live);
    assert!(created.job_id.is_some());

    // Neither answer says `already_running`: nothing ran for a new corpus.
    let (status, refused) = send(
        &w.gw,
        "POST",
        "/api/docs/corpora",
        Some(body("bytes", false)),
    )
    .await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["message"], "corpus bytes@1 already exists");
}

#[tokio::test]
async fn the_writes_keep_their_shapes() {
    let w = world().await;
    let a = get(&w.gw, "/api/docs/corpora").await["corpora"][0]["id"]
        .as_i64()
        .unwrap();
    let ids = chunk_ids(&w.state, a).await;

    let live = post(
        &w.gw,
        "/api/docs/golden",
        Some(json!({"corpus_id": a, "query": "a new one", "expected_chunk_ids": [ids[0]]})),
    )
    .await;
    assert_keys(&live, &["id", "ok"]);
    let saved: dto::GoldenSaved = round_trips("POST /api/docs/golden", &live);
    let live = post(&w.gw, "/api/docs/golden/candidates/1/accept", None).await;
    round_trips::<dto::GoldenSaved>("POST accept", &live);
    // The fixture's other two candidates are decided already, so reject a
    // fresh one.
    let fresh = qstore::insert_golden_candidate(
        &w.state.corpus,
        &qstore::NewGoldenCandidate {
            corpus_id: a,
            query: "one to turn down".into(),
            expected_chunk_ids: vec![ids[2].clone()],
            rationale: "the Middleware section covers it".into(),
            model: "ingest-model".into(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let live = post(
        &w.gw,
        &format!("/api/docs/golden/candidates/{fresh}/reject"),
        None,
    )
    .await;
    assert_eq!(live, json!({"ok": true}));
    round_trips::<dto::Ack>("POST reject", &live);
    let live = post(
        &w.gw,
        &format!("/api/docs/golden/{}/delete", saved.id),
        None,
    )
    .await;
    assert_eq!(live, json!({"ok": true}));
    round_trips::<dto::Ack>("POST golden delete", &live);

    let live = post(
        &w.gw,
        "/api/docs/requests/1/status",
        Some(json!({"status": "dismissed"})),
    )
    .await;
    assert_eq!(live, json!({"ok": true}));
    round_trips::<dto::Ack>("POST request status", &live);

    let live = post(&w.gw, &format!("/api/docs/corpora/{a}/delete"), None).await;
    assert_keys(&live, &["message", "ok"]);
    assert_eq!(live["message"], "deleted axum@0.8");
    round_trips::<dto::MessageAck>("POST corpus delete", &live);
}

#[tokio::test]
async fn the_document_says_what_the_docs_routes_send() {
    // What every job trigger sends; the corpus-creating answer sends the
    // corpus and a job id that is `null` for a corpus asked not to start.
    assert_eq!(
        required("DocsJobStarted"),
        ["already_running", "job_id", "ok"]
    );
    assert_eq!(required("CorpusCreated"), ["corpus", "job_id", "ok"]);
    assert_eq!(
        required("DocsOverview"),
        [
            "corpora",
            "pending_requests",
            "rerank_model",
            "schema_version"
        ]
    );
    let doc = lmgw_core::openapi::admin_doc();
    let answer = |method: &str, p: &str| {
        doc["paths"][p][method]["responses"]["200"]["content"]["application/json"]["schema"]
            ["title"]
            .clone()
    };
    assert_eq!(answer("post", "/api/docs/corpora"), "CorpusCreated");
    assert_eq!(
        answer("post", "/api/docs/corpora/{id}/ingest"),
        "DocsJobStarted"
    );
    assert_eq!(answer("post", "/api/docs/eval"), "DocsJobStarted");
}
