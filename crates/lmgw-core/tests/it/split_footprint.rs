//! A split GGUF and the single file of the same quant cost the card the
//! same (owner ruling 2026-11-04, find-a-model design WP8's split-GGUF
//! footprint): the plan's footprint, its fitted context and admission's
//! footprint agree for `Qwen/Qwen2.5-Coder-32B-Instruct-GGUF`'s `q4_0`,
//! `q4_k_m` and `q5_k_m`, each as one file and as three parts.
//!
//! The headers are the hub's, recorded by `scripts/split-set-record.py`
//! into `tests/fixtures/split_set/qwen2.5-coder-32b.zip`: every key and
//! every tensor descriptor as served, the tokenizer's strings emptied and
//! their bytes kept as padding, so each header ends where the hub's does.
//! Each is written to a sparse file of the real size, so a test lays out
//! 115 GiB of weights without the disk or the RAM. The tensor tables say
//! the two layouts are the same model: the same 771 tensors at the same
//! types and sizes, the parts' headers 224 bytes more than the single
//! file's; part 1 holds `token_embd.weight` and part 3 the untied
//! `output.weight`.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use lmgw_api_types::model_find::FileFit;
use lmgw_core::config::{LlamaParams, LocalModel, Settings};
use lmgw_core::modelinfo;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use lmgw_core::vram::plan::PlanCache;
use serde_json::Value;

const REPO: &str = "Qwen/Qwen2.5-Coder-32B-Instruct-GGUF";
/// A 24 GiB card's memory as an RTX 3090 reports it.
const CARD_MIB: u64 = 24_564;

fn single(quant: &str) -> String {
    format!("{REPO}/qwen2.5-coder-32b-instruct-{quant}.gguf")
}

fn part1(quant: &str) -> String {
    format!("{REPO}/qwen2.5-coder-32b-instruct-{quant}-00001-of-00003.gguf")
}

/// The recorded headers written under `root`, each file sparse at its real
/// size.
fn lay_out(root: &Path) {
    let zip_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/split_set/qwen2.5-coder-32b.zip");
    let mut zip = zip::ZipArchive::new(std::fs::File::open(zip_path).unwrap()).unwrap();
    let mut index = String::new();
    zip.by_name("index.json")
        .unwrap()
        .read_to_string(&mut index)
        .unwrap();
    let index: Value = serde_json::from_str(&index).unwrap();
    assert_eq!(index["repo"], REPO);
    let sizes: BTreeMap<String, u64> = serde_json::from_value(index["files"].clone()).unwrap();
    assert_eq!(sizes.len(), 12);
    for (file, size) in sizes {
        let mut header = Vec::new();
        zip.by_name(&file)
            .unwrap()
            .read_to_end(&mut header)
            .unwrap();
        let path = root.join(REPO).join(&file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &header).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(size)
            .unwrap();
    }
}

/// A state planning against that card as a declared budget,
/// with the default headroom, its chat models dir holding the recording.
async fn fixture() -> (SharedState, tempfile::TempDir) {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    lay_out(dir.path());
    let mut settings = Settings::default();
    settings.router.models_dir = dir.path().display().to_string();
    settings.vram.budget_mb = CARD_MIB;
    store::save_settings(&state.db, &settings).await.unwrap();
    state
        .vram
        .set_probe(Arc::new(lmgw_core::vram::nvml::NoTelemetry("none".into())));
    state.reload_snapshot().await.unwrap();
    (state, dir)
}

/// The plan of `rel` and its whole-card fit.
async fn plan(state: &SharedState, rel: &str) -> (Value, FileFit) {
    let plan = modelinfo::local_model_plan(state, rel, Some("chat"))
        .await
        .unwrap();
    let fit: FileFit = serde_json::from_value(plan["fit"]["whole_card"].clone()).unwrap();
    (plan, fit)
}

/// The row the plan of `plan` creates at the fit's context, as admission
/// reads it.
fn row(rel: &str, plan: &Value, ctx: u64) -> LocalModel {
    let mut params = plan["params"].clone();
    params["ctx_size"] = ctx.into();
    let params: LlamaParams = serde_json::from_value(params).unwrap();
    LocalModel {
        id: 1,
        model_id: "coder".into(),
        gguf_path: rel.into(),
        params,
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        fallback: None,
        capabilities_override: None,
        ladder: vec![],
    }
}

/// The split set and the single file of each quant: the same weights on
/// the card, the same cache and buffers, the same fitted context, and
/// admission charging the split row what the plan said.
#[tokio::test]
async fn a_split_set_costs_what_its_single_file_costs() {
    let (state, dir) = fixture().await;
    let models = dir.path().display().to_string();
    for quant in ["q4_0", "q4_k_m", "q5_k_m"] {
        let (one_plan, one) = plan(&state, &single(quant)).await;
        let (split_plan, split) = plan(&state, &part1(quant)).await;
        eprintln!(
            "{quant}: single {:?} ctx {:?} {:?} | split {:?} ctx {:?} {:?}",
            one.mark, one.ctx, one.terms, split.mark, split.ctx, split.terms
        );
        let but_path = |p: &Value| {
            let mut p = p["params"].as_object().unwrap().clone();
            p.remove("gguf_path");
            p
        };
        assert_eq!(but_path(&split_plan), but_path(&one_plan), "{quant}");
        assert_eq!(split.mark, one.mark, "{quant}");
        assert_eq!(split.ctx, one.ctx, "{quant}");
        assert_eq!(
            split.terms.kv_cache_bytes, one.terms.kv_cache_bytes,
            "{quant}"
        );
        assert_eq!(
            split.terms.compute_bytes, one.terms.compute_bytes,
            "{quant}"
        );
        // The weights differ by the parts' own headers alone: each part's
        // header, descriptors and alignment padding, less the single
        // file's, never an embedding table.
        let header = |rel: &str| {
            lmgw_core::gguf::summarize(&dir.path().join(rel))
                .unwrap()
                .header_bytes
        };
        let parts: u64 = (1..=3)
            .map(|n| {
                header(&format!(
                    "{REPO}/qwen2.5-coder-32b-instruct-{quant}-0000{n}-of-00003.gguf"
                ))
            })
            .sum();
        let extra = split.terms.weights_bytes - one.terms.weights_bytes;
        assert_eq!(extra, parts - header(&single(quant)), "{quant}");
        assert_eq!(split.parameters, one.parameters, "{quant}");
        assert_eq!(split.bits_per_weight, one.bits_per_weight, "{quant}");

        // Admission's footprint of the row the plan creates at the fitted
        // context is the plan's, for either layout.
        let ctx = one.ctx.unwrap_or(1);
        for (rel, plan, fit) in [
            (single(quant), &one_plan, &one),
            (part1(quant), &split_plan, &split),
        ] {
            let charged = PlanCache::default()
                .local(&models, &row(&rel, plan, ctx))
                .await;
            assert_eq!(charged.weights_bytes, fit.terms.weights_bytes, "{rel}");
            assert_eq!(charged.kv_cache_bytes, fit.terms.kv_cache_bytes, "{rel}");
            assert_eq!(charged.compute_bytes, fit.terms.compute_bytes, "{rel}");
            assert_eq!(charged.ctx_tokens, Some(ctx), "{rel}");
        }
    }
}
