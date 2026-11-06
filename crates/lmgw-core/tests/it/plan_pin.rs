//! Today's disk plans, pinned (find-a-model design WP2): the planner's own
//! disk reads moved into `PlanFiles`, and a plan made from that one input
//! must be the plan made before the move, byte for byte. Each fixture under
//! `tests/fixtures/plan_pin/` is the whole answer `local_model_plan` gave
//! for synthetic GGUFs before the move: a chat model with two projectors
//! and a drafter beside it, a split chat model, and an embedder with a
//! projector, each planned against a declared budget its trained context
//! doesn't fit, so the fit's search is in the warnings; the chat model
//! without a card and on a card it fits; a split file with its second part
//! missing; an embedder found in the chat dir, alone and with the same path
//! in the aux dir; and a non-causal decision model the plan puts on the CPU
//! with its ubatch tied to the context. All were recorded from the code
//! before WP2.
//!
//! The move added two keys to the answer (`source` and `fit`); every other
//! key is compared whole, with the fixture's temporary dir read as `<dir>`
//! and the CPU's rationale up to the cores it names, which are this
//! machine's. Every pinned plan's `fit` agrees with its warning (D136).
//! `LMGW_PLAN_PIN_RECORD=1` writes the fixtures again, which only a
//! deliberate change of the plan may do.

use std::path::Path;
use std::sync::Arc;

use lmgw_core::config::Settings;
use lmgw_core::gguf::synth;
use lmgw_core::modelinfo;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::Value;

const MIB: u64 = 1024 * 1024;

pub const CHAT: &str = "demo/demo-chat-Q4_K_M.gguf";
pub const CHAT_F16: &str = "demo/mmproj-demo-F16.gguf";
pub const CHAT_F32: &str = "demo/mmproj-demo-F32.gguf";
pub const DRAFTER: &str = "demo/demo-chat-dflash-drafter.gguf";
pub const SPLIT1: &str = "split/demo-chat-BF16-00001-of-00002.gguf";
pub const SPLIT2: &str = "split/demo-chat-BF16-00002-of-00002.gguf";
pub const EMBED: &str = "embed/demo-embedder-Q8_0.gguf";
pub const EMBED_PROJ: &str = "embed/mmproj-demo-embedder-F16.gguf";
/// The first part of a split set whose second part isn't there.
pub const LONELY: &str = "lonely/demo-chat-BF16-00001-of-00002.gguf";
/// An embedder in the chat class's dir, and one at the same path in both.
pub const EMBED_IN_CHAT: &str = "stray/demo-embedder-Q8_0.gguf";
pub const EMBED_IN_BOTH: &str = "both/demo-embedder-Q8_0.gguf";
/// A non-causal decision model, which the plan puts on the CPU.
pub const DECIDER: &str = "decide/demo-decider-Q8_0.gguf";

/// A causal chat header with a KV shape and `layers` of `layer_bytes` each.
pub fn chat_header(ctx: u32, layers: u32, layer_bytes: u64) -> synth::Header {
    let mut h = synth::chat("qwen3", ctx);
    h.u32("qwen3.block_count", layers)
        .u32("qwen3.embedding_length", 1024)
        .u32("qwen3.feed_forward_length", 2048)
        .u32("qwen3.attention.head_count", 8)
        .u32("qwen3.attention.head_count_kv", 8)
        .u32("qwen3.attention.key_length", 128)
        .u32("qwen3.attention.value_length", 128);
    h.tensor_shaped("token_embd.weight", &[1024, 4096], 4 * MIB);
    for i in 0..layers {
        h.tensor_shaped(
            &format!("blk.{i}.ffn_up.weight"),
            &[1024, 4096],
            layer_bytes,
        );
    }
    h
}

/// A vision projector for a 1024-wide text model, `bytes` of tensor data.
pub fn projector(bytes: u64) -> synth::Header {
    let mut h = synth::embedding("clip", 0, 0);
    h.str("general.type", "mmproj")
        .str("clip.projector_type", "gemma4uv")
        .u32("clip.vision.projection_dim", 1024);
    h.tensor_shaped("v.blk.0.attn_q.weight", &[1024, 1024], bytes);
    h
}

/// A dflash drafter stating its draft block.
pub fn drafter() -> synth::Header {
    let mut h = synth::chat("dflash", 4096);
    h.u32("dflash.block_size", 8)
        .u32("dflash.block_count", 1)
        .u32("dflash.embedding_length", 1024)
        .u32("dflash.attention.head_count", 8)
        .u32("dflash.attention.head_count_kv", 8)
        .u32("dflash.attention.key_length", 128)
        .u32("dflash.attention.value_length", 128);
    h.tensor_shaped("blk.0.ffn_up.weight", &[1024, 1024], MIB);
    h
}

/// A mean-pooling embedder whose input can't be split, with a KV shape.
pub fn embedder() -> synth::Header {
    let mut h = synth::embedding("gemma-embedding", 1, 32_768);
    h.u32("gemma-embedding.embedding_length", 1024)
        .u32("gemma-embedding.feed_forward_length", 2048)
        .u32("gemma-embedding.attention.head_count", 8)
        .u32("gemma-embedding.attention.head_count_kv", 8)
        .u32("gemma-embedding.attention.key_length", 128)
        .u32("gemma-embedding.attention.value_length", 128);
    h.tensor_shaped("blk.0.ffn_up.weight", &[1024, 4096], 6 * MIB);
    h
}

/// The second part of a split set: three keys and its tensors.
pub fn split_part2() -> synth::Header {
    let mut h = synth::Header::default();
    h.u64("split.no", 1)
        .u64("split.count", 2)
        .u64("split.tensors.count", 2);
    h.tensor_shaped("blk.2.ffn_up.weight", &[1024, 4096], 8 * MIB);
    h.tensor_shaped("blk.3.ffn_up.weight", &[1024, 4096], 8 * MIB);
    h
}

/// The first part of that set.
pub fn split_part1() -> synth::Header {
    let mut h = synth::chat("qwen3", 65_536);
    h.u32("qwen3.block_count", 4)
        .u32("qwen3.embedding_length", 1024)
        .u32("qwen3.feed_forward_length", 2048)
        .u32("qwen3.attention.head_count", 8)
        .u32("qwen3.attention.head_count_kv", 8)
        .u32("qwen3.attention.key_length", 128)
        .u32("qwen3.attention.value_length", 128)
        .u64("split.no", 0)
        .u64("split.count", 2)
        .u64("split.tensors.count", 5);
    h.tensor_shaped("token_embd.weight", &[1024, 4096], 8 * MIB);
    h.tensor_shaped("blk.0.ffn_up.weight", &[1024, 4096], 8 * MIB);
    h.tensor_shaped("blk.1.ffn_up.weight", &[1024, 4096], 8 * MIB);
    h
}

/// A non-causal decision model of 8192 trained tokens: the plan ties its
/// ubatch and batch to the context and proposes the CPU.
pub fn decider() -> synth::Header {
    let mut h = synth::Header::default();
    h.str("general.architecture", "modern-bert")
        .str("general.size_label", "421M")
        .u32("modern-bert.context_length", 8192)
        .u32("modern-bert.block_count", 2)
        .bool_flag("modern-bert.attention.causal", false)
        .str("modern-bert.decision.type", "laya")
        .u32("modern-bert.decision.max_head_tokens", 192)
        .str("tokenizer.chat_template.systemone", "{{ state }}");
    h.tensor_shaped("blk.0.ffn_up.weight", &[1024, 4096], 4 * MIB);
    h.tensor_shaped("cls.output.weight", &[1024, 4], 4096);
    h
}

/// Every file of the pinned plans, by its path under the class's dir.
pub fn files() -> Vec<(&'static str, &'static str, synth::Header)> {
    vec![
        ("chat", CHAT, chat_header(131_072, 4, 2 * MIB)),
        ("chat", CHAT_F16, projector(MIB)),
        ("chat", CHAT_F32, projector(2 * MIB)),
        ("chat", DRAFTER, drafter()),
        ("chat", SPLIT1, split_part1()),
        ("chat", SPLIT2, split_part2()),
        ("aux", EMBED, embedder()),
        ("aux", EMBED_PROJ, projector(MIB)),
        ("chat", LONELY, split_part1()),
        ("chat", EMBED_IN_CHAT, embedder()),
        ("chat", EMBED_IN_BOTH, embedder()),
        ("aux", EMBED_IN_BOTH, embedder()),
        ("aux", DECIDER, decider()),
    ]
}

/// A state whose class dirs hold [`files`], planning against a declared
/// budget of `budget_mib` with 32 MiB of headroom on a box without GPU
/// telemetry.
pub async fn fixture(budget_mib: u64) -> (SharedState, tempfile::TempDir) {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.router.models_dir = dir.path().join("chat").display().to_string();
    settings.aux_router.models_dir = dir.path().join("aux").display().to_string();
    settings.vram.budget_mb = budget_mib;
    settings.vram.headroom_mb = 32;
    store::save_settings(&state.db, &settings).await.unwrap();
    state
        .vram
        .set_probe(Arc::new(lmgw_core::vram::nvml::NoTelemetry("none".into())));
    state.reload_snapshot().await.unwrap();
    for (class, rel, h) in files() {
        h.write_to(&dir.path().join(class).join(rel));
    }
    (state, dir)
}

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/plan_pin")
        .join(format!("{name}.json"))
}

fn recording() -> bool {
    std::env::var_os("LMGW_PLAN_PIN_RECORD").is_some()
}

/// `plan` against the fixture `name`: every key the answer had before the
/// move compared whole, and only `source` and `fit` new. The fixture's
/// temporary dir `dir` reads as `<dir>`.
fn pinned(name: &str, plan: &Value, dir: &tempfile::TempDir) {
    let path = fixture_path(name);
    let text = plan
        .to_string()
        .replace(&dir.path().display().to_string(), "<dir>");
    let plan: Value = serde_json::from_str(&text).unwrap();
    let mut today = plan.as_object().unwrap().clone();
    // The CPU's rationale names this machine's cores, which the fixture
    // can't: it is compared up to them.
    if let Some(cpu) = today
        .get_mut("rationale")
        .and_then(|r| r.get_mut("backend"))
    {
        let text = cpu.as_str().unwrap_or_default();
        assert!(text.starts_with("the CPU, for "), "{text}");
        let up_to = text
            .find("the row runs this machine's")
            .unwrap_or(text.len());
        *cpu = Value::String(format!("{}<this machine's cores>", &text[..up_to]));
    }
    let added: Vec<String> = ["source", "fit"]
        .into_iter()
        .filter(|k| today.remove(*k).is_some())
        .map(str::to_string)
        .collect();
    if recording() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = serde_json::to_string_pretty(&Value::Object(today)).unwrap();
        std::fs::write(&path, text + "\n").unwrap();
        return;
    }
    let want: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap();
    assert_eq!(
        Value::Object(today),
        want,
        "{name}: the plan changed (new keys: {added:?})"
    );
}

#[tokio::test]
async fn a_chat_plan_with_its_companions_is_todays() {
    let (state, dir) = fixture(160).await;
    let plan = modelinfo::local_model_plan(&state, CHAT, Some("chat"))
        .await
        .unwrap();
    assert_eq!(plan["params"]["mmproj_path"], CHAT_F16, "{plan}");
    assert_eq!(plan["params"]["draft_gguf_path"], DRAFTER, "{plan}");
    assert!(
        plan["warnings"]
            .to_string()
            .contains("largest ctx_size that fits"),
        "{plan}"
    );
    pinned("chat", &plan, &dir);
}

#[tokio::test]
async fn a_split_chat_plan_is_todays() {
    let (state, dir) = fixture(64).await;
    let plan = modelinfo::local_model_plan(&state, SPLIT1, Some("chat"))
        .await
        .unwrap();
    pinned("split", &plan, &dir);
}

#[tokio::test]
async fn an_aux_plan_with_its_projector_is_todays() {
    let (state, dir) = fixture(64).await;
    let plan = modelinfo::local_model_plan(&state, EMBED, Some("aux"))
        .await
        .unwrap();
    assert_eq!(plan["params"]["mmproj_path"], EMBED_PROJ, "{plan}");
    assert_eq!(plan["params"]["ubatch_size"], 32_768, "{plan}");
    assert!(
        plan["warnings"]
            .to_string()
            .contains("largest ctx_size that fits"),
        "{plan}"
    );
    pinned("aux", &plan, &dir);
}

/// Without a card to measure against, the plan carries no fit warning, as
/// before.
#[tokio::test]
async fn a_plan_without_a_card_is_todays() {
    let (state, dir) = fixture(0).await;
    let plan = modelinfo::local_model_plan(&state, CHAT, Some("chat"))
        .await
        .unwrap();
    pinned("chat-no-card", &plan, &dir);
}

/// D136: the plan's `fit` on the whole card is the warning's own search. A
/// warning naming the largest context that fits is a fit of that context;
/// one that says no context fits is a fit of none; a plan without the
/// warning fits its whole context (or has no card to measure against, or
/// runs on the CPU). A split set that can't load as it is (a part missing)
/// is marked by its size alone instead, its bytes unknown. Not while
/// recording: the plan before the move had no `fit`.
fn fit_agrees(plan: &Value) {
    if recording() {
        return;
    }
    let fit = &plan["fit"]["whole_card"];
    let warnings = plan["warnings"].to_string();
    if warnings.contains("is one part of a GGUF split") {
        assert_eq!(fit["mark"], "unknown", "{fit}");
    } else if let Some(rest) = warnings.split("largest ctx_size that fits is ").nth(1) {
        let n: u64 = rest.split(' ').next().unwrap().parse().unwrap();
        assert_eq!(fit["mark"], "fits_shorter", "{fit}");
        assert_eq!(fit["ctx"], n, "{fit}");
    } else if warnings.contains("No ctx_size fits") {
        assert_eq!(fit["mark"], "does_not_fit", "{fit}");
    } else {
        let mark = fit["mark"].as_str().unwrap();
        assert!(matches!(mark, "fits" | "no_card"), "{fit}");
        if mark == "fits" {
            assert_eq!(fit["ctx"], plan["params"]["ctx_size"], "{fit}");
        }
    }
}

/// D136 over the plans pinned before WP2's fix round.
#[tokio::test]
async fn every_pinned_plans_fit_agrees_with_its_warning() {
    for (budget, rel, target) in [
        (160, CHAT, "chat"),
        (64, SPLIT1, "chat"),
        (64, EMBED, "aux"),
        (0, CHAT, "chat"),
    ] {
        let (state, _dir) = fixture(budget).await;
        let plan = modelinfo::local_model_plan(&state, rel, Some(target))
            .await
            .unwrap();
        fit_agrees(&plan);
    }
}

/// A split file whose second part isn't there: the plan says so, and
/// charges the part that is.
#[tokio::test]
async fn a_split_plan_with_a_part_missing_is_todays() {
    let (state, dir) = fixture(4096).await;
    let plan = modelinfo::local_model_plan(&state, LONELY, Some("chat"))
        .await
        .unwrap();
    assert!(
        plan["warnings"].to_string().contains("is one part of"),
        "{plan}"
    );
    fit_agrees(&plan);
    pinned("split-missing", &plan, &dir);
}

/// An embedder found in the chat class's dir: the aux plan, with the
/// location warning and not ready to apply; and the same with the same
/// path in the aux dir as well.
#[tokio::test]
async fn an_aux_file_in_the_chat_dir_is_todays() {
    let (state, dir) = fixture(4096).await;
    for (rel, name) in [
        (EMBED_IN_CHAT, "aux-in-chat"),
        (EMBED_IN_BOTH, "aux-in-both"),
    ] {
        let plan = modelinfo::local_model_plan(&state, rel, Some("chat"))
            .await
            .unwrap();
        assert_eq!(plan["class"], "aux", "{plan}");
        assert_eq!(plan["apply_ready"], false, "{plan}");
        fit_agrees(&plan);
        pinned(name, &plan, &dir);
    }
}

/// A non-causal decision model: its ubatch and batch tied to the context,
/// and the CPU proposed, where it fits with nothing on the card.
#[tokio::test]
async fn a_decision_plan_on_the_cpu_is_todays() {
    let (state, dir) = fixture(64).await;
    let plan = modelinfo::local_model_plan(&state, DECIDER, Some("aux"))
        .await
        .unwrap();
    assert_eq!(plan["params"]["backend"], "cpu", "{plan}");
    assert_eq!(plan["params"]["ubatch_size"], 8192, "{plan}");
    assert_eq!(plan["params"]["batch_size"], 8192, "{plan}");
    fit_agrees(&plan);
    if !recording() {
        assert_eq!(plan["fit"]["whole_card"]["mark"], "fits", "{plan}");
        assert_eq!(plan["fit"]["whole_card"]["uses_bytes"], 0, "{plan}");
    }
    pinned("decision", &plan, &dir);
}

/// A chat plan whose trained context fits the card: no fit warning.
#[tokio::test]
async fn a_plan_that_fits_is_todays() {
    let (state, dir) = fixture(4096).await;
    let plan = modelinfo::local_model_plan(&state, CHAT, Some("chat"))
        .await
        .unwrap();
    assert!(
        !plan["warnings"].to_string().contains("ctx_size that fits"),
        "{plan}"
    );
    fit_agrees(&plan);
    if !recording() {
        assert_eq!(plan["fit"]["whole_card"]["mark"], "fits", "{plan}");
    }
    pinned("fits", &plan, &dir);
}
