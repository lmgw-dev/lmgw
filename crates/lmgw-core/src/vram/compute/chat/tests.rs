//! The chat formula against llama.cpp's own `CUDA0 compute buffer size`
//! lines, one per context of every chat probe (b11549, 2026-10-13/14;
//! decision-models design, Appendix C). Headers are the probe files' own
//! (`hdr.py`).

use super::*;

const MIB: f64 = 1024.0 * 1024.0;
const GEMMA_VOCAB: u64 = 262_144;
const QWEN_VOCAB: u64 = 248_320;

/// Gemma 4's per-layer pattern: five windowed layers, then a global one.
fn gemma4_windows(n: usize) -> Vec<bool> {
    (0..n).map(|i| (i + 1) % 6 != 0).collect()
}

pub(crate) fn gemma4(
    n: usize,
    embd: u64,
    ff: u64,
    heads: u64,
    kv: (u64, u64),
    window: u64,
) -> ModelSummary {
    ModelSummary {
        block_count: Some(n as u64),
        embedding_length: Some(embd),
        feed_forward_length: Some(ff),
        head_count: Some(heads),
        head_count_kv_per_layer: Some(
            (0..n)
                .map(|i| if (i + 1) % 6 == 0 { kv.1 } else { kv.0 })
                .collect(),
        ),
        key_length: Some(512),
        value_length: Some(512),
        key_length_swa: Some(256),
        value_length_swa: Some(256),
        sliding_window: Some(window),
        sliding_window_layers: Some(gemma4_windows(n)),
        sliding_window_pattern: Some(n as u64),
        sliding_window_pattern_is_array: true,
        vocab_size: Some(GEMMA_VOCAB),
        ..Default::default()
    }
}

pub(crate) fn e4b() -> ModelSummary {
    ModelSummary {
        head_count_kv: Some(2),
        head_count_kv_per_layer: None,
        shared_kv_layers: Some(18),
        embedding_length_per_layer_input: Some(256),
        ..gemma4(42, 2560, 10240, 8, (2, 2), 512)
    }
}

/// A Gemma 4 MTP assistant: four layers sharing the target's cache.
pub(crate) fn assistant(embd: u64, heads: u64, kv: (u64, u64), window: u64) -> ModelSummary {
    let mut s = gemma4(4, embd, 8192, heads, kv, window);
    s.head_count_kv_per_layer = Some(vec![kv.0, kv.0, kv.0, kv.1]);
    s.sliding_window_layers = Some(vec![true, true, true, false]);
    s.shared_kv_layers = Some(4);
    // Its header counts every layer as an MTP layer.
    s.nextn_predict_layers = Some(4);
    s
}

pub(crate) fn e4b_assistant() -> ModelSummary {
    let mut s = assistant(256, 4, (2, 2), 512);
    s.feed_forward_length = Some(2048);
    s
}

pub(crate) fn qwen35(
    n: u64,
    embd: u64,
    ff: u64,
    heads: u64,
    kv: u64,
    nextn: Option<u64>,
) -> ModelSummary {
    ModelSummary {
        block_count: Some(n),
        embedding_length: Some(embd),
        feed_forward_length: Some(ff),
        head_count: Some(heads),
        head_count_kv: Some(kv),
        key_length: Some(256),
        value_length: Some(256),
        full_attention_interval: Some(4),
        ssm_conv_kernel: Some(4),
        ssm_state_size: Some(128),
        ssm_group_count: Some(16),
        // Attention heads × 256 is ssm.inner_size in every Qwen3.5 header probed.
        ssm_inner_size: Some(heads * 256),
        nextn_predict_layers: nextn,
        vocab_size: Some(QWEN_VOCAB),
        ..Default::default()
    }
}

pub(crate) fn qwen27b() -> ModelSummary {
    qwen35(65, 5120, 17408, 24, 4, Some(1))
}

pub(crate) fn readerlm() -> ModelSummary {
    ModelSummary {
        block_count: Some(28),
        embedding_length: Some(1536),
        feed_forward_length: Some(8960),
        head_count: Some(12),
        head_count_kv: Some(2),
        vocab_size: Some(151_936),
        ..Default::default()
    }
}

pub(crate) fn lfm25() -> ModelSummary {
    let heads = [
        0, 0, 8, 0, 0, 0, 8, 0, 0, 0, 8, 0, 0, 0, 8, 0, 0, 0, 8, 0, 0, 8, 0, 0,
    ];
    ModelSummary {
        block_count: Some(24),
        embedding_length: Some(2048),
        feed_forward_length: Some(7168),
        head_count: Some(32),
        head_count_kv_per_layer: Some(heads.to_vec()),
        expert_used_count: Some(4),
        has_recurrent_keys: true,
        expert_feed_forward_length: Some(1792),
        vocab_size: Some(128_000),
        ..Default::default()
    }
}

pub(crate) fn ling() -> ModelSummary {
    ModelSummary {
        block_count: Some(24),
        embedding_length: Some(1536),
        feed_forward_length: Some(4608),
        head_count: Some(16),
        head_count_kv_per_layer: Some((0..24).map(|i| u64::from((i + 1) % 4 == 0)).collect()),
        key_length: Some(576),
        value_length: Some(128),
        kv_lora_rank: Some(512),
        key_length_mla: Some(192),
        value_length_mla: Some(128),
        has_recurrent_keys: true,
        expert_used_count: Some(8),
        expert_feed_forward_length: Some(512),
        expert_shared_feed_forward_length: Some(512),
        vocab_size: Some(157_184),
        ..Default::default()
    }
}

/// medgemma-27b (gemma3): the header states a window but no pattern, so
/// every layer reads as full attention, as lmgw's KV term reads it too.
pub(crate) fn medgemma() -> ModelSummary {
    ModelSummary {
        block_count: Some(62),
        embedding_length: Some(5376),
        feed_forward_length: Some(21504),
        head_count: Some(32),
        head_count_kv: Some(16),
        key_length: Some(128),
        value_length: Some(128),
        sliding_window: Some(1024),
        vocab_size: Some(262_208),
        ..Default::default()
    }
}

/// muse-glimmer-30b: every 4th layer global, a 2048-token window.
pub(crate) fn muse() -> ModelSummary {
    ModelSummary {
        architecture: Some("muse-glimmer".into()),
        block_count: Some(52),
        embedding_length: Some(6656),
        feed_forward_length: Some(19968),
        head_count: Some(32),
        head_count_kv: Some(2),
        key_length: Some(128),
        value_length: Some(128),
        sliding_window: Some(2048),
        sliding_window_pattern: Some(4),
        vocab_size: Some(202_048),
        ..Default::default()
    }
}

/// Its DFlash drafter: five windowed layers of the target's widths.
pub(crate) fn dflash() -> ModelSummary {
    ModelSummary {
        architecture: Some("dflash".into()),
        block_count: Some(5),
        head_count_kv: Some(8),
        sliding_window_layers: Some(vec![true; 5]),
        sliding_window_pattern: Some(5),
        sliding_window_pattern_is_array: true,
        ..muse()
    }
}

#[derive(Clone, Copy)]
enum Ctx {
    Main,
    Mtp,
}

/// One probe context: header, which context, ctx, ub, batch, slots,
/// unified, the cache it reads quantized, outputs, measured MiB, the error
/// band in percent.
struct Case {
    name: &'static str,
    s: ModelSummary,
    ctx_kind: Ctx,
    ctx: u64,
    ub: u64,
    batch: u64,
    slots: u64,
    unified: bool,
    quantized: bool,
    outputs: u64,
    measured: f64,
    band: (f64, f64),
}

#[allow(clippy::too_many_arguments)]
fn case(
    name: &'static str,
    s: ModelSummary,
    ctx_kind: Ctx,
    (ctx, ub, batch): (u64, u64, u64),
    (slots, unified): (u64, bool),
    quantized: bool,
    outputs: u64,
    measured: f64,
    band: (f64, f64),
) -> Case {
    Case {
        name,
        s,
        ctx_kind,
        ctx,
        ub,
        batch,
        slots,
        unified,
        quantized,
        outputs,
        measured,
        band,
    }
}

fn estimate(c: &Case) -> f64 {
    let main = Layout::of(&c.s).unwrap();
    let layout = match c.ctx_kind {
        Ctx::Main => main,
        Ctx::Mtp => Layout::mtp(&c.s, &main).unwrap(),
    };
    let g = Graph {
        ctx: c.ctx,
        ubatch: c.ub,
        batch: c.batch,
        slots: c.slots,
        unified: c.unified,
        flash_attn_off: false,
        k_copied: c.quantized,
        v_copied: c.quantized,
        outputs: c.outputs,
        single_layer: matches!(c.ctx_kind, Ctx::Mtp),
        swa_full: false,
    };
    bytes(&c.s, &layout, &g).unwrap() as f64 / MIB
}

/// Every probe context, measured against the formula. The masks and the
/// f16 copy are exact, so a context they dominate sits within a percent;
/// the activation widths are a fit across architectures (module doc).
#[test]
fn the_formula_matches_llama_cpps_reserved_chat_buffers() {
    use Ctx::{Main, Mtp};
    let tight = (-1.0, 1.0);
    let cases = vec![
        // Gemma 4 E4B: per-layer inputs, window 512, 18 shared layers.
        case(
            "e4b-voice",
            e4b(),
            Main,
            (16384, 512, 2048),
            (1, false),
            true,
            5,
            157.30,
            (-3.0, 0.0),
        ),
        case(
            "e4b-voice asst",
            e4b_assistant(),
            Main,
            (16384, 512, 2048),
            (1, false),
            true,
            1,
            96.28,
            (0.0, 4.0),
        ),
        case(
            "e4b-voice 131072",
            e4b(),
            Main,
            (131072, 512, 2048),
            (1, false),
            true,
            5,
            717.30,
            tight,
        ),
        case(
            "e4b-voice 131072 asst",
            e4b_assistant(),
            Main,
            (131072, 512, 2048),
            (1, false),
            true,
            1,
            656.28,
            tight,
        ),
        case(
            "e4b-voice 131072 f16",
            e4b(),
            Main,
            (131072, 512, 2048),
            (1, false),
            false,
            5,
            256.02,
            (-3.0, 0.0),
        ),
        case(
            "e4b-voice f16",
            e4b(),
            Main,
            (16384, 512, 2048),
            (1, false),
            false,
            5,
            144.02,
            (-4.0, 0.0),
        ),
        case(
            "e4b-voice f16 asst",
            e4b_assistant(),
            Main,
            (16384, 512, 2048),
            (1, false),
            false,
            1,
            37.00,
            (-6.0, 0.0),
        ),
        case(
            "gemma4-e4b",
            e4b(),
            Main,
            (512_000, 512, 2048),
            (4, false),
            true,
            20,
            2202.30,
            tight,
        ),
        case(
            "gemma4-e4b asst",
            e4b_assistant(),
            Main,
            (512_000, 512, 2048),
            (4, false),
            true,
            4,
            2141.28,
            tight,
        ),
        // Gemma 4 12B: a vision row at ubatch 1280, two slots.
        case(
            "gemma4-12b",
            gemma4(48, 3840, 15360, 16, (8, 1), 1024),
            Main,
            (512_000, 1280, 2048),
            (2, false),
            true,
            6,
            1786.96,
            (0.0, 5.0),
        ),
        case(
            "gemma4-12b asst",
            assistant(1024, 16, (8, 1), 1024),
            Main,
            (512_000, 1280, 2048),
            (2, false),
            true,
            2,
            1749.41,
            (0.0, 4.0),
        ),
        // Gemma 4 26B-A4B: mixture of experts, four slots.
        case(
            "gemma4-26b 32768",
            gemma4_26b(),
            Main,
            (32768, 1280, 2048),
            (4, false),
            true,
            20,
            384.46,
            tight,
        ),
        case(
            "gemma4-26b 16384",
            gemma4_26b(),
            Main,
            (16384, 1280, 2048),
            (4, false),
            true,
            20,
            374.46,
            tight,
        ),
        case(
            "gemma4-26b 32768 asst",
            assistant(1024, 16, (8, 2), 1024),
            Main,
            (32768, 1280, 2048),
            (4, false),
            true,
            4,
            262.41,
            (0.0, 30.0),
        ),
        case(
            "gemma4-26b 16384 asst",
            assistant(1024, 16, (8, 2), 1024),
            Main,
            (16384, 1280, 2048),
            (4, false),
            true,
            4,
            188.41,
            (0.0, 45.0),
        ),
        // Qwen3.5 hybrids, one slot.
        case(
            "qwen3.5-4b",
            qwen35(32, 2560, 9216, 16, 4, None),
            Main,
            (262_144, 512, 1024),
            (1, false),
            true,
            1,
            1340.29,
            tight,
        ),
        case(
            "qwen3.5-4b f16 16384",
            qwen35(32, 2560, 9216, 16, 4, None),
            Main,
            (16384, 512, 1024),
            (1, false),
            false,
            1,
            90.02,
            tight,
        ),
        case(
            "qwen3.5-0.8b",
            qwen35(24, 1024, 3584, 8, 2, None),
            Main,
            (262_144, 512, 1024),
            (1, false),
            true,
            1,
            796.29,
            tight,
        ),
        case(
            "qwen3.8 maxctx 65000",
            qwen27b(),
            Main,
            (65000, 2048, 2048),
            (1, false),
            true,
            4,
            908.34,
            (-6.0, 0.0),
        ),
        case(
            "qwen3.8 maxctx 65000 mtp",
            qwen27b(),
            Mtp,
            (65000, 2048, 2048),
            (1, false),
            false,
            1,
            526.06,
            (0.0, 16.0),
        ),
        case(
            "qwen3.8 IQ4_XS rung 30000",
            qwen27b(),
            Main,
            (30000, 2048, 2048),
            (1, false),
            true,
            4,
            686.34,
            tight,
        ),
        case(
            "qwen3.8 IQ4_XS rung 30000 mtp",
            qwen27b(),
            Mtp,
            (30000, 2048, 2048),
            (1, false),
            false,
            1,
            510.06,
            (0.0, 12.0),
        ),
        case(
            "qwen3.6-27b 16384",
            qwen27b(),
            Main,
            (16384, 256, 1024),
            (1, false),
            true,
            5,
            122.28,
            (-6.0, 0.0),
        ),
        case(
            "qwen3.6-27b 16384 mtp",
            qwen27b(),
            Mtp,
            (16384, 256, 1024),
            (1, false),
            false,
            1,
            57.01,
            (0.0, 25.0),
        ),
        // A unified cache over four auto slots.
        case(
            "readerlm-v2",
            readerlm(),
            Main,
            (512_768, 512, 2048),
            (4, true),
            true,
            4,
            1016.59,
            tight,
        ),
        case(
            "readerlm-v2 f16 16384",
            readerlm(),
            Main,
            (16384, 512, 2048),
            (4, true),
            false,
            4,
            80.51,
            tight,
        ),
        case(
            "ling-3.0-tiny",
            ling(),
            Main,
            (131_072, 512, 2048),
            (4, true),
            true,
            4,
            351.01,
            (0.0, 2.0),
        ),
        case(
            "medgemma-27b 8192",
            medgemma(),
            Main,
            (8192, 512, 2048),
            (4, true),
            true,
            4,
            180.68,
            (-3.0, 0.0),
        ),
        // f16 cache, mixture of experts with short convolutions.
        case(
            "lfm2.5-8b",
            lfm25(),
            Main,
            (128_000, 512, 2048),
            (1, false),
            false,
            1,
            183.01,
            (0.0, 18.0),
        ),
        // muse-glimmer-30b at 4096 with its DFlash drafter (review probe,
        // 2026-10-14): the target reserves 1 + 16 outputs, the drafter, an
        // encoder to llama.cpp, one per batch token over its f16 window
        // cache.
        case(
            "muse-glimmer-30b 4096",
            muse(),
            Main,
            (4096, 512, 1024),
            (1, false),
            true,
            17,
            175.68,
            tight,
        ),
        case(
            "muse-glimmer-30b 4096 dflash",
            dflash(),
            Main,
            (4096, 512, 1024),
            (1, false),
            false,
            1024,
            803.02,
            (-2.0, 0.0),
        ),
    ];
    for c in cases {
        let got = estimate(&c);
        let err = (got - c.measured) / c.measured * 100.0;
        assert!(
            (c.band.0..=c.band.1).contains(&err),
            "{}: {got:.1} MiB against {} measured ({err:+.1} %)",
            c.name,
            c.measured
        );
    }
}

pub(crate) fn gemma4_26b() -> ModelSummary {
    ModelSummary {
        expert_used_count: Some(8),
        expert_feed_forward_length: Some(704),
        ..gemma4(30, 2816, 2112, 16, (8, 2), 1024)
    }
}

/// The f16 copy of a quantized cache is the term that grows with the
/// context: Qwen3.5-4B's at 262,144 is 1,000 MiB, a whole-context copy of
/// its four 256-wide K and V heads in f16, and nothing with an f16 cache.
#[test]
fn a_quantized_cache_adds_its_f16_copy() {
    let s = qwen35(32, 2560, 9216, 16, 4, None);
    let l = Layout::of(&s).unwrap();
    let g = |copied| Graph {
        ctx: 262_144,
        ubatch: 512,
        batch: 1024,
        slots: 1,
        unified: false,
        flash_attn_off: false,
        k_copied: copied,
        v_copied: copied,
        outputs: 1,
        single_layer: false,
        swa_full: false,
    };
    let mask = 2 * 262_144 * 512;
    let copy = 2 * 4 * 512 * 262_144;
    let attn = 16 * 512 * (2560 + 16 * 256);
    let ffn = 4 * 512 * (4 * 2560 + 3 * 9216);
    assert_eq!(bytes(&s, &l, &g(true)).unwrap(), mask + copy + attn);
    // With an f16 cache the FFN is the widest phase.
    assert_eq!(bytes(&s, &l, &g(false)).unwrap(), mask + ffn);
}

/// One stream per slot splits the cells and the batch; a unified cache
/// keeps one stream over the whole context. The mask's total is the same.
#[test]
fn slots_split_the_cells_unless_unified() {
    let g = |slots, unified| Graph {
        ctx: 512_000,
        ubatch: 1280,
        batch: 2048,
        slots,
        unified,
        flash_attn_off: false,
        k_copied: true,
        v_copied: true,
        outputs: 1,
        single_layer: false,
        swa_full: false,
    };
    assert_eq!(g(2, false).full_cells(), 256_000);
    assert_eq!(g(2, true).full_cells(), 512_000);
    // Gemma 4 12B's measured window cache: 2 × (1024 + 1280) cells split,
    // 4 × 1024 + 512 unified (medgemma's, at ubatch 512).
    assert_eq!(g(2, false).window_cells(1024), 2304);
    let unified = Graph {
        ubatch: 512,
        ..g(4, true)
    };
    assert_eq!(unified.window_cells(1024), 4608);
    // llama.cpp pads the context: 65,000 is reserved as 65,024.
    assert_eq!(padded(65_000), 65_024);
    // ...and pads each slot's share again (`n_ctx_seq`): 100,096 / 3 is
    // 33,365, kept as 33,536.
    let three = Graph {
        ctx: 100_000,
        ..g(3, false)
    };
    assert_eq!(three.full_cells(), 33_536);
    // `--swa-full`: the window cache is the whole stream.
    let full = Graph {
        swa_full: true,
        ..g(2, false)
    };
    assert_eq!(full.window_cells(1024), 256_000);
}

/// A value the estimate needs and the header does not state is named.
#[test]
fn a_missing_header_value_is_named() {
    let s = ModelSummary {
        vocab_size: None,
        ..readerlm()
    };
    let l = Layout::of(&readerlm()).unwrap();
    let g = Graph {
        ctx: 4096,
        ubatch: 512,
        batch: 2048,
        slots: 1,
        unified: false,
        flash_attn_off: false,
        k_copied: false,
        v_copied: false,
        outputs: 1,
        single_layer: false,
        swa_full: false,
    };
    assert!(bytes(&s, &l, &g).unwrap_err().contains("vocabulary"));
    let s = ModelSummary {
        feed_forward_length: None,
        ..readerlm()
    };
    assert!(bytes(&s, &l, &g)
        .unwrap_err()
        .contains("feed_forward_length"));
    let s = ModelSummary {
        expert_feed_forward_length: None,
        ..lfm25()
    };
    let l = Layout::of(&lfm25()).unwrap();
    assert!(bytes(&s, &l, &g)
        .unwrap_err()
        .contains("expert_feed_forward_length"));
}
