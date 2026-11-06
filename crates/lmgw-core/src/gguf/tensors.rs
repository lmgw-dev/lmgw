//! What each tensor occupies on disk, read from the descriptors' data
//! offsets rather than from a table of ggml types.
//!
//! A tensor's bytes run from its offset to the next tensor's (sorted by
//! offset), and the last one's to the end of the file. That counts the
//! alignment padding after a tensor as its own, at most `general.alignment`
//! bytes, which is nothing next to the tensors the callers size (an output
//! head of 100+ MiB, `crate::vram::compute`). A table of ggml type sizes
//! would go stale with the next quant type llama.cpp adds; the offsets are
//! what the file itself says.
//!
//! One reader wants the exact size instead: the repeating layers' width
//! ([`super::params::layer_tensors`]), whose Q8 cap is Q8_0's own 8.5 bits
//! per weight and so has no room for a padding byte. [`type_bytes`] gives a
//! tensor's size from its ggml type and element count, for the types whose
//! block llama.cpp's `gguf-py` (`GGML_QUANT_SIZES`) lists; a type the table
//! doesn't know falls back to the offsets.
//!
//! Only the file read is sized. In a split GGUF (`-00001-of-0000N`) the
//! header lmgw reads is the first shard's, so a tensor that sits in a later
//! shard is not there: an untied model whose `output.weight` is in a later
//! shard reads as having none, and `ModelSummary::output_head_bytes` falls
//! back to `token_embd.weight`, usually of the same size. That figure only
//! enters the compute buffer of a row at `--n-gpu-layers 0` with the device
//! passed.

/// ggml's F32 type id: what llama.cpp's quantizer keeps the tensors it
/// never quantizes in (norms, biases, MoE gating, conv1d).
pub(super) const GGML_F32: u32 = 0;

/// A ggml type's block: (elements per block, bytes per block), as llama.cpp's
/// `gguf-py/gguf/constants.py` `GGML_QUANT_SIZES` lists them by type id.
/// `None` for an id the table doesn't know (a type newer than it, or a
/// malformed header): its tensors are sized by their offsets.
fn block(ggml_type: u32) -> Option<(u64, u64)> {
    const QK_K: u64 = 256;
    Some(match ggml_type {
        0 => (1, 4),                                           // F32
        1 => (1, 2),                                           // F16
        2 => (32, 2 + 16),                                     // Q4_0
        3 => (32, 2 + 2 + 16),                                 // Q4_1
        6 => (32, 2 + 4 + 16),                                 // Q5_0
        7 => (32, 2 + 2 + 4 + 16),                             // Q5_1
        8 => (32, 2 + 32),                                     // Q8_0
        9 => (32, 4 + 4 + 32),                                 // Q8_1
        10 => (QK_K, 2 + 2 + QK_K / 16 + QK_K / 4),            // Q2_K
        11 => (QK_K, 2 + QK_K / 4 + QK_K / 8 + 12),            // Q3_K
        12 => (QK_K, 2 + 2 + QK_K / 2 + 12),                   // Q4_K
        13 => (QK_K, 2 + 2 + QK_K / 2 + QK_K / 8 + 12),        // Q5_K
        14 => (QK_K, 2 + QK_K / 2 + QK_K / 4 + QK_K / 16),     // Q6_K
        15 => (QK_K, 4 + QK_K + QK_K / 8),                     // Q8_K
        16 => (QK_K, 2 + QK_K / 4),                            // IQ2_XXS
        17 => (QK_K, 2 + QK_K / 4 + QK_K / 32),                // IQ2_XS
        18 => (QK_K, 2 + QK_K / 4 + QK_K / 8),                 // IQ3_XXS
        19 => (QK_K, 2 + QK_K / 8 + QK_K / 16),                // IQ1_S
        20 => (32, 2 + 16),                                    // IQ4_NL
        21 => (QK_K, 2 + QK_K / 4 + QK_K / 8 + QK_K / 32 + 4), // IQ3_S
        22 => (QK_K, 2 + QK_K / 4 + QK_K / 16),                // IQ2_S
        23 => (QK_K, 2 + 2 + QK_K / 2 + QK_K / 64),            // IQ4_XS
        24 => (1, 1),                                          // I8
        25 => (1, 2),                                          // I16
        26 => (1, 4),                                          // I32
        27 => (1, 8),                                          // I64
        28 => (1, 8),                                          // F64
        29 => (QK_K, QK_K / 8 + QK_K / 16 + QK_K / 32),        // IQ1_M
        30 => (1, 2),                                          // BF16
        34 => (QK_K, 2 + 4 * 13),                              // TQ1_0
        35 => (QK_K, 2 + 64),                                  // TQ2_0
        39 => (32, 1 + 16),                                    // MXFP4
        40 => (64, 4 + 32),                                    // NVFP4
        41 => (128, 2 + 16),                                   // Q1_0
        _ => return None,
    })
}

/// The bytes `elements` of `ggml_type` take, without padding (module doc).
/// `None` for a type the table doesn't know, or a count that isn't whole
/// blocks (a malformed header).
pub(super) fn type_bytes(ggml_type: u32, elements: u64) -> Option<u64> {
    let (per, bytes) = block(ggml_type)?;
    if !elements.is_multiple_of(per) {
        return None;
    }
    (elements / per).checked_mul(bytes)
}

/// GGUF's default `general.alignment` when the header does not set one.
pub(super) const DEFAULT_ALIGNMENT: u64 = 32;

/// Where the tensor data starts: `header_end` (the byte after the last
/// descriptor) rounded up to `alignment`. An alignment of 0 is GGUF's
/// default, since a zero stride aligns nothing.
pub(super) fn data_start(header_end: u64, alignment: u64) -> u64 {
    let a = if alignment == 0 {
        DEFAULT_ALIGNMENT
    } else {
        alignment
    };
    header_end.div_ceil(a).saturating_mul(a)
}

/// Each tensor's bytes, in the order of `offsets` (the descriptors' data
/// offsets, relative to the data start), given `data_len`, the bytes from
/// the data start to the end of the file. A tensor whose offset lies past
/// the end, or that shares its offset with another (a malformed or
/// synthetic header), gets what the arithmetic leaves, saturating at 0 —
/// never a guess.
pub(super) fn sizes(offsets: &[u64], data_len: u64) -> Vec<u64> {
    let mut order: Vec<usize> = (0..offsets.len()).collect();
    order.sort_by_key(|&i| offsets[i]);
    let mut out = vec![0u64; offsets.len()];
    for (k, &i) in order.iter().enumerate() {
        let end = order
            .get(k + 1)
            .map(|&next| offsets[next])
            .unwrap_or(data_len);
        out[i] = end.saturating_sub(offsets[i]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table's blocks against tensors of two real files
    /// (`bge-small-en-v1.5-q8_0.gguf`, `bge-reranker-v2-m3-Q4_K_M.gguf`,
    /// checked by hand: every tensor's offsets leave it exactly this, plus
    /// padding under the alignment), and an unknown type or a partial block
    /// sized by nothing.
    #[test]
    fn a_types_size_is_its_blocks() {
        assert_eq!(type_bytes(0, 384), Some(384 * 4));
        assert_eq!(type_bytes(8, 384 * 384), Some(384 * 12 * 34));
        assert_eq!(type_bytes(12, 1024 * 1024), Some(4096 * 144));
        assert_eq!(type_bytes(14, 1024 * 4096), Some(16384 * 210));
        assert_eq!(type_bytes(30, 10), Some(20));
        assert_eq!(type_bytes(8, 33), None, "not whole blocks");
        assert_eq!(type_bytes(4, 32), None, "an id the table doesn't know");
    }

    #[test]
    fn a_tensor_runs_to_the_next_offset_and_the_last_to_the_end() {
        // Descriptor order is not offset order.
        assert_eq!(sizes(&[64, 0, 96], 160), vec![32, 64, 64]);
        assert_eq!(sizes(&[], 100), Vec::<u64>::new());
        // Shared offsets and an offset past the end saturate, they do not wrap.
        assert_eq!(sizes(&[0, 0, 500], 100), vec![0, 500, 0]);
    }

    /// What `vram::compute` reads: the output head's bytes (an untied
    /// `output.weight`, else the tied `token_embd.weight`), the vocabulary,
    /// the largest per-layer FFN width and Qwen3-VL's deepstack count.
    #[test]
    fn a_summary_reads_the_compute_buffer_facts() {
        use crate::gguf::{read_header, synth, ModelSummary};
        let dir = tempfile::tempdir().unwrap();
        let tokens: Vec<String> = (0..300).map(|i| format!("t{i}")).collect();
        let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();

        let mut tied = synth::embedding("qwen3vl", 3, 4096);
        tied.u32("qwen3vl.feed_forward_length", 192)
            .u32("qwen3vl.n_deepstack_layers", 3)
            .arr_str("tokenizer.ggml.tokens", &tokens)
            .tensor_sized("blk.0.attn_q.weight", 4096)
            .tensor_sized("token_embd.weight", 19_200)
            .tensor_sized("output_norm.weight", 256);
        let path = dir.path().join("tied.gguf");
        tied.write_to(&path);
        let meta = read_header(&path).unwrap();
        assert_eq!(meta.tensor_size("blk.0.attn_q.weight"), Some(4096));
        assert_eq!(meta.tensor_size("output_norm.weight"), Some(256));
        let s = ModelSummary::from_meta(&meta);
        assert_eq!(s.output_head_bytes, Some(19_200));
        assert_eq!(s.vocab_size, Some(300));
        assert_eq!(s.feed_forward_length, Some(192));
        assert_eq!(s.deepstack_layers, Some(3));

        let mut untied = synth::embedding("modern-bert", 0, 8192);
        untied
            .arr_i32("modern-bert.feed_forward_length", &[2624, 4096, 2624])
            .tensor_sized("token_embd.weight", 19_200)
            .tensor_sized("output.weight", 7_000);
        let path = dir.path().join("untied.gguf");
        untied.write_to(&path);
        let s = ModelSummary::from_meta(&read_header(&path).unwrap());
        assert_eq!(s.output_head_bytes, Some(7_000), "the untied head wins");
        assert_eq!(s.feed_forward_length, Some(4096), "the largest layer");
        assert_eq!(s.vocab_size, None, "no tokenizer, no vocabulary");

        // Data-less descriptors (every older synthetic header) size nothing.
        let mut bare = synth::embedding("bert", 1, 512);
        bare.tensor("token_embd.weight");
        let path = dir.path().join("bare.gguf");
        bare.write_to(&path);
        let s = ModelSummary::from_meta(&read_header(&path).unwrap());
        assert_eq!(s.output_head_bytes, None);
    }

    #[test]
    fn the_data_starts_at_the_next_alignment_boundary() {
        assert_eq!(data_start(100, 32), 128);
        assert_eq!(data_start(128, 32), 128);
        assert_eq!(data_start(100, 0), 128);
        assert_eq!(data_start(1, 64), 64);
    }
}
