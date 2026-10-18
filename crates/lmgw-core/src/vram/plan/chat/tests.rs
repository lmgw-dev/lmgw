//! The chat footprint against llama.cpp's own buffer lines (b11549): the KV
//! cache to the MiB, recurrent snapshots, the second context, and the notes
//! for what a header cannot size.

use super::*;
use crate::config::LlamaParams;
use crate::vram::compute::chat::tests::assistant;
use crate::vram::compute::chat::tests::{
    dflash, e4b, e4b_assistant, gemma4_26b, lfm25, ling, muse, qwen27b, qwen35, readerlm,
};

const MIB: u64 = 1024 * 1024;

fn files(s: ModelSummary, draft: Option<ModelSummary>) -> ChatFiles {
    ChatFiles {
        files: GgufFiles {
            main_bytes: 10 * MIB,
            companion_bytes: MIB,
            summary: Ok(s),
        },
        draft: draft.map(Ok),
    }
}

fn mib(b: u64) -> f64 {
    b as f64 / MIB as f64
}

fn q8(p: LlamaParams) -> LlamaParams {
    LlamaParams {
        cache_type_k: Some("q8_0".into()),
        cache_type_v: Some("q8_0".into()),
        flash_attn: Some("on".into()),
        ..p
    }
}

/// A gemma4-e4b row: four split slots at 512,000, its global
/// cache 4,250 MiB and its window cache 85 MiB as measured, and an MTP
/// assistant that reads that cache and keeps none of its own; two compute
/// buffers, 2,202 and 2,141 MiB measured.
#[test]
fn gemma4_e4b_four_slots_and_its_assistant() {
    let p = q8(LlamaParams {
        ctx_size: Some(512_000),
        batch_size: Some(2048),
        parallel: Some(4),
        draft_gguf_path: Some("mtp.gguf".into()),
        spec_type: Some("draft-mtp".into()),
        spec_draft_n_max: Some(4),
        ..Default::default()
    });
    let fp = footprint(
        &files(e4b(), Some(e4b_assistant())),
        "e4b.gguf",
        &Shape::of(&p, &[]),
    );
    assert_eq!(mib(fp.kv_cache_bytes), 4250.0 + 85.0);
    let compute = mib(fp.compute_bytes);
    assert!((compute - (2202.30 + 2141.28)).abs() < 10.0, "{compute}");
    assert_eq!(fp.weights_bytes, 11 * MIB);
    assert_eq!(
        fp.total_bytes,
        fp.weights_bytes + fp.kv_cache_bytes + fp.compute_bytes
    );
    assert_eq!(fp.note, None, "everything is sized");
}

/// gemma4-26b-a4b at 32768 with four slots and ubatch 1280: its window
/// cache holds 4 × (1024 + 1280) cells, 956.25 MiB measured.
#[test]
fn a_window_cache_holds_window_plus_ubatch_per_slot() {
    let p = q8(LlamaParams {
        ctx_size: Some(32768),
        batch_size: Some(2048),
        ubatch_size: Some(1280),
        parallel: Some(4),
        ..Default::default()
    });
    let fp = footprint(&files(gemma4_26b(), None), "g.gguf", &Shape::of(&p, &[]));
    assert_eq!(mib(fp.kv_cache_bytes), 340.0 + 956.25);
}

/// The qwen3.8 maxctx row's shape at 65,000 (the probe's context): the
/// main cache 2,159 MiB, the MTP layer's own f16 cache 254 MiB, and the
/// recurrent state kept once plus once per draft position (3), 598.5 MiB:
/// the terms an estimate without them would miss at the MTP context.
#[test]
fn an_mtp_row_charges_its_mtp_cache_and_recurrent_snapshots() {
    let p = q8(LlamaParams {
        ctx_size: Some(65000),
        batch_size: Some(2048),
        ubatch_size: Some(2048),
        parallel: Some(1),
        spec_type: Some("draft-mtp".into()),
        spec_draft_n_max: Some(3),
        ..Default::default()
    });
    let fp = footprint(&files(qwen27b(), None), "q.gguf", &Shape::of(&p, &[]));
    let kv = mib(fp.kv_cache_bytes);
    let rs = mib(48 * crate::gguf::recurrent_state_bytes(&qwen27b()).unwrap() * 4);
    assert!((rs - 598.5).abs() < 0.1, "{rs}");
    assert!((kv - (2159.0 + 254.0 + rs)).abs() < 0.1, "{kv}");
    // Main 908.34 + MTP 526.06 measured.
    let compute = mib(fp.compute_bytes);
    assert!((1380.0..1500.0).contains(&compute), "{compute}");
    // Without the MTP: one recurrent state, no MTP cache or context.
    let plain = LlamaParams {
        spec_type: None,
        ..p
    };
    let fp = footprint(&files(qwen27b(), None), "q.gguf", &Shape::of(&plain, &[]));
    assert!((mib(fp.kv_cache_bytes) - (2159.0 + rs / 4.0)).abs() < 0.1);
}

/// A unified cache over the four auto slots (readerlm-v2, 7,448.7 MiB
/// measured); a Qwen2 header states no key_length, so heads are
/// n_embd / head_count, llama.cpp's default.
#[test]
fn a_unified_auto_row_and_the_default_head_size() {
    let p = q8(LlamaParams {
        ctx_size: Some(512_768),
        ..Default::default()
    });
    let fp = footprint(&files(readerlm(), None), "r.gguf", &Shape::of(&p, &[]));
    assert!((mib(fp.kv_cache_bytes) - 7448.68).abs() < 0.1);
    assert!((mib(fp.compute_bytes) - 1016.59).abs() < 10.0);
}

/// MLA keeps no V cache (Ling: 459 MiB of K, 0 of V, measured), and a
/// recurrent state the header gives no ssm keys for is named, not guessed.
#[test]
fn mla_and_an_unsized_recurrent_state() {
    let p = q8(LlamaParams {
        ctx_size: Some(131_072),
        ..Default::default()
    });
    let fp = footprint(&files(ling(), None), "l.gguf", &Shape::of(&p, &[]));
    assert_eq!(mib(fp.kv_cache_bytes), 459.0);
    let note = fp.note.unwrap_or_default();
    assert!(note.contains("recurrent state of its 18 layers"), "{note}");
    // LFM2.5 at f16, 1,500 MiB measured.
    let p = LlamaParams {
        ctx_size: Some(128_000),
        parallel: Some(1),
        ..Default::default()
    };
    let fp = footprint(&files(lfm25(), None), "f.gguf", &Shape::of(&p, &[]));
    assert_eq!(mib(fp.kv_cache_bytes), 1500.0);
}

/// What cannot be sized is said: an MTP row whose weights have no MTP
/// layers, a draft model that could not be read, an unknown cache type.
#[test]
fn what_cannot_be_sized_is_named() {
    let mtp = LlamaParams {
        ctx_size: Some(4096),
        spec_type: Some("draft-mtp".into()),
        ..Default::default()
    };
    let s = qwen35(32, 2560, 9216, 16, 4, None);
    let fp = footprint(&files(s.clone(), None), "q.gguf", &Shape::of(&mtp, &[]));
    assert!(fp.note.unwrap_or_default().contains("no MTP layers"));

    let draft = LlamaParams {
        draft_gguf_path: Some("d.gguf".into()),
        spec_type: Some("draft-simple".into()),
        ..mtp.clone()
    };
    let mut f = files(s.clone(), None);
    f.draft = Some(Err("no such file".into()));
    let fp = footprint(&f, "q.gguf", &Shape::of(&draft, &[]));
    let note = fp.note.unwrap_or_default();
    assert!(
        note.contains("'d.gguf' could not be read (no such file)"),
        "{note}"
    );
    assert!(fp.compute_bytes > 0, "the main context is still sized");

    let odd = LlamaParams {
        ctx_size: Some(4096),
        cache_type_k: Some("tq9".into()),
        ..Default::default()
    };
    let fp = footprint(&files(s, None), "q.gguf", &Shape::of(&odd, &[]));
    assert!(fp
        .note
        .unwrap_or_default()
        .contains("'tq9' is not one lmgw knows"));
}

/// A draft model with a cache of its own charges it at the draft cache
/// types; one that shares the target's charges none.
#[test]
fn a_draft_model_keeps_its_own_cache_unless_it_shares() {
    let mut own = assistant(1024, 16, (8, 2), 1024);
    own.shared_kv_layers = None;
    let p = q8(LlamaParams {
        ctx_size: Some(32768),
        parallel: Some(1),
        draft_gguf_path: Some("d.gguf".into()),
        spec_type: Some("draft-simple".into()),
        ..Default::default()
    });
    let shape = Shape::of(&p, &[]);
    let shared = footprint(
        &files(gemma4_26b(), Some(assistant(1024, 16, (8, 2), 1024))),
        "g.gguf",
        &shape,
    );
    let separate = footprint(&files(gemma4_26b(), Some(own)), "g.gguf", &shape);
    // One f16 global layer (2 heads × 1024 × 2 B × 32768) and three window
    // layers (8 heads × 512 × 2 B × 1536 cells: window 1024 + ubatch 512).
    let draft_kv = 2 * 1024 * 2 * 32768 + 3 * 8 * 512 * 2 * 1536;
    assert_eq!(separate.kv_cache_bytes - shared.kv_cache_bytes, draft_kv);
}

/// A header that states its head sizes but not its widths still has its
/// KV cache sized; the compute buffer it cannot size is named.
#[test]
fn a_header_without_widths_keeps_its_kv_cache() {
    let s = ModelSummary {
        block_count: Some(4),
        head_count_kv: Some(8),
        key_length: Some(128),
        value_length: Some(128),
        context_length: Some(8192),
        ..Default::default()
    };
    let p = q8(LlamaParams {
        parallel: Some(1),
        ..Default::default()
    });
    let fp = footprint(&files(s, None), "c.gguf", &Shape::of(&p, &[]));
    assert_eq!(fp.kv_cache_bytes, 8192 * 4 * 8 * 256 * 272 / 256);
    assert_eq!(fp.compute_bytes, 0);
    let note = fp.note.unwrap_or_default();
    assert!(note.contains("compute buffer not sized"), "{note}");
}

/// Review should-fix 5: llama.cpp applies `-ot` to the input embeddings
/// too, so a row that overrides tensors to a device keeps them charged.
#[test]
fn an_override_to_a_device_keeps_the_input_embeddings_charged() {
    let s = ModelSummary {
        host_input_bytes: 2 * MIB,
        ..readerlm()
    };
    let p = q8(LlamaParams {
        ctx_size: Some(4096),
        ..Default::default()
    });
    let fp = footprint(&files(s.clone(), None), "r.gguf", &Shape::of(&p, &[]));
    assert_eq!(fp.weights_bytes, 9 * MIB);
    assert_eq!(fp.note, None);
    let to_cpu = ["-ot".to_string(), "exps=CPU".to_string()];
    let fp = footprint(&files(s.clone(), None), "r.gguf", &Shape::of(&p, &to_cpu));
    assert_eq!(fp.weights_bytes, 9 * MIB);
    let to_card = ["-ot".to_string(), "token_embd=CUDA0".to_string()];
    let fp = footprint(&files(s, None), "r.gguf", &Shape::of(&p, &to_card));
    assert_eq!(fp.weights_bytes, 11 * MIB);
    let note = fp.note.unwrap_or_default();
    assert!(note.contains("--override-tensor"), "{note}");
}

/// Review should-fix 4: a hybrid draft model keeps a recurrent state per
/// slot in its own context, and one the header cannot size is named.
#[test]
fn a_hybrid_draft_model_keeps_its_recurrent_state() {
    let draft = qwen35(8, 512, 1024, 4, 2, None);
    let p = q8(LlamaParams {
        ctx_size: Some(4096),
        parallel: Some(2),
        draft_gguf_path: Some("d.gguf".into()),
        spec_type: Some("draft-simple".into()),
        ..Default::default()
    });
    let shape = Shape::of(&p, &[]);
    let main = qwen35(32, 2560, 9216, 16, 4, None);
    let with = footprint(&files(main.clone(), Some(draft.clone())), "q.gguf", &shape);
    let unsized_draft = ModelSummary {
        ssm_state_size: None,
        ..draft.clone()
    };
    let without = footprint(&files(main, Some(unsized_draft)), "q.gguf", &shape);
    let per = crate::gguf::recurrent_state_bytes(&draft).unwrap();
    assert_eq!(with.kv_cache_bytes - without.kv_cache_bytes, 6 * per * 2);
    let note = without.note.unwrap_or_default();
    assert!(note.contains("the draft model's 6 layers"), "{note}");
}

/// muse-glimmer-30b's row at 4096 (the review probe): its DFlash drafter
/// keeps a window cache of its own (50 MiB) and, an encoder to llama.cpp,
/// reserves an output per batch token: 803 MiB of compute beside the
/// target's 176.
#[test]
fn a_dflash_drafter_keeps_its_own_cache_and_a_batch_of_outputs() {
    let p = q8(LlamaParams {
        ctx_size: Some(4096),
        batch_size: Some(1024),
        parallel: Some(1),
        draft_gguf_path: Some("dflash.gguf".into()),
        spec_type: Some("draft-dflash".into()),
        spec_draft_n_max: Some(16),
        ..Default::default()
    });
    let fp = footprint(
        &files(muse(), Some(dflash())),
        "m.gguf",
        &Shape::of(&p, &[]),
    );
    assert!(
        (mib(fp.kv_cache_bytes) - (27.63 + 51.83 + 50.0)).abs() < 0.1,
        "{fp:?}"
    );
    let compute = mib(fp.compute_bytes);
    let measured = 175.68 + 803.02;
    assert!(
        ((measured * 0.98)..=measured).contains(&compute),
        "{compute}"
    );
}

/// Review should-fix 3: four slots of ngram-mod reserve 260 outputs, and
/// their logits over Gemma 4's 262,144-token vocabulary are the buffer.
#[test]
fn an_ngram_row_reserves_its_own_window_of_outputs() {
    let p = |spec: &str| LlamaParams {
        ctx_size: Some(16384),
        parallel: Some(4),
        spec_type: Some(spec.into()),
        ..Default::default()
    };
    let ngram = footprint(
        &files(e4b(), None),
        "e.gguf",
        &Shape::of(&p("ngram-mod"), &[]),
    );
    let none = footprint(&files(e4b(), None), "e.gguf", &Shape::of(&p("none"), &[]));
    assert!(ngram.compute_bytes >= 4 * 262_144 * 260, "{ngram:?}");
    assert!(none.compute_bytes < 4 * 262_144 * 260, "{none:?}");
}
