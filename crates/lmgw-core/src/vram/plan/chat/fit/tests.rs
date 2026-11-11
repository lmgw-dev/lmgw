//! A partial `--n-gpu-layers` charge, and what a `--fit` row is fitted to.

use std::sync::Arc;

use super::*;
use crate::config::LlamaParams;
use crate::gguf::{ModelSummary, TensorList};
use crate::vram::plan::chat::{footprint, ChatFiles};
use crate::vram::plan::GgufFiles;

const MIB: u64 = 1024 * 1024;
const LAYER: u64 = 60 * MIB;

/// A Qwen3-4B-shaped dense model: 36 layers of 60 MiB, an output head and
/// input embeddings of 300 MiB each, 8 KV heads of 128, trained on 262144.
pub(crate) fn dense() -> ModelSummary {
    let mut tensors: Vec<(String, u64)> = (0..36)
        .flat_map(|i| {
            [
                (format!("blk.{i}.attn_q.weight"), 20 * MIB),
                (format!("blk.{i}.ffn_up.weight"), 40 * MIB),
            ]
        })
        .collect();
    tensors.push(("output.weight".into(), 300 * MIB));
    tensors.push(("token_embd.weight".into(), 300 * MIB));
    ModelSummary {
        block_count: Some(36),
        embedding_length: Some(2560),
        feed_forward_length: Some(9728),
        head_count: Some(32),
        head_count_kv: Some(8),
        key_length: Some(128),
        value_length: Some(128),
        vocab_size: Some(151_936),
        context_length: Some(262_144),
        tensor_list: TensorList { tensors, parts: 1 },
        ..Default::default()
    }
}

pub(crate) fn files(s: ModelSummary) -> Arc<ChatFiles> {
    let bytes = s.tensor_list.tensors.iter().map(|(_, b)| b).sum();
    Arc::new(ChatFiles {
        files: GgufFiles {
            main_bytes: bytes,
            companion_bytes: 0,
            summary: Ok(s),
            missing: Vec::new(),
        },
        draft: None,
    })
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

pub(crate) fn shape(p: LlamaParams, a: &[&str]) -> Shape {
    Shape::of(&p, &args(a))
}

fn ctx(c: i64) -> LlamaParams {
    LlamaParams {
        ctx_size: Some(c),
        parallel: Some(1),
        ..Default::default()
    }
}

/// `--n-gpu-layers 19` is the output layer and the last 18 layers: the first
/// 18 layers' weights and their share of the KV cache stay in host memory.
#[test]
fn a_partial_n_gpu_layers_is_charged_its_share() {
    let f = files(dense());
    let all = footprint(&f, "m.gguf", &shape(ctx(8192), &[]));
    let part = footprint(&f, "m.gguf", &shape(ctx(8192), &["-ngl", "19"]));
    assert_eq!(all.weights_bytes - part.weights_bytes, 18 * LAYER);
    assert_eq!(part.kv_cache_bytes * 2, all.kv_cache_bytes, "{part:?}");
    assert_eq!(
        part.compute_bytes,
        all.compute_bytes + 40 * MIB,
        "the compute buffer in full, and op offload's copy of the largest host-kept weight"
    );
    assert!(
        part.note
            .as_deref()
            .unwrap_or_default()
            .contains("18 of its 36 layers"),
        "{part:?}"
    );
    // The field reads the same as the flag; all of them is the whole.
    let field = LlamaParams {
        n_gpu_layers: Some(19),
        ..ctx(8192)
    };
    assert_eq!(
        footprint(&f, "m.gguf", &shape(field, &[])).total_bytes,
        part.total_bytes
    );
    let every = footprint(&f, "m.gguf", &shape(ctx(8192), &["-ngl", "37"]));
    assert_eq!(every.total_bytes, all.total_bytes);
    // `--no-kv-offload` keeps the cache in host memory.
    let host = footprint(&f, "m.gguf", &shape(ctx(8192), &["-nkvo"]));
    assert_eq!(host.kv_cache_bytes, 0);
    assert_eq!(host.weights_bytes, all.weights_bytes);
}

/// A row that sets its context and layers, or turns `--fit` off, is held to
/// them as written: nothing to fit.
#[test]
fn an_explicit_row_has_nothing_to_fit() {
    let f = files(dense());
    let pinned = LlamaParams {
        n_gpu_layers: Some(999),
        ..ctx(8192)
    };
    assert!(Sizer::of(f.clone(), "m.gguf", &shape(pinned, &[])).is_none());
    assert!(Sizer::of(
        f.clone(),
        "m.gguf",
        &shape(LlamaParams::default(), &["--fit", "off"])
    )
    .is_none());
    // An explicit context with layers on auto: layers only.
    let s = Sizer::of(f.clone(), "m.gguf", &shape(ctx(8192), &[])).unwrap();
    assert!(!s.ctx_adapts && s.layers_adapt);
    // A tensor override keeps `--fit` off the layers; an unset context it
    // still lowers.
    let s = Sizer::of(f, "m.gguf", &shape(LlamaParams::default(), &["--cpu-moe"])).unwrap();
    assert!(s.ctx_adapts && !s.layers_adapt);
}

/// An unset context is lowered first, to the most that fits, in steps of
/// 256 and never below `--fit-ctx`; then whole layers go, the most that fit.
#[test]
fn a_fit_row_is_fitted_context_first_then_layers() {
    let f = files(dense());
    let s = Sizer::of(f, "m.gguf", &shape(LlamaParams::default(), &[])).unwrap();
    assert_eq!((s.trained, s.min_ctx, s.layers), (Some(262_144), 4096, 36));
    let whole = s.charge(&LlamaPlacement::default());
    assert_eq!(s.best(whole), Some(LlamaPlacement::default()));

    // 4 GiB: the context comes down, every layer stays.
    let p = s.best(4096 * MIB).unwrap();
    let c = p.ctx.unwrap();
    assert!(
        p.n_gpu_layers.is_none() && c.is_multiple_of(256) && c > 4096,
        "{p:?}"
    );
    assert!(s.charge(&p) <= 4096 * MIB);
    let more = LlamaPlacement {
        ctx: Some(c + 256),
        ..p
    };
    assert!(s.charge(&more) > 4096 * MIB, "the most that fits");

    // Less than every layer at the floor: the floor, and the most layers.
    let floor = s.charge(&LlamaPlacement {
        ctx: Some(4096),
        ..LlamaPlacement::default()
    });
    let p = s.best(floor - 5 * LAYER).unwrap();
    assert_eq!(p.ctx, Some(4096));
    let n = p.n_gpu_layers.unwrap();
    assert!(n < 37 && n > 1, "{p:?}");
    assert!(s.charge(&p) <= floor - 5 * LAYER);
    let more = LlamaPlacement {
        n_gpu_layers: Some(n + 1),
        ..p
    };
    assert!(s.charge(&more) > floor - 5 * LAYER);

    // Not even the output layer at the floor: nothing.
    let least = s.charge(&s.minimum());
    assert_eq!(s.best(least - 1), None);
    assert!(s.best(least).is_some());
}
