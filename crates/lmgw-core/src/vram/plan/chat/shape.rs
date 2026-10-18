//! What a chat row asks of llama.cpp that sizes its KV cache and compute
//! buffers, read from its typed params and, where a value still rides in its
//! freeform args, from there (the renderer passes those through, so the
//! container runs with them): the context, slots and a unified cache, the
//! batch sizes, flash attention, the cache types, a full-size window cache,
//! tensor overrides, and speculative decoding.
//!
//! Defaults are llama-server's own (b11549): ubatch 512, batch 2048, four
//! slots over a unified cache when `parallel` is left on auto, an f16 cache,
//! a draft window of 3 (`common_params_speculative_draft::n_max`), an f16
//! cache for a draft or MTP context (`--cache-type-k-draft`), and each
//! n-gram type's own window (`common_speculative_n_max`: 48 for the
//! n-gram maps, 64 for `ngram-mod`, 8 for `ngram-cache`).

use crate::config::LlamaParams;
use crate::runtime::argv::{canonical_key, flag_value};
use crate::runtime::aux::{LLAMA_DEFAULT_BATCH, LLAMA_DEFAULT_UBATCH};

use crate::vram::plan::aux::FLASH_ATTN_OFF;

/// llama.cpp's default draft window.
pub(crate) const LLAMA_DEFAULT_DRAFT_N_MAX: u64 = 3;

/// The speculative types that run a draft model or MTP head, and keep a
/// recurrent snapshot per draft position (`need_n_rs_seq`).
const DRAFT_TYPES: [&str; 5] = [
    "draft-simple",
    "draft-eagle3",
    "draft-mtp",
    "draft-dflash",
    "draft-dspark",
];

/// How a row decodes speculatively, as far as its memory goes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Second {
    /// No second context.
    None,
    /// `draft-mtp` without a draft model: the weights' own MTP layers run in
    /// a context of their own.
    Mtp,
    /// A draft model (`--spec-draft-model`, `--model-draft`, `-md`).
    /// `block` for the types that decode a whole block of draft positions in
    /// one pass (`draft-dflash`, `draft-dspark`), whose context is reserved
    /// for `n_max + 1` outputs per slot.
    Draft { path: String, block: bool },
}

/// A chat row's shape (module doc).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Shape {
    /// The row's context: `ctx_size` (field or `-c`), else a unified pool
    /// sized by `kv_unified_per_slot`, else `None` (the trained context
    /// applies).
    pub ctx: Option<u64>,
    pub slots: u64,
    pub unified: bool,
    pub ubatch: u64,
    pub batch: u64,
    pub flash_attn_off: bool,
    pub cache_k: Option<String>,
    pub cache_v: Option<String>,
    /// `--swa-full`: the window cache holds the whole context.
    pub swa_full: bool,
    /// A `--override-tensor` (`-ot`) in the args sends some tensor to a
    /// buffer other than the CPU's. llama.cpp applies the overrides to the
    /// input embeddings too, so they may sit on the card after all.
    pub tensors_to_device: bool,
    /// Speculative decoding: its second context, the largest draft window
    /// of its types (`common_speculative_n_max`, 0 without speculative
    /// decoding), which widens the main context's outputs, and the draft
    /// model's or MTP head's own window and cache types.
    pub second: Second,
    pub n_max: u64,
    pub draft_n_max: u64,
    /// The recurrent-state snapshots a speculative row keeps per slot for
    /// rolling back rejected drafts: the draft window for the draft types
    /// (`need_n_rs_seq`), else 0.
    pub rollback: u64,
    pub draft_cache_k: Option<String>,
    pub draft_cache_v: Option<String>,
}

impl Shape {
    /// The shape of a row with `params` and freeform `args`.
    pub(crate) fn of(params: &LlamaParams, args: &[String]) -> Self {
        let number = |key: &str| flag_value(args, key).and_then(|v| v.trim().parse::<i64>().ok());
        let count = |field: Option<i64>, key: &str, default: i64| {
            field
                .or_else(|| number(key))
                .filter(|n| *n > 0)
                .unwrap_or(default)
                .unsigned_abs()
        };
        let text = |field: &Option<String>, keys: &[&str]| {
            field
                .clone()
                .or_else(|| {
                    keys.iter()
                        .find_map(|k| flag_value(args, k))
                        .map(str::to_owned)
                })
                .map(|v| v.trim().to_ascii_lowercase())
                .filter(|v| !v.is_empty())
        };
        // The context, the slot count and the unified switch, with
        // llama-server's auto rule (`LlamaParams::effective_kv_unified`); a
        // copy left in the args counts as the field.
        let mut p = params.clone();
        p.ctx_size = p.ctx_size.or_else(|| number("ctx-size"));
        p.parallel = p.parallel.or_else(|| number("parallel"));
        p.kv_unified_per_slot = p
            .kv_unified_per_slot
            .or_else(|| number("kv-unified-per-slot"));
        if p.kv_unified.is_none() && present(args, "kv-unified") {
            p.kv_unified = Some(true);
        }
        let ctx = p.pool_tokens().filter(|c| *c > 0).map(i64::unsigned_abs);

        let types = text(&params.spec_type, &["spec-type"]).unwrap_or_default();
        let mut types: Vec<&str> = types
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty() && *t != "none")
            .collect();
        if present(args, "spec-default") {
            types.push("ngram-mod");
        }
        let has = |t: &str| types.contains(&t);
        let draft_n_max = params
            .spec_draft_n_max
            .or_else(|| number("spec-draft-n-max"))
            .map_or(LLAMA_DEFAULT_DRAFT_N_MAX, |n| n.max(0).unsigned_abs());
        let window =
            |key: &str, default: u64| number(key).map_or(default, |n| n.max(0).unsigned_abs());
        let n_max = types
            .iter()
            .map(|t| match *t {
                t if DRAFT_TYPES.contains(&t) => draft_n_max,
                "ngram-simple" => window("spec-ngram-simple-size-m", 48),
                "ngram-map-k" => window("spec-ngram-map-k-size-m", 48),
                "ngram-map-k4v" => window("spec-ngram-map-k4v-size-m", 48),
                "ngram-mod" => window("spec-ngram-mod-n-max", 64),
                "ngram-cache" => 8,
                // llama-server refuses a type it does not know.
                _ => 0,
            })
            .max()
            .unwrap_or(0);
        let draft_path = params
            .draft_gguf_path
            .clone()
            .filter(|p| !p.is_empty())
            .or_else(|| {
                ["spec-draft-model", "model-draft"]
                    .iter()
                    .find_map(|k| flag_value(args, k))
                    .map(|p| p.trim().trim_start_matches("/models/").to_string())
            });
        let second = match (&draft_path, has("draft-mtp")) {
            (Some(path), _) => Second::Draft {
                path: path.clone(),
                block: has("draft-dflash") || has("draft-dspark"),
            },
            (None, true) => Second::Mtp,
            (None, false) => Second::None,
        };
        let rollback = DRAFT_TYPES.iter().any(|t| has(t));
        Self {
            ctx,
            slots: p.effective_slots().unsigned_abs(),
            unified: p.effective_kv_unified(),
            ubatch: count(params.ubatch_size, "ubatch-size", LLAMA_DEFAULT_UBATCH),
            batch: count(params.batch_size, "batch-size", LLAMA_DEFAULT_BATCH),
            flash_attn_off: text(&params.flash_attn, &["flash-attn"])
                .is_some_and(|v| FLASH_ATTN_OFF.contains(&v.as_str())),
            cache_k: text(&params.cache_type_k, &["cache-type-k"]),
            cache_v: text(&params.cache_type_v, &["cache-type-v"]),
            swa_full: present(args, "swa-full"),
            tensors_to_device: overrides_to_device(args),
            second,
            n_max,
            draft_n_max,
            rollback: if rollback { draft_n_max } else { 0 },
            draft_cache_k: text(&None, &["cache-type-k-draft", "ctkd", "spec-draft-type-k"]),
            draft_cache_v: text(&None, &["cache-type-v-draft", "ctvd", "spec-draft-type-v"]),
        }
    }

    /// The outputs the main context is reserved for:
    /// `min(batch, slots × (1 + n_max))` (`server_output_limits`).
    pub(crate) fn main_outputs(&self) -> u64 {
        self.slots
            .saturating_mul(1 + self.n_max)
            .min(self.batch)
            .max(1)
    }
}

/// A flag (`--kv-unified`, `-kvu`, `--swa-full`) is present in `args`.
fn present(args: &[String], key: &str) -> bool {
    args.iter().any(|a| {
        let flag = a.trim_start_matches('-');
        let flag = flag.split_once('=').map_or(flag, |(k, _)| k);
        a.starts_with('-') && canonical_key(flag) == key
    })
}

/// Some `--override-tensor` / `-ot` pattern in `args` names a buffer other
/// than the CPU's (`pattern=CUDA0`; several may ride in one value,
/// comma-separated).
fn overrides_to_device(args: &[String]) -> bool {
    let mut it = args.iter().peekable();
    let mut values = Vec::new();
    while let Some(a) = it.next() {
        let flag = a.trim_start_matches('-');
        if !a.starts_with('-') {
            continue;
        }
        match flag.split_once('=') {
            Some((k, v)) if k == "ot" || k == "override-tensor" => values.push(v.to_string()),
            None if flag == "ot" || flag == "override-tensor" => {
                if let Some(v) = it.next() {
                    values.push(v.clone());
                }
            }
            _ => {}
        }
    }
    values.iter().flat_map(|v| v.split(',')).any(|o| {
        o.rsplit_once('=')
            .is_some_and(|(_, buf)| !buf.trim().to_ascii_uppercase().starts_with("CPU"))
    })
}

/// Bits per 32-element block of a llama.cpp cache type: its real size,
/// block scales included (q8_0 is 34 bytes per 32 values). `None` for a
/// spelling this table does not know, which the footprint names.
pub(crate) fn block_bits(t: Option<&str>) -> Option<u32> {
    Some(match t.unwrap_or("f16").trim() {
        "f32" => 1024,
        "" | "f16" | "bf16" => 512,
        "q8_0" => 272,
        "q5_1" => 192,
        "q5_0" => 176,
        "q4_1" => 160,
        "q4_0" | "iq4_nl" => 144,
        _ => return None,
    })
}

/// Flash attention reads a cache of this type through an f16 copy: every
/// type but f16 (the CUDA kernels' `need_f16_K/V`, b11549).
pub(crate) fn copied(t: Option<&str>) -> bool {
    !matches!(t.unwrap_or("f16").trim(), "" | "f16")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    /// A qwen3.6-27b row: its ubatch rides in the args (`-ub
    /// 256`), MTP from the weights, a window of 4.
    #[test]
    fn a_row_reads_its_fields_and_its_args() {
        let p = LlamaParams {
            ctx_size: Some(100_000),
            batch_size: Some(1024),
            parallel: Some(1),
            cache_type_k: Some("q8_0".into()),
            cache_type_v: Some("q8_0".into()),
            spec_type: Some("draft-mtp".into()),
            spec_draft_n_max: Some(4),
            ..Default::default()
        };
        let s = Shape::of(&p, &args(&["-ub", "256"]));
        assert_eq!(s.ctx, Some(100_000));
        assert_eq!(
            (s.ubatch, s.batch, s.slots, s.unified),
            (256, 1024, 1, false)
        );
        assert_eq!(s.second, Second::Mtp);
        assert_eq!((s.n_max, s.rollback, s.main_outputs()), (4, 4, 5));
        assert_eq!(s.cache_k.as_deref(), Some("q8_0"));
        assert_eq!(s.draft_cache_k, None);
        assert!(!s.swa_full && !s.tensors_to_device);
    }

    /// `parallel` left on auto is four slots over a unified cache, and a
    /// row without speculative decoding reserves one output per slot.
    #[test]
    fn auto_parallel_is_four_unified_slots() {
        let s = Shape::of(&LlamaParams::default(), &[]);
        assert_eq!(
            (s.slots, s.unified, s.ubatch, s.batch),
            (4, true, 512, 2048)
        );
        assert_eq!(
            (s.second.clone(), s.main_outputs(), s.rollback),
            (Second::None, 4, 0)
        );
        assert_eq!(s.ctx, None);
    }

    /// A draft model is a second context of its own; dflash decodes a block.
    #[test]
    fn a_draft_model_and_its_cache_types() {
        let p = LlamaParams {
            parallel: Some(2),
            draft_gguf_path: Some("org/m/dflash.gguf".into()),
            spec_type: Some("draft-dflash".into()),
            spec_draft_n_max: Some(16),
            ..Default::default()
        };
        let s = Shape::of(&p, &args(&["-ctkd", "q8_0", "--flash-attn", "off"]));
        assert_eq!(
            s.second,
            Second::Draft {
                path: "org/m/dflash.gguf".into(),
                block: true
            }
        );
        assert_eq!(s.draft_cache_k.as_deref(), Some("q8_0"));
        assert!(s.flash_attn_off);
        assert_eq!((s.main_outputs(), s.rollback), (34, 16));
        // The canonical spelling of the draft model's flag.
        let s = Shape::of(
            &LlamaParams::default(),
            &args(&["--spec-draft-model", "/models/org/m/d.gguf"]),
        );
        assert_eq!(
            s.second,
            Second::Draft {
                path: "org/m/d.gguf".into(),
                block: false
            }
        );
    }

    /// Review should-fix 3: each speculative type reserves its own window
    /// (`common_speculative_n_max`); `none` is no speculative decoding.
    #[test]
    fn each_speculative_type_widens_the_outputs_by_its_own_window() {
        let spec = |t: &str, extra: &[&str]| {
            let p = LlamaParams {
                parallel: Some(4),
                spec_type: Some(t.into()),
                ..Default::default()
            };
            Shape::of(&p, &args(extra))
        };
        assert_eq!(spec("ngram-mod", &[]).main_outputs(), 4 * 65);
        assert_eq!(spec("ngram-simple", &[]).n_max, 48);
        assert_eq!(spec("ngram-cache", &[]).n_max, 8);
        assert_eq!(
            spec("ngram-mod", &["--spec-ngram-mod-n-max", "16"]).n_max,
            16
        );
        assert_eq!(spec("draft-mtp,ngram-map-k", &[]).n_max, 48);
        assert_eq!(spec("draft-mtp,ngram-map-k", &[]).rollback, 3);
        assert_eq!(spec("ngram-mod", &[]).rollback, 0);
        let none = spec("none", &[]);
        assert_eq!((none.n_max, none.main_outputs()), (0, 4));
        let preset = Shape::of(&LlamaParams::default(), &args(&["--spec-default"]));
        assert_eq!(preset.n_max, 64);
    }

    /// The context, the per-slot pool and a full-size window cache in args.
    #[test]
    fn the_context_and_the_window_cache_ride_in_args() {
        let s = Shape::of(
            &LlamaParams::default(),
            &args(&["-c", "65536", "--swa-full"]),
        );
        assert_eq!(s.ctx, Some(65536));
        assert!(s.swa_full);
        let s = Shape::of(
            &LlamaParams::default(),
            &args(&["--kv-unified-per-slot", "8192"]),
        );
        assert_eq!(s.ctx, Some(4 * 8192), "auto parallel: 4 × the per-slot cap");
    }

    /// Review should-fix 5: an override to a device may put the input
    /// embeddings on the card; one to the CPU does not.
    #[test]
    fn tensor_overrides_to_a_device_are_noticed() {
        let shape = |a: &[&str]| Shape::of(&LlamaParams::default(), &args(a)).tensors_to_device;
        assert!(!shape(&[
            "--override-tensor",
            "blk\\.(5[8-9]|6[0-3])\\.ffn_(up|down|gate)\\.weight=CPU"
        ]));
        assert!(shape(&["-ot", "token_embd=CUDA0"]));
        assert!(shape(&["-ot", "exps=CPU,.*=CUDA0"]));
        assert!(shape(&["--override-tensor=.*=CUDA0"]));
        assert!(!shape(&[]));
    }

    #[test]
    fn cache_types_carry_their_block_scales() {
        assert_eq!(block_bits(Some("q8_0")), Some(272));
        assert_eq!(block_bits(None), Some(512));
        assert_eq!(block_bits(Some("q5_1")), Some(192));
        assert_eq!(block_bits(Some("tq2")), None);
        assert!(copied(Some("q8_0")) && copied(Some("bf16")) && !copied(None));
    }
}
