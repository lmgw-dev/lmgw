//! Fit again on a one-card machine (a bug found 2026-11-18):
//! the context search keeps free what admission keeps — a card's "Always
//! N" amount and the engine's learned cost — so the context it lands on is
//! one admission starts; the top-level verdict agrees with the cards'; and
//! the KV cache is sized at the row's own cache types.

use super::llama_fit_review::start;
use super::placing_splits::add_row;
use super::*;
use lmgw_api_types::model_find::FitMark;
use lmgw_api_types::model_get::ModelFitAgainArgs;
use lmgw_api_types::{KeepFree, KeepFreeMode};
use lmgw_core::config::LlamaParams;

const MIB: u64 = 1024 * 1024;

/// One 64 MiB card, "Always 8 MiB" kept free on it, 6 MiB of engine
/// learned there.
async fn one_card() -> Fixture {
    let f = fixture(64 * MIB, 8 * MIB, 8 * MIB, 4).await;
    f.world().cards = vec![("card 0".into(), 64 * MIB)];
    vram_status(&f.gateway).await;
    f.state.vram.seed_engine_cost(&card_id(0), 6 * MIB);
    let mut s = f.state.snapshot().settings.clone();
    s.vram.keep_free.insert(
        card_id(0),
        KeepFree {
            mode: KeepFreeMode::Always,
            mb: 8,
        },
    );
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    f
}

/// [`super::placing_splits::write_model`]'s model at its trained 32768
/// tokens, every layer on the card, with `cache` as both cache types.
fn params(cache: Option<&str>) -> LlamaParams {
    LlamaParams {
        ctx_size: Some(32768),
        n_gpu_layers: Some(999),
        cache_type_k: cache.map(str::to_string),
        cache_type_v: cache.map(str::to_string),
        ..Default::default()
    }
}

async fn fit_again(
    f: &Fixture,
    model_id: &str,
    confirm: Option<Vec<lmgw_api_types::model_get::FitChange>>,
) -> lmgw_api_types::model_get::ModelFitAgain {
    lmgw_core::ops::model_fit_again(
        &f.state,
        ModelFitAgainArgs {
            target: "chat".into(),
            model_id: model_id.into(),
            confirm,
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn fit_again_lands_on_a_context_admission_starts() {
    let f = one_card().await;
    add_row(&f, "fit-model", params(None), &[]).await;
    let offer = fit_again(&f, "fit-model", None).await;
    let fit = &offer.fit;
    assert_eq!(fit.mark, FitMark::FitsShorter, "{offer:?}");
    // What admission keeps beside it: the engine's 6 MiB and the card's
    // 8 MiB, whole.
    assert_eq!(fit.headroom_bytes, 14 * MIB, "{fit:?}");
    assert!(fit.uses_bytes + fit.headroom_bytes <= 64 * MIB, "{fit:?}");
    assert!(!offer.changes.is_empty(), "{offer:?}");
    // The sentence says it fits shorter, and the cards, judged at that
    // context, say it has room now (before, the sentence said the whole
    // context fitted while the cards said the card was short).
    assert!(
        offer.sentence.contains("can remember"),
        "{}",
        offer.sentence
    );
    let cards = fit.cards.as_ref().expect("the cards' verdict");
    if cards.ctx == fit.ctx {
        assert_eq!(
            cards.free.state,
            lmgw_api_types::model_find::FreeState::Now,
            "{cards:?}"
        );
    }

    // Saved, it starts: admission takes the context the fit found.
    let saved = fit_again(&f, "fit-model", Some(offer.changes.clone())).await;
    assert!(saved.saved, "{saved:?}");
    start(&f, "fit-model").await.unwrap();
    let a = super::llama_fit_review::argv(&f, "fit-model");
    assert_eq!(
        super::llama_fit_review::flag(&a, "--ctx-size").and_then(|c| c.parse::<u64>().ok()),
        fit.ctx,
        "{a:?}"
    );
}

/// q8_0 halves the KV cache, so the same card holds a longer context;
/// set in the row's own arguments it counts as in its fields, as admission
/// reads both.
#[tokio::test]
async fn the_rows_cache_types_size_its_kv_cache() {
    let f = one_card().await;
    add_row(&f, "f16-model", params(None), &[]).await;
    add_row(&f, "q8-model", params(Some("q8_0")), &[]).await;
    add_row(
        &f,
        "q8-args-model",
        params(None),
        &["--cache-type-k", "q8_0", "--cache-type-v", "q8_0"],
    )
    .await;
    let f16 = fit_again(&f, "f16-model", None).await.fit;
    let q8 = fit_again(&f, "q8-model", None).await.fit;
    let by_args = fit_again(&f, "q8-args-model", None).await.fit;
    let per = |fit: &lmgw_api_types::model_find::FileFit| {
        fit.terms.kv_cache_bytes as f64 / fit.ctx.unwrap() as f64
    };
    assert!(per(&q8) < per(&f16) * 0.6, "{q8:?} {f16:?}");
    assert!(q8.ctx > f16.ctx, "{q8:?} {f16:?}");
    assert_eq!(
        (by_args.ctx, by_args.terms.kv_cache_bytes),
        (q8.ctx, q8.terms.kv_cache_bytes),
        "{by_args:?}"
    );
}

/// Under a declared budget over several cards, Fit again reads one card by
/// its own rules and the engine's cost there, as admission places a model
/// on one card (owner, 2026-11-18): before, it summed every card's rules,
/// without the engine's cost, against the whole budget, and landed on a
/// context no card holds.
#[tokio::test]
async fn under_a_budget_over_several_cards_fit_again_reads_one_card() {
    let f = fixture(128 * MIB, 8 * MIB, 8 * MIB, 4).await;
    f.world().cards = (0..2).map(|i| (format!("card {i}"), 64 * MIB)).collect();
    vram_status(&f.gateway).await;
    let mut s = f.state.snapshot().settings.clone();
    s.vram.budget_mb = 100;
    for i in 0..2 {
        f.state.vram.seed_engine_cost(&card_id(i), 6 * MIB);
        s.vram.keep_free.insert(
            card_id(i),
            KeepFree {
                mode: KeepFreeMode::Always,
                mb: 8,
            },
        );
    }
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    add_row(&f, "fit-model", params(None), &[]).await;

    let offer = fit_again(&f, "fit-model", None).await;
    let fit = &offer.fit;
    assert_eq!(fit.mark, FitMark::FitsShorter, "{offer:?}");
    assert!(fit.card.is_some(), "one card, named: {fit:?}");
    assert_eq!(fit.headroom_bytes, 14 * MIB, "{fit:?}");
    assert!(fit.capacity_bytes <= 64 * MIB, "{fit:?}");
    assert!(fit.uses_bytes + fit.headroom_bytes <= 64 * MIB, "{fit:?}");
    let saved = fit_again(&f, "fit-model", Some(offer.changes.clone())).await;
    assert!(saved.saved, "{saved:?}");
    start(&f, "fit-model").await.unwrap();
}

/// Chat row `model_id` of a model shaped as [`super::placing_splits::write_model`]'s,
/// with layers of `layer` MiB of weights, trained on `trained` tokens, with
/// `params`.
async fn add_trained_with(
    f: &Fixture,
    model_id: &str,
    trained: u32,
    layer: u64,
    params: LlamaParams,
) {
    let file = format!("{model_id}.gguf");
    let mut h = lmgw_core::gguf::synth::chat("llama", trained);
    h.u32("llama.block_count", 8)
        .u32("llama.embedding_length", 256)
        .u32("llama.feed_forward_length", 512)
        .u32("llama.attention.head_count", 4)
        .u32("llama.attention.head_count_kv", 2)
        .u32("llama.vocab_size", 1024);
    for i in 0..8 {
        h.tensor_sized(&format!("blk.{i}.attn_q.weight"), layer * MIB / 4);
        h.tensor_sized(&format!("blk.{i}.ffn_up.weight"), 3 * layer * MIB / 4);
    }
    h.tensor_sized("output.weight", 2 * MIB);
    h.tensor_sized("token_embd.weight", 2 * MIB);
    h.write_to(&f._models_dir.path().join(&file));
    f.world().size.insert(model_id.to_string(), 50 * MIB);
    store::insert_local_model(
        &f.state.db,
        &NewLocalModel {
            model_id: model_id.into(),
            gguf_path: file,
            params,
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
    f.state.reload_snapshot().await.unwrap();
}

/// `n` cards of `total` each under a declared budget of `budget_mb`, the
/// fixed headroom of 4 MiB kept on each, no engine cost (the test state's).
async fn under_budget(n: u32, total: u64, budget_mb: u64) -> Fixture {
    let f = fixture(total * u64::from(n), 8 * MIB, 8 * MIB, 4).await;
    f.world().cards = (0..n).map(|i| (format!("card {i}"), total)).collect();
    vram_status(&f.gateway).await;
    let mut s = f.state.snapshot().settings.clone();
    s.vram.budget_mb = budget_mb;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    f
}

/// Under a declared budget over two cards the marks read the cards as
/// admission does (owner, 2026-11-18): a set-context model no card holds
/// whole but two do together (its split's parts, about 530 MiB together,
/// within the 600 MiB budget too) says "2 of 2" and "fits now" over both
/// cards, with each card's total and free memory as admission reads them,
/// and Get it offers the split; started, admission splits it over the
/// same two cards. Before, the marks read no card under a budget and said
/// lmgw does not split under one.
#[tokio::test]
async fn under_a_budget_the_marks_read_the_cards_admission_reads() {
    use lmgw_api_types::model_find::{FreeState, GetLayout, MinCardsState};
    let f = under_budget(2, 320 * MIB, 600).await;
    add_trained_with(
        &f,
        "split-model",
        16384,
        32,
        LlamaParams {
            ctx_size: Some(16384),
            n_gpu_layers: Some(999),
            ..Default::default()
        },
    )
    .await;
    let fit = fit_again(&f, "split-model", None).await.fit;
    let cards = fit.cards.as_ref().expect("the cards, read one by one");
    assert_eq!(cards.of, 2, "{cards:?}");
    assert_eq!(cards.min_cards.state, MinCardsState::Holds, "{cards:?}");
    assert_eq!(cards.min_cards.cards, Some(2), "{cards:?}");
    assert_eq!(cards.free.state, FreeState::Now, "{cards:?}");
    assert_eq!(cards.free.cards.len(), 2, "{cards:?}");
    for p in &cards.free.cards {
        assert_eq!(p.total_bytes, 320 * MIB, "{p:?}");
        assert!(p.free_bytes <= 320 * MIB, "{p:?}");
    }
    assert!(
        cards.choices.iter().any(|c| c.layout == GetLayout::Split),
        "{cards:?}"
    );

    // Admission does what the marks said: a split over both cards.
    start(&f, "split-model").await.unwrap();
    let a = super::llama_fit_review::argv(&f, "split-model");
    assert_eq!(
        super::placing_splits::devices(&a),
        vec![card_id(0).0.clone(), card_id(1).0.clone()],
        "{a:?}"
    );
    assert!(
        super::llama_fit_review::flag(&a, "--tensor-split").is_some(),
        "{a:?}"
    );
}

/// One card under a budget larger than the card: the card holds no more
/// than its own total, in the marks and in the fit, as admission reads it.
#[tokio::test]
async fn one_card_under_a_larger_budget_holds_its_own_total() {
    let f = under_budget(1, 64 * MIB, 200).await;
    add_row(&f, "fit-model", params(None), &[]).await;
    let offer = fit_again(&f, "fit-model", None).await;
    let fit = &offer.fit;
    assert_eq!(fit.capacity_bytes, 64 * MIB, "{fit:?}");
    assert_eq!(fit.mark, FitMark::FitsShorter, "{fit:?}");
    let cards = fit.cards.as_ref().expect("the card, read");
    assert!(
        cards.free.cards.iter().all(|p| p.total_bytes == 64 * MIB),
        "{cards:?}"
    );
    let saved = fit_again(&f, "fit-model", Some(offer.changes.clone())).await;
    assert!(saved.saved, "{saved:?}");
    start(&f, "fit-model").await.unwrap();
}

/// Fit again searches the cards admission may put the row on: a row pinned
/// to the smaller of two cards is fitted to that card, not to the roomiest
/// one (a bug found 2026-11-18: a row pinned to card 1 was offered a
/// context only card 2 holds); saved, it starts there.
#[tokio::test]
async fn fit_again_sizes_a_pinned_row_for_its_pin() {
    let f = fixture(192 * MIB, 8 * MIB, 8 * MIB, 4).await;
    f.world().cards = vec![("card 0".into(), 64 * MIB), ("card 1".into(), 128 * MIB)];
    vram_status(&f.gateway).await;
    add_row(&f, "pin-model", params(None), &[]).await;
    super::placing_pins::pin_chat(&f, "pin-model", &[&card_id(0).0]).await;
    let offer = fit_again(&f, "pin-model", None).await;
    let fit = &offer.fit;
    assert_eq!(fit.mark, FitMark::FitsShorter, "{offer:?}");
    assert!(fit.capacity_bytes <= 64 * MIB, "card 0's room: {fit:?}");
    assert!(fit.uses_bytes + fit.headroom_bytes <= 64 * MIB, "{fit:?}");
    if let Some(c) = &fit.card {
        assert_eq!(c.index, 0, "{fit:?}");
    }
    let saved = fit_again(&f, "pin-model", Some(offer.changes.clone())).await;
    assert!(saved.saved, "{saved:?}");
    start(&f, "pin-model").await.unwrap();
    assert_eq!(
        super::placing_splits::devices(&super::llama_fit_review::argv(&f, "pin-model")),
        vec![card_id(0).0.clone()]
    );
}

/// Get it's one-card choice where only a split holds the full context,
/// under a card's declared keep (a bug found 2026-11-18 on the finder): it
/// shows what the model takes on that card (not "0 B"), its context is the
/// header's own fit, so "room now" agrees with the header, and every
/// figure reads at one decimal.
#[tokio::test]
async fn the_one_card_choice_shows_its_bytes_and_agrees_with_the_header() {
    use lmgw_api_types::model_find::{FreeState, GetLayout};
    let f = fixture(520 * MIB, 8 * MIB, 8 * MIB, 4).await;
    f.world().cards = (0..2).map(|i| (format!("card {i}"), 260 * MIB)).collect();
    vram_status(&f.gateway).await;
    let mut s = f.state.snapshot().settings.clone();
    for i in 0..2 {
        s.vram.keep_free.insert(
            card_id(i),
            KeepFree {
                mode: KeepFreeMode::Always,
                mb: 8,
            },
        );
    }
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    add_trained_with(
        &f,
        "both-model",
        16384,
        24,
        LlamaParams {
            ctx_size: Some(16384),
            n_gpu_layers: Some(999),
            ..Default::default()
        },
    )
    .await;
    let fit = fit_again(&f, "both-model", None).await.fit;
    assert_eq!(fit.mark, FitMark::FitsShorter, "{fit:?}");
    let cards = fit.cards.as_ref().expect("the cards");
    let one = cards
        .choices
        .iter()
        .find(|c| c.layout == GetLayout::OneCard)
        .unwrap_or_else(|| panic!("a one-card choice: {cards:?}"));
    assert_eq!(one.ctx, fit.ctx.unwrap(), "the header's context");
    assert!(
        one.cards.iter().all(|p| p.bytes > 0),
        "what it takes there: {one:?}"
    );
    assert_eq!(
        one.free.state,
        FreeState::Now,
        "as the header says: {one:?}"
    );
    assert!(!one.sentence.contains(" 0 B "), "{}", one.sentence);
}

/// Evicting an idle model frees what the ledger keeps for it too (owner,
/// 2026-11-18): an embedding model that has served nothing has its 327 MiB
/// first-prompt margin kept free beside it on card 0. A chat model that
/// fits card 0 only once that margin is counted as freed evicts it and
/// starts there, instead of being sent to card 1, which cannot hold it,
/// and timing out.
#[tokio::test]
async fn evicting_an_unserved_model_frees_its_first_prompt_margin() {
    let f = fixture(880 * MIB, 150 * MIB, 8 * MIB, 4).await;
    f.world().cards = (0..2).map(|i| (format!("card {i}"), 440 * MIB)).collect();
    f.world().outside_on = vec![(1, 300 * MIB)];
    vram_status(&f.gateway).await;
    // On card 0, its margin kept beside it: it has served nothing.
    start(&f, "embed-model").await.unwrap();
    assert_eq!(
        super::placing_splits::devices(&super::llama_fit_review::argv(&f, "embed-model")),
        vec![card_id(0).0.clone()]
    );
    let r = chat(&f.gateway).await;
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    assert!(
        f.stops().iter().any(|s| s == "embed-model"),
        "the idle model is evicted: {:?}",
        f.stops()
    );
    assert_eq!(
        super::placing_splits::devices(&super::llama_fit_review::argv(&f, "chat-model")),
        vec![card_id(0).0.clone()]
    );
}

/// Under a declared budget over two cards a split's parts together, the
/// engine's cost included, fit within what the budget leaves (owner,
/// 2026-11-18): two 320 MiB cards, a model no card holds whole; under a
/// 600 MiB budget its split (about 560 MiB together) starts over both
/// cards; under 500 MiB it is refused as too large, naming the budget, and
/// the marks say "too large" too, while each card alone would hold its
/// part.
#[tokio::test]
async fn under_a_budget_a_split_s_parts_together_fit_what_it_leaves() {
    use lmgw_api_types::model_find::MinCardsState;
    let row = LlamaParams {
        ctx_size: Some(4096),
        n_gpu_layers: Some(999),
        ..Default::default()
    };
    let fits = under_budget(2, 320 * MIB, 600).await;
    super::placing_splits::add_row_with(&fits, "split-model", 40, row.clone(), &[]).await;
    start(&fits, "split-model").await.unwrap();
    let a = super::llama_fit_review::argv(&fits, "split-model");
    assert_eq!(
        super::placing_splits::devices(&a),
        vec![card_id(0).0.clone(), card_id(1).0.clone()],
        "{a:?}"
    );

    let short = under_budget(2, 320 * MIB, 500).await;
    super::placing_splits::add_row_with(&short, "split-model", 40, row, &[]).await;
    let refused = start(&short, "split-model").await.unwrap_err();
    assert!(refused.contains("GPU memory budget"), "{refused}");
    let fit = fit_again(&short, "split-model", None).await.fit;
    let cards = fit.cards.as_ref().expect("the cards");
    assert_eq!(cards.min_cards.state, MinCardsState::TooLarge, "{cards:?}");
}
