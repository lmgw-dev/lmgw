//! The indexer's cache and scoring phase, against the shapes read from the shapes read from the `unsloth/Qwen3.8-Flash-Next-GGUF` UD-IQ4_XS header (2026-11-09).

use super::*;
use crate::vram::compute::chat::bytes;

const MIB: u64 = 1024 * 1024;

/// The header of `unsloth/Qwen3.8-Flash-Next-GGUF` UD-IQ4_XS, as far as the
/// footprint reads it: 48 layers, every fourth full attention (2 KV heads of
/// 256), 512 experts of 640 with 10 used, an indexer of one 128-wide key
/// head over blocks of 4.
pub(crate) fn qwen38_flash_next() -> ModelSummary {
    ModelSummary {
        architecture: Some("qwen4exp".into()),
        block_count: Some(48),
        embedding_length: Some(2560),
        head_count: Some(24),
        head_count_kv: Some(2),
        key_length: Some(256),
        value_length: Some(256),
        full_attention_interval: Some(4),
        ssm_conv_kernel: Some(4),
        ssm_state_size: Some(128),
        ssm_group_count: Some(16),
        ssm_inner_size: Some(6144),
        has_recurrent_keys: true,
        expert_used_count: Some(10),
        expert_feed_forward_length: Some(640),
        expert_shared_feed_forward_length: Some(640),
        embedding_length_per_layer_input: Some(160),
        vocab_size: Some(248_320),
        indexer_key_length: Some(128),
        indexer_compress_ratio: Some(4),
        ..Default::default()
    }
}

/// A row of 262,144 cells over four unified slots, q8_0, flash
/// attention on.
fn graph(ubatch: u64) -> Graph {
    Graph {
        ctx: 262_144,
        ubatch,
        batch: 2048,
        slots: 4,
        unified: true,
        flash_attn_off: false,
        k_copied: true,
        v_copied: true,
        outputs: 4,
        single_layer: false,
        swa_full: false,
    }
}

#[test]
fn the_indexer_keeps_one_key_head_per_cell_in_each_full_attention_layer() {
    let s = qwen38_flash_next();
    let layout = Layout::of(&s).unwrap();
    // 12 layers × 262,144 cells × 128 at q8_0's 8.5 bits.
    assert_eq!(cache_bytes(&s, &layout, &graph(1024), 272), 408 * MIB);
    // Another architecture with the same keys builds no such cache.
    let other = ModelSummary {
        architecture: Some("qwen35moe".into()),
        ..qwen38_flash_next()
    };
    assert_eq!(cache_bytes(&other, &layout, &graph(1024), 272), 0);
    assert_eq!(scoring_bytes(&other, &graph(1024)), 0);
}

/// The scoring grows with the batch, and at 262,144 cells it is the widest
/// phase: the buffer is the masks plus it.
#[test]
fn the_scoring_is_the_widest_phase_at_a_long_context() {
    let s = qwen38_flash_next();
    let layout = Layout::of(&s).unwrap();
    for (ub, scoring) in [(512, 1152), (1024, 2304), (2048, 4608)] {
        let g = graph(ub);
        assert_eq!(scoring_bytes(&s, &g), scoring * MIB, "ubatch {ub}");
        let masks = 2 * 262_144 * ub;
        assert_eq!(bytes(&s, &layout, &g).unwrap(), masks + scoring * MIB);
    }
    // Without a ratio no layer scores.
    let none = ModelSummary {
        indexer_compress_ratio: None,
        ..qwen38_flash_next()
    };
    assert_eq!(scoring_bytes(&none, &graph(1024)), 0);
}
