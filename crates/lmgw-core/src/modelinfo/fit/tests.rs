//! The fit of a planned file (find-a-model design §3.1, §10's fit group):
//! over a synthetic header's plan input and the plan's own parameters, so
//! every number is the one the plan's warning and admission compute.

use serde_json::{json, Map, Value};

use super::*;
use crate::gguf::{read_header_from, synth, ModelSummary};
use crate::modelinfo::plan_files::{Companion, Source};
use crate::runtime::Class;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// A 4-layer causal header: 8 KV heads × (128 + 128), so 8.5 KiB per token
/// at q8_0, with `weights` bytes of `blk.*` tensors.
fn summary(ctx: u32, weights: u64) -> ModelSummary {
    let mut h = synth::chat("qwen3", ctx);
    h.u32("qwen3.embedding_length", 1024)
        .u32("qwen3.attention.head_count", 8)
        .u32("qwen3.attention.head_count_kv", 8)
        .u32("qwen3.attention.key_length", 128)
        .u32("qwen3.attention.value_length", 128);
    h.tensor_shaped("blk.0.ffn_up.weight", &[1024, 1024], weights);
    let b = h.bytes();
    ModelSummary::from_meta(
        &read_header_from(std::io::Cursor::new(b.clone()), b.len() as u64).unwrap(),
    )
}

fn files(s: ModelSummary) -> PlanFiles {
    PlanFiles {
        rel: "demo/m.gguf".into(),
        weights_bytes: s.file_size,
        parameters: s.parameter_count,
        layer_bits: s.layer_bits_per_weight,
        layer_bits_estimated: false,
        layer_elements: s.layer_tensors.map(|(_, e)| e),
        host_input_elements: None,
        overhead_bytes: 0,
        summary: s,
        weights_missing: None,
        projector: None,
        drafter: None,
        notes: Vec::new(),
        size_only: None,
        listed_bytes: None,
        found_in: Class::Chat,
        in_aux_dir: false,
        source: Source::Disk,
    }
}

fn chat_params(ctx: u64) -> Map<String, Value> {
    json!({
        "ctx_size": ctx, "n_gpu_layers": 999, "flash_attn": "on",
        "cache_type_k": "q8_0", "cache_type_v": "q8_0",
    })
    .as_object()
    .unwrap()
    .clone()
}

fn card(capacity: u64) -> CardNow {
    CardNow {
        card: Card {
            capacity,
            headroom: 64 * MIB,
            budget: true,
            admission: true,
        },
        hold: false,
        outside: Some(0),
        residents: Vec::new(),
        left_out: Vec::new(),
    }
}

fn whole(f: &PlanFiles, params: &Map<String, Value>, c: &CardNow) -> FileFit {
    fit(f, &Sizer::chat(f, params), Some(c), FitMeasure::WholeCard)
}

#[test]
fn the_trained_context_fits() {
    let f = files(summary(8192, 8 * MIB));
    let w = whole(&f, &chat_params(8192), &card(GIB));
    assert_eq!(w.mark, FitMark::Fits, "{w:?}");
    assert_eq!((w.ctx, w.trained_ctx), (Some(8192), Some(8192)));
    assert!(w.uses_bytes + w.headroom_bytes <= w.capacity_bytes, "{w:?}");
    assert_eq!(w.terms.weights_bytes, f.weights_bytes);
    assert!(w.terms.kv_cache_bytes > 0, "{w:?}");
    assert_eq!(w.parameters, Some(1024 * 1024));
    // The synthetic tensor is typed F32 with 8 MiB laid out for its 1 Mi
    // weights: its width is the type's 32 bits, not the offsets' 64 (D140).
    assert_eq!(w.bits_per_weight, Some(32.0));
}

/// A shorter context fits: the largest, by the plan's own search, the same
/// number the plan's warning names.
#[test]
fn a_shorter_context_fits_and_is_the_plans_number() {
    let f = files(summary(131_072, 8 * MIB));
    let params = chat_params(131_072);
    let c = card(256 * MIB);
    let w = whole(&f, &params, &c);
    assert_eq!(w.mark, FitMark::FitsShorter, "{w:?}");
    let n = w.ctx.unwrap();
    let sizer = Sizer::chat(&f, &params);
    assert!(c.card.fits(&sizer.at(n)) && !c.card.fits(&sizer.at(n + 1)));
    let warning = plan_fit::over_card(&c.card, 131_072, sizer.instead, |x| sizer.at(x)).unwrap();
    assert!(
        warning.contains(&format!("largest ctx_size that fits is {n} ")),
        "{warning}"
    );
    assert_eq!(w.uses_bytes, sizer.at(n).total_bytes);
}

#[test]
fn nothing_fits_at_one_token() {
    let f = files(summary(8192, 512 * MIB));
    let w = whole(&f, &chat_params(8192), &card(256 * MIB));
    assert_eq!(w.mark, FitMark::DoesNotFit, "{w:?}");
    assert_eq!(w.ctx, None);
    assert!(
        w.uses_bytes >= 512 * MIB,
        "the footprint at one token: {w:?}"
    );
}

/// Beside what runs now (D120): the card less other programs' use and lmgw's
/// own models, each its charge plus the peak kept free for it. A file that
/// fits the whole card fits a shorter conversation beside a game and a
/// resident voice model.
#[test]
fn beside_takes_off_the_outside_use_and_the_residents() {
    let f = files(summary(65_536, 64 * MIB));
    let params = chat_params(65_536);
    let mut c = card(GIB);
    c.outside = Some(256 * MIB);
    c.residents = vec![CardResident {
        class: "audio".into(),
        model: "voice-demo".into(),
        bytes: 128 * MIB,
    }];
    let sizer = Sizer::chat(&f, &params);
    let w = fit(&f, &sizer, Some(&c), FitMeasure::WholeCard);
    let b = fit(&f, &sizer, Some(&c), FitMeasure::Beside);
    assert_eq!(w.mark, FitMark::Fits, "{w:?}");
    assert_eq!(b.mark, FitMark::FitsShorter, "{b:?}");
    assert_eq!(b.capacity_bytes, GIB - 256 * MIB - 128 * MIB);
    assert_eq!(
        b.beside,
        Some(BesideTerms {
            outside_bytes: Some(256 * MIB),
            residents_bytes: 128 * MIB,
        })
    );
    assert_eq!(w.beside, None);
    assert!(b.ctx < w.ctx, "{b:?} {w:?}");

    // Without a device reading: the card less lmgw's own models, said.
    c.outside = None;
    let b = fit(&f, &sizer, Some(&c), FitMeasure::Beside);
    assert_eq!(b.capacity_bytes, GIB - 128 * MIB);
    assert!(
        b.notes
            .iter()
            .any(|n| n.contains("can't tell what other programs")),
        "{:?}",
        b.notes
    );
    let w = fit(&f, &sizer, Some(&c), FitMeasure::WholeCard);
    assert!(
        !w.notes.iter().any(|n| n.contains("other programs")),
        "{:?}",
        w.notes
    );
}

/// The hold changes no mark (D10); it adds the line saying where to resume.
#[test]
fn the_hold_adds_its_line_and_changes_no_mark() {
    let f = files(summary(131_072, 8 * MIB));
    let params = chat_params(131_072);
    let free = whole(&f, &params, &card(256 * MIB));
    let mut held = card(256 * MIB);
    held.hold = true;
    let h = whole(&f, &params, &held);
    assert_eq!((h.mark, h.ctx), (free.mark, free.ctx));
    assert!(h.notes.iter().any(|n| n == HOLD_LINE), "{:?}", h.notes);
    assert!(HOLD_LINE.contains("HOLD button") && HOLD_LINE.contains("tray menu"));
    assert!(!free.notes.iter().any(|n| n == HOLD_LINE));
}

#[test]
fn without_a_card_the_mark_is_no_card() {
    let f = files(summary(8192, 8 * MIB));
    let n = fit(
        &f,
        &Sizer::chat(&f, &chat_params(8192)),
        None,
        FitMeasure::Beside,
    );
    assert_eq!(n.mark, FitMark::NoCard);
    assert!(
        n.notes[0].contains("can't measure the graphics card"),
        "{n:?}"
    );
}

/// With admission off "fits" still means fits; the note says nothing refuses.
#[test]
fn admission_off_still_fits_and_says_so() {
    let f = files(summary(8192, 8 * MIB));
    let mut c = card(GIB);
    c.card.admission = false;
    let w = whole(&f, &chat_params(8192), &c);
    assert_eq!(w.mark, FitMark::Fits);
    assert!(
        w.notes.iter().any(|n| n.contains("admission is off")),
        "{:?}",
        w.notes
    );
}

/// The projector and the drafter are counted, the drafter's own cache and
/// buffers too.
#[test]
fn the_projector_and_the_drafter_are_counted() {
    let mut f = files(summary(8192, 8 * MIB));
    let plain = whole(&f, &chat_params(8192), &card(GIB));
    let mut draft = synth::chat("dflash", 4096);
    draft
        .u32("dflash.block_size", 8)
        .u32("dflash.block_count", 1)
        .u32("dflash.embedding_length", 1024)
        .u32("dflash.attention.head_count", 8)
        .u32("dflash.attention.head_count_kv", 8)
        .u32("dflash.attention.key_length", 128)
        .u32("dflash.attention.value_length", 128);
    let db = draft.bytes();
    let ds = ModelSummary::from_meta(
        &read_header_from(std::io::Cursor::new(db.clone()), db.len() as u64).unwrap(),
    );
    f.projector = Some(Companion {
        path: "demo/mmproj-F16.gguf".into(),
        bytes: 32 * MIB,
        missing: None,
        summary: ModelSummary::default(),
    });
    f.drafter = Some(Companion {
        path: "demo/drafter.gguf".into(),
        bytes: 4 * MIB,
        missing: None,
        summary: ds,
    });
    let mut params = chat_params(8192);
    params.insert("mmproj_path".into(), json!("demo/mmproj-F16.gguf"));
    params.insert("draft_gguf_path".into(), json!("demo/drafter.gguf"));
    params.insert("spec_type".into(), json!("draft-dflash"));
    let both = whole(&f, &params, &card(GIB));
    assert_eq!(
        both.terms.weights_bytes,
        plain.terms.weights_bytes + 36 * MIB,
        "{both:?}"
    );
    assert!(
        both.terms.kv_cache_bytes > plain.terms.kv_cache_bytes,
        "{both:?}"
    );
}

/// An aux plan that ties its ubatch and batch to the context keeps them tied
/// at the fitted context: the footprint is searched that way, and the
/// fitted parameters move all three (D26).
#[test]
fn an_aux_fit_moves_its_ubatch_and_batch_with_the_context() {
    let mut h = synth::embedding("gemma-embedding", 1, 32_768);
    h.u32("gemma-embedding.embedding_length", 1024)
        .u32("gemma-embedding.feed_forward_length", 2048)
        .u32("gemma-embedding.attention.head_count", 8)
        .u32("gemma-embedding.attention.head_count_kv", 8)
        .u32("gemma-embedding.attention.key_length", 128)
        .u32("gemma-embedding.attention.value_length", 128);
    h.tensor_shaped("blk.0.ffn_up.weight", &[1024, 1024], 8 * MIB);
    let b = h.bytes();
    let s = ModelSummary::from_meta(
        &read_header_from(std::io::Cursor::new(b.clone()), b.len() as u64).unwrap(),
    );
    let f = files(s);
    let params = json!({"kind": "embed", "pooling": "mean", "ctx_size": 32768,
                        "ubatch_size": 32768, "batch_size": 32768})
    .as_object()
    .unwrap()
    .clone();
    let sizer = Sizer::aux(&f, &params);
    let c = card(256 * MIB);
    let w = fit(&f, &sizer, Some(&c), FitMeasure::WholeCard);
    assert_eq!(w.mark, FitMark::FitsShorter, "{w:?}");
    let n = w.ctx.unwrap();
    // The tie held in the search: at n with a ubatch of n it fits, and the
    // plan's own warning names the same n.
    let warning = plan_fit::over_card(&c.card, 32_768, sizer.instead, |x| sizer.at(x)).unwrap();
    assert!(warning.contains(&format!("fits is {n} ")), "{warning}");
    let fitted = fitted_params(&params, &w).unwrap();
    assert_eq!(fitted["ctx_size"], n);
    assert_eq!(fitted["ubatch_size"], n);
    assert_eq!(fitted["batch_size"], n);
    assert_eq!(fitted["pooling"], "mean");

    // A chat plan's batches aren't tied: only the context moves.
    let cf = files(summary(131_072, 8 * MIB));
    let mut cp = chat_params(131_072);
    cp.insert("ubatch_size".into(), json!(1280));
    let cw = whole(&cf, &cp, &card(256 * MIB));
    let fitted = fitted_params(&cp, &cw).unwrap();
    assert_eq!(fitted["ctx_size"], cw.ctx.unwrap());
    assert_eq!(fitted["ubatch_size"], 1280);
    // A plan that fits whole keeps its parameters.
    let small = files(summary(8192, 8 * MIB));
    let fits = whole(&small, &chat_params(8192), &card(GIB));
    assert_eq!(fitted_params(&chat_params(8192), &fits), None);
}

/// A header that couldn't be read is marked by size alone (D8), with why.
#[test]
fn an_unread_header_is_marked_by_size() {
    let c = card(GIB);
    let f = by_size(
        Some(512 * MIB),
        Some(&c),
        FitMeasure::WholeCard,
        "the CDN cut the read".into(),
    );
    assert_eq!(f.mark, FitMark::FitsBySize);
    assert!(f.notes.iter().any(|n| n.contains("cut the read")), "{f:?}");
    let f = by_size(Some(2 * GIB), Some(&c), FitMeasure::WholeCard, "x".into());
    assert_eq!(f.mark, FitMark::DoesNotFitBySize);
    assert!(!f.mark.fits());
    assert_eq!(
        by_size(Some(MIB), None, FitMeasure::Beside, "x".into()).mark,
        FitMark::NoCard
    );
    // Bytes nobody knows are never a fit, however large the card.
    let f = by_size(
        None,
        Some(&card(64 * GIB)),
        FitMeasure::WholeCard,
        "x".into(),
    );
    assert_eq!(f.mark, FitMark::Unknown, "{f:?}");
    assert!(!f.mark.fits());
}

/// A split set that can't load as it is (a part missing, or the parts'
/// count not its name's) is marked by its size alone, with why; its
/// bytes unknown when a part is missing.
#[test]
fn a_split_set_that_cant_load_is_marked_by_size() {
    let mut f = files(summary(8192, 8 * MIB));
    f.size_only = Some("part 2 is missing".into());
    let w = whole(&f, &chat_params(8192), &card(GIB));
    assert_eq!(w.mark, FitMark::Unknown, "{w:?}");
    assert!(w.notes.iter().any(|n| n == "part 2 is missing"), "{w:?}");
    f.listed_bytes = Some(16 * MIB);
    let w = whole(&f, &chat_params(8192), &card(GIB));
    assert_eq!(w.mark, FitMark::FitsBySize, "{w:?}");
    assert_eq!(w.uses_bytes, 16 * MIB);
}

/// A plan on the CPU fits with nothing on the card (D57), with a card to
/// measure against and without one.
#[test]
fn a_cpu_plan_fits_with_or_without_a_card() {
    let f = files(summary(8192, 8 * MIB));
    let mut params = chat_params(8192);
    params.insert("n_gpu_layers".into(), json!(0));
    let sizer = Sizer::chat(&f, &params);
    for c in [None, Some(card(MIB))] {
        let w = fit(&f, &sizer, c.as_ref(), FitMeasure::WholeCard);
        assert_eq!(w.mark, FitMark::Fits, "{w:?}");
        assert_eq!((w.uses_bytes, w.ctx), (0, Some(8192)), "{w:?}");
        assert!(w.notes.iter().any(|n| n.contains("on the CPU")), "{w:?}");
    }
}

/// D26's re-fit isn't fitted beside itself: lmgw's model on the card whose
/// row serves the planned file is left out of the beside measure, with a
/// note, and every other resident stays.
#[test]
fn a_loaded_row_of_the_planned_file_is_not_beside_it() {
    use crate::config::{LocalModel, Snapshot};
    let f = files(summary(65_536, 64 * MIB));
    let params = chat_params(65_536);
    let snap = Snapshot {
        local_models: vec![LocalModel {
            id: 1,
            model_id: "demo-m".into(),
            gguf_path: f.rel.clone(),
            params: Default::default(),
            args: Vec::new(),
            idle_seconds: 0,
            enabled: true,
            public: false,
            image: None,
            extra_run_args: None,
            warm_start: false,
            fallback: None,
            capabilities_override: None,
            ladder: Vec::new(),
        }],
        ..Snapshot::default()
    };
    let mut c = card(GIB);
    c.residents = vec![
        CardResident {
            class: "chat".into(),
            model: "demo-m".into(),
            bytes: 768 * MIB,
        },
        CardResident {
            class: "audio".into(),
            model: "demo-m".into(),
            bytes: 64 * MIB,
        },
    ];
    let sizer = Sizer::chat(&f, &params);
    let crowded = fit(&f, &sizer, Some(&c), FitMeasure::Beside);
    assert_eq!(crowded.capacity_bytes, GIB - 832 * MIB, "{crowded:?}");
    let c = c.beside_itself(&snap, Class::Chat, &f.rel);
    assert_eq!(c.left_out, vec!["demo-m".to_string()]);
    let b = fit(&f, &sizer, Some(&c), FitMeasure::Beside);
    assert_eq!(
        b.capacity_bytes,
        GIB - 64 * MIB,
        "the audio one stays: {b:?}"
    );
    assert_ne!(crowded.mark, FitMark::Fits, "{crowded:?}");
    assert!(
        b.mark == FitMark::Fits || b.ctx > crowded.ctx,
        "{b:?} {crowded:?}"
    );
    assert!(
        b.notes
            .iter()
            .any(|n| n.contains("demo-m on the graphics card now serves this file")),
        "{:?}",
        b.notes
    );
    // Another file's plan keeps it beside.
    let mut c2 = card(GIB);
    c2.residents = vec![CardResident {
        class: "chat".into(),
        model: "demo-m".into(),
        bytes: 768 * MIB,
    }];
    assert_eq!(
        c2.beside_itself(&snap, Class::Chat, "demo/other.gguf")
            .residents
            .len(),
        1
    );
}

/// The note a fitted row keeps (D26): its context, the measure and its bytes.
#[test]
fn a_fitted_row_s_note_names_its_measure() {
    let f = files(summary(131_072, 8 * MIB));
    let w = whole(&f, &chat_params(131_072), &card(256 * MIB));
    let n = note(&w, "2026-11-04 10:00:00").unwrap();
    assert_eq!(n.ctx, w.ctx.unwrap());
    assert_eq!(n.trained_ctx, Some(131_072));
    assert_eq!(n.measure, FitMeasure::WholeCard);
    assert_eq!(n.capacity_bytes, 256 * MIB);
    let small = files(summary(8192, 8 * MIB));
    let fits = whole(&small, &chat_params(8192), &card(GIB));
    assert_eq!(note(&fits, "x"), None, "a row planned whole needs no note");
}
