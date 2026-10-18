//! The attention layout a GGUF header describes, as llama.cpp builds a chat
//! context's KV cache and graph from it: which layers attend over the whole
//! context, which over a sliding window, which keep a recurrent state
//! instead, which own a KV cache and which reuse another layer's, and the
//! head shapes of each kind. The chat KV cache ([`crate::vram::plan::chat`])
//! and the chat compute buffer ([`super::chat`]) both read it.
//!
//! Every value comes from the header. The one rule that is not a header key
//! is llama.cpp's own: a header without `attention.key_length` has heads of
//! `embedding_length / head_count` (`llama_model_loader`'s default for
//! `n_embd_head_k`), which is what Qwen2-family conversions rely on.

use crate::gguf::ModelSummary;

/// Full-attention or sliding-window layers of one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Kind {
    /// Layers of this kind that keep a KV cache of their own.
    pub own_layers: u64,
    /// Their KV heads, summed: the cache holds this many heads per cell.
    pub own_heads: u64,
    /// The most KV heads any layer of this kind attends with, whether it
    /// owns its cache or reuses one: the widest per-layer read.
    pub max_heads: u64,
    /// The head dims of K and V; `dv` is 0 when V is a view of K (MLA).
    pub dk: u64,
    pub dv: u64,
}

/// A header's attention layout ([`Layout::of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Layout {
    pub full: Option<Kind>,
    /// The windowed layers, and the window (`attention.sliding_window`).
    pub swa: Option<(Kind, u64)>,
    /// Layers without attention that keep a fixed-size state per sequence:
    /// a hybrid's (`full_attention_interval`) or, beside the header's
    /// recurrent keys, a zero-head layer's (Qwen3.5's gated delta net,
    /// LFM2's short convolutions, Ling's KDA). A zero-head layer of a header
    /// without such keys is attention-free and keeps nothing.
    pub recurrent_layers: u64,
    /// The full-attention query width, `head_count × key_length`; 0 when
    /// the header states no head_count, which the compute buffer then names.
    pub q_width: u64,
}

impl Layout {
    /// The layout of `s`'s main graph: its MTP layers
    /// (`nextn_predict_layers`) excluded, which run in a context of their own
    /// ([`Self::mtp`]). `Err` names the header value it needs and the file
    /// does not state.
    pub(crate) fn of(s: &ModelSummary) -> Result<Self, String> {
        let need = |v: Option<u64>, key: &str| {
            v.filter(|n| *n > 0)
                .ok_or_else(|| format!("the header states no {key}"))
        };
        let total = need(s.block_count, "block_count")?;
        // llama.cpp's default head size (module doc), when the header states
        // none.
        let default_head = || {
            let n_embd = need(s.embedding_length, "embedding_length")?;
            let heads = need(s.head_count, "attention.head_count")?;
            Ok::<u64, String>(n_embd / heads)
        };
        let dk = match s.key_length {
            Some(k) => k,
            None => default_head()?,
        };
        let dv = match s.value_length {
            Some(v) => v,
            None => default_head()?,
        };
        let per_layer = match &s.head_count_kv_per_layer {
            Some(h) if h.len() as u64 == total => Some(h.as_slice()),
            Some(h) => {
                return Err(format!(
                    "the header's attention.head_count_kv lists {} layers for a block_count \
                     of {total}",
                    h.len()
                ))
            }
            None => None,
        };
        let scalar = match per_layer {
            Some(_) => 0,
            None => need(s.head_count_kv, "attention.head_count_kv")?,
        };
        let interval = s.full_attention_interval.filter(|i| *i > 1);
        let window = s.sliding_window.filter(|w| *w > 0);
        // A window array names windowed layers only beside a window to size
        // them by; without one, as llama.cpp would, they read as full.
        let windows = s
            .sliding_window_layers
            .as_deref()
            .filter(|w| w.len() as u64 == total && window.is_some());
        let stride = s
            .sliding_window_pattern
            .filter(|p| *p > 1 && !s.sliding_window_pattern_is_array && window.is_some());
        // Of the main graph's layers, the last `shared_kv_layers` read an
        // earlier layer's cache. A header whose every layer is an MTP layer
        // is a drafter (a Gemma 4 assistant): all of them run.
        let nextn = s.nextn_predict_layers.unwrap_or(0);
        let n = if nextn < total { total - nextn } else { total };
        let owners = n.saturating_sub(s.shared_kv_layers.unwrap_or(0));
        let mut full = Kind {
            own_layers: 0,
            own_heads: 0,
            max_heads: 0,
            dk,
            dv,
        };
        let mut swa = Kind {
            dk: s.key_length_swa.unwrap_or(dk),
            dv: s.value_length_swa.unwrap_or(dv),
            ..full
        };
        let mut recurrent = 0;
        if per_layer.is_some() || windows.is_some() {
            // Bounded: the array matched block_count, and arrays are kept
            // only up to `gguf::NUMERIC_ARRAY_RETAIN` elements.
            for i in 0..n {
                let h = per_layer.map_or(scalar, |p| p[i as usize]);
                let attends = match interval {
                    Some(k) => (i + 1).is_multiple_of(k),
                    None => h > 0,
                };
                if !attends {
                    // A recurrent block keeps a state; an attention-free
                    // one (Deci-style) keeps nothing.
                    if interval.is_some() || s.has_recurrent_keys {
                        recurrent += 1;
                    }
                    continue;
                }
                let windowed = match (windows, stride) {
                    (Some(w), _) => w[i as usize],
                    (None, Some(p)) => !(i + 1).is_multiple_of(p),
                    (None, None) => false,
                };
                let k = if windowed { &mut swa } else { &mut full };
                k.max_heads = k.max_heads.max(h);
                if i < owners {
                    k.own_layers += 1;
                    k.own_heads += h;
                }
            }
        } else {
            // A scalar header, in closed form: block_count is the file's
            // word, never a loop bound. Every k-th layer is the special one:
            // the attending one among recurrent layers, or the full one
            // among windowed layers.
            let every = interval.or(stride);
            let special = every.map_or(n, |k| n / k);
            let own_special = every.map_or(owners, |k| owners / k);
            full.own_layers = own_special;
            full.max_heads = if special > 0 { scalar } else { 0 };
            if interval.is_some() {
                recurrent = n - special;
            } else if stride.is_some() {
                swa.own_layers = owners - own_special;
                swa.max_heads = if n > special { scalar } else { 0 };
            }
            full.own_heads = full.own_layers.saturating_mul(scalar);
            swa.own_heads = swa.own_layers.saturating_mul(scalar);
        }
        if Self::is_mla(s) {
            // V is a view of K: llama.cpp keeps no V cache.
            full.dv = 0;
            swa.dv = 0;
        }
        let some = |k: Kind| (k.max_heads > 0).then_some(k);
        Ok(Self {
            full: some(full),
            swa: some(swa).zip(window),
            recurrent_layers: recurrent,
            q_width: s.head_count.unwrap_or(0).saturating_mul(dk),
        })
    }

    /// llama.cpp's absorbed MLA graph, which keeps no V cache: both
    /// `attention.key_length_mla` and `value_length_mla` stated
    /// (`llama_hparams::is_mla`). `kv_lora_rank` alone is an older
    /// conversion that runs plain attention with a V cache.
    pub(crate) fn is_mla(s: &ModelSummary) -> bool {
        s.key_length_mla.is_some_and(|n| n > 0) && s.value_length_mla.is_some_and(|n| n > 0)
    }

    /// The layout of `s`'s MTP context: its `nextn_predict_layers`, each a
    /// full-attention layer of the main model's shape with a cache of its
    /// own. `None` when the header has no MTP layers.
    pub(crate) fn mtp(s: &ModelSummary, main: &Self) -> Option<Self> {
        let n = s.nextn_predict_layers.filter(|n| *n > 0)?;
        let mut full = main.full?;
        full.own_layers = n;
        full.own_heads = n.saturating_mul(full.max_heads);
        Some(Self {
            full: Some(full),
            swa: None,
            recurrent_layers: 0,
            q_width: main.q_width,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gemma4_e4b() -> ModelSummary {
        // 42 layers, every 6th global, the last 18 sharing a cache, 2 KV
        // heads of 512 (global) and 256 (window), window 512.
        ModelSummary {
            block_count: Some(42),
            head_count: Some(8),
            embedding_length: Some(2560),
            head_count_kv: Some(2),
            key_length: Some(512),
            value_length: Some(512),
            key_length_swa: Some(256),
            value_length_swa: Some(256),
            sliding_window: Some(512),
            sliding_window_layers: Some((0..42).map(|i| (i + 1) % 6 != 0).collect()),
            sliding_window_pattern: Some(42),
            sliding_window_pattern_is_array: true,
            shared_kv_layers: Some(18),
            ..Default::default()
        }
    }

    /// Measured (b11549): E4B's global cache holds 4 layers and its window
    /// cache 20, out of 42: the last 18 reuse earlier ones.
    #[test]
    fn shared_kv_layers_own_no_cache() {
        let l = Layout::of(&gemma4_e4b()).unwrap();
        let full = l.full.unwrap();
        let (swa, window) = l.swa.unwrap();
        assert_eq!((full.own_layers, full.own_heads, full.dk), (4, 8, 512));
        assert_eq!((swa.own_layers, swa.own_heads, swa.dk), (20, 40, 256));
        assert_eq!(window, 512);
        assert_eq!(l.recurrent_layers, 0);
        assert_eq!(l.q_width, 8 * 512);
    }

    /// Qwen3.5: every 4th layer attends, the rest keep a recurrent state.
    #[test]
    fn a_hybrid_attends_every_nth_layer() {
        let s = ModelSummary {
            block_count: Some(64),
            head_count: Some(24),
            embedding_length: Some(5120),
            head_count_kv: Some(4),
            key_length: Some(256),
            value_length: Some(256),
            full_attention_interval: Some(4),
            ..Default::default()
        };
        let l = Layout::of(&s).unwrap();
        assert_eq!(l.full.unwrap().own_layers, 16);
        assert_eq!(l.full.unwrap().own_heads, 64);
        assert_eq!(l.recurrent_layers, 48);
        assert!(l.swa.is_none());
    }

    /// Zero-head layers of a per-layer array are recurrent (LFM2), and MLA
    /// caches K only (Ling).
    #[test]
    fn zero_head_layers_are_recurrent_and_mla_keeps_no_v() {
        let s = ModelSummary {
            block_count: Some(6),
            head_count: Some(16),
            embedding_length: Some(1536),
            head_count_kv_per_layer: Some(vec![0, 0, 1, 0, 0, 1]),
            key_length: Some(576),
            value_length: Some(128),
            kv_lora_rank: Some(512),
            key_length_mla: Some(192),
            value_length_mla: Some(128),
            has_recurrent_keys: true,
            ..Default::default()
        };
        let l = Layout::of(&s).unwrap();
        let full = l.full.unwrap();
        assert_eq!(
            (full.own_layers, full.own_heads, full.dk, full.dv),
            (2, 2, 576, 0)
        );
        assert_eq!(l.recurrent_layers, 4);
    }

    /// Review must-fix 1: `kv_lora_rank` alone is a DeepSeek or Kimi file
    /// converted before the `*_length_mla` keys, which llama.cpp runs as
    /// plain attention with a V cache (`llama_hparams::is_mla`).
    #[test]
    fn mla_needs_its_mla_head_dims_not_the_rank_alone() {
        let old = ModelSummary {
            block_count: Some(27),
            head_count: Some(16),
            embedding_length: Some(2048),
            head_count_kv: Some(16),
            key_length: Some(192),
            value_length: Some(128),
            kv_lora_rank: Some(512),
            ..Default::default()
        };
        assert!(!Layout::is_mla(&old));
        let full = Layout::of(&old).unwrap().full.unwrap();
        assert_eq!((full.own_heads, full.dk, full.dv), (27 * 16, 192, 128));
        let absorbed = ModelSummary {
            head_count_kv: Some(1),
            key_length: Some(576),
            value_length: Some(512),
            key_length_mla: Some(192),
            value_length_mla: Some(128),
            ..old
        };
        assert!(Layout::is_mla(&absorbed));
        let full = Layout::of(&absorbed).unwrap().full.unwrap();
        assert_eq!((full.own_heads, full.dk, full.dv), (27, 576, 0));
    }

    /// Zero-head layers of a header without recurrent keys are
    /// attention-free (Deci-style): no state, so nothing to size or name.
    #[test]
    fn attention_free_layers_keep_no_state() {
        let s = ModelSummary {
            block_count: Some(4),
            head_count: Some(8),
            embedding_length: Some(1024),
            head_count_kv_per_layer: Some(vec![8, 0, 8, 0]),
            ..Default::default()
        };
        let l = Layout::of(&s).unwrap();
        assert_eq!(l.recurrent_layers, 0);
        assert_eq!(l.full.unwrap().own_layers, 2);
    }

    /// Review should-fix 2: a window array without `attention.sliding_window`
    /// leaves every layer full, never in a window cache that then vanishes.
    #[test]
    fn a_window_array_without_a_window_is_all_full() {
        let mut s = gemma4_e4b();
        s.sliding_window = None;
        s.shared_kv_layers = None;
        let l = Layout::of(&s).unwrap();
        assert!(l.swa.is_none());
        assert_eq!(l.full.unwrap().own_layers, 42);
    }

    /// A Qwen2 header states no key_length: heads are n_embd / head_count,
    /// llama.cpp's own default, and an integer window pattern splits the
    /// layers (every Nth global).
    #[test]
    fn head_size_defaults_like_llama_cpp_and_an_integer_pattern_splits() {
        let s = ModelSummary {
            block_count: Some(28),
            head_count: Some(12),
            embedding_length: Some(1536),
            head_count_kv: Some(2),
            ..Default::default()
        };
        let full = Layout::of(&s).unwrap().full.unwrap();
        assert_eq!((full.own_layers, full.dk, full.dv), (28, 128, 128));
        let windowed = ModelSummary {
            sliding_window: Some(2048),
            sliding_window_pattern: Some(4),
            ..s
        };
        let l = Layout::of(&windowed).unwrap();
        assert_eq!(l.full.unwrap().own_layers, 7);
        assert_eq!(l.swa.unwrap().0.own_layers, 21);
        assert_eq!(l.swa.unwrap().1, 2048);
    }

    #[test]
    fn a_missing_value_is_named() {
        let e = Layout::of(&ModelSummary::default()).unwrap_err();
        assert!(e.contains("block_count"), "{e}");
        let s = ModelSummary {
            block_count: Some(4),
            head_count: Some(4),
            embedding_length: Some(256),
            ..Default::default()
        };
        let e = Layout::of(&s).unwrap_err();
        assert!(e.contains("head_count_kv"), "{e}");
    }
}
