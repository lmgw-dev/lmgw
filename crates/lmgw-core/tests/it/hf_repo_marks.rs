//! The file list's marks and its recommended file (find-a-model design WP3,
//! §3.3, D60 to D65): `hf_repo` with `fit` against the mock hub and CDN of
//! `plan_hub`, serving synthetic GGUF headers.
//!
//! - Without `fit` nothing is read; with it, one header, the files near the
//!   card's edge and the pick's candidates are.
//! - Over a sweep of cards, every mark is the one the file's own header
//!   gives, and the recommended file and both picks are the ones every
//!   header read gives: the estimates and the band never change an answer,
//!   they only save reads.
//! - A file whose estimate doesn't fit but whose header does is read (the
//!   band); a file far past the edge isn't; a file under 4 bits per weight
//!   by its bytes alone is never read for the pick.
//! - A reference whose header the CDN cuts is marked by its size and the
//!   next file is read instead, a failed read's bytes counted; no card reads
//!   nothing; the image class reads nothing; the hold is said once.
//! - [`sweeps`]: the same sweep over the repo shapes the review asked for
//!   (D66, D67): a split reference whose head is in its second part,
//!   subdirectories with and without a projector, a tied head beside
//!   per-layer embeddings, 32- and 16-bit embeddings beside 2-bit layers, a
//!   mixture of experts, an aux repo, residents on the card, a split set
//!   whose header declares another count.
//! - [`refusals`]: a hub that refuses every read is asked once (D66); a
//!   metered connection reads nothing (D68).
//!
//! The names are fake and nothing here reaches the network. Every test sets
//! `HF_ENDPOINT`, so each holds `common::process_env_lock` for its body.

use lmgw_api_types as dto;
use lmgw_api_types::model_find::{FitMark, PlanFit};
use lmgw_core::gguf::synth;
use lmgw_core::modelinfo::{self, hub_fit};
use lmgw_core::ops;
use lmgw_core::state::{AppState, SharedState};

use crate::common;
use crate::plan_hub::{hub, on_card, quants, Cdn, MIB, REPO};

mod refusals;
mod sweeps;

/// One chat model in five widths whose head isn't tied: the input
/// embeddings and the output head are each 4 Mi elements of the file's own
/// type, and the embeddings stay on the host, so how much of a file the
/// card holds differs by quant. 4 layers of a 4 Mi-element matrix and an
/// F32 norm each; 256 tokens of context, so the weights decide.
fn untied() -> Vec<(String, Vec<u8>)> {
    [
        ("Q2_K", 10),
        ("Q4_K_M", 12),
        ("Q8_0", 8),
        ("BF16", 30),
        ("F32", 0),
    ]
    .into_iter()
    .map(|(q, ty)| {
        let mut h = synth::chat("qwen3", 256);
        h.u32("qwen3.block_count", 4)
            .u32("qwen3.embedding_length", 1024)
            .u32("qwen3.attention.head_count", 8)
            .u32("qwen3.attention.head_count_kv", 8)
            .u32("qwen3.attention.key_length", 128)
            .u32("qwen3.attention.value_length", 128);
        h.tensor_typed("token_embd.weight", &[1024, 4096], ty)
            .tensor_typed("output.weight", &[1024, 4096], ty);
        for i in 0..4 {
            h.tensor_typed(&format!("blk.{i}.attn_norm.weight"), &[1024], 0)
                .tensor_typed(&format!("blk.{i}.ffn_up.weight"), &[1024, 4096], ty);
        }
        (format!("quants/demo-untied-{q}.gguf"), h.bytes())
    })
    .collect()
}

async fn marks(state: &SharedState) -> dto::RepoFiles {
    marks_for(state, "chat").await
}

async fn marks_for(state: &SharedState, target: &str) -> dto::RepoFiles {
    ops::hf_repo(state, REPO, None, target, dto::RepoFit::Headers)
        .await
        .unwrap()
}

/// Every file's exact fit on `state`'s card, each its own header's, for
/// `target`.
async fn exact_fits(
    state: &SharedState,
    files: &[(String, Vec<u8>)],
    target: &str,
) -> Vec<hub_fit::FileFits> {
    let listing = lmgw_core::hf::list_repo_files(&state.http, "", REPO)
        .await
        .unwrap();
    let mut out = Vec::new();
    for (p, _) in files {
        // A split file's later parts aren't rows of the list.
        if lmgw_core::hf::split_part_name(p).is_some_and(|(_, n, _)| n > 1) {
            continue;
        }
        out.push(hub_fit::file_fit(state, REPO, p, &listing, Some(target)).await);
    }
    out
}

fn row<'a>(listed: &'a dto::RepoFiles, file: &str) -> &'a dto::RepoFile {
    listed
        .files
        .iter()
        .find(|f| f.file == file)
        .unwrap_or_else(|| panic!("{file} in {:?}", listed.files))
}

fn fit_of(listed: &dto::RepoFiles, file: &str) -> PlanFit {
    row(listed, file)
        .fit
        .clone()
        .unwrap_or_else(|| panic!("{file} has no fit"))
}

/// What the CDN was asked since `from`.
fn asked_since(cdn: &Cdn, from: usize) -> Vec<String> {
    cdn.asked()[from..].to_vec()
}

#[tokio::test]
async fn without_fit_no_header_is_read() {
    let _env = common::process_env_lock().await;
    let files = quants();
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    let state = on_card(192 * MIB, 0, false).await;
    let listed = ops::hf_repo(&state, REPO, None, "chat", dto::RepoFit::No)
        .await
        .unwrap();
    assert!(cdn.asked().is_empty(), "{:?}", cdn.asked());
    assert!(listed.files.iter().all(|f| f.fit.is_none()));
    assert_eq!(
        (
            &listed.recommended,
            &listed.picks,
            &listed.card,
            listed.header_bytes_read
        ),
        (&None, &None, &None, 0)
    );

    // Over HTTP the same, and `fit=true` reads.
    let gw = common::serve(state.clone()).await;
    let get = |q: String| {
        let gw = &gw;
        async move {
            let r = gw
                .client()
                .get(format!("{gw}/api/hf/repo?repo={REPO}{q}"))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), 200);
            let v: serde_json::Value = r.json().await.unwrap();
            common::round_trips::<dto::RepoFiles>("GET /api/hf/repo", &v)
        }
    };
    let plain = get(String::new()).await;
    assert!(plain.files.iter().all(|f| f.fit.is_none()));
    assert!(cdn.asked().is_empty());
    let fitted = get("&fit=true".into()).await;
    assert_eq!(
        fitted.recommended.as_deref(),
        Some("quants/demo-chat-Q8_0.gguf"),
        "{fitted:?}"
    );
    assert!(fitted.header_bytes_read > 0);
    assert!(!cdn.asked().is_empty());
    // The page's own ask on a connection that isn't metered reads as well.
    let page = get("&fit=unless_metered".into()).await;
    assert_eq!(page.metered, None);
    assert_eq!(page.recommended, fitted.recommended);
}

/// The estimates, the band and the pick's reads never change an answer:
/// over cards from too small for anything to roomy for everything, empty
/// and with another program holding a third, every mark is the file's own
/// header's, a file read is marked exactly as its plan is, an estimate is
/// never below it, and the recommended file and both picks are what every
/// header gives.
#[tokio::test]
async fn the_marks_and_the_pick_are_what_every_header_gives() {
    let _env = common::process_env_lock().await;
    let files = untied();
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    let s = sweeps::sweep(&cdn, &files, "chat", (40, 200, 8), sweeps::Card::Plain).await;
    assert!(s.estimated > 0, "some files were estimated");
    assert!(
        s.reads < s.steps * files.len(),
        "{} reads over {} lists of {} files",
        s.reads,
        s.steps,
        files.len()
    );
}

/// The band (D61): with the card just big enough for the BF16 file's own
/// footprint at one token, its estimate (its embeddings charged to the
/// card) doesn't fit, but its header does: it is read and marked as
/// fitting. The F32 file, past the edge by more than its
/// embeddings could weigh, isn't read.
#[tokio::test]
async fn a_file_near_the_edge_is_read_exactly() {
    let _env = common::process_env_lock().await;
    let files = untied();
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    let (bf16, f32) = (&files[3].0, &files[4].0);
    // The BF16 file's own footprint at one token, on a roomy card.
    let roomy = on_card(1024 * MIB, 0, false).await;
    let listing = lmgw_core::hf::list_repo_files(&roomy.http, "", REPO)
        .await
        .unwrap();
    let exact = hub_fit::file_fit(&roomy, REPO, bf16, &listing, Some("chat")).await;
    let plan = modelinfo::local_model_plan_hub(&roomy, REPO, bf16, Some("chat"))
        .await
        .unwrap();
    let params = plan["params"].as_object().unwrap().clone();
    let pf = exact.files.as_ref().unwrap();
    let at1 = modelinfo::fit::Sizer::chat(pf, &params).at(1).total_bytes;
    let headroom = 32 * MIB;
    let total = at1 + headroom + 64 * 1024;
    let bf16_bytes = exact.bytes.unwrap();
    assert!(
        bf16_bytes + headroom > total,
        "the BF16 file must not fit by its size alone, or it is the reference"
    );
    assert!(
        pf.summary.host_input_bytes > 64 * 1024,
        "its embeddings decide"
    );

    let state = on_card(total, 0, false).await;
    let from = cdn.asked().len();
    let listed = marks(&state).await;
    let asked = asked_since(&cdn, from);
    assert_ne!(asked.first(), Some(bf16), "not the reference: {asked:?}");
    assert!(asked.contains(bf16), "{asked:?}");
    assert_eq!(row(&listed, bf16).estimated_from, None);
    let read = fit_of(&listed, bf16).whole_card.mark;
    assert!(read.fits() && read != FitMark::FitsBySize, "{read:?}");
    assert!(!asked.contains(f32), "{asked:?}");
    assert!(row(&listed, f32).estimated_from.is_some());
    assert_eq!(fit_of(&listed, f32).whole_card.mark, FitMark::DoesNotFit);
}

/// A reference whose header the CDN cuts is marked by its size alone (D8),
/// the bytes it brought counted (D66), and the next file in the order is
/// read in its place: the Q8_0, the largest file whose label names a width
/// the pick takes (D67), then the Q4_K_M. A file marked by its size is
/// never the pick, so the Q4_K_M is. The 3-bit file is under 4 bits by its
/// bytes and the BF16 and F32 files over 8.5 by theirs (D67's cap side): none
/// of them is read.
#[tokio::test]
async fn a_cut_reference_is_marked_by_size_and_the_next_one_read() {
    let _env = common::process_env_lock().await;
    let files = quants();
    let q8 = &files[2].0;
    let (_hub, _cdn_server, cdn) = hub(&files, &[(q8.as_str(), 40)]).await;
    let state = on_card(192 * MIB, 0, false).await;
    let listed = marks(&state).await;
    assert_eq!(cdn.asked(), vec![q8.clone(), files[1].0.clone()]);
    assert!(listed.header_bytes_read > 40);
    let cut = fit_of(&listed, q8);
    assert_eq!(cut.whole_card.mark, FitMark::FitsBySize, "{cut:?}");
    assert!(cut
        .whole_card
        .notes
        .iter()
        .any(|n| n.contains("marked by its size")));
    assert_eq!(listed.recommended.as_deref(), Some(files[1].0.as_str()));
    for other in [&files[0].0, &files[3].0, &files[4].0] {
        assert_eq!(
            row(&listed, other).estimated_from.as_deref(),
            Some(files[1].0.as_str())
        );
        assert_eq!(fit_of(&listed, other).whole_card.mark, FitMark::Fits);
    }
    let picks = listed.picks.unwrap();
    assert!(picks.beside.why.contains("The largest file"), "{picks:?}");
    // Every file here is weights, so every one has a fit.
    assert!(listed.files.iter().all(|f| f.fit.is_some()));
    // The list says once where the estimates came from.
    assert!(listed
        .fit_notes
        .iter()
        .any(|n| n.contains(&format!("estimated from the header of {}", files[1].0))));
}

/// A read that fails after it began counts what it received (review 9):
/// every header cut 40 bytes in, each file is tried as the reference once,
/// and the list says it read 40 bytes of each.
#[tokio::test]
async fn a_failed_read_counts_the_bytes_it_received() {
    let _env = common::process_env_lock().await;
    let files = quants();
    let cut: Vec<(&str, usize)> = files.iter().map(|(p, _)| (p.as_str(), 40)).collect();
    let (_hub, _cdn_server, cdn) = hub(&files, &cut).await;
    let state = on_card(192 * MIB, 0, false).await;
    let listed = marks(&state).await;
    assert_eq!(cdn.asked().len(), files.len(), "{:?}", cdn.asked());
    assert_eq!(listed.header_bytes_read, 40 * files.len() as u64);
    assert_eq!(listed.recommended, None);
}

/// Without a card to measure against (D9) nothing is read and nothing is
/// recommended: every mark is `no_card`, and the list says why.
#[tokio::test]
async fn without_a_card_nothing_is_read() {
    let _env = common::process_env_lock().await;
    let files = quants();
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    let state = AppState::init_for_tests().await.unwrap();
    let listed = marks(&state).await;
    assert!(cdn.asked().is_empty(), "{:?}", cdn.asked());
    assert_eq!(listed.recommended, None);
    assert_eq!(listed.card, None);
    assert!(
        listed
            .fit_notes
            .iter()
            .any(|n| n.contains("can't measure the graphics card")),
        "{:?}",
        listed.fit_notes
    );
    for f in &listed.files {
        let fit = f.fit.as_ref().unwrap();
        assert_eq!(
            (fit.whole_card.mark, fit.beside.mark),
            (FitMark::NoCard, FitMark::NoCard)
        );
    }
}

/// The image class has no header plan (D51): `fit` reads nothing and says
/// why; the GPU hold changes no mark and is said once for the list (D10).
#[tokio::test]
async fn the_image_class_reads_nothing_and_the_hold_is_said_once() {
    let _env = common::process_env_lock().await;
    let files = quants();
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    let state = on_card(192 * MIB, 0, false).await;
    let image = ops::hf_repo(&state, REPO, None, "image", dto::RepoFit::Headers)
        .await
        .unwrap();
    assert!(cdn.asked().is_empty(), "{:?}", cdn.asked());
    assert!(image.files.iter().all(|f| f.fit.is_none()));
    assert_eq!(image.picks, None);
    assert!(
        image.fit_notes.iter().any(|n| n.contains("image")),
        "{:?}",
        image.fit_notes
    );

    let free = marks(&state).await;
    let held = marks(&on_card(192 * MIB, 0, true).await).await;
    assert_eq!(held.recommended, free.recommended);
    assert!(held
        .fit_notes
        .contains(&modelinfo::fit::HOLD_LINE.to_string()));
    assert!(!free
        .fit_notes
        .contains(&modelinfo::fit::HOLD_LINE.to_string()));
    for f in &free.files {
        let (a, b) = (f.fit.as_ref().unwrap(), fit_of(&held, &f.file));
        assert_eq!(
            (a.whole_card.mark, a.beside.mark),
            (b.whole_card.mark, b.beside.mark)
        );
    }
}
