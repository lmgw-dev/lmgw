//! The sweep that holds every mark and the pick to every header's (WP3,
//! §3.3), over the repo shapes the review of WP3 asked for (D66, D67).
//!
//! Each shape is one synthetic model in several widths, sized so the
//! card's edge crosses its files as the sweep's cards grow: 512-wide, a
//! vocabulary of 8192 (4 Mi elements of input embeddings beside 4 Mi of
//! layers), so where the embeddings sit decides marks.

use std::collections::BTreeSet;

use lmgw_api_types as dto;
use lmgw_api_types::model_find::FitMark;
use lmgw_core::config::Settings;
use lmgw_core::gguf::synth;
use lmgw_core::modelinfo::hub_fit::{self, marks::compacted};
use lmgw_core::runtime::Class;
use lmgw_core::state::SharedState;
use lmgw_core::store;

use super::{asked_since, exact_fits, marks_for, row};
use crate::common;
use crate::plan_hub::{hub, on_card, Cdn, MIB};

/// ggml type ids.
const F32: u32 = 0;
const F16: u32 = 1;
const Q8_0: u32 = 8;
const Q2_K: u32 = 10;
const Q3_K: u32 = 11;
const Q4_K: u32 = 12;
const Q5_K: u32 = 13;
const Q6_K: u32 = 14;
const BF16: u32 = 30;

const WIDTH: u64 = 512;
const VOCAB: u64 = 8192;

/// What the sweep's cards carry beside the repo's files.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Card {
    Plain,
    /// Two of lmgw's chat models on the card: one serving another file
    /// (beside every file), one serving the first file of the repo (left out
    /// for that file alone, D55).
    Residents,
}

/// What a sweep read and estimated.
#[derive(Default)]
pub(super) struct Sweep {
    pub reads: usize,
    pub estimated: usize,
    pub steps: usize,
    /// Every file an estimate came from.
    pub references: BTreeSet<String>,
}

/// A card of `total` with `used` taken by other programs, and for
/// [`Card::Residents`] lmgw's two models on it.
async fn state_for(
    total: u64,
    used: u64,
    card: Card,
    first: &str,
) -> (SharedState, Option<tempfile::TempDir>) {
    if card == Card::Plain {
        return (on_card(total, used, false).await, None);
    }
    let state = on_card(total, used, false).await;
    let dir = tempfile::tempdir().unwrap();
    let mut settings: Settings = state.snapshot().settings.clone();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    let mut other = base(256);
    embeddings(&mut other, Q8_0, true);
    layers(&mut other, Q8_0);
    other.write_to(&dir.path().join("other/voice-Q8_0.gguf"));
    let own = format!("{}/{first}", crate::plan_hub::REPO);
    for (id, gguf) in [("voice", "other/voice-Q8_0.gguf"), ("loaded", own.as_str())] {
        store::insert_local_model(
            &state.db,
            &store::NewLocalModel {
                model_id: id.into(),
                gguf_path: gguf.into(),
                params: Default::default(),
                args: vec![],
                idle_seconds: 0,
                enabled: true,
                public: true,
                image: None,
                extra_run_args: None,
                warm_start: false,
                fallback: None,
                capabilities_override: None,
                ladder: vec![],
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    state.runtime().ready_for_tests(Class::Chat, "voice", 0);
    state.runtime().ready_for_tests(Class::Chat, "loaded", 0);
    (state, Some(dir))
}

/// Over cards of `mibs` MiB (from, to, step), empty and with a third held
/// by another program: every file's marks are its own header's, a file
/// read is marked as its plan is, an estimate is never below its file's
/// plan, each header is read once, and the recommended file and both picks
/// are what every header gives.
pub(super) async fn sweep(
    cdn: &Cdn,
    files: &[(String, Vec<u8>)],
    target: &str,
    mibs: (u64, u64, u64),
    card: Card,
) -> Sweep {
    let mut out = Sweep::default();
    let (from, to, step) = mibs;
    let first = files[0].0.clone();
    for total in (from..=to).step_by(step as usize).map(|m| m * MIB) {
        for used in [0, total / 3] {
            out.steps += 1;
            let (state, _dir) = state_for(total, used, card, &first).await;
            let at = format!("{} MiB, {} MiB used", total / MIB, used / MIB);
            let start = cdn.asked().len();
            let listed = marks_for(&state, target).await;
            let asked = asked_since(cdn, start);
            out.reads += asked.len();
            let exact = exact_fits(&state, files, target).await;
            let list_card = listed.card.clone().expect("a card");
            if card == Card::Residents {
                assert_eq!(list_card.residents.len(), 2, "at {at}: {list_card:?}");
                // The loaded row's own file is measured without it.
                let own = row(&listed, &first).fit.clone().unwrap();
                let own_card = own.card.expect("its own card, not the list's");
                assert_eq!(own_card.residents.len(), 1, "at {at}");
            }
            for e in &exact {
                let r = row(&listed, &e.file);
                let a = r.fit.clone().unwrap();
                assert_eq!(
                    (a.whole_card.mark, a.beside.mark),
                    (e.fit.whole_card.mark, e.fit.beside.mark),
                    "{} at {at}: {a:?} against {:?}",
                    e.file,
                    e.fit
                );
                match &r.estimated_from {
                    None => assert_eq!(
                        a,
                        compacted(e.fit.clone(), &list_card, &listed.fit_notes),
                        "{} at {at}",
                        e.file
                    ),
                    Some(from) => {
                        out.estimated += 1;
                        out.references.insert(from.clone());
                        assert!(!asked.contains(&e.file), "{} at {at}", e.file);
                        for (est, real) in [
                            (&a.whole_card, &e.fit.whole_card),
                            (&a.beside, &e.fit.beside),
                        ] {
                            assert!(est.ctx <= real.ctx, "{} at {at}", e.file);
                            if est.ctx == real.ctx {
                                assert!(est.uses_bytes >= real.uses_bytes, "{} at {at}", e.file);
                            }
                            assert_eq!(est.bits_per_weight, None);
                        }
                    }
                }
            }
            let (whole, beside) = hub_fit::pick_both(&exact);
            assert_eq!(
                listed.picks,
                Some(dto::RepoPicks {
                    whole_card: whole.clone(),
                    beside: beside.clone()
                }),
                "at {at}"
            );
            assert_eq!(listed.recommended, beside.file.or(whole.file), "at {at}");
            let mut once = asked.clone();
            once.sort();
            once.dedup();
            assert_eq!(once.len(), asked.len(), "at {at}: {asked:?}");
            assert!(listed.header_bytes_read > 0, "at {at}");
        }
    }
    out
}

/// A causal 512-wide model of four layers and `ctx` trained tokens.
fn base(ctx: u32) -> synth::Header {
    let mut h = synth::chat("qwen3", ctx);
    h.u32("qwen3.block_count", 4)
        .u32("qwen3.embedding_length", WIDTH as u32)
        .u32("qwen3.attention.head_count", 8)
        .u32("qwen3.attention.head_count_kv", 8)
        .u32("qwen3.attention.key_length", 64)
        .u32("qwen3.attention.value_length", 64);
    h
}

/// The input embeddings of `ty`, and an untied head of `ty` when `untied`.
fn embeddings(h: &mut synth::Header, ty: u32, untied: bool) {
    h.tensor_typed("token_embd.weight", &[WIDTH, VOCAB], ty);
    if untied {
        h.tensor_typed("output.weight", &[WIDTH, VOCAB], ty);
    }
}

/// Four layers of a 1 Mi-element matrix of `ty` and an F32 norm each.
fn layers(h: &mut synth::Header, ty: u32) {
    layer_range(h, ty, 0..4);
}

fn layer_range(h: &mut synth::Header, ty: u32, range: std::ops::Range<u32>) {
    for i in range {
        h.tensor_typed(&format!("blk.{i}.attn_norm.weight"), &[WIDTH], F32)
            .tensor_typed(&format!("blk.{i}.ffn_up.weight"), &[WIDTH, 2048], ty);
    }
}

/// One plain file of `ty` throughout, its head untied.
fn plain(path: &str, ty: u32) -> (String, Vec<u8>) {
    let mut h = base(256);
    embeddings(&mut h, ty, true);
    layers(&mut h, ty);
    (path.to_string(), h.bytes())
}

/// The two parts of a split Q8_0 whose first part's header declares
/// `declared` parts: the input embeddings and two layers in the first, two
/// layers and the untied head in the second, so the first part alone reads
/// as a tied model.
fn split_q8(prefix: &str, declared: u64) -> [(String, Vec<u8>); 2] {
    let mut one = base(256);
    one.u64("split.no", 0)
        .u64("split.count", declared)
        .u64("split.tensors.count", 10);
    one.tensor_typed("token_embd.weight", &[WIDTH, VOCAB], Q8_0);
    layer_range(&mut one, Q8_0, 0..2);
    let mut two = synth::Header::default();
    two.u64("split.no", 1)
        .u64("split.count", declared)
        .u64("split.tensors.count", 10);
    layer_range(&mut two, Q8_0, 2..4);
    two.tensor_typed("output.weight", &[WIDTH, VOCAB], Q8_0);
    [
        (format!("{prefix}-00001-of-00002.gguf"), one.bytes()),
        (format!("{prefix}-00002-of-00002.gguf"), two.bytes()),
    ]
}

/// A split reference whose untied head is in its second part (review 2):
/// the band takes the embeddings off by every part's tensors, so the other
/// files' marks stay their headers'.
#[tokio::test]
async fn a_split_reference_whose_head_is_in_a_later_part() {
    let _env = common::process_env_lock().await;
    let mut files: Vec<(String, Vec<u8>)> = split_q8("q/m-Q8_0", 2).into();
    files.push(plain("q/m-Q4_K_M.gguf", Q4_K));
    files.push(plain("q/m-Q2_K.gguf", Q2_K));
    files.push(plain("q/m-BF16.gguf", BF16));
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    let s = sweep(&cdn, &files, "chat", (36, 80, 2), Card::Plain).await;
    assert!(
        s.references.contains(&files[0].0),
        "the split file was a reference: {:?}",
        s.references
    );
}

/// A split set whose first part declares three parts while its name says
/// two (D54): it doesn't load as it is, so it is marked by its size, and
/// the files beside it are still marked as their headers mark them.
#[tokio::test]
async fn a_split_set_that_declares_another_count() {
    let _env = common::process_env_lock().await;
    let mut files: Vec<(String, Vec<u8>)> = split_q8("q/m-Q8_0", 3).into();
    files.push(plain("q/m-Q4_K_M.gguf", Q4_K));
    files.push(plain("q/m-Q6_K.gguf", Q6_K));
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    sweep(&cdn, &files, "chat", (36, 80, 4), Card::Plain).await;
    let state = on_card(64 * MIB, 0, false).await;
    let listed = marks_for(&state, "chat").await;
    let split = row(&listed, &files[0].0);
    let mark = split.fit.as_ref().unwrap().whole_card.mark;
    assert!(
        matches!(mark, FitMark::FitsBySize | FitMark::DoesNotFitBySize)
            || split.estimated_from.is_some(),
        "by its size, or estimated: {split:?}"
    );
}

/// Weights in subdirectories (review 1): a directory with its own
/// projector is estimated only from a file of its own, one without from a
/// file of a directory without either, so no estimate charges a projector
/// its file's plan wouldn't name, or leaves out one it would.
#[tokio::test]
async fn subdirectories_with_and_without_a_projector() {
    let _env = common::process_env_lock().await;
    let files = vec![
        plain("m-Q4_K_M.gguf", Q4_K),
        plain("m-Q8_0.gguf", Q8_0),
        (
            "mmproj-m-F16.gguf".to_string(),
            crate::plan_pin::projector(6 * MIB).bytes(),
        ),
        plain("plain/m-Q6_K.gguf", Q6_K),
        plain("plain/m-BF16.gguf", BF16),
        plain("vis/m-Q5_K_M.gguf", Q5_K),
        plain("vis/m-Q3_K_M.gguf", Q3_K),
        (
            "vis/mmproj-m-F16.gguf".to_string(),
            crate::plan_pin::projector(2 * MIB).bytes(),
        ),
    ];
    let weights: Vec<(String, Vec<u8>)> = files
        .iter()
        .filter(|(p, _)| !p.contains("mmproj"))
        .cloned()
        .collect();
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    let s = sweep(&cdn, &weights, "chat", (36, 90, 3), Card::Plain).await;
    assert!(s.estimated > 0);
    // Every estimate came from a file with the same companions.
    for step in [48u64, 60, 72] {
        let state = on_card(step * MIB, 0, false).await;
        let listed = marks_for(&state, "chat").await;
        for f in &listed.files {
            if let Some(from) = &f.estimated_from {
                let dir = |p: &str| p.rsplit_once('/').map(|(d, _)| d.to_string());
                match dir(&f.file).as_deref() {
                    None | Some("vis") => assert_eq!(dir(from), dir(&f.file), "{f:?}"),
                    Some(_) => assert_eq!(dir(from).as_deref(), Some("plain"), "{f:?}"),
                }
            }
        }
    }
}

/// A tied head beside per-layer input embeddings (the Gemma 4 E-models'
/// shape): those stay on the host though the head is tied, so the band
/// isn't empty (review, D61's correction).
#[tokio::test]
async fn a_tied_head_with_per_layer_embeddings() {
    let _env = common::process_env_lock().await;
    let files: Vec<(String, Vec<u8>)> = [
        ("Q2_K", Q2_K),
        ("Q4_K_M", Q4_K),
        ("Q8_0", Q8_0),
        ("BF16", BF16),
    ]
    .into_iter()
    .map(|(q, ty)| {
        let mut h = base(256);
        embeddings(&mut h, ty, false);
        h.tensor_typed("per_layer_token_embd.weight", &[2048, VOCAB], ty);
        layers(&mut h, ty);
        (format!("pl/m-{q}.gguf"), h.bytes())
    })
    .collect();
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    let s = sweep(&cdn, &files, "chat", (40, 120, 4), Card::Plain).await;
    assert!(s.estimated > 0);
}

/// 2- and 3-bit layers beside input embeddings and a head kept in F32 and
/// F16: the F32 file's embeddings weigh exactly the band's bound.
#[tokio::test]
async fn low_bit_layers_beside_wide_embeddings() {
    let _env = common::process_env_lock().await;
    let wide = |path: &str, embd: u32, ty: u32| {
        let mut h = base(256);
        embeddings(&mut h, embd, true);
        layers(&mut h, ty);
        (path.to_string(), h.bytes())
    };
    let files = vec![
        wide("w/m-Q2_K_XL.gguf", F32, Q2_K),
        wide("w/m-Q3_K_XL.gguf", F16, Q3_K),
        plain("w/m-Q4_K_M.gguf", Q4_K),
        plain("w/m-Q8_0.gguf", Q8_0),
    ];
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    sweep(&cdn, &files, "chat", (36, 100, 4), Card::Plain).await;
}

/// A mixture of experts: each layer's experts one 3-D tensor of the file's
/// type, its router an F32 matrix the width leaves out (D140).
#[tokio::test]
async fn a_mixture_of_experts() {
    let _env = common::process_env_lock().await;
    let files: Vec<(String, Vec<u8>)> = [
        ("Q4_K_M", Q4_K),
        ("Q6_K", Q6_K),
        ("Q8_0", Q8_0),
        ("BF16", BF16),
    ]
    .into_iter()
    .map(|(q, ty)| {
        let mut h = synth::chat("qwen3moe", 256);
        h.u32("qwen3moe.block_count", 4)
            .u32("qwen3moe.embedding_length", WIDTH as u32)
            .u32("qwen3moe.attention.head_count", 8)
            .u32("qwen3moe.attention.head_count_kv", 8)
            .u32("qwen3moe.attention.key_length", 64)
            .u32("qwen3moe.attention.value_length", 64)
            .u32("qwen3moe.expert_count", 4)
            .u32("qwen3moe.expert_used_count", 2)
            .u32("qwen3moe.expert_feed_forward_length", 512);
        embeddings(&mut h, ty, true);
        for i in 0..4 {
            h.tensor_typed(&format!("blk.{i}.attn_norm.weight"), &[WIDTH], F32)
                .tensor_typed(&format!("blk.{i}.ffn_gate_inp.weight"), &[WIDTH, 4], F32)
                .tensor_typed(&format!("blk.{i}.ffn_up_exps.weight"), &[WIDTH, 512, 4], ty);
        }
        (format!("moe/m-{q}.gguf"), h.bytes())
    })
    .collect();
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    sweep(&cdn, &files, "chat", (36, 100, 4), Card::Plain).await;
}

/// An embedder's quants listed for the aux class: the aux plan charges the
/// embeddings to the card, so the estimates are exact but for the width.
#[tokio::test]
async fn an_aux_repo() {
    let _env = common::process_env_lock().await;
    let files: Vec<(String, Vec<u8>)> =
        [("Q4_K_M", Q4_K), ("Q8_0", Q8_0), ("F16", F16), ("F32", F32)]
            .into_iter()
            .map(|(q, ty)| {
                let mut h = synth::Header::default();
                h.str("general.architecture", "bert")
                    .str("general.name", "synthetic embedder")
                    .u32("bert.context_length", 512)
                    .u32("bert.block_count", 4)
                    .u32("bert.embedding_length", WIDTH as u32)
                    .u32("bert.attention.head_count", 8)
                    .u32("bert.pooling_type", 1);
                h.tensor_typed("token_embd.weight", &[WIDTH, VOCAB], ty);
                layers(&mut h, ty);
                (format!("embed/m-{q}.gguf"), h.bytes())
            })
            .collect();
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    sweep(&cdn, &files, "aux", (34, 80, 2), Card::Plain).await;
}

/// lmgw's own models on the card (D55, D120): one beside every file, and one
/// serving the repo's first file, left out of that file's measure alone.
#[tokio::test]
async fn residents_on_the_card() {
    let _env = common::process_env_lock().await;
    let files = vec![
        plain("r/m-Q4_K_M.gguf", Q4_K),
        plain("r/m-Q2_K.gguf", Q2_K),
        plain("r/m-Q8_0.gguf", Q8_0),
        plain("r/m-BF16.gguf", BF16),
    ];
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    sweep(&cdn, &files, "chat", (48, 120, 4), Card::Residents).await;
}
