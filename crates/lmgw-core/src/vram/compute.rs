//! The compute buffer llama.cpp reserves beside a model's weights and KV
//! cache, derived from the GGUF header and the row's batch sizes. This
//! module's formula is an aux row's, whose graph is reserved for an output
//! per token; a chat row's contexts are reserved for a few outputs per slot,
//! and their formula is [`chat`], over the attention layout [`layers`]
//! reads from the header.
//!
//! llama.cpp sizes it once, at load, for the worst graph it can be handed
//! (`sched_reserve`: one physical batch of `n_ubatch` tokens, every one an
//! output), and holds it for the container's life, so it is a resident cost
//! like the KV cache. It grows with the ubatch, and for a decoder with the
//! context too. The terms below were fitted to llama.cpp's own
//! `CUDA0 compute buffer size` lines (b11549, `-lv 4`), 34 loads of seven
//! aux GGUFs on a 24 GB GPU at ubatch 512 to 8192, fully offloaded and at
//! `--n-gpu-layers 0` with the device passed (decision-models design, owner
//! item b and Appendix C; the probe logs are not in the repo):
//!
//! | model | placement | ubatch | ctx | measured | this |
//! |---|---|---|---|---|---|
//! | Qwen3-Embedding-0.6B | GPU | 512 / 8192 | 32768 | 336 / 5,380 MiB | 336 / 5,380 |
//! | Qwen3-Embedding-4B | GPU | 512 / 8192 | 40960 | 356 / 5,700 | 356 / 5,700 |
//! | Qwen3-Reranker-0.6B | GPU | 2048 | 40960 | 1,377 | 1,377 |
//! | Kev-4B (decision) | GPU | 512 / 8192 | 32768 | 537 / 8,592 | 537 / 8,592 |
//! | Qwen3-VL-Embedding-2B | GPU | 2048 | 32768 | 1,603 | 1,571 (−2 %) |
//! | embeddinggemma-300M | GPU | 2048 | 2048 | 67 | 67 |
//! | Laya (decision) | GPU | 8192 | 8192 | 630 | 768 (+22 %) |
//! | Qwen3-Embedding-4B | `-ngl 0` | 8192 | 40960 | 5,363 | 5,363 |
//! | Kev-4B | `-ngl 0` | 2048 | 32768 | 2,664 | 2,664 |
//! | Qwen3-Embedding-0.6B | GPU, `-fa off` | 2048 | 32768 | 4,432 | 4,384 (−1 %) |
//!
//! **The terms.** `ub` is the physical batch the graph is reserved for:
//! `min(ubatch, batch, ctx)`, since llama.cpp lowers the ubatch to the batch
//! and reserves no more tokens than the context holds.
//! - **Logits** (decoders only): `4 · vocab · ub` bytes. A decoder's graph
//!   ends in its LM head whatever the server reads from it, embeddings and
//!   rerank scores included, so this dominates every Qwen-family row: 296 MiB
//!   at ub 512 for a 151k vocabulary, 4.6 GiB at 8192. Beside it stay
//!   `16 · n_embd_inp` bytes per token of hidden state, where `n_embd_inp`
//!   is `embedding_length × (1 + n_deepstack_layers)`.
//! - **The attention mask**, f16 with flash attention: `2 · n_kv · ub`
//!   bytes, once more for a sliding-window model's second mask. `n_kv` is
//!   the KV cache's size (the context) for a decoder and the batch itself
//!   for an encoder, which keeps no cache.
//! - **An encoder's layers**, where its peak is: `4 · (4 · n_embd_inp +
//!   3 · feed_forward_length) · ub` bytes. Exact for embeddinggemma; Laya's
//!   per-layer widths differ, and the largest one makes it 20–30 % high.
//! - **Flash attention off** (`--flash-attn off`, or llama.cpp's other false values, in the row's args): the
//!   mask is f32 and the scores are materialized, `4 · head_count · n_kv · ub`
//!   bytes, three times the whole buffer above for Qwen3-Embedding-0.6B at
//!   ub 2048. llama-server's default (`auto`) turned it on for every model
//!   measured, so `auto` and `on` are counted as on.
//! - **Weights on the host** (`--n-gpu-layers 0` with the device passed):
//!   llama.cpp still runs the large-batch matrix products on the card,
//!   copying their weights over, and attention stays on the host with the
//!   KV cache. A decoder's buffer is then its logits term plus a copy of
//!   the output head (`output_head_bytes`) and no mask; an encoder's is the
//!   same as on the GPU (measured within 2 %, except Laya at ub 512: 46 MiB
//!   for 33).
//!
//! **Decoder or encoder** is read from the header: `attention.causal =
//! false` is an encoder (the BERT family's converters write it), and so is a
//! mean or cls pooling, which bidirectional sentence encoders declare. That
//! second signal is what catches embeddinggemma, whose header carries no
//! `attention.causal` (llama.cpp sets it for the architecture in code).
//! It is not certain: a causal decoder converted with mean pooling
//! (LLM2Vec- or jina-v4-style) would be counted as an encoder and charged
//! short by its logits. Charging those to every mean-pooled header would put
//! 2 GiB on embeddinggemma at ubatch 2048 against a measured 67 MiB, so the
//! estimate keeps the encoder figure and [`uncharged_if_decoder`] says how
//! much a decoder would add, which the footprint's note and the plan's
//! rationale name.
//!
//! **Not counted:** the CUDA context and cuBLAS workspace (0.38–0.44 GiB
//! per container on the measured card, the process's `nvidia-smi` figure less its
//! buffers), allocator slack, and a projector's own compute buffer, which
//! llama.cpp sizes from per-projector image limits no header carries
//! (Qwen3-VL-Embedding-2B's: 322 MiB). Those are what `vram.headroom_mb`
//! (default 1 GiB) is for. The default-ubatch part of the buffer is not
//! left to the headroom: 4 × vocab × 512 is already 296 MiB for Qwen3 and
//! 485 MiB for Kev, and with Kev's mask at its trained 262144 tokens its
//! buffer is 761 MiB at ubatch 512, past the headroom once the CUDA context
//! is added.

use crate::gguf::{pooling_name, ModelSummary};

pub(crate) mod chat;
pub(crate) mod layers;

/// Where a row's weights are when its compute buffer is reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Weights {
    /// Offloaded to the card (an aux row with no `backend`).
    Device,
    /// On the host, with the device still passed (`--n-gpu-layers 0` on a
    /// GPU image): the batch products run on the card all the same.
    Host,
}

/// What a row asks of llama.cpp that sizes the buffer, besides its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Ask {
    /// The context it runs with (the row's `ctx_size`, else the trained one).
    pub ctx: u64,
    /// The ubatch and batch in effect, llama-server's defaults filling in.
    pub ubatch: u64,
    pub batch: u64,
    /// `--flash-attn` set to a value llama.cpp reads as false in the row's args.
    pub flash_attn_off: bool,
    pub weights: Weights,
}

/// Whether the header describes a decoder graph: an LM head over a KV
/// cache. False for an encoder (module doc).
pub(crate) fn is_decoder(s: &ModelSummary) -> bool {
    s.attention_causal != Some(false) && !mean_or_cls(s)
}

/// The header's pooling is mean or cls.
fn mean_or_cls(s: &ModelSummary) -> bool {
    matches!(
        s.pooling_type.and_then(pooling_name),
        Some("mean") | Some("cls")
    )
}

/// The header is read as an encoder from its pooling alone: it states no
/// `attention.causal` (module doc). Such a model's non-causality is not
/// certain, so it keeps its KV-cache charge and its estimate names what a
/// decoder would add ([`uncharged_if_decoder`]).
pub(crate) fn encoder_by_pooling_alone(s: &ModelSummary) -> bool {
    s.attention_causal.is_none() && mean_or_cls(s)
}

/// What the compute buffer would grow by if a header read as an encoder
/// from its pooling alone were a causal decoder: its logits and a mask over
/// the whole context. `None` for any other header, or when the decoder
/// figure cannot be estimated.
pub(crate) fn uncharged_if_decoder(s: &ModelSummary, ask: &Ask) -> Option<u64> {
    if !encoder_by_pooling_alone(s) {
        return None;
    }
    let as_decoder = bytes_as(s, ask, true).ok()?;
    let charged = bytes_as(s, ask, false).ok()?;
    Some(as_decoder.saturating_sub(charged))
}

/// The physical batch the graph is reserved for (module doc).
pub(crate) fn reserved_ubatch(ask: &Ask) -> u64 {
    ask.ubatch.min(ask.batch).min(ask.ctx).max(1)
}

/// The compute buffer, in bytes, llama.cpp reserves on the card for a row
/// of model `s` asking `ask` (module doc). `Err` names the header value the
/// estimate needs and the file does not state; nothing is guessed in its
/// place.
pub(crate) fn bytes(s: &ModelSummary, ask: &Ask) -> Result<u64, String> {
    bytes_as(s, ask, is_decoder(s))
}

/// [`bytes`] with the graph's shape given: `decoder` (LM head, KV cache) or
/// an encoder.
fn bytes_as(s: &ModelSummary, ask: &Ask, decoder: bool) -> Result<u64, String> {
    let need = |v: Option<u64>, key: &str| {
        v.filter(|n| *n > 0)
            .ok_or_else(|| format!("the header states no {key}"))
    };
    let n_embd = need(s.embedding_length, "embedding_length")?;
    let inp = n_embd.saturating_mul(1 + s.deepstack_layers.unwrap_or(0));
    let ub = reserved_ubatch(ask);
    let n_kv = if decoder { ask.ctx } else { ub };
    let mul = |terms: &[u64]| terms.iter().fold(1u64, |a, b| a.saturating_mul(*b));

    let masks = 1 + u64::from(s.sliding_window.is_some());
    let mask_width = if ask.flash_attn_off { 4 } else { 2 };
    let mask = mul(&[masks, n_kv, ub, mask_width]);
    let hidden = mul(&[16, inp, ub]);
    let logits = if decoder {
        let vocab = need(s.vocab_size, "vocabulary (tokenizer.ggml.tokens)")?;
        mul(&[4, vocab, ub]).saturating_add(hidden)
    } else {
        0
    };
    // Both shapes peak at the larger of their logits (0 for an encoder) and
    // their layers. A decoder's width is optional: with a real vocabulary
    // (tens of times an FFN) its logits win, so a header without one is
    // still sized; a small vocabulary lets the layers win (the synthetic
    // decision headers in tests/it/decision_plan.rs rely on that). An
    // encoder needs the width.
    let layers = match s.feed_forward_length {
        Some(ff) => mul(&[
            4,
            ub,
            inp.saturating_mul(4).saturating_add(ff.saturating_mul(3)),
        ]),
        None if decoder => 0,
        None => need(None, "feed_forward_length")?,
    };
    let scores = if ask.flash_attn_off {
        let heads = need(s.head_count, "attention.head_count")?;
        mul(&[4, heads, n_kv, ub]).saturating_add(hidden)
    } else {
        0
    };
    Ok(match ask.weights {
        Weights::Host if decoder => {
            let head = need(
                s.output_head_bytes,
                "output head (output.weight or token_embd.weight)",
            )?;
            logits.saturating_add(head).max(layers)
        }
        Weights::Device | Weights::Host => mask.saturating_add(logits.max(layers).max(scores)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: f64 = 1024.0 * 1024.0;

    struct Header {
        vocab: u64,
        n_embd: u64,
        n_ff: u64,
        heads: u64,
        pooling: Option<u64>,
        causal: Option<bool>,
        swa: Option<u64>,
        deepstack: Option<u64>,
        head_bytes: u64,
    }

    fn summary(h: &Header) -> ModelSummary {
        ModelSummary {
            vocab_size: Some(h.vocab),
            embedding_length: Some(h.n_embd),
            feed_forward_length: Some(h.n_ff),
            head_count: Some(h.heads),
            pooling_type: h.pooling,
            attention_causal: h.causal,
            sliding_window: h.swa,
            deepstack_layers: h.deepstack,
            output_head_bytes: Some(h.head_bytes),
            ..Default::default()
        }
    }

    // The probe files' headers (`ggufhdr.py`, 2026-10-13).
    const Q06: Header = Header {
        vocab: 151_669,
        n_embd: 1024,
        n_ff: 3072,
        heads: 16,
        pooling: Some(3),
        causal: None,
        swa: None,
        deepstack: None,
        head_bytes: 165_015_872,
    };
    const Q4B: Header = Header {
        vocab: 151_665,
        n_embd: 2560,
        n_ff: 9728,
        heads: 32,
        head_bytes: 318_496_512,
        ..Q06
    };
    const RERANK06: Header = Header {
        pooling: None,
        ..Q06
    };
    const VL2B: Header = Header {
        vocab: 151_936,
        n_embd: 2048,
        n_ff: 6144,
        deepstack: Some(3),
        head_bytes: 255_252_480,
        ..Q06
    };
    const GEMMA: Header = Header {
        vocab: 262_144,
        n_embd: 768,
        n_ff: 1152,
        heads: 3,
        pooling: Some(1),
        causal: None,
        swa: Some(512),
        deepstack: None,
        head_bytes: 213_909_504,
    };
    const LAYA: Header = Header {
        vocab: 50_368,
        n_embd: 1024,
        n_ff: 4096,
        heads: 16,
        pooling: None,
        causal: Some(false),
        swa: Some(128),
        deepstack: None,
        head_bytes: 54_800_384,
    };
    const KEV: Header = Header {
        vocab: 248_320,
        n_embd: 2560,
        n_ff: 9216,
        heads: 16,
        pooling: None,
        causal: None,
        swa: None,
        deepstack: None,
        head_bytes: 675_430_400,
    };

    fn ask(ctx: u64, ub: u64, weights: Weights) -> Ask {
        Ask {
            ctx,
            ubatch: ub,
            batch: if ub == 512 { 2048 } else { ub },
            flash_attn_off: false,
            weights,
        }
    }

    /// Every probe, against llama.cpp's own `CUDA0 compute buffer size`:
    /// (header, ask, measured MiB, lowest and highest error in percent).
    #[test]
    fn the_formula_matches_llama_cpps_reserved_buffers() {
        use Weights::{Device as Gpu, Host};
        let off = |a: Ask| Ask {
            flash_attn_off: true,
            ..a
        };
        let cases: Vec<(&str, &Header, Ask, f64, f64, f64)> = vec![
            ("q06", &Q06, ask(32768, 512, Gpu), 336.24, -0.5, 0.5),
            ("q06", &Q06, ask(32768, 2048, Gpu), 1344.98, -0.5, 0.5),
            ("q06", &Q06, ask(32768, 8192, Gpu), 5379.91, -0.5, 0.5),
            ("q06", &Q06, ask(8192, 8192, Gpu), 4995.91, -0.5, 0.5),
            ("q4b", &Q4B, ask(40960, 512, Gpu), 356.24, -0.5, 0.5),
            ("q4b", &Q4B, ask(40960, 2048, Gpu), 1424.95, -0.5, 0.5),
            ("q4b", &Q4B, ask(40960, 8192, Gpu), 5699.78, -0.5, 0.5),
            ("rerank", &RERANK06, ask(40960, 512, Gpu), 344.24, -0.5, 0.5),
            (
                "rerank",
                &RERANK06,
                ask(40960, 8192, Gpu),
                5507.91,
                -0.5,
                0.5,
            ),
            ("vl2b", &VL2B, ask(32768, 512, Gpu), 400.77, -2.5, 0.5),
            ("vl2b", &VL2B, ask(32768, 2048, Gpu), 1603.09, -2.5, 0.5),
            ("gemma", &GEMMA, ask(2048, 512, Gpu), 13.77, -0.5, 0.5),
            ("gemma", &GEMMA, ask(2048, 2048, Gpu), 67.07, -0.5, 0.5),
            ("laya", &LAYA, ask(8192, 512, Gpu), 25.02, 0.0, 35.0),
            ("laya", &LAYA, ask(8192, 8192, Gpu), 630.14, 0.0, 25.0),
            ("kev", &KEV, ask(32768, 512, Gpu), 537.02, -0.5, 0.5),
            ("kev", &KEV, ask(262_144, 2048, Gpu), 3044.1, -0.5, 0.5),
            ("kev", &KEV, ask(32768, 8192, Gpu), 8592.34, -0.5, 0.5),
            ("q06", &Q06, ask(32768, 512, Host), 463.60, -1.5, 0.5),
            ("q06", &Q06, ask(32768, 8192, Host), 5092.28, -1.5, 0.5),
            ("q4b", &Q4B, ask(40960, 512, Host), 619.97, -0.5, 0.5),
            ("q4b", &Q4B, ask(8192, 8192, Host), 5363.34, -0.5, 0.5),
            ("vl2b", &VL2B, ask(32768, 2048, Host), 1718.44, -2.5, 0.5),
            ("gemma", &GEMMA, ask(2048, 2048, Host), 68.22, -2.5, 0.5),
            ("kev", &KEV, ask(32768, 2048, Host), 2664.16, -0.5, 0.5),
            ("q06", &Q06, off(ask(32768, 2048, Gpu)), 4432.05, -1.5, 0.5),
            (
                "gemma",
                &GEMMA,
                off(ask(2048, 2048, Gpu)),
                110.07,
                -6.0,
                0.5,
            ),
        ];
        for (name, h, a, measured, lo, hi) in cases {
            let got = bytes(&summary(h), &a).unwrap() as f64 / MIB;
            let err = (got - measured) / measured * 100.0;
            assert!(
                (lo..=hi).contains(&err),
                "{name} {a:?}: {got:.1} MiB against {measured} measured ({err:+.1} %)"
            );
        }
    }

    /// The ubatch above the batch or the context is not the one reserved.
    #[test]
    fn the_reserved_batch_is_the_smallest_of_ubatch_batch_and_context() {
        let a = |ctx, ubatch, batch| Ask {
            ctx,
            ubatch,
            batch,
            flash_attn_off: false,
            weights: Weights::Device,
        };
        assert_eq!(reserved_ubatch(&a(32768, 8192, 2048)), 2048);
        assert_eq!(reserved_ubatch(&a(1024, 8192, 8192)), 1024);
        assert_eq!(reserved_ubatch(&a(32768, 512, 2048)), 512);
        assert_eq!(
            bytes(&summary(&Q06), &a(32768, 8192, 2048)),
            bytes(&summary(&Q06), &a(32768, 2048, 2048))
        );
    }

    /// The header decides decoder or encoder, with embeddinggemma's mean
    /// pooling standing in for the `attention.causal` its header lacks.
    #[test]
    fn decoder_or_encoder_is_read_from_the_header() {
        assert!(is_decoder(&summary(&Q06)));
        assert!(is_decoder(&summary(&RERANK06)));
        assert!(is_decoder(&summary(&KEV)));
        assert!(!is_decoder(&summary(&LAYA)));
        assert!(!is_decoder(&summary(&GEMMA)));
        let cls = Header {
            pooling: Some(2),
            ..Q06
        };
        assert!(!is_decoder(&summary(&cls)));
    }

    /// Review finding 3: an encoder read from its pooling alone is not
    /// certain, and what a decoder would add is said, not dropped:
    /// embeddinggemma's would-be logits at ubatch 2048 (2 GiB of a 262k
    /// vocabulary) against the 67 MiB it was measured at. An encoder the
    /// header names (`attention.causal = false`) owes nothing.
    #[test]
    fn an_encoder_by_pooling_alone_names_what_a_decoder_would_add() {
        let gemma = summary(&GEMMA);
        assert!(encoder_by_pooling_alone(&gemma));
        let a = ask(2048, 2048, Weights::Device);
        let extra = uncharged_if_decoder(&gemma, &a).unwrap();
        // 4 × 262144 × 2048 + 16 × 768 × 2048, less the encoder's 51 MiB of
        // layers; the masks are the same at ctx = ubatch.
        let logits = 4 * 262_144 * 2048 + 16 * 768 * 2048;
        let layers = 4 * 2048 * (4 * 768 + 3 * 1152);
        assert_eq!(extra, logits - layers);
        assert_eq!(bytes(&gemma, &a).unwrap() as f64 / MIB, 67.0);
        assert!(!encoder_by_pooling_alone(&summary(&LAYA)));
        assert_eq!(uncharged_if_decoder(&summary(&LAYA), &a), None);
        assert_eq!(uncharged_if_decoder(&summary(&Q06), &a), None);
    }

    /// A value the estimate needs and the header does not state is named,
    /// never filled in.
    #[test]
    fn a_missing_header_value_is_named() {
        let mut s = summary(&Q06);
        s.vocab_size = None;
        let e = bytes(&s, &ask(4096, 512, Weights::Device)).unwrap_err();
        assert!(e.contains("vocabulary"), "{e}");
        let mut s = summary(&Q06);
        s.output_head_bytes = None;
        assert!(bytes(&s, &ask(4096, 512, Weights::Device)).is_ok());
        let e = bytes(&s, &ask(4096, 512, Weights::Host)).unwrap_err();
        assert!(e.contains("output head"), "{e}");
        let mut s = summary(&GEMMA);
        s.feed_forward_length = None;
        let e = bytes(&s, &ask(2048, 512, Weights::Device)).unwrap_err();
        assert!(e.contains("feed_forward_length"), "{e}");
        let e = bytes(&ModelSummary::default(), &ask(2048, 512, Weights::Device)).unwrap_err();
        assert!(e.contains("embedding_length"), "{e}");
    }
}
