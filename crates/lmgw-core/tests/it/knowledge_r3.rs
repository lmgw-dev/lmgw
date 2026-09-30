//! Knowledge bases, review R3: the defects the R2 fixes introduced.
//!
//! A re-upload that restores a missing original; a Resume that re-chunks only
//! the files that overrun the model (and never reads an unchanged PDF page
//! with vision twice); measurement inside the job and busy checks first; a
//! search turn that costs no tokenizer call, keeps what it already computed
//! when a stage fails, and never invents a rerank score; citations that
//! survive a re-ingest; and an oversize refusal that is not a licence to halve
//! a chunk down to single bytes.

use std::sync::atomic::Ordering;

use lmgw_core::jobs::JobView;
use lmgw_core::knowledge::retrieve::{self, Options};
use lmgw_core::knowledge::{fit, limit, ops, originals, read, store as kstore};
use lmgw_core::state::SharedState;
use serde_json::{json, Value};
use wiremock::MockServer;

use super::knowledge::{cleanup, get, ingested, mount, pdf, post, setup, upload, wait_job, NOTES};
use super::knowledge_hardening::{base_path, wait_file, world, LONG_NOTES};
use crate::common::serve;

fn detail(row: &lmgw_core::store::JobRow) -> Value {
    JobView::from_row(row).detail
}

/// Make the stored chunks of a file look as large as `tokens` — what a base
/// cut for another model, or under another limit, holds.
async fn oversize(state: &SharedState, file: i64, tokens: i64) {
    sqlx::query("UPDATE kb_chunk SET tokens = ?2 WHERE file_id = ?1")
        .bind(file)
        .bind(tokens)
        .execute(&state.knowledge.pool)
        .await
        .unwrap();
}

async fn file_named(state: &SharedState, kb: i64, name: &str) -> kstore::KbFile {
    kstore::list_files(&state.knowledge.pool, kb)
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no file {name}"))
}

// ---------------------------------------------------------------------------
// 1. A re-upload restores the original
// ---------------------------------------------------------------------------

/// A file that failed because its original went missing was re-queued by
/// uploading the same bytes again — and failed forever, because the re-queue
/// never stored the bytes it was handed.
#[tokio::test]
async fn re_uploading_the_same_bytes_restores_a_missing_original() {
    let (_mock, state, _w) = world().await;
    let gw = serve(state.clone()).await;
    let bytes = b"# One\n\nApples.\n".to_vec();
    let id = ingested(
        &state,
        &gw,
        json!({"name": "Taxes", "embed_alias": "embed-model"}),
        &[("a.md", bytes.clone())],
    )
    .await;
    let file = file_named(&state, id, "a.md").await;
    // The original vanishes, and the next ingest fails on it.
    let original = originals::path(&state.data_dir, &file.sha256).unwrap();
    std::fs::remove_file(&original).unwrap();
    let ops_v = ops::reingest_file(&state, file.id).await.unwrap();
    wait_job(&state, ops_v["job"].as_i64().unwrap()).await;
    wait_file(&state, file.id, "failed").await;
    let failed = file_named(&state, id, "a.md").await;
    assert!(
        failed.error.unwrap().contains("upload it again"),
        "the failure says what to do"
    );

    let (_, v) = upload(&gw, id, &[("a.md", bytes.clone())]).await;
    assert_eq!(v["items"][0]["outcome"], "replaced", "{v}");
    assert!(original.exists(), "the upload put the original back");
    let row = wait_job(&state, v["job"].as_i64().unwrap()).await;
    assert_eq!(row.status, "done", "{:?}", row.error);
    wait_file(&state, file.id, "ready").await;
    assert_eq!(
        originals::read(&state.data_dir, &file.sha256)
            .await
            .unwrap(),
        bytes
    );
    cleanup(&state);
}

// ---------------------------------------------------------------------------
// 2. Resume re-chunks only what does not fit
// ---------------------------------------------------------------------------

/// Two files, one whose stored chunks overrun the model. Moving the base to
/// another model measures inside the job, sends only the overrunning file back
/// to ingest, and re-embeds the other in place — its row is not rewritten.
#[tokio::test]
async fn only_the_files_whose_chunks_overrun_the_model_are_rechunked() {
    let (_mock, state, _w) = world().await;
    let gw = serve(state.clone()).await;
    let id = ingested(
        &state,
        &gw,
        json!({"name": "Taxes", "embed_alias": "embed-model", "chunk_tokens": 60, "chunk_overlap": 8}),
        &[
            ("small.md", b"# One\n\nApples.\n".to_vec()),
            ("notes.md", LONG_NOTES.as_bytes().to_vec()),
        ],
    )
    .await;
    let small = file_named(&state, id, "small.md").await;
    let notes = file_named(&state, id, "notes.md").await;
    // A marker an ingest would overwrite: it survives only if small.md is
    // never re-ingested.
    sqlx::query("UPDATE kb_file SET notes = '[\"untouched\"]' WHERE id = ?1")
        .bind(small.id)
        .execute(&state.knowledge.pool)
        .await
        .unwrap();
    oversize(&state, notes.id, 5000).await;

    let (status, v) = post(
        &gw,
        &base_path(id, "/settings").await,
        json!({"embed_alias": "embed-small"}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let job = v["reembed_job"].as_i64().expect("one re-embed job");
    assert!(
        v["ingest_job"].is_null() && v["rechunk_files"] == 0,
        "the handler decided nothing: {v}"
    );
    let row = wait_job(&state, job).await;
    assert_eq!(row.status, "done", "{:?}", row.error);
    let d = detail(&row);
    assert_eq!(d["rechunk_files"], json!(["notes.md"]), "{d}");
    assert!(
        d["measure"].as_str().unwrap().contains("measured on"),
        "the method is stated: {d}"
    );
    assert!(
        d["rechunk_why"]
            .as_str()
            .unwrap()
            .contains("stored chunks are larger"),
        "{d}"
    );

    // The hand-on ingest re-chunked notes.md alone.
    wait_file(&state, notes.id, "ready").await;
    let notes_now = kstore::file_chunks(&state.knowledge.pool, notes.id)
        .await
        .unwrap();
    assert!(
        notes_now.iter().all(|c| c.tokens < 5000),
        "re-chunked: {:?}",
        notes_now.iter().map(|c| c.tokens).collect::<Vec<_>>()
    );
    let small_now = file_named(&state, id, "small.md").await;
    assert_eq!(small_now.notes, ["untouched"], "small.md was never re-read");
    let (_, d) = get(&gw, &base_path(id, "").await).await;
    let c = &d["base"]["counts"];
    for _ in 0..200 {
        // The hand-on ingest closes the status.
        if d["base"]["status"] == "ready" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(c["embedded"], c["chunks"], "{d}");
    cleanup(&state);
}

/// A file that failed and holds stale chunks without a vector neither keeps
/// the base `re_embed_required` nor is read again on every Resume; the base
/// names it.
#[tokio::test]
async fn a_persistently_failing_file_is_reported_not_retried() {
    let (_mock, state, _w) = world().await;
    let gw = serve(state.clone()).await;
    let id = ingested(
        &state,
        &gw,
        json!({"name": "Taxes", "embed_alias": "embed-model", "chunk_tokens": 60, "chunk_overlap": 8}),
        &[
            ("good.md", b"# One\n\nApples.\n".to_vec()),
            ("bad.md", LONG_NOTES.as_bytes().to_vec()),
        ],
    )
    .await;
    let pool = &state.knowledge.pool;
    let bad = file_named(&state, id, "bad.md").await;
    // bad.md failed on its re-chunk; its stale chunks overrun and have no
    // vector; the base was left needing a re-embed.
    oversize(&state, bad.id, 5000).await;
    kstore::clear_embeddings(pool, id).await.unwrap();
    kstore::set_file_status(pool, bad.id, "failed", Some("could not be read"))
        .await
        .unwrap();
    kstore::set_kb_status(pool, id, "re_embed_required")
        .await
        .unwrap();

    let view = ops::get(&state, id).await.unwrap();
    let notes = view.notes.join("\n");
    assert!(
        notes.contains("'bad.md' failed (could not be read)")
            && notes.contains("Resume does not retry it"),
        "{notes}"
    );

    let v = ops::resume(&state, id).await.unwrap();
    assert_eq!(v["kind"], "kb_reembed", "{v}");
    let row = wait_job(&state, v["job"].as_i64().unwrap()).await;
    assert_eq!(row.status, "done", "{:?}", row.error);
    let d = detail(&row);
    assert_eq!(d["failed_over"], json!(["bad.md"]), "reported: {d}");
    assert_eq!(d["rechunk_files"], json!([]), "not re-read: {d}");
    assert_eq!(file_named(&state, id, "bad.md").await.status, "failed");
    let kb = kstore::get_kb(pool, id).await.unwrap().unwrap();
    assert_eq!(kb.status, "ready", "the failed file does not hold the base");
    // good.md's chunks have their vectors; bad.md's are still without.
    let left = kstore::unembedded_files(pool, id).await.unwrap();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].name, "bad.md");
    let view = ops::get(&state, id).await.unwrap();
    assert!(
        !view.notes.iter().any(|n| n.contains("Resume re-embeds")),
        "no misleading note: {:?}",
        view.notes
    );
    // Nothing left for Resume to do.
    let v = ops::resume(&state, id).await.unwrap();
    assert!(v["job"].is_null(), "{v}");
    cleanup(&state);
}

/// A re-chunk of a PDF whose bytes did not change reads no page with the
/// vision model again.
#[tokio::test]
async fn a_rechunk_does_not_read_unchanged_pages_again() {
    if !lmgw_core::extract::pdf::available().await {
        eprintln!("skipped: poppler-utils (pdftotext/pdftoppm) is not installed");
        return;
    }
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let gw = serve(state.clone()).await;
    let doc = pdf(&[Some("Rent invoice March 800 EUR"), None, None]);
    let id = super::knowledge::ingested(
        &state,
        &gw,
        json!({"name": "Papers", "embed_alias": "embed-model", "vision_alias": "vision-model"}),
        &[("doc.pdf", doc)],
    )
    .await;
    let vision_calls = || async {
        mock.received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().ends_with("/chat/completions"))
            .count()
    };
    assert_eq!(
        vision_calls().await,
        2,
        "the two blank pages were read once"
    );

    // A chunk-size change sends every file back to ingest.
    let (status, v) = post(
        &gw,
        &base_path(id, "/settings").await,
        json!({"chunk_tokens": 40, "chunk_overlap": 4}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let row = wait_job(&state, v["ingest_job"].as_i64().unwrap()).await;
    assert_eq!(row.status, "done", "{:?}", row.error);
    assert_eq!(vision_calls().await, 2, "no page was read again");
    let f = file_named(&state, id, "doc.pdf").await;
    assert_eq!(f.status, "ready");
    assert!(
        f.notes.iter().any(|n| n.contains("reused the reading")),
        "said on the file: {:?}",
        f.notes
    );
    // A different vision alias is a different reading: nothing is reused.
    assert_eq!(
        kstore::get_page_read(
            &state.knowledge.pool,
            &f.sha256,
            2,
            "other-vision",
            "ocr",
            lmgw_core::extract::vision_prompts::VISION_PROMPT_VERSION
        )
        .await
        .unwrap(),
        None
    );
    cleanup(&state);
}

// ---------------------------------------------------------------------------
// 3. The edit is fast and refuses a busy base first
// ---------------------------------------------------------------------------

/// A model change on a base that has a job is refused before the new model is
/// touched — no probe embed reaches it.
#[tokio::test]
async fn an_edit_checks_for_a_running_job_before_it_touches_the_new_model() {
    let (_mock, state, w) = world().await;
    let gw = serve(state.clone()).await;
    let (status, v) = post(
        &gw,
        "/api/knowledge/bases",
        json!({"name": "Taxes", "embed_alias": "embed-model"}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let id = v["id"].as_i64().unwrap();
    w.embed_delay_ms.store(700, Ordering::Relaxed);
    let (_, up) = upload(&gw, id, &[("a.md", b"# One\n\nApples.\n".to_vec())]).await;
    let job = up["job"].as_i64().unwrap();
    let before = w.calls("embed-other");
    let (status, v) = post(
        &gw,
        &base_path(id, "/settings").await,
        json!({"embed_alias": "embed-2"}),
    )
    .await;
    assert_ne!(status, 200, "{v}");
    assert!(v.to_string().contains("job running"), "{v}");
    assert_eq!(
        w.calls("embed-other"),
        before,
        "the new model was not probed"
    );
    wait_job(&state, job).await;
    cleanup(&state);
}

/// The measurement samples a bounded number of chunks per base — however many
/// chunks the base holds — and says so.
#[tokio::test]
async fn measuring_samples_a_bounded_number_of_chunks_and_says_so() {
    let (_mock, state, _w) = world().await;
    let gw = serve(state.clone()).await;
    let text: String = (0..80)
        .map(|i| {
            format!(
                "Paragraph {i} says something quite specific about item {i}, the supplier of \
                 item {i} and the price paid for item {i} in the spring.\n\n"
            )
        })
        .collect();
    let id = ingested(
        &state,
        &gw,
        json!({"name": "Many", "embed_alias": "embed-model", "chunk_tokens": 30, "chunk_overlap": 0}),
        &[("many.md", text.into_bytes())],
    )
    .await;
    let kb = kstore::get_kb(&state.knowledge.pool, id)
        .await
        .unwrap()
        .unwrap();
    let total = kstore::chunk_total(&state.knowledge.pool, id)
        .await
        .unwrap();
    assert!(
        total > fit::SAMPLE_CHUNKS as i64 * 2,
        "{total} chunks: {:?}",
        file_named(&state, id, "many.md").await
    );
    let m = fit::measure(&state, &kb, "embed-model").await.unwrap();
    assert!(
        m.method.contains(&format!(
            "measured on {} of {total} chunks",
            fit::SAMPLE_CHUNKS
        )),
        "{}",
        m.method
    );
    assert!(m.over.is_empty(), "nothing overruns 2048: {m:?}");
    // The same base against a 20-token model: every chunk is judged from its
    // stored count — no chunk text is loaded and no call is made per chunk.
    let tiny = fit::measure(&state, &kb, "embed-tiny").await.unwrap();
    assert_eq!(tiny.max, Some(20));
    assert!(
        tiny.chunks_over > 0 && tiny.chunks_total as i64 == total,
        "{tiny:?}"
    );
    cleanup(&state);
}

// ---------------------------------------------------------------------------
// 4. What a search turn costs, and what a failing stage keeps
// ---------------------------------------------------------------------------

/// The tokenizer ratio is remembered per model identity: measured once, used
/// after; a remapped alias is a different model and starts over.
#[tokio::test]
async fn the_tokenizer_ratio_is_remembered_per_model_identity() {
    let (_mock, state, _w) = world().await;
    let up = state.snapshot().upstreams.values().next().unwrap().id;
    let add = |alias: &'static str, target: &'static str| {
        let state = state.clone();
        async move {
            lmgw_core::store::insert_alias(
                &state.db,
                &lmgw_core::store::NewAlias {
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
            state.reload_snapshot().await.unwrap();
        }
    };
    // A real OpenAI model name has a tokenizer this gateway can run.
    add("emb-oai", "text-embedding-3-small").await;
    assert!(!limit::ratio_known(&state, "emb-oai"));
    let (c, _) = limit::cached_counter(&state, "emb-oai", "hello world, some text").await;
    assert!(c.measured());
    assert!(limit::ratio_known(&state, "emb-oai"), "measured once");
    // What was remembered is what later calls use — no new measurement.
    limit::remember_ratio(
        &state,
        "emb-oai",
        &limit::ScaledCounter::with_ratio("emb-oai", 7.0),
    );
    let (c, why) = limit::cached_counter(&state, "emb-oai", "anything else").await;
    assert_eq!(c.ratio(), 7.0, "{why}");
    assert!(why.contains("remembered"), "{why}");
    // Another model behind another alias has its own entry.
    add("emb-oai-large", "text-embedding-3-large").await;
    assert!(!limit::ratio_known(&state, "emb-oai-large"));
    cleanup(&state);
}

/// A failing reranker costs the rerank stage only: the query was embedded
/// once, and the vector results are kept.
#[tokio::test]
async fn a_failing_reranker_does_not_embed_the_query_again() {
    let (_mock, state, w) = world().await;
    let gw = serve(state.clone()).await;
    let id = super::knowledge::ingested(
        &state,
        &gw,
        json!({"name": "Taxes", "embed_alias": "embed-model", "rerank_alias": "rerank-model",
               "chunk_tokens": 60, "chunk_overlap": 8}),
        &[("notes.md", NOTES.as_bytes().to_vec())],
    )
    .await;
    let mut p = lmgw_core::quickdoc::query::default_params(&state.snapshot());
    p.rerank = true;
    p.k_rerank = 10;
    let opts = Options {
        budget_tokens: None,
        params: Some(p),
    };
    w.rerank_fails.store(true, Ordering::Relaxed);
    let before = w.embedded("embed-tgt");
    let r = retrieve::retrieve(&state, &[id], "refund May", &opts).await;
    assert!(
        r.notes.iter().any(|n| n.contains("reranker unavailable")),
        "{:?}",
        r.notes
    );
    assert_eq!(
        w.embedded("embed-tgt") - before,
        1,
        "the query was embedded once, not once per degrade step"
    );
    assert!(r.traces[0].knn_skipped.is_none() && !r.traces[0].knn.is_empty());
    cleanup(&state);
}

/// A held local reranker is refused up front, like a held embedder: no
/// request reaches a fallback, and the note says why.
#[tokio::test]
async fn a_held_local_reranker_is_refused_up_front() {
    let (_mock, state, w) = world().await;
    let gw = serve(state.clone()).await;
    lmgw_core::store::insert_aux_model(
        &state.db,
        &lmgw_core::store::NewAuxModel {
            model_id: "rerank-local".into(),
            gguf_path: "r.gguf".into(),
            kind: lmgw_core::config::AuxKind::Rerank,
            pooling: None,
            ctx_size: None,
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let alias = state.snapshot().aux_public_name("rerank-local");
    let id = super::knowledge::ingested(
        &state,
        &gw,
        json!({"name": "Taxes", "embed_alias": "embed-model", "chunk_tokens": 60, "chunk_overlap": 8}),
        &[("notes.md", NOTES.as_bytes().to_vec())],
    )
    .await;
    sqlx::query("UPDATE kb SET rerank_alias = ?2 WHERE id = ?1")
        .bind(id)
        .bind(&alias)
        .execute(&state.knowledge.pool)
        .await
        .unwrap();
    lmgw_core::ops::hold_set(&state, true).await.unwrap();
    let mut p = lmgw_core::quickdoc::query::default_params(&state.snapshot());
    p.rerank = true;
    let opts = Options {
        budget_tokens: None,
        params: Some(p),
    };
    let r = retrieve::retrieve(&state, &[id], "refund May", &opts).await;
    assert!(!r.excerpts.is_empty(), "{:?}", r.notes);
    let held = r
        .notes
        .iter()
        .find(|n| n.contains("not reranked") && n.contains("holding the GPU"))
        .unwrap_or_else(|| panic!("{:?}", r.notes));
    assert!(held.contains("will not rank through a fallback"), "{held}");
    assert!(
        !r.notes.iter().any(|n| n.contains("reranker unavailable")),
        "refused up front, not after a failed call: {:?}",
        r.notes
    );
    assert!(w.rerank_docs.lock().unwrap().is_empty());
    assert!(r.traces[0].rerank_skipped.is_some());
    cleanup(&state);
}

// ---------------------------------------------------------------------------
// 5. A candidate the reranker did not score keeps its own score
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unscored_candidates_are_flagged_not_given_invented_scores() {
    let (_mock, state, _w) = world().await;
    let gw = serve(state.clone()).await;
    let id = super::knowledge::ingested(
        &state,
        &gw,
        json!({"name": "Taxes", "embed_alias": "embed-model", "rerank_alias": "rerank-small",
               "chunk_tokens": 60, "chunk_overlap": 8}),
        &[("notes.md", NOTES.as_bytes().to_vec())],
    )
    .await;
    let mut p = lmgw_core::quickdoc::query::default_params(&state.snapshot());
    p.rerank = true;
    p.k_rerank = 10;
    let opts = Options {
        budget_tokens: None,
        params: Some(p),
    };
    let r = retrieve::retrieve(&state, &[id], "refund May", &opts).await;
    let skipped: Vec<_> = r.excerpts.iter().filter(|e| e.rerank_skipped).collect();
    let scored: Vec<_> = r.excerpts.iter().filter(|e| !e.rerank_skipped).collect();
    assert!(!skipped.is_empty() && !scored.is_empty(), "{r:#?}");
    // Reranked first, skipped after.
    let first_skipped = r.excerpts.iter().position(|e| e.rerank_skipped).unwrap();
    assert!(
        r.excerpts[first_skipped..].iter().all(|e| e.rerank_skipped),
        "{:?}",
        r.excerpts
            .iter()
            .map(|e| e.rerank_skipped)
            .collect::<Vec<_>>()
    );
    // A skipped excerpt's score is its fused score, and the trace says so.
    let t = &r.traces[0];
    for e in &skipped {
        let fused = t.fused.iter().find(|f| f.chunk_id == e.chunk_id).unwrap();
        assert_eq!(e.score, fused.rrf_score, "no invented score");
        let stage = t.rerank.iter().find(|h| h.chunk_id == e.chunk_id).unwrap();
        assert!(stage.skipped);
        assert_eq!(stage.score, fused.rrf_score);
    }
    for e in &scored {
        assert!(
            !t.rerank
                .iter()
                .find(|h| h.chunk_id == e.chunk_id)
                .unwrap()
                .skipped
        );
    }
    // Skipped, by fused score, best first.
    assert!(skipped.windows(2).all(|w| w[0].score >= w[1].score));
    cleanup(&state);
}

// ---------------------------------------------------------------------------
// 6. A citation survives a re-ingest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_citation_whose_chunk_id_is_gone_resolves_by_its_stored_position() {
    let (_mock, state, _w) = world().await;
    let gw = serve(state.clone()).await;
    let id = ingested(
        &state,
        &gw,
        json!({"name": "Taxes", "embed_alias": "embed-model", "chunk_tokens": 60, "chunk_overlap": 8}),
        &[("notes.md", NOTES.as_bytes().to_vec())],
    )
    .await;
    let r = retrieve::retrieve(&state, &[id], "refund May", &Options::default()).await;
    let ex = r.excerpts[0].clone();
    let file = file_named(&state, id, "notes.md").await;
    assert_eq!(
        ex.file_sha, file.sha256,
        "the excerpt names the file version"
    );
    let cited = read::Cited {
        span_start: Some(ex.span_start),
        span_end: Some(ex.span_end),
        file_sha: Some(ex.file_sha.clone()),
    };

    // The chunk id from before a re-ingest: gone. Its position still shows
    // the passage.
    let gone = "0".repeat(64);
    let s = read::source(&state, ex.file_id, Some(&gone), &cited)
        .await
        .unwrap();
    let h = s.highlight.unwrap();
    let text = s.text.unwrap();
    assert_eq!(&text[h.span_start as usize..h.span_end as usize], ex.text);
    assert!(s.notice.unwrap().contains("stored position"));

    // A citation stored before the sha was kept: found by position, and the
    // notice says the file may have changed.
    let old = read::Cited {
        file_sha: None,
        ..cited.clone()
    };
    let s = read::source(&state, ex.file_id, Some(&gone), &old)
        .await
        .unwrap();
    assert!(s.notice.unwrap().contains("predates"), "the caveat is said");

    // The file's bytes are not the ones cited: say so, never highlight a
    // position in other content.
    let other = read::Cited {
        file_sha: Some("f".repeat(64)),
        ..cited.clone()
    };
    let e = read::source(&state, ex.file_id, Some(&gone), &other)
        .await
        .unwrap_err();
    assert!(
        e.contains("the document changed since this citation"),
        "{e}"
    );

    // Only an id and no position: the old refusal.
    let e = read::source(&state, ex.file_id, Some(&gone), &read::Cited::default())
        .await
        .unwrap_err();
    assert!(e.contains("is not in"), "{e}");

    // The route takes the position and the sha.
    let (status, v) = get(
        &gw,
        &format!(
            "/api/knowledge/files/{}/text?chunk={gone}&span_start={}&span_end={}&sha={}",
            ex.file_id, ex.span_start, ex.span_end, ex.file_sha
        ),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert!(
        v["notice"].as_str().unwrap().contains("stored position"),
        "{v}"
    );
    cleanup(&state);
}

// ---------------------------------------------------------------------------
// 7. An oversize refusal is not a licence to halve to single bytes
// ---------------------------------------------------------------------------

/// A persistent error that merely mentions a size ("too large", "context
/// length") is not an oversize refusal: the file fails at once with the
/// upstream's words, after one embed call.
#[tokio::test]
async fn an_unrelated_error_naming_a_size_fails_the_file_without_halving() {
    let (_mock, state, w) = world().await;
    let gw = serve(state.clone()).await;
    let (_, v) = post(
        &gw,
        "/api/knowledge/bases",
        json!({"name": "Taxes", "embed_alias": "embed-model", "chunk_tokens": 60, "chunk_overlap": 8}),
    )
    .await;
    let id = v["id"].as_i64().unwrap();
    w.embed_error.lock().unwrap().insert(
        "embed-tgt".into(),
        (
            400,
            "the file is too large to upload; context length parameter is invalid".into(),
        ),
    );
    let before = w.calls("embed-tgt");
    let (_, up) = upload(&gw, id, &[("a.md", LONG_NOTES.as_bytes().to_vec())]).await;
    let row = wait_job(&state, up["job"].as_i64().unwrap()).await;
    assert_eq!(row.status, "done", "{:?}", row.error);
    let f = file_named(&state, id, "a.md").await;
    assert_eq!(f.status, "failed");
    assert!(f.error.unwrap().contains("too large to upload"));
    assert_eq!(
        w.calls("embed-tgt") - before,
        1,
        "one call, no halving on an error that is not about size"
    );
    cleanup(&state);
}

/// A model that refuses every input as too large, however small, costs a
/// handful of calls: halving stops at a fraction of the limit, and the file
/// fails naming the upstream's refusal.
#[tokio::test]
async fn halving_stops_at_a_fraction_of_the_limit() {
    let (_mock, state, w) = world().await;
    let gw = serve(state.clone()).await;
    let (_, v) = post(
        &gw,
        "/api/knowledge/bases",
        json!({"name": "Big", "embed_alias": "embed-model", "chunk_tokens": 1500, "chunk_overlap": 0}),
    )
    .await;
    let id = v["id"].as_i64().unwrap();
    w.embed_error.lock().unwrap().insert(
        "embed-tgt".into(),
        (
            400,
            "input is too large to process. increase the physical batch size".into(),
        ),
    );
    let words: String = (0..4000).map(|i| format!("word{} ", i % 50)).collect();
    let before = w.calls("embed-tgt");
    let (_, up) = upload(&gw, id, &[("big.md", words.into_bytes())]).await;
    wait_job(&state, up["job"].as_i64().unwrap()).await;
    let f = file_named(&state, id, "big.md").await;
    assert_eq!(f.status, "failed", "{:?}", f.error);
    let why = f.error.unwrap();
    assert!(
        why.contains("is not cut further") && why.contains("input is too large"),
        "{why}"
    );
    let calls = w.calls("embed-tgt") - before;
    assert!(
        (3..=10).contains(&calls),
        "a handful of calls, not one per byte: {calls}"
    );
    cleanup(&state);
}
