//! `GET /api/ladder-rung-plan` over the real router, on GGUF files the tests
//! write (`gguf::synth`): the live answer is read into the document's
//! `RungPlan` and written back equal (`common::round_trips`), for a file that
//! states its trained context and one that does not (`null`, not absent), one
//! with MTP tensors, and a footprint with and without the KV term.

use lmgw_api_types::RungPlan;
use lmgw_core::config::Settings;
use lmgw_core::gguf::synth;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::Value;

use crate::common::{self, Gw};

fn rich(ctx: u32) -> synth::Header {
    let mut h = synth::Header::default();
    h.str("general.architecture", "qwen3")
        .str("general.name", "rich")
        .u32("qwen3.context_length", ctx)
        .u32("qwen3.block_count", 4)
        .u32("qwen3.embedding_length", 512)
        .u32("qwen3.attention.head_count", 8)
        .u32("qwen3.attention.head_count_kv", 2)
        .u32("qwen3.attention.key_length", 64)
        .u32("qwen3.attention.value_length", 64)
        .str("tokenizer.chat_template", "{{ messages }}");
    h
}

async fn gateway() -> (SharedState, Gw, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    rich(40960).write_to(&dir.path().join("rich.gguf"));
    synth::chat("qwen3", 32768).write_to(&dir.path().join("plain.gguf"));
    let mut mtp = synth::Header::default();
    mtp.str("general.architecture", "qwen3")
        .u32("qwen3.context_length", 8192)
        .u32("qwen3.block_count", 4)
        .tensor("blk.4.nextn.embed_tokens.weight");
    mtp.write_to(&dir.path().join("mtp.gguf"));
    let mut noctx = synth::Header::default();
    noctx
        .str("general.architecture", "qwen3")
        .u32("qwen3.block_count", 4);
    noctx.write_to(&dir.path().join("noctx.gguf"));

    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = Settings::default();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = common::serve(state.clone()).await;
    (state, gw, dir)
}

async fn plan(gw: &Gw, query: &str) -> Value {
    let r = gw
        .client()
        .get(format!("{gw}/api/ladder-rung-plan?{query}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{query}");
    r.json().await.unwrap()
}

#[tokio::test]
async fn the_rung_plan_is_the_documented_type() {
    let (_state, gw, _dir) = gateway().await;

    let live = plan(&gw, "gguf_path=rich.gguf&ctx_size=8192").await;
    let p: RungPlan = common::round_trips("rung plan", &live);
    assert_eq!(p.trained_context, Some(40960));
    assert!(!p.has_mtp_layers);
    assert_eq!(p.footprint.ctx_tokens, Some(8192), "{live}");
    assert!(
        p.footprint.kv_cache_bytes > 0 && p.footprint.weights_bytes > 0,
        "{live}"
    );
    assert_eq!(
        p.footprint.total_bytes,
        p.footprint.weights_bytes + p.footprint.kv_cache_bytes
    );

    // A smaller cache type is a smaller KV term, the same weights.
    let q = plan(
        &gw,
        "gguf_path=rich.gguf&ctx_size=8192&cache_type_k=q8_0&cache_type_v=q4_0",
    )
    .await;
    let q: RungPlan = common::round_trips("rung plan, quantized cache", &q);
    assert!(q.footprint.kv_cache_bytes < p.footprint.kv_cache_bytes);
    assert_eq!(q.footprint.weights_bytes, p.footprint.weights_bytes);

    // The tensors that mark MTP layers.
    let live = plan(&gw, "gguf_path=mtp.gguf&ctx_size=4096").await;
    let m: RungPlan = common::round_trips("rung plan, MTP", &live);
    assert!(m.has_mtp_layers);
    assert_eq!(m.trained_context, Some(8192));

    // A header that does not state a context: `null`, and a note on the
    // footprint saying why there is no KV term.
    let live = plan(&gw, "gguf_path=noctx.gguf&ctx_size=4096").await;
    let n: RungPlan = common::round_trips("rung plan, no trained context", &live);
    assert_eq!(n.trained_context, None);
    assert!(live["trained_context"].is_null(), "{live}");
    assert_eq!(n.footprint.ctx_tokens, Some(4096));
    assert_eq!(n.footprint.kv_cache_bytes, 0, "{live}");
    assert!(n.footprint.note.is_some(), "{live}");
    assert!(live["footprint"].get("note").is_some(), "{live}");
}

#[tokio::test]
async fn a_rung_that_cannot_be_planned_is_still_a_400_with_its_sentence() {
    let (_state, gw, _dir) = gateway().await;
    for (query, said) in [
        (
            "gguf_path=plain.gguf&ctx_size=0",
            "ctx_size must be a positive token count",
        ),
        ("gguf_path=missing.gguf&ctx_size=4096", "missing.gguf"),
        (
            "gguf_path=plain.gguf&ctx_size=4096&mmproj_path=..%2Fx.gguf",
            "plain path",
        ),
    ] {
        let r = gw
            .client()
            .get(format!("{gw}/api/ladder-rung-plan?{query}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{query}");
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["code"], "op_failed", "{body}");
        assert!(body["message"].as_str().unwrap().contains(said), "{body}");
    }
}
