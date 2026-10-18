//! The compute buffer llama.cpp reserves for one context of a chat row: the
//! row's own, and the second context a speculative row runs (an MTP head, a
//! draft model). Fitted to llama.cpp's own `CUDA0 compute buffer size` lines
//! (b11549, `-lv 4`), 34 contexts of 21 loads of chat models and
//! variants on a 24 GB GPU (decision-models design, Appendix C.2; the probe
//! logs are not in the repo).
//!
//! **Why a chat context is not an aux one.** llama-server reserves a chat
//! context's graph for `min(batch, slots × (1 + draft))` outputs, not one
//! per token (`server_output_limits`), so the logits that dominate an
//! embedder's buffer are a few MiB here. What dominates instead:
//!
//! - **The attention masks**, f16 (f32 with flash attention off):
//!   `2 · n_kv · ub` bytes, `n_kv` one stream's cells: the whole context
//!   with a unified cache, `ctx / slots` with one stream per slot (the
//!   reserved batch is split across the streams, so the total is the same
//!   `n_kv_stream · ub`). A sliding-window model adds a second mask over its
//!   window's cells: `window + ub` per stream, `window · slots + ub` unified,
//!   padded to 256.
//! - **The f16 copy of a quantized cache.** With flash attention on and a
//!   cache type other than f16, CUDA's tile and MMA kernels (the ones a
//!   prompt batch runs) read K and V through an f16 copy that is allocated
//!   with the attention op, in this buffer (`ggml_cuda_flash_attn_ext_get_
//!   alloc_size`): `2 · heads · (dk + dv) · cells` bytes for the widest
//!   layer, all streams. A q8_0 cache at a long context makes this the
//!   largest term: 1,000 MiB for Qwen3.5-4B at 262,144, 2,000 MiB for
//!   Gemma 4 E4B at 512,000.
//! - **The activations** of the widest phase, linear in `ub`: attention
//!   `16 · ub · (n_embd + heads · dk)` (+ `4 · ub` per-layer-input width),
//!   which the f16 copy adds to; the FFN `4 · ub · (4 · n_embd + 3 · n_ff)`,
//!   for a mixture-of-experts layer `4 · ub · (4 · n_embd + used · (3 ·
//!   n_ff_exp + 2 · n_embd) + 3 · n_ff_shexp)` when that is wider (+ `8 · ub`
//!   per-layer-input width); and the logits, `4 · vocab · outputs`.
//!   The buffer is the masks plus the largest phase. A one-layer MTP
//!   context frees its mask before its FFN, so there the masks join the
//!   attention phase only.
//!
//! Measured against this: within ±1 % wherever the masks and the f16 copy
//! dominate, −5 % (−48 MiB) at worst on Qwen3.x-27B's attention phase, and
//! up to +42 % (+79 MiB) on a Gemma 4 assistant's and +15–25 % on an MTP
//! context's small remainders. The activation widths are a fit across
//! architectures (Qwen3.5's gated attention runs ~1.8× a plain one's), so
//! that part is approximate; the masks and the copy are exact.
//!
//! **Not here:** a projector's own buffer (13–248 MiB measured, sized by
//! llama.cpp from per-projector image limits no header carries), the CUDA
//! context (~450 MiB per process), allocator slack: the headroom's.

use super::layers::Layout;
use crate::gguf::ModelSummary;

/// What one context of a chat row asks of llama.cpp that sizes its buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Graph {
    /// The total context the cache holds (all slots).
    pub ctx: u64,
    pub ubatch: u64,
    pub batch: u64,
    /// Parallel slots, and whether they share one cache stream.
    pub slots: u64,
    pub unified: bool,
    pub flash_attn_off: bool,
    /// The cache this context's attention reads is not f16, so flash
    /// attention reads it through an f16 copy (module doc). Per K and V.
    pub k_copied: bool,
    pub v_copied: bool,
    /// The outputs the graph is reserved for.
    pub outputs: u64,
    /// One decoder layer (an MTP head): its mask is freed before its FFN.
    pub single_layer: bool,
    /// `--swa-full`: the window cache holds the whole context.
    pub swa_full: bool,
}

/// llama.cpp pads a context to 256 cells.
pub(crate) fn padded(n: u64) -> u64 {
    n.div_ceil(256).saturating_mul(256)
}

impl Graph {
    /// The physical batch the graph is reserved for: llama.cpp lowers the
    /// ubatch to the batch and reserves no more tokens than the context.
    pub(crate) fn ub(&self) -> u64 {
        self.ubatch.min(self.batch).min(padded(self.ctx)).max(1)
    }

    /// Cache streams: one per slot unless the cache is unified.
    pub(crate) fn streams(&self) -> u64 {
        if self.unified {
            1
        } else {
            self.slots.max(1)
        }
    }

    /// Cells of one full-attention stream: the padded context, split per
    /// slot and padded again (`n_ctx_seq`, `llama_context`).
    pub(crate) fn full_cells(&self) -> u64 {
        padded(padded(self.ctx) / self.streams())
    }

    /// Cells of one sliding-window stream for a window of `w` tokens
    /// (`llama_kv_cache_iswa`), or the whole stream with `--swa-full`.
    pub(crate) fn window_cells(&self, w: u64) -> u64 {
        if self.swa_full {
            return self.full_cells();
        }
        let want = if self.unified {
            w.saturating_mul(self.slots.max(1))
        } else {
            w
        };
        padded(want.saturating_add(self.ub())).min(self.full_cells())
    }
}

/// The compute buffer, in bytes, of a context of model `s` (laid out as
/// `layout`) asking `g` (module doc). `Err` names the header value the
/// estimate needs and the file does not state.
pub(crate) fn bytes(s: &ModelSummary, layout: &Layout, g: &Graph) -> Result<u64, String> {
    let need = |v: Option<u64>, key: &str| {
        v.filter(|n| *n > 0)
            .ok_or_else(|| format!("the header states no {key}"))
    };
    let n_embd = need(s.embedding_length, "embedding_length")?;
    need(s.head_count, "attention.head_count")?;
    let vocab = need(s.vocab_size, "vocabulary (tokenizer.ggml.tokens)")?;
    let used = s.expert_used_count.unwrap_or(0);
    if used == 0 && s.feed_forward_length.is_none() {
        need(None, "feed_forward_length")?;
    }
    let mul = |terms: &[u64]| terms.iter().fold(1u64, |a, b| a.saturating_mul(*b));
    let ub = g.ub();
    let streams = g.streams();
    let mask_width = if g.flash_attn_off { 4 } else { 2 };

    let mut masks = 0u64;
    let mut copy = 0u64;
    if let Some(full) = &layout.full {
        let cells = g.full_cells();
        masks = masks.saturating_add(mul(&[mask_width, cells, ub]));
        copy = copy.max(f16_copy(
            full.max_heads,
            full.dk,
            full.dv,
            cells,
            streams,
            g,
        ));
    }
    if let Some((swa, w)) = &layout.swa {
        let cells = g.window_cells(*w);
        masks = masks.saturating_add(mul(&[mask_width, cells, ub]));
        copy = copy.max(f16_copy(swa.max_heads, swa.dk, swa.dv, cells, streams, g));
    }
    let ple = if g.single_layer {
        0
    } else {
        s.block_count
            .unwrap_or(0)
            .saturating_mul(s.embedding_length_per_layer_input.unwrap_or(0))
    };
    let mut attn =
        mul(&[16, ub, n_embd.saturating_add(layout.q_width)]).saturating_add(mul(&[4, ub, ple]));
    if g.flash_attn_off {
        // The scores are materialized: f32, every head over the stream.
        let heads = need(s.head_count, "attention.head_count")?;
        attn = attn.saturating_add(mul(&[4, heads, g.full_cells(), ub]));
    }
    let dense = n_embd
        .saturating_mul(4)
        .saturating_add(s.feed_forward_length.unwrap_or(0).saturating_mul(3));
    let moe = if used > 0 {
        let exp = need(s.expert_feed_forward_length, "expert_feed_forward_length")?;
        n_embd
            .saturating_mul(4)
            .saturating_add(used.saturating_mul(exp.saturating_mul(3).saturating_add(n_embd * 2)))
            .saturating_add(
                s.expert_shared_feed_forward_length
                    .unwrap_or(0)
                    .saturating_mul(3),
            )
    } else {
        0
    };
    let ffn = mul(&[4, ub, dense.max(moe)]).saturating_add(mul(&[8, ub, ple]));
    let logits = mul(&[4, vocab, g.outputs.max(1)]);
    Ok(if g.single_layer {
        masks
            .saturating_add(copy)
            .saturating_add(attn)
            .max(ffn)
            .max(logits)
    } else {
        masks.saturating_add(copy.saturating_add(attn).max(ffn).max(logits))
    })
}

/// The f16 copy flash attention reads a non-f16 cache through, for the
/// widest layer of a kind: `2 · heads · (dk + dv) · cells · streams` over
/// whichever of K and V is copied (module doc). 0 with flash attention off.
fn f16_copy(heads: u64, dk: u64, dv: u64, cells: u64, streams: u64, g: &Graph) -> u64 {
    if g.flash_attn_off {
        return 0;
    }
    let width = dk * u64::from(g.k_copied) + dv * u64::from(g.v_copied);
    [2, heads, width, cells, streams]
        .iter()
        .fold(1u64, |a, b| a.saturating_mul(*b))
}

#[cfg(test)]
pub(crate) mod tests;
