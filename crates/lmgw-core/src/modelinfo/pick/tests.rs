//! The pick (find-a-model design §3.2, D7 as ruled 2026-11-04, §10's pick
//! group): each rule, the 4-bit floor, the Q8 cap, ties, the two measures
//! differing, the fold's file, and a file marked by size never picked.

use super::*;

const GB: u64 = 1_000_000_000;

/// A file of `gb` GB at `bits` per weight whose fit is `mark` at `ctx` of a
/// model trained on `trained` tokens.
fn file(
    name: &str,
    gb: f64,
    bits: f64,
    mark: FitMark,
    ctx: Option<u64>,
    trained: u64,
) -> Candidate {
    Candidate {
        file: format!("demo-{name}.gguf"),
        quant: Some(name.into()),
        bytes: (gb * GB as f64) as u64,
        fit: FileFit {
            mark,
            ctx,
            trained_ctx: Some(trained),
            bits_per_weight: Some(bits),
            ..FileFit::default()
        },
    }
}

const T: u64 = 262_144;

fn whole(name: &str, gb: f64, bits: f64) -> Candidate {
    file(name, gb, bits, FitMark::Fits, Some(T), T)
}

fn shorter(name: &str, gb: f64, bits: f64, ctx: u64) -> Candidate {
    file(name, gb, bits, FitMark::FitsShorter, Some(ctx), T)
}

fn none(name: &str, gb: f64, bits: f64) -> Candidate {
    file(name, gb, bits, FitMark::DoesNotFit, None, T)
}

fn picked(c: &[Candidate]) -> FilePick {
    pick(c, FitMeasure::WholeCard, 24 * GB)
}

/// Rule 1: the largest file with the whole trained context, over a larger
/// one that fits only a shorter conversation.
#[test]
fn the_largest_file_with_the_whole_context_is_picked() {
    let c = [
        whole("Q4_K_M", 16.8, 4.9),
        whole("Q5_K_M", 19.4, 5.7),
        shorter("Q6_K", 22.1, 6.6, 65_536),
        none("Q8_0", 28.6, 8.5),
    ];
    let p = picked(&c);
    assert_eq!(p.file.as_deref(), Some("demo-Q5_K_M.gguf"), "{p:?}");
    assert_eq!(p.rule, PickRule::WholeContext);
    assert!(
        p.why.contains("whole conversation length") && p.why.contains("262 144 tokens"),
        "{}",
        p.why
    );
    assert_eq!(p.full_context, None);
}

/// Rule 2: no file keeps the whole context, so the largest that still
/// remembers 32 768 tokens; one that remembers fewer is passed over.
#[test]
fn the_32k_fallback() {
    let c = [
        shorter("Q4_K_M", 16.8, 4.9, 49_152),
        shorter("Q5_K_M", 19.4, 5.7, 32_768),
        shorter("Q6_K", 22.1, 6.6, 20_000),
        none("Q8_0", 28.6, 8.5),
    ];
    let p = picked(&c);
    assert_eq!(p.file.as_deref(), Some("demo-Q5_K_M.gguf"), "{p:?}");
    assert_eq!(p.rule, PickRule::ShorterContext);
    assert!(
        p.why.contains("can remember 262 144 tokens") && p.why.contains("remembers 32 768"),
        "{}",
        p.why
    );
}

/// A model trained on fewer than 32 768 tokens: rule 2's conversation is the
/// trained one, so a file that fits less isn't picked.
#[test]
fn a_trained_context_below_32k_is_its_own_floor() {
    let t = 16_384;
    let c = [file(
        "Q8_0",
        9.0,
        8.5,
        FitMark::FitsShorter,
        Some(12_000),
        t,
    )];
    assert_eq!(shorter_target(Some(t)), t);
    let p = picked(&c);
    assert_eq!((p.file, p.rule), (None, PickRule::None));
    assert!(p.why.contains("16 384 tokens"), "{}", p.why);
    let c = [
        file("Q8_0", 9.0, 8.5, FitMark::FitsShorter, Some(12_000), t),
        file("Q6_K", 7.0, 6.6, FitMark::Fits, Some(t), t),
    ];
    assert_eq!(picked(&c).file.as_deref(), Some("demo-Q6_K.gguf"));
}

/// Never below 4 bits per weight: a 3-bit file that keeps the whole context
/// is passed over for a 4-bit one that fits 32 768.
#[test]
fn the_4_bit_floor() {
    let c = [
        whole("Q3_K_M", 13.0, 3.9),
        whole("IQ2_M", 9.0, 2.7),
        shorter("Q4_K_M", 16.8, 4.0, 40_000),
    ];
    let p = picked(&c);
    assert_eq!(p.file.as_deref(), Some("demo-Q4_K_M.gguf"), "{p:?}");
    assert_eq!(p.rule, PickRule::ShorterContext);
    // Only below it: nothing.
    let p = picked(&c[..2]);
    assert_eq!(p.file, None, "{p:?}");
    assert!(p.why.contains("4 to 8.5 bits per weight"), "{}", p.why);
}

/// Capped at the Q8 class: a small model that fits in BF16 and F32 gets its
/// Q8_0; the wider files keep their marks.
#[test]
fn the_q8_cap_with_bf16_and_f32_present() {
    let c = [
        whole("F32", 2.4, 32.0),
        whole("BF16", 1.2, 16.0),
        whole("Q8_0", 0.64, 8.5),
        whole("Q4_K_M", 0.4, 4.8),
    ];
    let p = picked(&c);
    assert_eq!(p.file.as_deref(), Some("demo-Q8_0.gguf"), "{p:?}");
    assert_eq!(
        c[1].fit.mark,
        FitMark::Fits,
        "the wider files keep their marks"
    );
}

/// The width a header gives a file of `ty` matrices with F32 norms and
/// biases beside every layer, as llama.cpp's quantizer writes it.
fn header_bits(ty: u32) -> f64 {
    use crate::gguf::{read_header_from, synth, ModelSummary};
    let mut h = synth::chat("demoarch", 4096);
    h.tensor_typed("token_embd.weight", &[256, 1000], 0);
    for i in 0..4 {
        h.tensor_typed(&format!("blk.{i}.attn_norm.weight"), &[256], 0)
            .tensor_typed(&format!("blk.{i}.attn_q.weight"), &[256, 256], ty)
            .tensor_typed(&format!("blk.{i}.attn_q.bias"), &[256], 0)
            .tensor_typed(&format!("blk.{i}.ffn_up.weight"), &[256, 1024], ty);
    }
    let b = h.bytes();
    let meta = read_header_from(std::io::Cursor::new(b.clone()), b.len() as u64).unwrap();
    ModelSummary::from_meta(&meta)
        .layer_bits_per_weight
        .unwrap()
}

/// The Q8 cap over widths read from headers that carry F32 norms and biases
/// (review blocker): the Q8_0 reads 8.5, not over the cap, and is rule 1's
/// pick over the BF16 and F32 files, rule 2's when nothing keeps the whole
/// context, and the fold's file for a wider file that fits shorter.
#[test]
fn a_q8_0_with_f32_norms_is_within_the_cap() {
    let (f32s, bf16, q8, q4) = (
        header_bits(0),
        header_bits(30),
        header_bits(8),
        header_bits(12),
    );
    assert_eq!((f32s, bf16, q8, q4), (32.0, 16.0, 8.5, 4.5));
    let c = [
        whole("F32", 2.4, f32s),
        whole("BF16", 1.2, bf16),
        whole("Q8_0", 0.64, q8),
        whole("Q4_K_M", 0.4, q4),
    ];
    let p = picked(&c);
    assert_eq!(p.file.as_deref(), Some("demo-Q8_0.gguf"), "{p:?}");
    assert_eq!(p.rule, PickRule::WholeContext);

    let c = [
        shorter("BF16", 1.2, bf16, 65_536),
        shorter("Q8_0", 0.64, q8, 49_152),
        shorter("Q4_K_M", 0.4, q4, 40_000),
    ];
    let p = picked(&c);
    assert_eq!(p.file.as_deref(), Some("demo-Q8_0.gguf"), "{p:?}");
    assert_eq!(p.rule, PickRule::ShorterContext);

    let c = [shorter("BF16", 1.2, bf16, 65_536), whole("Q8_0", 0.64, q8)];
    let f = full_context_for(&c[0], &c).unwrap();
    assert_eq!(f.file, "demo-Q8_0.gguf");
    assert!(
        f.sentence.contains("at 8.5 bits per weight"),
        "{}",
        f.sentence
    );
}

/// The fold (D7): a file the card shows that isn't the pick (the person's
/// own, or the whole-card pick beside the beside one) and fits only a
/// shorter conversation names the smaller file of at least 4 bits that keeps
/// the whole one. The pick itself never needs it.
#[test]
fn the_fold_names_the_file_that_remembers_every_token() {
    let c = [
        shorter("Q8_0", 28.6, 8.5, 65_536),
        whole("Q4_K_S", 15.9, 4.6),
        whole("Q3_K_M", 13.0, 3.9),
    ];
    let f = full_context_for(&c[0], &c).unwrap();
    assert_eq!(f.file, "demo-Q4_K_S.gguf");
    assert_eq!(
        f.sentence,
        "Q4_K_S remembers all 262 144 tokens, at 4.6 bits per weight."
    );
    assert_eq!(
        full_context_for(&c[1], &c),
        None,
        "it keeps the whole context"
    );
    // The pick is Q4_K_S itself, by rule 1, with no fold.
    let p = picked(&c);
    assert_eq!(p.file.as_deref(), Some("demo-Q4_K_S.gguf"));
    assert_eq!(p.full_context, None);
    // Only the 3-bit one keeps it: nothing to name.
    assert_eq!(full_context_for(&c[0], &[c[0].clone(), c[2].clone()]), None);
}

/// No file at all, and files marked by size only (D8): never picked.
#[test]
fn no_file_or_only_size_marks_pick_nothing() {
    assert_eq!(picked(&[]).file, None);
    let mut c = whole("Q4_K_M", 16.8, 4.9);
    c.fit.mark = FitMark::FitsBySize;
    let p = picked(&[c]);
    assert_eq!((p.file, p.rule), (None, PickRule::None));
}

/// Equal sizes go by name, whichever order the list has.
#[test]
fn ties_by_size_go_by_name() {
    let a = whole("Q4_K_M", 16.8, 4.9);
    let b = whole("Q4_K_L", 16.8, 4.9);
    assert_eq!(
        picked(&[a.clone(), b.clone()]).file.as_deref(),
        Some("demo-Q4_K_L.gguf")
    );
    assert_eq!(picked(&[b, a]).file.as_deref(), Some("demo-Q4_K_L.gguf"));
}

/// The two measures pick differently (D120): beside a game the 19 GB file
/// fits only a shorter conversation, so the beside pick is the 16.8 GB one.
#[test]
fn the_whole_card_and_beside_picks_differ() {
    let card = [whole("Q4_K_M", 16.8, 4.9), whole("Q5_K_M", 19.4, 5.7)];
    let beside = [
        whole("Q4_K_M", 16.8, 4.9),
        shorter("Q5_K_M", 19.4, 5.7, 40_000),
    ];
    let w = pick(&card, FitMeasure::WholeCard, 24 * GB);
    let b = pick(&beside, FitMeasure::Beside, 24 * GB);
    assert_eq!(w.file.as_deref(), Some("demo-Q5_K_M.gguf"));
    assert_eq!(b.file.as_deref(), Some("demo-Q4_K_M.gguf"));
    assert!(
        b.why.contains("beside what's on your graphics card"),
        "{}",
        b.why
    );
    assert_eq!(b.measure, FitMeasure::Beside);
    // The whole-card pick, shown under the beside one, names the file that
    // keeps the whole conversation beside what runs: here it is the pick.
    assert_eq!(
        full_context_for(&beside[1], &beside).unwrap().file,
        "demo-Q4_K_M.gguf"
    );
}

#[test]
fn tokens_read_with_their_thousands_apart() {
    assert_eq!(tokens(262_144), "262 144");
    assert_eq!(tokens(32_768), "32 768");
    assert_eq!(tokens(512), "512");
    assert_eq!(tokens(1_048_576), "1 048 576");
}
