//! The Knowledge bases API against its documented types
//! (`lmgw-api-types::knowledge`): each JSON answer, read into its type and
//! written back, is the answer again, key by key. A field the gateway adds to
//! an answer without adding it to the type is dropped by the read and fails
//! here. The file text and the original are checked by content type.

use lmgw_api_types::knowledge::{
    CancelResult, KnowledgeBase, KnowledgeBaseDeleted, KnowledgeBaseDetail, KnowledgeBaseEdited,
    KnowledgeBaseList, KnowledgeFileDeleted, KnowledgeFileList, KnowledgeFileRequeued,
    KnowledgeSearchResult, KnowledgeSource, ResumeResult, UploadResult, UploadVerdict,
};
use serde_json::json;
use wiremock::MockServer;

use crate::common::{round_trips, serve};
use crate::knowledge::{create, get, mount, post, setup, upload, wait_job, LETTER, NOTES};

#[tokio::test]
async fn the_knowledge_routes_answer_their_types() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let gw = serve(state.clone()).await;

    // Create.
    let (status, v) = post(
        &gw,
        "/api/knowledge/bases",
        json!({"name": "Taxes", "description": "tax papers", "embed_alias": "embed-model",
               "chunk_tokens": 40, "chunk_overlap": 8}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let made = round_trips::<KnowledgeBase>("create", &v);
    let id = made.id;

    // Upload: two added, one refused; the same bytes again are unchanged.
    let (status, v) = upload(
        &gw,
        id,
        &[
            ("notes.md", NOTES.as_bytes().to_vec()),
            ("letter.txt", LETTER.as_bytes().to_vec()),
            ("photo.png", b"\x89PNG\r\n\x1a\nrest".to_vec()),
        ],
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let up = round_trips::<UploadResult>("upload", &v);
    let verdicts: Vec<_> = up.items.iter().map(|i| i.outcome).collect();
    assert_eq!(
        verdicts,
        [
            UploadVerdict::Added,
            UploadVerdict::Added,
            UploadVerdict::Refused
        ],
        "{v}"
    );
    wait_job(&state, up.job.expect("an upload starts the ingest")).await;
    let (_, v) = upload(&gw, id, &[("notes.md", NOTES.as_bytes().to_vec())]).await;
    let again = round_trips::<UploadResult>("upload again", &v);
    assert_eq!(again.items[0].outcome, UploadVerdict::Unchanged);
    assert_eq!(again.job, None, "nothing was queued");

    // Reads.
    let (_, v) = get(&gw, "/api/knowledge/bases").await;
    assert_eq!(round_trips::<KnowledgeBaseList>("list", &v).bases.len(), 1);
    let (_, v) = get(&gw, &format!("/api/knowledge/bases/{id}")).await;
    let detail = round_trips::<KnowledgeBaseDetail>("detail", &v);
    assert!(detail.base.job.is_some(), "the last job is on the base");
    assert_eq!(detail.base.counts.ready, 2);
    let (_, v) = get(&gw, &format!("/api/knowledge/bases/{id}/files")).await;
    let files = round_trips::<KnowledgeFileList>("files", &v).files;
    assert_eq!(files.len(), 2);
    let notes_id = files.iter().find(|f| f.name == "notes.md").unwrap().id;
    let letter_id = files.iter().find(|f| f.name == "letter.txt").unwrap().id;

    // Settings.
    let (status, v) = post(
        &gw,
        &format!("/api/knowledge/bases/{id}/settings"),
        json!({"description": "tax papers 2025"}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let edited = round_trips::<KnowledgeBaseEdited>("settings", &v);
    assert_eq!(edited.kb.description, "tax papers 2025");

    // Search, with and without a budget.
    let (status, v) = post(
        &gw,
        "/api/knowledge/search",
        json!({"query": "tax refund", "kb_ids": [id]}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let found = round_trips::<KnowledgeSearchResult>("search", &v);
    assert!(!found.excerpts.is_empty(), "{v}");
    assert!(!found.traces.is_empty(), "{v}");
    let (status, v) = post(
        &gw,
        "/api/knowledge/search",
        json!({"query": "tax refund", "budget_tokens": 30, "params": {"rerank": false}}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    round_trips::<KnowledgeSearchResult>("search with a budget", &v);

    // The source viewer, and the original's bytes.
    let chunk = &found.excerpts[0];
    let (status, v) = get(
        &gw,
        &format!(
            "/api/knowledge/files/{}/text?chunk={}",
            chunk.file_id, chunk.chunk_id
        ),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let src = round_trips::<KnowledgeSource>("source", &v);
    assert!(src.highlight.is_some() && src.text.is_some());
    let r = gw
        .client()
        .get(format!("{gw}/api/knowledge/files/{notes_id}/original"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/"));
    assert!(r.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .starts_with("attachment; filename=\"notes.md\""));
    assert_eq!(r.bytes().await.unwrap().as_ref(), NOTES.as_bytes());

    // Re-ingest, resume and cancel.
    let (status, v) = post(
        &gw,
        &format!("/api/knowledge/files/{letter_id}/reingest"),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let again = round_trips::<KnowledgeFileRequeued>("reingest", &v);
    wait_job(&state, again.job.unwrap()).await;
    let (status, v) = post(&gw, &format!("/api/knowledge/bases/{id}/resume"), json!({})).await;
    assert_eq!(status, 200, "{v}");
    let resumed = round_trips::<ResumeResult>("resume with nothing waiting", &v);
    assert_eq!(resumed.job, None, "{v}");
    let (status, v) = post(&gw, &format!("/api/knowledge/bases/{id}/cancel"), json!({})).await;
    if status == 200 {
        round_trips::<CancelResult>("cancel", &v);
    } else {
        assert_eq!(v["code"], "op_failed", "no job to cancel: {v}");
    }

    // Deletes.
    let (status, v) = post(
        &gw,
        &format!("/api/knowledge/files/{letter_id}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let gone = round_trips::<KnowledgeFileDeleted>("delete file", &v);
    assert_eq!(gone.name, "letter.txt");
    let (status, v) = post(&gw, &format!("/api/knowledge/bases/{id}/delete"), json!({})).await;
    assert_eq!(status, 200, "{v}");
    let gone = round_trips::<KnowledgeBaseDeleted>("delete base", &v);
    assert_eq!((gone.name.as_str(), gone.files), ("Taxes", 1));

    // A second base, to say create's name rule is the API's error shape.
    let id2 = create(&gw, json!({"name": "Other", "embed_alias": "embed-model"})).await;
    let (status, v) = post(
        &gw,
        "/api/knowledge/bases",
        json!({"name": "Other", "embed_alias": "embed-model"}),
    )
    .await;
    assert_eq!(
        (status, v["code"].as_str()),
        (400, Some("op_failed")),
        "{v}"
    );
    let _ = id2;
    crate::knowledge::cleanup(&state);
}

/// The trace on the wire is quickdoc's raw `SearchTrace` serialisation: a
/// skipped vector stage (`knn_skipped`), a candidate the reranker did not
/// score (`skipped`) and a NaN score (written `null`) all survive the typed
/// view.
#[test]
fn a_search_trace_with_skipped_stages_is_the_raw_trace() {
    use quickdoc_core::retrieve::{
        FtsWeights, FusedHit, SearchParams, SearchTrace, StageHit, Timings,
    };
    let hit = |id: &str, rank, score, skipped| StageHit {
        chunk_id: id.into(),
        rank,
        score,
        skipped,
    };
    let trace = SearchTrace {
        corpus_id: "kb".into(),
        embed_model: "embed/test".into(),
        params: SearchParams {
            budget_tokens: Some(300),
            fts_weights: FtsWeights::default(),
            ..SearchParams::default()
        },
        fts_query: "\"a\" OR \"b\"".into(),
        fts: vec![hit("c1", 1, -2.5, false)],
        knn_skipped: Some("the embedding model is on hold".into()),
        knn: vec![],
        fused: vec![FusedHit {
            chunk_id: "c1".into(),
            rrf_score: 0.016,
            fts_rank: Some(1),
            knn_rank: None,
        }],
        rerank_model: Some("rerank/test".into()),
        rerank_skipped: None,
        rerank: vec![hit("c1", 1, 0.9, false), hit("c2", 2, f32::NAN, true)],
        token_counter: "approx".into(),
        budget_used_tokens: 12,
        budget_dropped: vec!["c3".into()],
        resident_vectors: 7,
        resident_bytes: 4096,
        knn_kernel: "scalar",
        timings: Timings::default(),
    };
    let raw = serde_json::to_value(&trace).unwrap();
    assert!(raw["knn_skipped"].is_string(), "{raw}");
    assert_eq!(raw["rerank"][1]["skipped"], json!(true));
    assert!(raw["rerank"][1]["score"].is_null(), "NaN is written null");
    let view = lmgw_core::knowledge::wire::trace(&trace);
    assert_eq!(serde_json::to_value(&view).unwrap(), raw);
    // Without a skipped stage neither key is written, as before.
    let plain = SearchTrace {
        knn_skipped: None,
        rerank: vec![hit("c1", 1, 0.9, false)],
        ..trace
    };
    let raw = serde_json::to_value(&plain).unwrap();
    assert!(raw.get("knn_skipped").is_none() && raw["rerank"][0].get("skipped").is_none());
    assert_eq!(
        serde_json::to_value(lmgw_core::knowledge::wire::trace(&plain)).unwrap(),
        raw
    );
}

/// The docs playground's hits are quickdoc's raw `Hit` serialisation: the
/// `rerank_skipped` flag (written only when true), absent ranks as `null`
/// and a NaN score as `null` all survive the typed view.
#[test]
fn a_docs_search_hit_is_the_raw_hit() {
    use quickdoc_core::retrieve::Hit;
    use quickdoc_core::store::Chunk;
    let hit = |skipped, fts_rank, rerank_score| Hit {
        chunk: Chunk {
            id: "axum@0.8#1".into(),
            document_id: 3,
            corpus_id: 2,
            heading_path: "Extractors > Json".into(),
            span_start: 10,
            span_end: 99,
            payload: "```rust\nfn f() {}\n```".into(),
            derived_title: "Json".into(),
            derived_summary: "".into(),
        },
        score: 0.016,
        rrf_score: 0.016,
        fts_rank,
        knn_rank: None,
        knn_score: Some(0.5),
        rerank_score,
        rerank_skipped: skipped,
        tokens: 42,
    };
    for h in [
        hit(true, Some(2), None),
        hit(false, None, Some(0.9)),
        hit(false, Some(1), Some(f32::NAN)),
    ] {
        let raw = serde_json::to_value(&h).unwrap();
        assert_eq!(
            raw.get("rerank_skipped").is_some(),
            h.rerank_skipped,
            "written only when true: {raw}"
        );
        assert_eq!(
            serde_json::to_value(lmgw_core::knowledge::wire::hit(&h)).unwrap(),
            raw
        );
    }
}
