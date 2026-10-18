//! The aux plan's decision branch (decision-models design §3.4), through
//! synthetic headers shaped after Appendix B: a non-causal Laya, a causal
//! Kev-4B with its `cls.output` head, a Clef with `decision.*` tensors and a
//! lev that reads label logits. Every planned value is the header's or a
//! stated choice, and its rationale says which.

use lmgw_core::config::Settings;
use lmgw_core::gguf::synth;
use lmgw_core::modelinfo;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

const LAYA: &str = "laya/Laya-Q8_0.gguf";
const LAYA_MMPROJ: &str = "laya/mmproj-Laya.gguf";
const KEV: &str = "kev/Kev-4B-Q4_K_M.gguf";
const KEV_MMPROJ: &str = "kev/mmproj-Kev-4B-Q8_0.gguf";
const CLEF: &str = "clef/Clef-Q4_K_M.gguf";
const LEV: &str = "lev/lev-Q4_K_M.gguf";

/// Laya: non-causal `modern-bert`, 8192 trained tokens, the `cls.*` head.
fn laya() -> synth::Header {
    let mut h = synth::Header::default();
    h.str("general.architecture", "modern-bert")
        .str("general.size_label", "421M")
        .u32("modern-bert.context_length", 8192)
        .u32("modern-bert.block_count", 28)
        .bool_flag("modern-bert.attention.causal", false)
        .str("modern-bert.decision.type", "laya")
        .u32("modern-bert.decision.max_head_tokens", 192)
        .str("tokenizer.chat_template.systemone", "{{ state }}")
        .tensor("cls.output.weight");
    h
}

/// A causal `qwen35` decision model with the attention shape the KV cost is
/// read from (every fourth layer full attention, 4 KV heads of 256) and what
/// its compute buffer is estimated from: Kev's width, a 1,000-token
/// vocabulary.
fn causal(decision_type: &str) -> synth::Header {
    let mut h = synth::chat("qwen35", 262_144);
    let tokens: Vec<String> = (0..1000).map(|i| format!("t{i}")).collect();
    let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();
    h.str("qwen35.decision.type", decision_type)
        .str("tokenizer.chat_template.systemone", "{{ state }}")
        .u32("qwen35.attention.head_count_kv", 4)
        .u32("qwen35.attention.key_length", 256)
        .u32("qwen35.attention.value_length", 256)
        .u32("qwen35.full_attention_interval", 4)
        .u32("qwen35.embedding_length", 2560)
        .u32("qwen35.feed_forward_length", 9216)
        .arr_str("tokenizer.ggml.tokens", &tokens);
    h
}

fn projector() -> synth::Header {
    let mut h = synth::embedding("clip", 0, 0);
    h.str("general.type", "mmproj")
        .str("clip.projector_type", "qwen3vl_merger");
    h
}

async fn fixture() -> (SharedState, tempfile::TempDir) {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.aux_router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let write = |rel: &str, h: &synth::Header| h.write_to(&dir.path().join(rel));
    write(LAYA, &laya());
    write(LAYA_MMPROJ, &projector());
    let mut kev = causal("kev");
    kev.tensor("cls.output.weight");
    write(KEV, &kev);
    write(KEV_MMPROJ, &projector());
    let mut clef = causal("clef");
    clef.tensor("decision.head.weight");
    write(CLEF, &clef);
    write(LEV, &causal("lev"));
    (state, dir)
}

async fn plan(state: &SharedState, rel: &str) -> Value {
    modelinfo::local_model_plan(state, rel, Some("aux"))
        .await
        .unwrap()
}

fn why<'a>(plan: &'a Value, field: &str) -> &'a str {
    plan["rationale"][field]
        .as_str()
        .unwrap_or_else(|| panic!("no rationale for {field}: {plan}"))
}

/// Non-causal (Laya): the whole prompt in one ubatch, so ubatch = batch = the
/// trained context, on the CPU; no pooling, no projector even with one beside
/// it, and no rerank warning.
#[tokio::test]
async fn a_non_causal_decision_model_plans_its_context_as_one_ubatch_on_the_cpu() {
    let (state, _dir) = fixture().await;
    let p = plan(&state, LAYA).await;
    let params = &p["params"];
    assert_eq!(params["kind"], "decision", "{p}");
    assert_eq!(params["ctx_size"], 8192);
    assert_eq!(params["ubatch_size"], 8192);
    assert_eq!(params["batch_size"], 8192);
    assert_eq!(params["backend"], "cpu");
    assert!(
        params["pooling"].is_null() && params["threads"].is_null(),
        "{p}"
    );
    assert!(params["mmproj_path"].is_null(), "{p}");

    assert!(why(&p, "kind").contains("non-causal"), "{p}");
    assert!(why(&p, "ctx_size").contains("does not truncate"), "{p}");
    let ubatch = why(&p, "ubatch_size");
    assert!(
        ubatch.contains("one ubatch") && ubatch.contains("512"),
        "{ubatch}"
    );
    assert!(
        ubatch.contains("RAM") && ubatch.contains("2.22 GB"),
        "the compute buffer on a CPU row: {ubatch}"
    );
    let cpu = why(&p, "backend");
    assert!(
        cpu.contains("421M") && cpu.contains("questions") && cpu.contains("60 s"),
        "the rationale names the size, the question count and the timeout: {cpu}"
    );
    assert!(
        cpu.contains("Only Laya") && cpu.contains("model's size"),
        "Laya's figures as Laya's: {cpu}"
    );
    assert!(
        cpu.contains("physical cores") || cpu.contains("logical CPUs"),
        "{cpu}"
    );
    let warnings = p["warnings"].to_string();
    assert!(!warnings.contains("rerank"), "{warnings}");
    assert!(
        warnings.contains(LAYA_MMPROJ),
        "the projector is named, not proposed: {warnings}"
    );
    assert!(
        p["next_step"].as_str().unwrap().contains("System One"),
        "{p}"
    );
}

/// Causal with a decision head (Kev's `cls.output`, Clef's `decision.*`):
/// 2048, lmgw's choice over the effective 512, on the GPU, with the KV cost
/// of the trained context in the ctx rationale and the sibling projector.
#[tokio::test]
async fn a_causal_decision_model_with_a_head_plans_2048_on_the_gpu() {
    let (state, _dir) = fixture().await;
    for rel in [KEV, CLEF] {
        let p = plan(&state, rel).await;
        let params = &p["params"];
        assert_eq!(params["kind"], "decision", "{p}");
        assert_eq!(params["ctx_size"], 262_144);
        assert_eq!(params["ubatch_size"], 2048, "{rel}: {p}");
        assert_eq!(params["batch_size"], 2048);
        assert!(params["backend"].is_null(), "on the GPU: {p}");
        assert!(why(&p, "kind").contains("decision head"), "{p}");
        let ubatch = why(&p, "ubatch_size");
        assert!(
            ubatch.contains("lmgw's choice") && ubatch.contains("512"),
            "{ubatch}"
        );
        // Owner item b: the ledger charges the buffer, so the rationale gives
        // its estimate for this header (a 1 GiB mask at 262144 tokens, and
        // the layers' 296 MiB, which outgrow this small vocabulary's logits)
        // beside Kev's measured figures.
        assert!(
            ubatch.contains("the VRAM ledger charges it")
                && ubatch.contains("about 1.3 GiB for this model"),
            "{ubatch}"
        );
        assert!(ubatch.contains("3.0 GiB at its trained 262144"), "{ubatch}");
        assert!(!ubatch.contains("headroom"), "{ubatch}");
        let ctx = why(&p, "ctx_size");
        assert!(ctx.contains("KV cache") && ctx.contains("1.0 GiB"), "{ctx}");
    }
    let kev = plan(&state, KEV).await;
    assert_eq!(kev["params"]["mmproj_path"], KEV_MMPROJ, "{kev}");
}

/// A header the compute estimate cannot read (no width, no vocabulary) says
/// so instead of a number, and the plan stands.
#[tokio::test]
async fn a_compute_buffer_the_header_cannot_size_is_said_so() {
    let (state, dir) = fixture().await;
    let mut kev = synth::chat("qwen35", 262_144);
    kev.str("qwen35.decision.type", "kev")
        .str("tokenizer.chat_template.systemone", "{{ state }}")
        .tensor("cls.output.weight");
    kev.write_to(&dir.path().join("thin/kev.gguf"));
    let p = plan(&state, "thin/kev.gguf").await;
    assert_eq!(p["params"]["ubatch_size"], 2048, "{p}");
    let ubatch = why(&p, "ubatch_size");
    assert!(
        ubatch.contains("not estimated here") && ubatch.contains("embedding_length"),
        "{ubatch}"
    );
}

/// Causal without a head (lev): llama.cpp's own batch defaults, no ubatch or
/// batch planned, and the kind's rationale says why.
#[tokio::test]
async fn a_label_logit_decision_model_keeps_llama_cpps_batch_defaults() {
    let (state, _dir) = fixture().await;
    let p = plan(&state, LEV).await;
    let params = &p["params"];
    assert_eq!(params["kind"], "decision", "{p}");
    assert_eq!(params["ctx_size"], 262_144);
    assert!(
        params["ubatch_size"].is_null() && params["batch_size"].is_null(),
        "{p}"
    );
    assert!(params["backend"].is_null(), "{p}");
    let kind = why(&p, "kind");
    assert!(
        kind.contains("label logits") && kind.contains("n_batch 2048"),
        "{kind}"
    );
}

/// A configured row is compared on the planned fields, the ubatch and batch
/// among them, but never on placement: a Laya kept on the GPU is the
/// owner's choice, not a difference.
#[tokio::test]
async fn a_configured_decision_row_is_compared_without_its_placement() {
    let (state, _dir) = fixture().await;
    lmgw_core::ops::aux_model_set(
        &state,
        serde_json::from_value(json!({"action": "create", "model_id": "laya",
                                      "gguf_path": LAYA, "kind": "decision",
                                      "ctx_size": 8192}))
        .unwrap(),
    )
    .await
    .unwrap();
    let p = plan(&state, LAYA).await;
    let row = &p["configured_as"][0];
    assert_eq!(row["in_sync"], false, "{p}");
    assert_eq!(changes(&p, "laya"), ["batch_size", "ubatch_size"], "{p}");
    assert!(row["apply"].get("backend").is_none(), "{p}");
}

async fn create(state: &SharedState, row: Value) {
    lmgw_core::ops::aux_model_set(state, serde_json::from_value(row).unwrap())
        .await
        .unwrap();
}

/// The fields of the configured row `model_id` that differ from the plan,
/// read through the api-types `ConfiguredAs` the aux editor fills from, which
/// must hold every entry whole.
fn changes(plan: &Value, model_id: &str) -> Vec<String> {
    let live = &plan["configured_as"];
    let typed: Vec<lmgw_api_types::ConfiguredAs> =
        serde_json::from_value(live.clone()).unwrap_or_else(|e| panic!("{e}: {live}"));
    assert_eq!(
        &serde_json::to_value(&typed).unwrap(),
        live,
        "ConfiguredAs drops or invents a field"
    );
    let row = typed
        .into_iter()
        .find(|r| r.model_id == model_id)
        .unwrap_or_else(|| panic!("{model_id} not configured: {plan}"));
    row.change.into_iter().map(|c| c.field).collect()
}

/// The tuning a causal plan's own rationale asks for is no difference: a
/// ctx_size below the trained context (its KV cost) and a ubatch and batch
/// above 2048 (a whole-prompt type). Taken the other way, or on a
/// non-causal row whose ubatch is its whole prompt, it is one.
#[tokio::test]
async fn a_re_plan_keeps_the_tuning_its_rationale_asks_for() {
    let (state, _dir) = fixture().await;
    let kev = |id: &str, ctx: i64, ubatch: i64| {
        json!({"action": "create", "model_id": id, "gguf_path": KEV, "kind": "decision",
               "ctx_size": ctx, "ubatch_size": ubatch, "batch_size": ubatch,
               "mmproj_path": KEV_MMPROJ})
    };
    create(&state, kev("kev-tuned", 65_536, 8192)).await;
    create(&state, kev("kev-4096", 4096, 4096)).await;
    create(&state, kev("kev-short", 65_536, 1024)).await;
    create(
        &state,
        json!({"action": "create", "model_id": "laya-short", "gguf_path": LAYA,
               "kind": "decision", "ctx_size": 4096, "ubatch_size": 4096, "batch_size": 4096}),
    )
    .await;

    let p = plan(&state, KEV).await;
    assert!(changes(&p, "kev-tuned").is_empty(), "{p}");
    assert!(changes(&p, "kev-4096").is_empty(), "{p}");
    assert_eq!(
        changes(&p, "kev-short"),
        ["batch_size", "ubatch_size"],
        "{p}"
    );
    let p = plan(&state, LAYA).await;
    assert_eq!(
        changes(&p, "laya-short"),
        ["batch_size", "ctx_size", "ubatch_size"],
        "{p}"
    );
}

/// A header without a context length: nothing the context decides is
/// planned, a non-causal one says what that costs and still runs on the CPU,
/// and a causal one with a head keeps its 2048.
#[tokio::test]
async fn a_decision_header_without_a_context_length_plans_what_the_shape_decides() {
    let (state, dir) = fixture().await;
    let mut laya = synth::Header::default();
    laya.str("general.architecture", "modern-bert")
        .bool_flag("modern-bert.attention.causal", false)
        .str("modern-bert.decision.type", "laya")
        .str("tokenizer.chat_template.systemone", "{{ state }}")
        .tensor("cls.output.weight");
    laya.write_to(&dir.path().join("bare/laya.gguf"));
    let mut kev = synth::Header::default();
    kev.str("general.architecture", "qwen35")
        .str("qwen35.decision.type", "kev")
        .str("tokenizer.chat_template.systemone", "{{ state }}")
        .tensor("cls.output.weight");
    kev.write_to(&dir.path().join("bare/kev.gguf"));

    let p = plan(&state, "bare/laya.gguf").await;
    let params = &p["params"];
    for field in ["ctx_size", "ubatch_size", "batch_size"] {
        assert!(params[field].is_null(), "{field}: {p}");
    }
    assert_eq!(params["backend"], "cpu", "{p}");
    let warnings = p["warnings"].to_string();
    assert!(
        warnings.contains("no context length") && warnings.contains("over 512 tokens"),
        "{warnings}"
    );

    let p = plan(&state, "bare/kev.gguf").await;
    let params = &p["params"];
    assert!(params["ctx_size"].is_null(), "{p}");
    assert_eq!(params["ubatch_size"], 2048, "{p}");
    assert_eq!(params["batch_size"], 2048, "{p}");
    assert!(
        p["warnings"].to_string().contains("no context length"),
        "{p}"
    );
}
