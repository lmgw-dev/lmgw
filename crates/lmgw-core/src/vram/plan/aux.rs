//! An aux row's footprint (decision-models design §3.3, owner item b): its
//! weights and projector, its KV cache, and the compute buffer
//! [`crate::vram::compute`] derives, by where its container computes.
//!
//! - **On the GPU** (no `backend`, no `--n-gpu-layers 0`): all three terms.
//! - **`--n-gpu-layers 0` with the device passed** (a GPU image, run args
//!   that pass the GPU, `backend` unset): the weights and KV cache stay in
//!   host RAM, but llama.cpp still opens the device, runs the large-batch
//!   products there and loads the projector there. Charged are the compute
//!   buffer and the projector. Measured: ~5.7 GiB for Qwen3-Embedding-4B at
//!   ubatch 8192, which used to be charged nothing. The boot hoist moves the
//!   flag into `backend: cpu` only on an image without a GPU backend, so a
//!   row left in this state runs on a GPU image or one lmgw could not read,
//!   and the charge errs on the side of the card.
//! - **`--n-gpu-layers 0` with no device**, and **`backend: cpu`**: nothing on
//!   the card.

use crate::config::{AuxModel, RouterSettings};
use crate::runtime::argv::flag_value;
use crate::runtime::aux::{self as aux_rt, LLAMA_DEFAULT_BATCH, LLAMA_DEFAULT_UBATCH};
use crate::vram::compute::{self, Ask, Weights};

use super::{cache_bits, cpu_only, cpu_only_args, kv_footprint, Footprint, GgufFiles, PlanCache};

/// Where an aux row's container computes, as the footprint charges it
/// (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Placement {
    /// `backend: cpu`.
    Cpu,
    /// `--n-gpu-layers 0` in its args, and its run args pass no GPU.
    HostNoDevice,
    /// `--n-gpu-layers 0` in its args, and its run args pass the GPU.
    HostWithDevice,
    /// Everything offloaded.
    Gpu,
}

impl Placement {
    /// `m`'s placement, its run args read as the descriptor renders them
    /// from `class` ([`aux_rt::run_args`]).
    pub(crate) fn of(m: &AuxModel, class: &RouterSettings) -> Self {
        if m.backend.is_some() {
            Self::Cpu
        } else if !cpu_only_args(&m.args) {
            Self::Gpu
        } else if aux_rt::passes_gpu(&aux_rt::run_args(m, class)) {
            Self::HostWithDevice
        } else {
            Self::HostNoDevice
        }
    }
}

/// The batch sizes and attention switch a row runs with: what the compute
/// buffer reads besides the header and the context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Batches {
    /// The ubatch and batch in effect ([`aux_rt::ubatch_in_effect`]),
    /// llama-server's defaults filling in.
    pub ubatch: u64,
    pub batch: u64,
    /// `--flash-attn` (or `-fa`) in the row's args set to a value llama.cpp
    /// reads as false ([`FLASH_ATTN_OFF`]); llama-server's `auto` turns it on
    /// for every model measured.
    pub flash_attn_off: bool,
}

/// The values llama.cpp's `-fa` takes as off: `common_arg_utils::is_falsey`
/// (common/arg.cpp, b11549).
pub(crate) const FLASH_ATTN_OFF: [&str; 4] = ["off", "disabled", "false", "0"];

impl Batches {
    pub(crate) fn of(m: &AuxModel) -> Self {
        let count = |c: Option<(i64, aux_rt::CountFrom)>, default: i64| {
            c.map(|(n, _)| n)
                .filter(|n| *n > 0)
                .unwrap_or(default)
                .unsigned_abs()
        };
        Self {
            ubatch: count(aux_rt::ubatch_in_effect(m), LLAMA_DEFAULT_UBATCH),
            batch: count(aux_rt::batch_in_effect(m), LLAMA_DEFAULT_BATCH),
            flash_attn_off: flag_value(&m.args, "flash-attn")
                .is_some_and(|v| FLASH_ATTN_OFF.contains(&v.trim())),
        }
    }
}

/// The footprint of an aux row whose files were read (`files`), at the
/// context `ctx` (the row's, else the trained one), cache widths `bits`,
/// `placement` and `batches`. Pure, so the plan can ask it at other
/// contexts without reading the files again.
pub(crate) fn footprint(
    files: &GgufFiles,
    gguf_path: &str,
    ctx: Option<i64>,
    bits: (u32, u32),
    placement: Placement,
    batches: Batches,
) -> Footprint {
    let fp = no_cache_for_an_encoder(kv_footprint(files, gguf_path, ctx, bits.0, bits.1), files);
    match placement {
        Placement::Cpu | Placement::HostNoDevice => cpu_only(fp),
        Placement::HostWithDevice => {
            let projector = if files.companion_bytes > 0 {
                ", and its projector, which loads there"
            } else {
                ""
            };
            let on_card =
                Footprint::new(files.companion_bytes, 0, fp.ctx_tokens, None).noting(format!(
                    "n-gpu-layers 0, but its container gets the GPU: the weights and KV cache \
                     stay in host RAM while llama.cpp runs the batch products on the card, so \
                     charged are its compute buffer{projector}"
                ));
            with_compute(on_card, files, Weights::Host, batches)
        }
        Placement::Gpu => with_compute(fp, files, Weights::Device, batches),
    }
}

/// `fp` without its KV cache when the header says the model is an encoder
/// (`attention.causal = false`): llama.cpp gives it no KV cache (measured 0
/// for Laya). Where the encoder reading rests on the pooling alone
/// (embeddinggemma, [`compute::encoder_by_pooling_alone`]) the cache stays
/// charged, the safe side.
fn no_cache_for_an_encoder(fp: Footprint, files: &GgufFiles) -> Footprint {
    match &files.summary {
        // Whatever the KV sizing said (a header without KV heads, which an
        // encoder's often lacks) is moot: there is no cache to size.
        Ok(s) if s.attention_causal == Some(false) => Footprint::new(
            fp.weights_bytes,
            0,
            fp.ctx_tokens,
            Some(
                "no KV cache: the header declares the model non-causal (attention.causal = \
                 false), and llama.cpp keeps none for an encoder"
                    .into(),
            ),
        ),
        _ => fp,
    }
}

/// `fp` with the compute buffer of a row asking `batches` charged on top,
/// or a note saying why it could not be sized.
fn with_compute(fp: Footprint, files: &GgufFiles, weights: Weights, b: Batches) -> Footprint {
    let s = match &files.summary {
        Ok(s) => s,
        Err(e) => {
            return fp.noting(format!(
                "compute buffer not sized: the header could not be read ({e})"
            ))
        }
    };
    let Some(ctx) = fp.ctx_tokens else {
        return fp.noting(
            "compute buffer not sized: no context to size it for (the row sets no ctx_size \
             and the header states none)"
                .into(),
        );
    };
    let ask = Ask {
        ctx,
        ubatch: b.ubatch,
        batch: b.batch,
        flash_attn_off: b.flash_attn_off,
        weights,
    };
    match compute::bytes(s, &ask) {
        Ok(c) => {
            let fp = fp.with_compute(c);
            match compute::uncharged_if_decoder(s, &ask) {
                Some(extra) => fp.noting(uncertain_encoder(extra)),
                None => fp,
            }
        }
        Err(why) => fp.noting(format!(
            "compute buffer not sized: {why}, so the estimate leaves it out"
        )),
    }
}

/// What the estimate says of a model read as an encoder from its pooling
/// alone ([`compute::uncharged_if_decoder`]): the figure it does not charge.
pub(crate) fn uncertain_encoder(extra: u64) -> String {
    format!(
        "its compute buffer is sized as an encoder's from its mean or cls pooling alone (the \
         header states no attention.causal); if llama.cpp runs it as a causal decoder, its \
         logits and a whole-context mask add about {} that this estimate does not charge",
        crate::hf::fmt_bytes(extra)
    )
}

impl PlanCache {
    /// Row `m`'s footprint (module doc); `class` is the aux class's
    /// settings, for its models dir and the run args a row without its own
    /// inherits.
    pub async fn aux(&self, class: &RouterSettings, m: &AuxModel) -> Footprint {
        // Aux sections have no dedicated cache-type fields; the escape hatch is
        // the only place either can be set, so that is where they are read from.
        let bits = (
            cache_bits(flag_value(&m.args, "cache-type-k")),
            cache_bits(flag_value(&m.args, "cache-type-v")),
        );
        let ctx = m.ctx_size;
        // The projector is loaded beside the weights (decision-models design
        // §3.3). The field first; a `--mmproj`/`-mm` still riding in the
        // escape hatch renders too, so it is charged too.
        let companions: Vec<String> = m
            .mmproj_path
            .clone()
            .or_else(|| {
                flag_value(&m.args, "mmproj").map(|p| p.trim_start_matches("/models/").to_string())
            })
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect();
        let placement = Placement::of(m, class);
        let batches = Batches::of(m);
        let key = format!(
            "aux|{}|{}|{ctx:?}|{bits:?}|{placement:?}|{batches:?}|{}",
            class.models_dir,
            m.gguf_path,
            companions.join(",")
        );
        let (dir, path) = (class.models_dir.clone(), m.gguf_path.clone());
        self.get_or_insert(key, move || {
            let files = GgufFiles::read(&dir, &path, &companions);
            footprint(&files, &path, ctx, bits, placement, batches)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuxBackend, AuxKind};
    use crate::gguf::synth;

    const REL: &str = "org/embedder/embedder.gguf";
    const MMPROJ: &str = "org/embedder/mmproj.gguf";
    const HEAD: u64 = 64_000;

    /// A small causal last-pooling embedder with everything the compute
    /// term reads, and a projector beside it.
    fn files(dir: &std::path::Path) {
        let tokens: Vec<String> = (0..1000).map(|i| format!("t{i}")).collect();
        let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();
        let mut h = synth::embedding("qwen3", 3, 4096);
        h.u32("qwen3.feed_forward_length", 192)
            .u32("qwen3.attention.head_count", 4)
            .u32("qwen3.attention.head_count_kv", 2)
            .u32("qwen3.attention.key_length", 16)
            .u32("qwen3.attention.value_length", 16)
            .arr_str("tokenizer.ggml.tokens", &tokens)
            .tensor_sized("token_embd.weight", HEAD);
        h.write_to(&dir.join(REL));
        let mut p = synth::embedding("clip", 0, 0);
        p.str("general.type", "mmproj")
            .tensor_sized("v.patch_embd.weight", 8192);
        p.write_to(&dir.join(MMPROJ));
    }

    fn row(args: &[&str], backend: Option<AuxBackend>, ubatch: Option<i64>) -> AuxModel {
        AuxModel {
            id: 1,
            model_id: "embedder".into(),
            gguf_path: REL.into(),
            kind: AuxKind::Embed,
            pooling: Some("last".into()),
            ctx_size: None,
            ubatch_size: ubatch,
            batch_size: ubatch,
            mmproj_path: Some(MMPROJ.into()),
            backend,
            threads: None,
            args: args.iter().map(|s| s.to_string()).collect(),
            idle_seconds: 0,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        }
    }

    fn compute_of(dir: &std::path::Path, ubatch: u64, weights: Weights) -> u64 {
        let s = crate::gguf::summarize(&dir.join(REL)).unwrap();
        let ask = Ask {
            ctx: 4096,
            ubatch,
            batch: if ubatch == 512 { 2048 } else { ubatch },
            flash_attn_off: false,
            weights,
        };
        compute::bytes(&s, &ask).unwrap()
    }

    /// Owner item b: the compute buffer is charged wherever the container
    /// gets the device, `--n-gpu-layers 0` on a GPU image included, and
    /// nowhere else.
    #[tokio::test]
    async fn the_compute_buffer_is_charged_where_the_container_gets_the_device() {
        let dir = tempfile::tempdir().unwrap();
        files(dir.path());
        let size = |rel: &str| std::fs::metadata(dir.path().join(rel)).unwrap().len();
        let (weights, mmproj) = (size(REL), size(MMPROJ));
        let gpu_class = RouterSettings {
            models_dir: dir.path().display().to_string(),
            ..RouterSettings::default_aux()
        };
        let no_gpu_class = RouterSettings {
            extra_run_args: vec!["--security-opt".into(), "label=disable".into()],
            ..gpu_class.clone()
        };
        let plans = PlanCache::default();

        // On the GPU: weights, projector, KV and the compute buffer at the
        // ubatch in effect — llama-server's 512 when unset, the field's
        // otherwise.
        for (ubatch, want) in [(None, 512), (Some(2048), 2048)] {
            let fp = plans.aux(&gpu_class, &row(&[], None, ubatch)).await;
            assert_eq!(fp.weights_bytes, weights + mmproj, "{fp:?}");
            assert!(fp.kv_cache_bytes > 0, "{fp:?}");
            assert_eq!(
                fp.compute_bytes,
                compute_of(dir.path(), want, Weights::Device),
                "{fp:?}"
            );
            assert_eq!(
                fp.total_bytes,
                fp.weights_bytes + fp.kv_cache_bytes + fp.compute_bytes
            );
        }
        // A copy of the ubatch in the freeform args counts like the field.
        let from_args = plans
            .aux(&gpu_class, &row(&["-ub", "2048", "-b", "2048"], None, None))
            .await;
        assert_eq!(
            from_args.compute_bytes,
            compute_of(dir.path(), 2048, Weights::Device)
        );

        // `-ngl 0` with the device: the projector and the host-weights
        // compute buffer, no weights or KV.
        let host = plans
            .aux(&gpu_class, &row(&["--n-gpu-layers", "0"], None, None))
            .await;
        assert_eq!(host.weights_bytes, mmproj, "{host:?}");
        assert_eq!(host.kv_cache_bytes, 0);
        assert_eq!(
            host.compute_bytes,
            compute_of(dir.path(), 512, Weights::Host)
        );
        // On the host the mask stays with the KV cache and the output head
        // is copied over: the GPU figure less its f16 mask over 4096 × 512,
        // plus the head's bytes.
        assert_eq!(
            host.compute_bytes,
            compute_of(dir.path(), 512, Weights::Device) - 2 * 4096 * 512 + HEAD,
            "{host:?}"
        );
        let note = host.note.as_deref().unwrap_or_default();
        assert!(
            note.contains("gets the GPU") && note.contains("projector"),
            "{note}"
        );
        assert_eq!(host.ctx_tokens, Some(4096));

        // `-ngl 0` without a device, and `backend: cpu`: nothing.
        let none = plans
            .aux(&no_gpu_class, &row(&["--n-gpu-layers", "0"], None, None))
            .await;
        assert_eq!(none.total_bytes, 0, "{none:?}");
        let cpu = plans
            .aux(&gpu_class, &row(&[], Some(AuxBackend::Cpu), Some(2048)))
            .await;
        assert_eq!(cpu.total_bytes, 0, "{cpu:?}");
        assert_eq!(cpu.compute_bytes, 0);
    }

    /// A header without what the term needs keeps weights + KV and says so.
    #[tokio::test]
    async fn a_compute_buffer_the_header_cannot_size_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = synth::embedding("qwen3", 3, 4096);
        h.u32("qwen3.attention.head_count_kv", 2)
            .u32("qwen3.attention.key_length", 16)
            .u32("qwen3.attention.value_length", 16);
        h.write_to(&dir.path().join(REL));
        let class = RouterSettings {
            models_dir: dir.path().display().to_string(),
            ..RouterSettings::default_aux()
        };
        let mut m = row(&[], None, None);
        m.mmproj_path = None;
        let fp = PlanCache::default().aux(&class, &m).await;
        assert_eq!(fp.compute_bytes, 0);
        assert_eq!(fp.total_bytes, fp.weights_bytes + fp.kv_cache_bytes);
        let note = fp.note.as_deref().unwrap_or_default();
        assert!(
            note.contains("compute buffer not sized") && note.contains("vocabulary"),
            "{note}"
        );
    }

    /// Every value llama.cpp's `is_falsey` takes turns flash attention off,
    /// under both spellings; its truthy and auto values do not (review
    /// finding 2: `-fa 0` was charged as on, 1.3 GiB for 4.4 at ubatch 2048).
    #[test]
    fn flash_attention_off_is_read_as_llama_cpp_reads_it() {
        for v in ["off", "disabled", "false", "0"] {
            assert!(
                Batches::of(&row(&["-fa", v], None, None)).flash_attn_off,
                "{v}"
            );
            let eq = format!("--flash-attn={v}");
            assert!(Batches::of(&row(&[&eq], None, None)).flash_attn_off, "{v}");
        }
        for v in ["on", "enabled", "true", "1", "auto", "-1"] {
            assert!(
                !Batches::of(&row(&["--flash-attn", v], None, None)).flash_attn_off,
                "{v}"
            );
        }
        assert!(!Batches::of(&row(&[], None, None)).flash_attn_off);
    }

    /// A header that declares the model non-causal charges no KV cache (Laya
    /// measured 0); one read as an encoder from its mean pooling alone keeps
    /// the cache and its note names what a decoder would add.
    #[tokio::test]
    async fn an_encoder_s_cache_and_its_uncertainty() {
        let dir = tempfile::tempdir().unwrap();
        let kv = |h: &mut synth::Header, arch: &str| {
            h.u32(&format!("{arch}.feed_forward_length"), 192)
                .u32(&format!("{arch}.attention.head_count_kv"), 2)
                .u32(&format!("{arch}.attention.key_length"), 16)
                .u32(&format!("{arch}.attention.value_length"), 16)
                .arr_str("tokenizer.ggml.tokens", &["a", "b", "c"]);
        };
        let mut bert = synth::embedding("bert", 2, 4096);
        bert.bool_flag("bert.attention.causal", false);
        kv(&mut bert, "bert");
        bert.write_to(&dir.path().join("bert.gguf"));
        let mut gemma = synth::embedding("gemma-embedding", 1, 4096);
        kv(&mut gemma, "gemma-embedding");
        gemma.write_to(&dir.path().join("gemma.gguf"));
        let class = RouterSettings {
            models_dir: dir.path().display().to_string(),
            ..RouterSettings::default_aux()
        };
        let at = |rel: &str| {
            let mut m = row(&[], None, None);
            m.gguf_path = rel.into();
            m.pooling = None;
            m.mmproj_path = None;
            m
        };
        let plans = PlanCache::default();

        let bert = plans.aux(&class, &at("bert.gguf")).await;
        assert_eq!(bert.kv_cache_bytes, 0, "{bert:?}");
        assert!(bert.compute_bytes > 0);
        assert_eq!(bert.ctx_tokens, Some(4096), "the context is still reported");
        let note = bert.note.as_deref().unwrap_or_default();
        assert!(
            note.contains("no KV cache") && !note.contains("decoder"),
            "{note}"
        );

        let gemma = plans.aux(&class, &at("gemma.gguf")).await;
        assert!(gemma.kv_cache_bytes > 0, "the safe side: {gemma:?}");
        let note = gemma.note.as_deref().unwrap_or_default();
        assert!(
            note.contains("pooling alone") && note.contains("does not charge"),
            "{note}"
        );
    }
}
