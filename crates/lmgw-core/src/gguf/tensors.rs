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
//! Only the file read is sized. In a split GGUF (`-00001-of-0000N`) the
//! header lmgw reads is the first shard's, so a tensor that sits in a later
//! shard is not there: an untied model whose `output.weight` is in a later
//! shard reads as having none, and `ModelSummary::output_head_bytes` falls
//! back to `token_embd.weight`, usually of the same size. That figure only
//! enters the compute buffer of a row at `--n-gpu-layers 0` with the device
//! passed.

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
