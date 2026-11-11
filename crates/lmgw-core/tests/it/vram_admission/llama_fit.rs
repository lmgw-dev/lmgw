//! A llama-server row that llama.cpp's `--fit` would shrink starts with what
//! lmgw fitted it to, told as explicit values with `--fit off`; one that sets
//! its values is held to them as written; a partial `--n-gpu-layers` is
//! charged what lands on the card (`vram::partial::llama`, the owner's
//! ruling of 2026-11-11).

use super::*;
use lmgw_core::config::LlamaParams;
use lmgw_core::gguf::synth;
use lmgw_core::runtime::Class;

const MIB: u64 = 1024 * 1024;

/// A dense synthetic chat model: 8 layers of 4 MiB, an output head and
/// input embeddings of 2 MiB each, 2 KV heads of 64 (4 KiB of f16 cache a
/// token), trained on 32768 tokens — 128 MiB of cache at that context.
fn write_model(f: &Fixture, file: &str) {
    let mut h = synth::chat("llama", 32768);
    h.u32("llama.block_count", 8)
        .u32("llama.embedding_length", 256)
        .u32("llama.feed_forward_length", 512)
        .u32("llama.attention.head_count", 4)
        .u32("llama.attention.head_count_kv", 2)
        .u32("llama.vocab_size", 1024);
    for i in 0..8 {
        h.tensor_sized(&format!("blk.{i}.attn_q.weight"), MIB);
        h.tensor_sized(&format!("blk.{i}.ffn_up.weight"), 3 * MIB);
    }
    h.tensor_sized("output.weight", 2 * MIB);
    h.tensor_sized("token_embd.weight", 2 * MIB);
    h.write_to(&f._models_dir.path().join(file));
}

async fn add_row(f: &Fixture, model_id: &str, params: LlamaParams, args: &[&str]) {
    let file = format!("{model_id}.gguf");
    write_model(f, &file);
    f.world().size.insert(model_id.to_string(), 8 * MIB);
    store::insert_local_model(
        &f.state.db,
        &NewLocalModel {
            model_id: model_id.into(),
            gguf_path: file,
            params,
            args: args.iter().map(|a| a.to_string()).collect(),
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            fallback: None,
            capabilities_override: None,
            ladder: vec![],
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

async fn start(f: &Fixture, model_id: &str) -> Value {
    crate::common::container_wire(
        lmgw_core::ops::container(&f.state, None, Some(model_id), "start", false, None)
            .await
            .unwrap(),
    )
}

fn argv(f: &Fixture, model_id: &str) -> Vec<String> {
    f.world().argv.get(model_id).cloned().unwrap_or_default()
}

fn flag(argv: &[String], name: &str) -> Option<String> {
    let at = argv.iter().rposition(|a| a == name)?;
    argv.get(at + 1).cloned()
}

fn entry(f: &Fixture, model_id: &str) -> lmgw_core::runtime::registry::RuntimeView {
    f.state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.class == Class::Chat && v.model_id == model_id)
        .expect("it is up")
}

/// The card holds less than the trained context: the context comes down,
/// every layer stays, and llama-server is told so with `--fit off`. The
/// ledger charges what it was given, and the entry says it.
#[tokio::test]
async fn a_fit_row_starts_with_the_context_that_fits() {
    let f = fixture(96 * MIB, 8 * MIB, 8 * MIB, 4).await;
    add_row(&f, "fit-model", LlamaParams::default(), &[]).await;
    let out = start(&f, "fit-model").await;
    assert_eq!(out["ok"], true, "{out}");
    let a = argv(&f, "fit-model");
    assert_eq!(flag(&a, "--fit").as_deref(), Some("off"), "{a:?}");
    let ctx: u64 = flag(&a, "--ctx-size").unwrap().parse().unwrap();
    assert!(
        (4096..32768).contains(&ctx) && ctx.is_multiple_of(256),
        "{ctx}"
    );
    assert_eq!(flag(&a, "--n-gpu-layers"), None, "every layer on the card");
    let e = entry(&f, "fit-model");
    let g = e.grant.as_ref().expect("its grant");
    assert!(
        g.bytes <= 92 * MIB,
        "within the card less the headroom: {g:?}"
    );
    assert!(
        e.warnings
            .iter()
            .any(|w| w.starts_with("Context fitted to the GPU") && w.contains("--fit off")),
        "{:?}",
        e.warnings
    );
}

/// Not even every layer at the floor context fits: layers go off the card,
/// the most that fit, with the partial-fit warning and its numbers.
#[tokio::test]
async fn a_fit_row_starts_partially_with_a_warning() {
    // The floor context is 16 MiB of cache and 36 MiB of weights.
    let f = fixture(40 * MIB, 8 * MIB, 8 * MIB, 4).await;
    add_row(&f, "fit-model", LlamaParams::default(), &[]).await;
    let out = start(&f, "fit-model").await;
    assert_eq!(out["ok"], true, "{out}");
    let a = argv(&f, "fit-model");
    assert_eq!(flag(&a, "--ctx-size").as_deref(), Some("4096"), "{a:?}");
    let n: u64 = flag(&a, "--n-gpu-layers").unwrap().parse().unwrap();
    assert!((1..9).contains(&n), "{n}");
    assert_eq!(flag(&a, "--fit").as_deref(), Some("off"));
    let e = entry(&f, "fit-model");
    let w = e
        .warnings
        .iter()
        .find(|w| w.starts_with("It will work but only fits partially on the GPU"))
        .unwrap_or_else(|| panic!("{:?}", e.warnings));
    assert!(
        w.contains("headroom") && w.contains("MiB") && w.contains("--n-gpu-layers"),
        "{w}"
    );
    assert!(e.grant.unwrap().bytes <= 36 * MIB);
}

/// Not even the output layer at the floor fits the card: refused, with
/// what the least needs.
#[tokio::test]
async fn a_fit_row_too_large_for_even_its_least_is_refused_with_numbers() {
    let f = fixture(5 * MIB, 8 * MIB, 8 * MIB, 4).await;
    add_row(&f, "fit-model", LlamaParams::default(), &[]).await;
    let err = lmgw_core::ops::container(&f.state, None, Some("fit-model"), "start", false, None)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        err.contains("Even the least llama.cpp's --fit would run it with")
            && err.contains("--n-gpu-layers 1")
            && err.contains("MiB"),
        "{err}"
    );
}

/// A row that sets its context and layers is held to them as written: no
/// `--fit off`, nothing lowered, and too large for the card is refused.
#[tokio::test]
async fn an_explicit_row_is_honoured_as_written() {
    let f = fixture(40 * MIB, 8 * MIB, 8 * MIB, 4).await;
    let pinned = LlamaParams {
        ctx_size: Some(8192),
        n_gpu_layers: Some(999),
        ..Default::default()
    };
    add_row(&f, "pinned", pinned, &[]).await;
    let err = lmgw_core::ops::container(&f.state, None, Some("pinned"), "start", false, None)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("headroom"), "refused as too large: {err}");
    assert!(argv(&f, "pinned").is_empty(), "never started");

    // The same with room: started exactly as written.
    let f = fixture(96 * MIB, 8 * MIB, 8 * MIB, 4).await;
    let pinned = LlamaParams {
        ctx_size: Some(8192),
        n_gpu_layers: Some(999),
        ..Default::default()
    };
    add_row(&f, "pinned", pinned, &[]).await;
    assert_eq!(start(&f, "pinned").await["ok"], true);
    let a = argv(&f, "pinned");
    assert_eq!(flag(&a, "--ctx-size").as_deref(), Some("8192"));
    assert_eq!(flag(&a, "--n-gpu-layers").as_deref(), Some("999"));
    assert_eq!(flag(&a, "--fit"), None, "{a:?}");
    let w = entry(&f, "pinned").warnings;
    assert!(
        !w.iter()
            .any(|w| w.starts_with("Context fitted") || w.starts_with("It will work")),
        "{w:?}"
    );
}

/// A partial `--n-gpu-layers` is charged its layers' share: it starts where
/// all of it would not fit, and is charged less than the whole.
#[tokio::test]
async fn a_partial_n_gpu_layers_row_is_charged_its_share() {
    let f = fixture(40 * MIB, 8 * MIB, 8 * MIB, 4).await;
    let part = LlamaParams {
        ctx_size: Some(8192),
        n_gpu_layers: Some(3),
        ..Default::default()
    };
    add_row(&f, "part", part, &[]).await;
    let out = start(&f, "part").await;
    assert_eq!(out["ok"], true, "{out}");
    let a = argv(&f, "part");
    assert_eq!(flag(&a, "--n-gpu-layers").as_deref(), Some("3"));
    assert_eq!(flag(&a, "--fit"), None, "nothing for lmgw to fit");
    let v = vram_status(&f.gateway).await;
    let r = v["resident"]
        .as_array()
        .and_then(|rs| rs.iter().find(|r| r["model"] == "part").cloned())
        .unwrap();
    let charged = r["estimated_bytes"].as_u64().unwrap();
    // Two layers (8 MiB) and the output head (2 MiB), a quarter of the
    // cache (8 MiB of 32), the compute buffer.
    assert!((18 * MIB..36 * MIB).contains(&charged), "{r}");
    assert!(
        r["note"]
            .as_str()
            .unwrap_or_default()
            .contains("--n-gpu-layers 3 puts 2 of its 8 layers"),
        "{r}"
    );
}
