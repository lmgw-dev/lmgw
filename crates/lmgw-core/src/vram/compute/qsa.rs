//! The sparse attention of `qwen4exp` (Qwen3.8-Flash-Next), as llama.cpp
//! runs it (`src/models/qwen4exp.cpp`, `src/llama-memory-hybrid-idx.cpp`,
//! `src/llama-model.cpp`, 171e8846b): each full-attention layer attends only
//! to the cells an indexer picks, and the indexer costs a cache and a phase
//! of the graph that the rest of the chat formula ([`super::chat`]) does not
//! know.
//!
//! **The indexer's cache.** `llama_memory_hybrid_idx` keeps, beside the
//! attention cache and the recurrent state, a cache of indexer keys: one
//! head of `attention.indexer.key_length` per cell and no V (it is shaped as
//! an MLA cache so that none is kept), at the K cache type, in every
//! full-attention layer (`filter_idx`), over the attention cache's cells.
//! llama.cpp builds it when the header states the key length; lmgw charges
//! it beside the KV cache. At 262,144 cells, q8_0 and 12 full-attention
//! layers it is 408 MiB.
//!
//! **The scoring.** `build_qsa_top_k` scores blocks of `compress_ratio`
//! cells, then gives every cell its block's score: an f32 tensor of every
//! cell by every batch token (`ggml_get_rows` over `cell_blk`), made
//! contiguous into a second one (`ggml_cont` of its permute), then added to
//! an f32 copy of the mask before the top-k. Neither `get_rows` nor `cont`
//! runs in place, so two of these tensors are live together at whichever
//! step peaks: 8 bytes per cell and token, beside the per-block bias input
//! (f32, one value per block and token) and the mask. The attention that
//! follows holds less beside its f16 copy of the cache (its masks are f16),
//! so at a long context the scoring is the layer's widest phase: 2,304 MiB
//! at 262,144 cells and ubatch 1024, where the attention phase is the 512
//! MiB copy and under 200 MiB of activations.
//!
//! The scoring term is read from the graph as built at 171e8846b and from
//! `ggml-alloc`'s rules for which ops reuse their input; the image a row
//! runs may build it differently. Checked against four loads of Qwen3.8-Flash-Next UD-IQ4_XS on 2026-11-09 (decision-models design,
//! Appendix C.2, "Estimate against the process").

use super::chat::Graph;
use super::layers::Layout;
use crate::gguf::ModelSummary;

/// The indexer's key width, for a header llama.cpp builds the indexer for:
/// a `qwen4exp` one stating `attention.indexer.key_length`.
fn indexer_key(s: &ModelSummary) -> Option<u64> {
    if s.architecture.as_deref() != Some("qwen4exp") {
        return None;
    }
    s.indexer_key_length.filter(|n| *n > 0)
}

/// The indexer's cache at `g`, its keys at `bk` bits per 32 elements (the K
/// cache type's), in bytes; 0 for a header without an indexer (module doc).
pub(crate) fn cache_bytes(s: &ModelSummary, layout: &Layout, g: &Graph, bk: u32) -> u64 {
    let Some(key) = indexer_key(s) else {
        return 0;
    };
    let layers = layout.full.as_ref().map_or(0, |k| k.own_layers);
    [layers, g.full_cells(), g.streams(), key, u64::from(bk)]
        .iter()
        .fold(1u64, |a, b| a.saturating_mul(*b))
        / 256
}

/// The scoring phase of a context of `s` at `g`, beside the masks, in bytes;
/// 0 for a header without an indexer or without a layer that scores (no
/// nonzero `compress_ratios`) (module doc). A stream's cells are scored for
/// its share of the batch, so the total is one stream's cells by the whole
/// batch.
pub(crate) fn scoring_bytes(s: &ModelSummary, g: &Graph) -> u64 {
    let (Some(_), Some(ratio)) = (indexer_key(s), s.indexer_compress_ratio.filter(|r| *r > 0))
    else {
        return 0;
    };
    let cells = g.full_cells();
    let per_token = cells
        .saturating_mul(8)
        .saturating_add(cells.div_ceil(ratio).saturating_mul(4));
    per_token.saturating_mul(g.ub())
}

#[cfg(test)]
pub(crate) mod tests;
