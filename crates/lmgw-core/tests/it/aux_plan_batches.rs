//! The aux plan's batch sizes and its fit on the card (decision-models
//! design §7, owner items a and b, finding 3): an embedder or reranker whose
//! input llama-server cannot split gets its context as ubatch and batch, a
//! non-causal projector gets the chat plan's floor, and a GPU plan that
//! cannot fit the card says so — in the chat plan too — without lowering
//! the context.

use std::sync::Arc;

use lmgw_core::config::Settings;
use lmgw_core::gguf::synth;
use lmgw_core::modelinfo;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

const GEMMA: &str = "ggml-org/embeddinggemma-300M-Q8_0.gguf";
const QWEN: &str = "qwen/Qwen3-Embedding-0.6B-Q8_0.gguf";
const QWEN_RERANK: &str = "qwen/Qwen3-Reranker-0.6B-Q8_0.gguf";
const BERT_RERANK: &str = "bge/bge-reranker-Q8_0.gguf";
const VISION: &str = "vision/vision-embedder.gguf";
const VISION_MMPROJ: &str = "vision/mmproj-vision.gguf";
const LONG: &str = "long/long-embedder.gguf";
const LAYA: &str = "laya/Laya-Q8_0.gguf";
const CHAT: &str = "chat/long-chat.gguf";

/// embeddinggemma's header as converted: mean pooling, a sliding window and
/// no `attention.causal` (llama.cpp sets it for the architecture in code).
fn gemma() -> synth::Header {
    let mut h = synth::embedding("gemma-embedding", 1, 2048);
    h.u32("gemma-embedding.embedding_length", 768)
        .u32("gemma-embedding.feed_forward_length", 1152)
        .u32("gemma-embedding.attention.head_count", 3)
        .u32("gemma-embedding.attention.head_count_kv", 1)
        .u32("gemma-embedding.attention.key_length", 256)
        .u32("gemma-embedding.attention.value_length", 256)
        .u32("gemma-embedding.attention.sliding_window", 512);
    h
}

/// A causal header with a KV shape: 4 layers × 8 KV heads × (128 + 128),
/// 16 KiB per token at f16.
fn causal_kv(h: &mut synth::Header, arch: &str) {
    h.u32(&format!("{arch}.attention.head_count_kv"), 8)
        .u32(&format!("{arch}.attention.key_length"), 128)
        .u32(&format!("{arch}.attention.value_length"), 128);
}

async fn fixture() -> (SharedState, tempfile::TempDir) {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.aux_router.models_dir = dir.path().join("aux").display().to_string();
    settings.router.models_dir = dir.path().join("chat").display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let aux = |rel: &str, h: &synth::Header| h.write_to(&dir.path().join("aux").join(rel));

    aux(GEMMA, &gemma());
    aux(QWEN, &synth::embedding("qwen3", 3, 32768));
    aux(QWEN_RERANK, &synth::embedding("qwen3", 4, 40960));
    let mut bert = synth::embedding("bert", 4, 8192);
    bert.bool_flag("bert.attention.causal", false)
        .tensor("cls.output.weight");
    aux(BERT_RERANK, &bert);
    aux(VISION, &synth::embedding("qwen3vl", 3, 32768));
    let mut mmproj = synth::embedding("clip", 0, 0);
    mmproj
        .str("general.type", "mmproj")
        .str("clip.projector_type", "gemma4uv");
    aux(VISION_MMPROJ, &mmproj);
    let mut long = synth::embedding("qwen3", 3, 262_144);
    causal_kv(&mut long, "qwen3");
    aux(LONG, &long);
    let mut laya = synth::embedding("modern-bert", 0, 262_144);
    laya.bool_flag("modern-bert.attention.causal", false)
        .str("modern-bert.decision.type", "laya")
        .str("tokenizer.chat_template.systemone", "{{ state }}")
        .tensor("cls.output.weight");
    aux(LAYA, &laya);

    let mut chat = synth::chat("qwen3", 262_144);
    causal_kv(&mut chat, "qwen3");
    chat.write_to(&dir.path().join("chat").join(CHAT));
    (state, dir)
}

async fn plan(state: &SharedState, rel: &str, target: &str) -> Value {
    modelinfo::local_model_plan(state, rel, Some(target))
        .await
        .unwrap()
}

fn why<'a>(plan: &'a Value, field: &str) -> &'a str {
    plan["rationale"][field]
        .as_str()
        .unwrap_or_else(|| panic!("no rationale for {field}: {plan}"))
}

/// A declared budget on a box without GPU telemetry: what admission plans
/// against, and so what the plan's fit is checked against.
async fn budget(state: &SharedState, mib: u64) {
    state
        .vram
        .set_probe(Arc::new(lmgw_core::vram::nvml::NoTelemetry("none".into())));
    let mut s = state.snapshot().settings.clone();
    s.vram.budget_mb = mib;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

/// Owner item a: embeddinggemma's mean pooling reads every token, so its
/// whole input has to fit one ubatch — planned at its context, with the
/// compute buffer it costs (67 MiB, the measured figure) named.
#[tokio::test]
async fn an_encoder_that_cannot_split_an_input_plans_its_context_as_ubatch() {
    let (state, _dir) = fixture().await;
    let p = plan(&state, GEMMA, "aux").await;
    let params = &p["params"];
    assert_eq!(params["kind"], "embed", "{p}");
    assert_eq!(params["ctx_size"], 2048);
    assert_eq!(params["ubatch_size"], 2048, "{p}");
    assert_eq!(params["batch_size"], 2048, "{p}");
    let ub = why(&p, "ubatch_size");
    assert!(
        ub.contains("'mean'") && ub.contains("too large to process"),
        "{ub}"
    );
    assert!(
        ub.contains("about 67.0 MiB") && ub.contains("ledger"),
        "{ub}"
    );
    assert!(why(&p, "ctx_size").contains("ceiling per input"), "{p}");

    // A non-causal reranker (the BERT family) too.
    let p = plan(&state, BERT_RERANK, "aux").await;
    assert_eq!(p["params"]["kind"], "rerank", "{p}");
    assert_eq!(p["params"]["ubatch_size"], 8192, "{p}");
    assert!(why(&p, "ubatch_size").contains("non-causal"), "{p}");
}

/// What llama-server splits keeps llama.cpp's defaults: a last-token
/// embedder and a causal reranker (both measured at 3,500 tokens on the
/// default ubatch).
#[tokio::test]
async fn a_last_token_embedder_and_a_causal_reranker_keep_the_defaults() {
    let (state, _dir) = fixture().await;
    for (rel, kind) in [(QWEN, "embed"), (QWEN_RERANK, "rerank")] {
        let p = plan(&state, rel, "aux").await;
        assert_eq!(p["params"]["kind"], kind, "{p}");
        assert!(
            p["params"]["ubatch_size"].is_null() && p["params"]["batch_size"].is_null(),
            "{rel}: {p}"
        );
        assert!(
            why(&p, "ctx_size").contains("splits"),
            "{rel}: {}",
            why(&p, "ctx_size")
        );
    }
}

/// A configured row is offered the fix: its ubatch and batch differ from
/// the plan, and the apply object carries them. Placement stays the
/// owner's: a CPU row is no difference.
#[tokio::test]
async fn a_configured_encoder_row_is_offered_the_ubatch_and_batch() {
    let (state, _dir) = fixture().await;
    lmgw_core::ops::aux_model_set(
        &state,
        serde_json::from_value(json!({"action": "create", "model_id": "gemma",
                                      "gguf_path": GEMMA, "kind": "embed",
                                      "pooling": "mean", "ctx_size": 2048,
                                      "backend": "cpu"}))
        .unwrap(),
    )
    .await
    .unwrap();
    let p = plan(&state, GEMMA, "aux").await;
    let row = &p["configured_as"][0];
    assert_eq!(row["in_sync"], false, "{p}");
    let mut fields: Vec<&str> = row["change"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["field"].as_str().unwrap())
        .collect();
    fields.sort();
    assert_eq!(fields, ["batch_size", "ubatch_size"], "{p}");
    assert_eq!(row["apply"]["ubatch_size"], 2048, "{p}");
    assert_eq!(row["apply"]["batch_size"], 2048, "{p}");
    assert!(row["apply"].get("backend").is_none(), "{p}");
}

/// Review finding 1: a causal header that declares no pooling plans no
/// ubatch (the row's pooling decides, and the warning says which needs
/// one), and a configured row that splits by its own pooling is not offered
/// the context as its ubatch, which would charge its logits at that size.
#[tokio::test]
async fn a_row_that_splits_by_its_own_pooling_is_not_offered_a_ubatch() {
    let (state, dir) = fixture().await;
    const BARE: &str = "bare/no-pooling.gguf";
    synth::chat("qwen3", 40960).write_to(&dir.path().join("aux").join(BARE));
    let p = plan(&state, BARE, "aux").await;
    assert!(p["params"]["ubatch_size"].is_null(), "{p}");
    assert!(p["params"]["batch_size"].is_null(), "{p}");
    let warnings = p["warnings"].to_string();
    assert!(
        warnings.contains("no pooling_type") && warnings.contains("ctx_size (40960)"),
        "{warnings}"
    );
    assert!(why(&p, "ctx_size").contains("'last' pooling"), "{p}");

    // embeddinggemma's header says mean; a row set to last splits by its
    // own pooling, so only the pooling is a difference.
    lmgw_core::ops::aux_model_set(
        &state,
        serde_json::from_value(json!({"action": "create", "model_id": "gemma-last",
                                      "gguf_path": GEMMA, "kind": "embed",
                                      "pooling": "last", "ctx_size": 2048}))
        .unwrap(),
    )
    .await
    .unwrap();
    let p = plan(&state, GEMMA, "aux").await;
    assert_eq!(p["params"]["ubatch_size"], 2048, "the plan itself: {p}");
    let row = &p["configured_as"][0];
    let fields: Vec<&str> = row["change"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["field"].as_str().unwrap())
        .collect();
    assert_eq!(fields, ["pooling"], "{p}");
    assert!(row["apply"].get("ubatch_size").is_none(), "{p}");
}

/// A projector whose images llama.cpp decodes non-causally gets the chat
/// plan's floor on an aux row as well (Gemma 4's measured 1280), since a
/// last-token embedder otherwise keeps the default 512.
#[tokio::test]
async fn a_non_causal_projector_gets_the_chat_plans_ubatch_floor() {
    let (state, _dir) = fixture().await;
    let p = plan(&state, VISION, "aux").await;
    assert_eq!(p["params"]["mmproj_path"], VISION_MMPROJ, "{p}");
    assert_eq!(p["params"]["ubatch_size"], 1280, "{p}");
    assert!(
        p["params"]["batch_size"].is_null(),
        "the default batch covers it: {p}"
    );
    let ub = why(&p, "ubatch_size");
    assert!(
        ub.contains("gemma4uv") && ub.contains("GGML_ASSERT"),
        "{ub}"
    );
}

/// Finding 3: a GPU plan past the card names its estimate, the capacity and
/// the largest context that fits, and keeps the trained context; a CPU plan
/// says nothing, and nor does a plan when nothing measures the card.
#[tokio::test]
async fn a_plan_past_the_card_names_the_largest_context_that_fits() {
    let (state, _dir) = fixture().await;
    let quiet = plan(&state, LONG, "aux").await;
    assert!(
        !quiet["warnings"].to_string().contains("largest ctx_size"),
        "no telemetry, no budget: nothing to check against: {quiet}"
    );

    budget(&state, 2048).await;
    let p = plan(&state, LONG, "aux").await;
    assert_eq!(
        p["params"]["ctx_size"], 262_144,
        "the plan keeps the trained context: {p}"
    );
    let warnings = p["warnings"].to_string();
    assert!(
        warnings.contains("ctx_size 262144") && warnings.contains("vram.budget_mb (2.0 GiB)"),
        "{warnings}"
    );
    assert!(warnings.contains("KV cache 4.0 GiB"), "{warnings}");
    // 2 GiB less the 1 GiB headroom, less the weights, at 16 KiB a token.
    let weights = std::fs::metadata(
        std::path::Path::new(&state.snapshot().settings.aux_router.models_dir).join(LONG),
    )
    .unwrap()
    .len();
    let fits = ((1u64 << 30) - weights) / 16_384;
    assert!(
        warnings.contains(&format!("largest ctx_size that fits is {fits} ")),
        "{fits}: {warnings}"
    );
    assert!(warnings.contains("backend=cpu"), "{warnings}");

    let laya = plan(&state, LAYA, "aux").await;
    assert_eq!(laya["params"]["backend"], "cpu", "{laya}");
    assert!(
        !laya["warnings"].to_string().contains("largest ctx_size"),
        "a CPU plan takes no VRAM: {laya}"
    );

    // The chat plan runs the same check on its own footprint (q8_0 KV: 2 GiB
    // at the trained context).
    let p = plan(&state, CHAT, "chat").await;
    assert_eq!(p["params"]["ctx_size"], 262_144, "{p}");
    let warnings = p["warnings"].to_string();
    assert!(
        warnings.contains("largest ctx_size that fits") && warnings.contains("n_gpu_layers 0"),
        "{warnings}"
    );
}
