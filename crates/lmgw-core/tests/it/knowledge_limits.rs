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
/// (lmgw passes no batch for an aux row) unless the row's extra args raise
/// it; a cloud alias takes the catalog's context.
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
    state.reload_snapshot().await.unwrap();
    let snap = state.snapshot();

    let l = limit::input_limit(&state, &snap.aux_public_name("emb-default")).await;
    assert_eq!(l.tokens, Some(512), "{l:?}");
    assert!(l.source.contains("ubatch default of 512"), "{}", l.source);
    assert_eq!(
        l.tokenizer, "model",
        "a local model is counted by /tokenize"
    );

    let l = limit::input_limit(&state, &snap.aux_public_name("emb-raised")).await;
    assert_eq!(l.tokens, Some(2048), "{l:?}");
    assert!(l.source.contains("-ub 2048"), "{}", l.source);

    let l = limit::input_limit(&state, "embed-model").await;
    assert_eq!(l.tokens, Some(2048), "the mock catalog's context: {l:?}");
    assert!(l.source.contains("catalog"), "{}", l.source);
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
