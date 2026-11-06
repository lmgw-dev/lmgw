//! A split GGUF (`…-00001-of-0000N.gguf`) is charged every part, not the
//! one its row names (find-a-model design review, finding F1): the chat
//! and aux footprints admission charges, the model editor's footprint
//! (`PlanCache::local`, what `/api/ladder-rung-plan` answers) and the
//! plan's fit on the card. A part that is not on disk is said in plain
//! words, never counted as 0 in silence; a single file is unchanged.

use std::path::Path;
use std::sync::Arc;

use lmgw_core::config::{AuxKind, AuxModel, LlamaParams, LocalModel, RouterSettings, Settings};
use lmgw_core::gguf::synth;
use lmgw_core::modelinfo;
use lmgw_core::state::AppState;
use lmgw_core::store;
use lmgw_core::vram::plan::PlanCache;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const SINGLE: &str = "org/single/m-Q4.gguf";
const PART1: &str = "org/split/m-BF16-00001-of-00003.gguf";
const PART2: &str = "org/split/m-BF16-00002-of-00003.gguf";
const PART3: &str = "org/split/m-BF16-00003-of-00003.gguf";

/// A small causal chat header with a KV shape.
fn chat_header() -> synth::Header {
    let mut h = synth::chat("qwen3", 4096);
    h.u32("qwen3.embedding_length", 256)
        .u32("qwen3.feed_forward_length", 512)
        .u32("qwen3.attention.head_count", 8)
        .u32("qwen3.attention.head_count_kv", 8)
        .u32("qwen3.attention.key_length", 128)
        .u32("qwen3.attention.value_length", 128);
    h
}

/// `h` at `rel` under `root`; its size on disk.
fn write(root: &Path, rel: &str, h: &synth::Header) -> u64 {
    let path = root.join(rel);
    h.write_to(&path);
    std::fs::metadata(&path).unwrap().len()
}

/// A later part of a split set: `len` bytes, sparse, so a test can lay out
/// gigabytes on a tmpfs without the RAM.
fn part(root: &Path, rel: &str, len: u64) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::File::create(&path).unwrap().set_len(len).unwrap();
}

fn chat_row(gguf_path: &str, n_gpu_layers: Option<i64>) -> LocalModel {
    LocalModel {
        id: 1,
        model_id: "m".into(),
        gguf_path: gguf_path.into(),
        params: LlamaParams {
            ctx_size: Some(4096),
            n_gpu_layers,
            ..Default::default()
        },
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

#[tokio::test]
async fn a_split_chat_model_is_charged_every_part_and_a_single_file_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().display().to_string();
    let single = write(dir.path(), SINGLE, &chat_header());
    let first = write(dir.path(), PART1, &chat_header());
    part(dir.path(), PART2, 3 * MIB);
    part(dir.path(), PART3, 5 * MIB);
    let plans = PlanCache::default();

    let one = plans.local(&models, &chat_row(SINGLE, None)).await;
    assert_eq!(one.weights_bytes, single, "{one:?}");

    let split = plans.local(&models, &chat_row(PART1, None)).await;
    assert_eq!(split.weights_bytes, first + 8 * MIB, "{split:?}");
    // Every part is there, so nothing more to say than of the single file.
    assert_eq!(split.note, one.note, "{split:?}");
    // The same header, so the same cache and buffers: only the weights grow.
    assert_eq!(split.kv_cache_bytes, one.kv_cache_bytes);
    assert_eq!(split.compute_bytes, one.compute_bytes);
    assert_eq!(split.total_bytes, one.total_bytes + 8 * MIB);
}

#[tokio::test]
async fn a_missing_part_is_said_first_and_only_the_parts_on_disk_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().display().to_string();
    let first = write(dir.path(), PART1, &chat_header());
    part(dir.path(), PART3, 5 * MIB);

    let fp = PlanCache::default()
        .local(&models, &chat_row(PART1, None))
        .await;
    assert_eq!(fp.weights_bytes, first + 5 * MIB, "{fp:?}");
    let note = fp.note.as_deref().unwrap_or_default();
    assert!(
        note.starts_with(&format!(
            "'{PART1}' is one part of a GGUF split into 3 files, and part 2 ('{PART2}') is \
             not in the models dir: llama-server cannot load the model until every part is \
             there"
        )),
        "{note}"
    );

    // On the CPU nothing is charged, and the missing part is still said.
    let cpu = PlanCache::default()
        .local(&models, &chat_row(PART1, Some(0)))
        .await;
    assert_eq!(cpu.total_bytes, 0, "{cpu:?}");
    let note = cpu.note.as_deref().unwrap_or_default();
    assert!(
        note.contains(&format!("part 2 ('{PART2}')")) && note.contains("CPU-only"),
        "{note}"
    );
}

#[tokio::test]
async fn a_split_aux_model_is_charged_every_part() {
    let dir = tempfile::tempdir().unwrap();
    let class = RouterSettings {
        models_dir: dir.path().display().to_string(),
        extra_run_args: vec!["--security-opt".into(), "label=disable".into()],
        ..RouterSettings::default_aux()
    };
    let row = |gguf_path: &str| AuxModel {
        id: 1,
        model_id: "embedder".into(),
        gguf_path: gguf_path.into(),
        kind: AuxKind::Embed,
        pooling: Some("last".into()),
        ctx_size: None,
        ubatch_size: None,
        batch_size: None,
        mmproj_path: None,
        backend: None,
        threads: None,
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        fallback: None,
    };
    let header = synth::embedding("qwen3", 3, 8192);
    let single = write(dir.path(), SINGLE, &header);
    let first = write(dir.path(), PART1, &header);
    part(dir.path(), PART2, 2 * MIB);
    part(dir.path(), PART3, 2 * MIB);

    let plans = PlanCache::default();
    let one = plans.aux(&class, &row(SINGLE)).await;
    assert_eq!(one.weights_bytes, single, "{one:?}");
    let split = plans.aux(&class, &row(PART1)).await;
    assert_eq!(split.weights_bytes, first + 4 * MIB, "{split:?}");

    std::fs::remove_file(dir.path().join(PART2)).unwrap();
    let short = PlanCache::default().aux(&class, &row(PART1)).await;
    assert_eq!(short.weights_bytes, first + 2 * MIB, "{short:?}");
    let note = short.note.as_deref().unwrap_or_default();
    assert!(
        note.starts_with(&format!(
            "'{PART1}' is one part of a GGUF split into 3 files"
        )),
        "{note}"
    );
}

/// The plan's fit on the card weighs the whole set: two 2 GiB parts behind
/// a header of a few KiB do not fit a 4 GiB budget with its 1 GiB headroom,
/// where the first part alone would. A missing part is the plan's warning.
#[tokio::test]
async fn the_plan_fits_the_whole_split_set_and_warns_of_a_missing_part() {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.router.models_dir = dir.path().display().to_string();
    settings.vram.budget_mb = 4096;
    store::save_settings(&state.db, &settings).await.unwrap();
    state
        .vram
        .set_probe(Arc::new(lmgw_core::vram::nvml::NoTelemetry("none".into())));
    state.reload_snapshot().await.unwrap();

    write(dir.path(), PART1, &chat_header());
    part(dir.path(), PART2, 2 * GIB);
    part(dir.path(), PART3, 2 * GIB);

    let plan = modelinfo::local_model_plan(&state, PART1, Some("chat"))
        .await
        .unwrap();
    let warnings = plan["warnings"].to_string();
    assert!(
        warnings.contains("No ctx_size fits") && warnings.contains("weights 4.0 GiB"),
        "{warnings}"
    );
    assert!(!warnings.contains("is one part of"), "{warnings}");

    std::fs::remove_file(dir.path().join(PART3)).unwrap();
    let plan = modelinfo::local_model_plan(&state, PART1, Some("chat"))
        .await
        .unwrap();
    let warnings = plan["warnings"].to_string();
    assert!(
        warnings.contains(&format!("part 3 ('{PART3}') is not in the models dir")),
        "{warnings}"
    );
}
