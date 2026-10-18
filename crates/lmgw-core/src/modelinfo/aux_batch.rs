//! The ubatch and batch the aux plan sets for an embedder or reranker whose
//! inputs llama-server cannot split across ubatches (owner item a,
//! decision-models design §7): `ubatch_size` = `batch_size` = the planned
//! context, as the decision branch does for Laya. Such an input is evaluated
//! in one ubatch, so at llama-server's default of 512 every longer input fails
//! with a 500 whatever the context. Measured 2026-10-13 on b11549/b11561:
//! embeddinggemma-300M (mean pooling, ctx 2048) refused 1,492 tokens at the
//! default and answered at ubatch = batch = 2048, while Qwen3-Embedding-0.6B
//! (last pooling) and Qwen3-Reranker-0.6B (causal) answered 3,500 tokens at the
//! default. Those keep llama.cpp's defaults.
//!
//! The rule is llama-server's own ([`aux_rt::splits_input`]), read from the
//! header and the planned pooling: no table of architecture names.

use serde_json::{json, Value};

use crate::config::AuxKind;
use crate::gguf::ModelSummary;
use crate::runtime::aux::{self as aux_rt, LLAMA_DEFAULT_UBATCH};
use crate::vram::compute::{self, Ask, Weights};

/// Whether llama-server splits an input of this header across ubatches, as
/// the plan can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Split {
    /// It does ([`aux_rt::splits_input`]): a last-token embedder or a causal
    /// reranker. llama.cpp's batch defaults stay.
    Splits,
    /// It does not, and the clause says why: the whole input goes through
    /// one ubatch, planned at the context.
    OneUbatch(String),
    /// A causal (or unstated) embedder whose header declares no pooling: it
    /// splits with `last` pooling and does not with any other, and only the
    /// row's pooling, set from the model card, decides. Nothing is planned,
    /// since a ubatch the size of the context on a row that splits would
    /// charge its logits at that size for nothing (review finding 1: about
    /// 23 GiB at ctx 40960 with a 151k vocabulary).
    DependsOnPooling,
}

/// [`Split`] for a `kind` row of header `s` with `pooling`, the pooling the
/// plan sets (the header's).
pub(super) fn split_of(s: &ModelSummary, kind: AuxKind, pooling: Option<&str>) -> Split {
    let causal = s.attention_causal != Some(false);
    if kind == AuxKind::Embed && causal && pooling.is_none() {
        return Split::DependsOnPooling;
    }
    if aux_rt::splits_input(kind, pooling, causal) {
        return Split::Splits;
    }
    Split::OneUbatch(if !causal {
        "it is non-causal (attention.causal = false) and keeps no KV cache".to_string()
    } else {
        let p = pooling.unwrap_or("none");
        format!("its pooling ('{p}') reads every token, not only the last one")
    })
}

/// The decision branch's counterpart for embed and rerank rows: when
/// llama-server cannot split this model's inputs, plan `ubatch_size` =
/// `batch_size` = the trained context, or say why not. Returns whether the
/// two were planned.
pub(super) fn plan(
    s: &ModelSummary,
    kind: AuxKind,
    pooling: Option<&str>,
    put: &mut dyn FnMut(&str, Value, String),
    warnings: &mut Vec<String>,
) -> bool {
    let reason = match split_of(s, kind, pooling) {
        Split::Splits => return false,
        Split::DependsOnPooling => {
            warnings.push(format!(
                "no ubatch_size is planned, because the header declares no pooling_type and \
                 the pooling decides it: with 'last' (a causal embedder's) llama-server splits \
                 an input across batches and its defaults serve; with 'mean', 'cls' or 'none' \
                 it evaluates a whole input in one ubatch, and then set ubatch_size and \
                 batch_size to ctx_size{}, or every input over {LLAMA_DEFAULT_UBATCH} tokens \
                 fails with a 500",
                match s.context_length {
                    Some(c) => format!(" ({c})"),
                    None => String::new(),
                }
            ));
            return false;
        }
        Split::OneUbatch(reason) => reason,
    };
    let Some(ctx) = s.context_length else {
        warnings.push(format!(
            "the GGUF declares no context length, so ubatch_size and batch_size are not \
             planned either: llama-server evaluates a whole input of this model in one ubatch \
             ({reason}), and at its default of {LLAMA_DEFAULT_UBATCH} every input over \
             {LLAMA_DEFAULT_UBATCH} tokens fails with a 500 — set all three to the model \
             card's context"
        ));
        return false;
    };
    let ask = Ask {
        ctx,
        ubatch: ctx,
        batch: ctx,
        flash_attn_off: false,
        weights: Weights::Device,
    };
    let cost = match compute::bytes(s, &ask) {
        Ok(b) => {
            let mut cost = format!(
                "about {} on the GPU at this size, which the VRAM ledger charges with the row \
                 (on a backend=cpu row it is RAM)",
                crate::hf::fmt_bytes(b)
            );
            // Review finding 3: an encoder read from its pooling alone may
            // be a decoder, whose logits at this ubatch are not charged.
            if let Some(extra) = compute::uncharged_if_decoder(s, &ask) {
                cost.push_str(&format!(
                    ". That figure is an encoder's: {}",
                    crate::vram::plan::aux::uncertain_encoder(extra)
                ));
            }
            cost
        }
        Err(why) => format!("not estimated here ({why})"),
    };
    put(
        "ubatch_size",
        json!(ctx),
        format!(
            "= ctx_size ({ctx}, the header's context_length): llama-server cannot split one \
             input of this model across ubatches — {reason} — so a whole input has to fit one \
             ubatch, and embedding mode lowers the batch to the ubatch. At llama-server's \
             default of {LLAMA_DEFAULT_UBATCH}, every input over {LLAMA_DEFAULT_UBATCH} tokens \
             fails with a 500 (\"input (N tokens) is too large to process. increase the \
             physical batch size\") whatever the context. The compute buffer grows with it: \
             {cost}"
        ),
    );
    put(
        "batch_size",
        json!(ctx),
        "= ubatch_size: llama.cpp lowers the ubatch to the batch, and embedding mode lowers \
         the batch to the ubatch, so the two are raised together"
            .into(),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(causal: Option<bool>, pooling: Option<u64>) -> ModelSummary {
        ModelSummary {
            attention_causal: causal,
            pooling_type: pooling,
            context_length: Some(2048),
            embedding_length: Some(768),
            feed_forward_length: Some(1152),
            sliding_window: Some(512),
            vocab_size: Some(262_144),
            ..Default::default()
        }
    }

    fn reason(split: Split) -> String {
        match split {
            Split::OneUbatch(why) => why,
            other => panic!("not one ubatch: {other:?}"),
        }
    }

    /// llama-server's own rule, read from the header: only a last-token
    /// embedder and a causal reranker split their inputs, and a causal
    /// embedder without a declared pooling is left to the row's pooling.
    #[test]
    fn only_a_last_token_or_causal_rerank_input_splits() {
        let q3 = header(None, Some(3));
        assert_eq!(split_of(&q3, AuxKind::Embed, Some("last")), Split::Splits);
        assert_eq!(split_of(&q3, AuxKind::Rerank, None), Split::Splits);
        // embeddinggemma: no attention.causal in its header, mean pooling.
        let gemma = header(None, Some(1));
        let why = reason(split_of(&gemma, AuxKind::Embed, Some("mean")));
        assert!(why.contains("'mean'"), "{why}");
        let bert = header(Some(false), Some(2));
        let why = reason(split_of(&bert, AuxKind::Embed, Some("cls")));
        assert!(why.contains("non-causal"), "{why}");
        let why = reason(split_of(&bert, AuxKind::Rerank, None));
        assert!(why.contains("non-causal"), "{why}");
        // Review finding 1: no pooling in a causal header decides nothing.
        let bare = header(None, None);
        assert_eq!(
            split_of(&bare, AuxKind::Embed, None),
            Split::DependsOnPooling
        );
        // A non-causal one without a pooling still reads one ubatch.
        let why = reason(split_of(&header(Some(false), None), AuxKind::Embed, None));
        assert!(why.contains("non-causal"), "{why}");
    }

    /// Review finding 1: a header without a pooling plans no ubatch and says
    /// which pooling needs one.
    #[test]
    fn a_header_without_pooling_plans_no_ubatch_and_says_why() {
        let mut nothing = |_: &str, _: Value, _: String| panic!("nothing planned");
        let mut warnings = Vec::new();
        let bare = header(None, None);
        assert!(!plan(
            &bare,
            AuxKind::Embed,
            None,
            &mut nothing,
            &mut warnings
        ));
        assert!(
            warnings[0].contains("no pooling_type")
                && warnings[0].contains("'last'")
                && warnings[0].contains("ctx_size (2048)"),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_non_splitting_encoder_plans_its_context_as_ubatch_and_batch() {
        let mut params = serde_json::Map::new();
        let mut why = serde_json::Map::new();
        let mut put = |k: &str, v: Value, r: String| {
            params.insert(k.into(), v);
            why.insert(k.into(), Value::String(r));
        };
        let mut warnings = Vec::new();
        let gemma = header(None, Some(1));
        assert!(plan(
            &gemma,
            AuxKind::Embed,
            Some("mean"),
            &mut put,
            &mut warnings
        ));
        assert_eq!(params["ubatch_size"], 2048);
        assert_eq!(params["batch_size"], 2048);
        let ub = why["ubatch_size"].as_str().unwrap();
        assert!(
            ub.contains("too large to process") && ub.contains("'mean'"),
            "{ub}"
        );
        assert!(
            ub.contains("67.0 MiB") && ub.contains("charges"),
            "the measured 67 MiB at 2048: {ub}"
        );
        // Review finding 3: read as an encoder from its pooling alone, it
        // names the decoder logits it does not charge.
        assert!(
            ub.contains("an encoder's") && ub.contains("does not charge"),
            "{ub}"
        );
        assert!(warnings.is_empty());

        let q3 = header(None, Some(3));
        let mut nothing = |_: &str, _: Value, _: String| panic!("nothing planned");
        assert!(!plan(
            &q3,
            AuxKind::Embed,
            Some("last"),
            &mut nothing,
            &mut warnings
        ));

        let mut bare = gemma.clone();
        bare.context_length = None;
        assert!(!plan(
            &bare,
            AuxKind::Embed,
            Some("mean"),
            &mut nothing,
            &mut warnings
        ));
        assert!(
            warnings[0].contains("no context length") && warnings[0].contains("over 512"),
            "{warnings:?}"
        );
    }
}
