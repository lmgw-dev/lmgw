//! Knowledge bases: what one embedding input may hold (`embed_input_limit`),
//! and the upload that lands as an ingest job ends.

use std::time::Duration;

use lmgw_core::config::AuxKind;
use lmgw_core::knowledge::limit;
use lmgw_core::knowledge::{ingest, originals, store as kstore};
use lmgw_core::store::{self, NewAuxModel};
use serde_json::json;
use wiremock::MockServer;

use super::knowledge::{cleanup, create, get, mount, post, setup, upload, wait_job, NOTES};
use crate::common::serve;

fn aux_row(id: &str, args: &[&str]) -> NewAuxModel {
    NewAuxModel {
        model_id: id.into(),
        gguf_path: format!("{id}.gguf"),
        kind: AuxKind::Embed,
        pooling: None,
        ctx_size: Some(8192),
        ubatch_size: None,
        batch_size: None,
        mmproj_path: None,
        backend: None,
        threads: None,
        args: args.iter().map(|a| a.to_string()).collect(),
        idle_seconds: 0,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    }
}

/// A local aux embedder takes llama-server's default ubatch of 512 per input
/// unless the row raises it — in its `ubatch_size`/`batch_size` fields, or
/// with a copy in its extra args while a field is unset; a cloud alias takes
/// the catalog's context.
#[tokio::test]
async fn the_input_limit_is_the_ubatch_for_local_aux_and_the_catalog_for_cloud() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    store::insert_aux_model(&state.db, &aux_row("emb-default", &[]))
        .await
        .unwrap();
    store::insert_aux_model(
        &state.db,
        &aux_row("emb-raised", &["-ub", "2048", "-b", "2048"]),
    )
    .await
    .unwrap();
    // Where the boot hoist puts them (decision-models WP2b): the fields. An
    // args copy of a field is dropped by the renderer, so the field wins.
    let mut fields = aux_row("emb-fields", &["-ub", "1024"]);
    fields.ubatch_size = Some(8192);
    fields.batch_size = Some(8192);
    store::insert_aux_model(&state.db, &fields).await.unwrap();
    let mut ub_only = aux_row("emb-ub-only", &[]);
    ub_only.ubatch_size = Some(8192);
    store::insert_aux_model(&state.db, &ub_only).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let snap = state.snapshot();

    let l = limit::input_limit(&state, &snap.aux_name_for(AuxKind::Embed, "emb-fields")).await;
    assert_eq!(l.tokens, Some(8192), "{l:?}");
    assert!(l.source.contains("ubatch_size of 8192"), "{}", l.source);

    let l = limit::input_limit(&state, &snap.aux_name_for(AuxKind::Embed, "emb-ub-only")).await;
    assert_eq!(
        l.tokens,
        Some(2048),
        "the batch default lowers the ubatch: {l:?}"
    );
    assert!(
        l.source.contains("batch_size") && !l.source.contains("-ub N"),
        "{}",
        l.source
    );

    let l = limit::input_limit(&state, &snap.aux_name_for(AuxKind::Embed, "emb-default")).await;
    assert_eq!(l.tokens, Some(512), "{l:?}");
    assert!(l.source.contains("ubatch default of 512"), "{}", l.source);
    assert_eq!(
        l.tokenizer, "model",
        "a local model is counted by /tokenize"
    );

    let l = limit::input_limit(&state, &snap.aux_name_for(AuxKind::Embed, "emb-raised")).await;
    assert_eq!(l.tokens, Some(2048), "{l:?}");
    assert!(l.source.contains("-ub 2048"), "{}", l.source);

    let l = limit::input_limit(&state, "embed-model").await;
    assert_eq!(l.tokens, Some(2048), "the mock catalog's context: {l:?}");
    assert!(l.source.contains("catalog"), "{}", l.source);
    cleanup(&state);
}

/// A reranker's limit follows its header: a causal one splits its input
/// (its context bounds it, from llama.cpp b11223 on, which the source
/// names), a BERT-family one (`attention.causal = false`) reads it in one
/// ubatch, and one whose header cannot be read counts as unsplit — the
/// smaller limit.
#[tokio::test]
async fn a_reranker_s_limit_follows_its_header() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let dir = tempfile::tempdir().unwrap();
    let mut s = state.snapshot().settings.clone();
    s.aux_router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &s).await.unwrap();
    lmgw_core::gguf::synth::embedding("qwen3", 4, 8192)
        .write_to(&dir.path().join("rr-causal.gguf"));
    let mut bert = lmgw_core::gguf::synth::embedding("bert", 4, 8192);
    bert.bool_flag("bert.attention.causal", false);
    bert.write_to(&dir.path().join("rr-bert.gguf"));
    for id in ["rr-causal", "rr-bert", "rr-missing"] {
        let mut row = aux_row(id, &[]);
        row.kind = AuxKind::Rerank;
        store::insert_aux_model(&state.db, &row).await.unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let snap = state.snapshot();

    let l = limit::input_limit(&state, &snap.aux_name_for(AuxKind::Rerank, "rr-causal")).await;
    assert_eq!(l.tokens, Some(8192), "{l:?}");
    assert!(
        l.source.contains("causal reranker") && l.source.contains("b11223"),
        "{}",
        l.source
    );
    for id in ["rr-bert", "rr-missing"] {
        let l = limit::input_limit(&state, &snap.aux_name_for(AuxKind::Rerank, id)).await;
        assert_eq!(l.tokens, Some(512), "{id}: {l:?}");
        assert!(l.source.contains("ubatch default of 512"), "{}", l.source);
    }
    cleanup(&state);
}

/// The base's view shows the limit; a chunk size above it is refused with the
/// number, and a size inside it (heading path included) ingests.
#[tokio::test]
async fn the_view_shows_the_limit_and_chunks_stay_inside_it() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let gw = serve(state.clone()).await;
    let id = create(
        &gw,
        json!({"name": "Limits", "embed_alias": "embed-model", "chunk_tokens": 60, "chunk_overlap": 8}),
    )
    .await;
    let (_, v) = get(&gw, &format!("/api/knowledge/bases/{id}")).await;
    assert_eq!(v["base"]["embed_input_limit"], 2048, "{v}");
    assert!(v["base"]["embed_input_limit_source"]
        .as_str()
        .unwrap()
        .contains("catalog"));
    assert_eq!(v["base"]["embed_tokenizer"], "tiktoken_guess", "{v}");

    let (status, v) = upload(&gw, id, &[("notes.md", NOTES.as_bytes().to_vec())]).await;
    assert_eq!(status, 200, "{v}");
    let row = wait_job(&state, v["job"].as_i64().unwrap()).await;
    assert_eq!(row.status, "done", "{:?}", row.error);
    let (_, v) = post(
        &gw,
        "/api/knowledge/search",
        json!({"query": "refund", "kb_ids": [id]}),
    )
    .await;
    let excerpts = v["excerpts"].as_array().unwrap();
    assert!(!excerpts.is_empty(), "{v}");
    for e in excerpts {
        assert!(
            e["tokens"].as_u64().unwrap() <= 60,
            "heading path and payload fit chunk_tokens: {e}"
        );
    }
    cleanup(&state);
}

/// An upload that finds the ingest job still registered — while that job has
/// already looked for pending files for the last time — must not stay pending.
/// `hand_on` is what the finishing job does; here it runs for a file that
/// arrived after the job's last look.
#[tokio::test]
async fn a_file_that_lands_as_the_ingest_ends_is_still_ingested() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let gw = serve(state.clone()).await;
    let id = create(&gw, json!({"name": "Race", "embed_alias": "embed-model"})).await;
    let (_, v) = upload(&gw, id, &[("a.md", NOTES.as_bytes().to_vec())]).await;
    let job = v["job"].as_i64().unwrap();
    assert_eq!(wait_job(&state, job).await.status, "done");

    // What the raced upload leaves behind: a stored original, a pending row,
    // and no job.
    let bytes = b"# Late\n\nThis file arrived after the job's last look.\n".to_vec();
    let sha = originals::sha256_hex(&bytes);
    originals::store(&state.data_dir, &bytes).await.unwrap();
    let fid = kstore::insert_file(
        &state.knowledge.pool,
        &kstore::NewKbFile {
            kb_id: id,
            name: "late.md".into(),
            kind: "text".into(),
            sub: "markdown".into(),
            mime: "text/markdown".into(),
            size: bytes.len() as i64,
            sha256: sha,
        },
    )
    .await
    .unwrap();
    ingest::hand_on(state.clone(), id, job);
    for _ in 0..3000 {
        let f = kstore::get_file(&state.knowledge.pool, fid)
            .await
            .unwrap()
            .unwrap();
        if f.status == "ready" {
            cleanup(&state);
            return;
        }
        assert_ne!(f.status, "failed", "{:?}", f.error);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the late file stayed pending with no job running");
}

/// Many uploads in quick succession, each possibly landing as a job ends:
/// every file is ready in the end and no job is left running.
#[tokio::test]
async fn a_burst_of_uploads_leaves_nothing_pending() {
    let mock = MockServer::start().await;
    mount(&mock).await;
    let state = setup(&mock).await;
    let gw = serve(state.clone()).await;
    let id = create(&gw, json!({"name": "Burst", "embed_alias": "embed-model"})).await;
    let n = 12;
    for i in 0..n {
        let body = format!("# File {i}\n\nContent number {i} about topic {i}.\n");
        let (status, v) = upload(&gw, id, &[(&format!("f{i}.md"), body.into_bytes())]).await;
        assert_eq!(status, 200, "{v}");
        tokio::time::sleep(Duration::from_millis(3 * i as u64)).await;
    }
    for _ in 0..3000 {
        let files = kstore::list_files(&state.knowledge.pool, id).await.unwrap();
        if files.len() == n && files.iter().all(|f| f.status == "ready") {
            cleanup(&state);
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let files = kstore::list_files(&state.knowledge.pool, id).await.unwrap();
    panic!(
        "not all ready: {:?}",
        files
            .iter()
            .map(|f| (&f.name, &f.status))
            .collect::<Vec<_>>()
    );
}
