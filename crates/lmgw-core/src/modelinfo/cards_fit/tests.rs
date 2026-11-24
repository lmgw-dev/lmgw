//! The fit over the cards in its cases (device-ledger design WP12): a
//! stated header's plan input on stated cards.

use std::sync::Arc;

use lmgw_api_types::model_find::{FileFit, FitMeasure, FreeState, GetLayout, MinCardsState, Sign};
use serde_json::{json, Map, Value};

use super::super::fit::{self, Card, CardNow, Sizer};
use super::super::plan_files::{PlanFiles, Source};
use crate::gguf::ModelSummary;
use crate::runtime::Class;
use crate::vram::place::marks::{CardsNow, StatedCard};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// 36 layers of 60 MiB (and, for a mixture of experts, 100 MiB of routed
/// experts each), a 300 MiB output layer.
fn summary(experts: bool) -> ModelSummary {
    let mut s = crate::vram::plan::chat::fit::tests::dense();
    if experts {
        for i in 0..36 {
            s.tensor_list
                .tensors
                .push((format!("blk.{i}.ffn_up_exps.weight"), 100 * MIB));
        }
        s.expert_used_count = Some(8);
    }
    s
}

fn files(s: ModelSummary) -> PlanFiles {
    let bytes = s.tensor_list.tensors.iter().map(|(_, b)| b).sum();
    PlanFiles {
        rel: "demo/m.gguf".into(),
        weights_bytes: bytes,
        parameters: None,
        layer_bits: None,
        layer_bits_estimated: false,
        layer_elements: None,
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

fn params() -> Map<String, Value> {
    json!({ "ctx_size": 4096, "n_gpu_layers": 999 })
        .as_object()
        .unwrap()
        .clone()
}

/// `n` empty cards of `gib` GiB each, 64 MiB kept free beside a model.
fn empty(n: u32, gib: u64) -> Vec<(u32, u64, u64)> {
    (0..n).map(|i| (i, gib * GIB, gib * GIB)).collect()
}

fn cards_of(list: &[(u32, u64, u64)]) -> CardsNow {
    let stated: Vec<StatedCard> = list
        .iter()
        .map(|(index, total, free)| StatedCard {
            index: *index,
            total: *total,
            free: *free,
            keep: 64 * MIB,
            holders: &[],
        })
        .collect();
    CardsNow::stated(&stated)
}

/// A card for the one-card fit (its capacity set by the caller) beside
/// `cards`.
fn card_now(cards: CardsNow) -> CardNow {
    CardNow {
        card: Card {
            capacity: 0,
            headroom: 64 * MIB,
            budget: false,
            admission: true,
        },
        hold: false,
        beside_alone: 64 * MIB,
        beside_with: 64 * MIB,
        outside: Some(0),
        residents: Vec::new(),
        left_out: Vec::new(),
        which: None,
        cards: Some(Arc::new(cards)),
    }
}

/// The whole card's fit of `f` on `cards`, the one-card fit measured on a
/// card of `capacity`.
fn fit_on(f: &PlanFiles, sizer: &Sizer, cards: CardsNow, capacity: u64) -> FileFit {
    let mut c = card_now(cards);
    c.card.capacity = capacity;
    fit::fit(f, sizer, Some(&c), FitMeasure::WholeCard)
}

#[test]
fn a_file_one_card_holds_is_one_of_four_and_get_it_does_not_ask() {
    let f = files(summary(false));
    let s = Sizer::chat(&f, &params());
    let w = fit_on(&f, &s, cards_of(&empty(4, 8)), 8 * GIB);
    let c = w.cards.expect("a fit over the cards");
    assert_eq!(c.min_cards.state, MinCardsState::Holds);
    assert_eq!(c.min_cards.label, "1 of 4");
    assert_eq!(c.free.state, FreeState::Now, "{c:?}");
    assert_eq!(c.free.sign, Sign::Ok);
    assert!(c.choices.is_empty());
    assert_eq!(c.ctx, Some(4096));
}

#[test]
fn a_file_only_a_split_holds_asks_with_the_numbers_per_card() {
    let f = files(summary(false));
    let s = Sizer::chat(&f, &params());
    let w = fit_on(&f, &s, cards_of(&empty(4, 2)), 2 * GIB);
    let c = w.cards.expect("a fit over the cards");
    assert_eq!(c.min_cards.label, "2 of 4", "{c:?}");
    assert_eq!(c.min_cards.sign, Sign::Ok);
    assert_eq!(c.free.state, FreeState::Now, "{c:?}");
    assert_eq!(c.free.cards.len(), 2);
    let split = c
        .choices
        .iter()
        .find(|ch| ch.layout == GetLayout::Split)
        .expect("a split to choose");
    assert_eq!(split.ctx, 4096);
    assert_eq!(split.cards.len(), 2);
    assert!(split.sentence.starts_with("Split over cards"), "{split:?}");
    assert!(split.sentence.contains("GiB on card"), "{split:?}");
    assert!(c.asks());
}

#[test]
fn an_engine_that_cannot_split_says_why_and_get_it_does_not_ask() {
    let f = files(summary(false));
    let s = Sizer::chat(&f, &params());
    let cards = cards_of(&empty(4, 2)).without_split("the image has no --tensor-split");
    let w = fit_on(&f, &s, cards, 2 * GIB);
    let c = w.cards.unwrap();
    assert_eq!(c.min_cards.state, MinCardsState::SplitNotHere);
    assert_eq!(c.min_cards.sign, Sign::Warn);
    assert!(
        c.min_cards.sentence.starts_with("Fits only split")
            && c.min_cards.sentence.contains("no --tensor-split"),
        "{c:?}"
    );
    assert!(c.choices.is_empty());
}

#[test]
fn an_aux_model_runs_on_one_card() {
    let f = files(summary(false));
    let s = Sizer::aux(&f, &params());
    let w = fit_on(&f, &s, cards_of(&empty(4, 2)), 2 * GIB);
    let c = w.cards.unwrap();
    assert_eq!(c.min_cards.state, MinCardsState::SplitNotHere, "{c:?}");
    assert!(
        c.min_cards.sentence.contains("this kind runs on one card"),
        "{c:?}"
    );
    assert!(c.choices.is_empty());
}

#[test]
fn larger_than_every_card_together() {
    let f = files(summary(false));
    let s = Sizer::chat(&f, &params());
    let w = fit_on(&f, &s, cards_of(&empty(2, 1)), GIB);
    let c = w.cards.unwrap();
    assert_eq!(c.min_cards.state, MinCardsState::TooLarge);
    assert_eq!(c.min_cards.sign, Sign::No);
    assert!(
        c.min_cards
            .sentence
            .starts_with("Larger than all 2 cards together"),
        "{c:?}"
    );
    // What it needs and what is kept free beside it on each card (64 MiB
    // and a split card's engine's 64 MiB more), against what the cards
    // hold (not the model's bytes alone against them).
    assert!(
        c.min_cards.sentence.contains(&format!(
            "plus {} kept free beside it",
            crate::hf::fmt_bytes(256 * MIB)
        )) && c.min_cards.sentence.contains("the 2 cards hold 2.0 GiB"),
        "{}",
        c.min_cards.sentence
    );
    assert_eq!(c.free.sign, Sign::No);
    assert!(c.free.label.starts_with("needs "), "{c:?}");
    // One decimal throughout, as the sentence and the rest of the card.
    let numbers: Vec<&str> = c
        .free
        .label
        .split(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .filter(|w| w.chars().any(|ch| ch.is_ascii_digit()))
        .collect();
    assert!(
        numbers.iter().filter(|w| w.contains('.')).count() >= 2,
        "{}",
        c.free.label
    );
}

#[test]
fn a_computer_of_one_card_hides_min_cards() {
    let f = files(summary(false));
    let s = Sizer::chat(&f, &params());
    let w = fit_on(&f, &s, cards_of(&empty(1, 8)), 8 * GIB);
    let c = w.cards.unwrap();
    assert_eq!(c.min_cards.state, MinCardsState::OneCard);
    assert!(c.min_cards.label.is_empty());
    assert_eq!(c.free.state, FreeState::Now);
    assert_eq!(c.free.label, "fits now");
}

#[test]
fn a_mixture_of_experts_offers_its_experts_in_ram() {
    let f = files(summary(true));
    let s = Sizer::chat(&f, &params());
    let w = fit_on(&f, &s, cards_of(&empty(2, 4)), 4 * GIB);
    let c = w.cards.unwrap();
    assert_eq!(c.min_cards.label, "2 of 2", "{c:?}");
    let e = c.experts_in_ram.clone().expect("the third signal");
    assert_eq!(e.label, "with experts in RAM: 1 card", "{e:?}");
    assert_eq!(e.ram_bytes, 36 * 100 * MIB);
    let layouts: Vec<GetLayout> = c.choices.iter().map(|ch| ch.layout).collect();
    assert_eq!(layouts, vec![GetLayout::Split, GetLayout::ExpertsInRam]);
    let ram = &c.choices[1];
    assert!(ram.sentence.contains("in RAM"), "{ram:?}");

    // On one card of the same size: the experts in RAM, or nothing.
    let w = fit_on(&f, &s, cards_of(&empty(1, 4)), 4 * GIB);
    let c = w.cards.unwrap();
    let layouts: Vec<GetLayout> = c.choices.iter().map(|ch| ch.layout).collect();
    assert_eq!(layouts, vec![GetLayout::ExpertsInRam], "{c:?}");
}

#[test]
fn free_vram_names_the_models_that_unload() {
    let f = files(summary(false));
    let s = Sizer::chat(&f, &params());
    let cards = CardsNow::stated(&[StatedCard {
        index: 0,
        total: 8 * GIB,
        free: GIB,
        keep: 64 * MIB,
        holders: &[("old-chat", 7 * GIB, true)],
    }]);
    let w = fit_on(&f, &s, cards, 8 * GIB);
    let c = w.cards.unwrap();
    assert_eq!(c.free.state, FreeState::AfterUnloading, "{c:?}");
    assert_eq!(c.free.sign, Sign::Warn);
    assert_eq!(c.free.unload, vec!["chat/old-chat".to_string()]);
    assert_eq!(c.free.label, "after unloading chat/old-chat");
}

#[test]
fn a_budget_says_budget_in_place_of_min_cards() {
    let f = files(summary(false));
    let s = Sizer::chat(&f, &params());
    let mut c = card_now(cards_of(&empty(1, 8)));
    c.cards = None;
    c.card.budget = true;
    c.card.capacity = 8 * GIB;
    let w = fit::fit(&f, &s, Some(&c), FitMeasure::WholeCard);
    let cf = w.cards.unwrap();
    assert_eq!(cf.min_cards.state, MinCardsState::Budget);
    assert_eq!(cf.min_cards.sign, Sign::Ok);
    c.card.capacity = GIB;
    let w = fit::fit(&f, &s, Some(&c), FitMeasure::WholeCard);
    let cf = w.cards.unwrap();
    assert_eq!(cf.min_cards.sign, Sign::No, "{cf:?}");
}

#[test]
fn an_unknown_size_is_a_question_mark_with_the_reason() {
    let c = card_now(cards_of(&empty(2, 8)));
    let w = fit::by_size(None, Some(&c), FitMeasure::Beside, "not read".into());
    let cf = w.cards.unwrap();
    assert_eq!(cf.min_cards.label, "?");
    assert_eq!(cf.free.label, "?");
    assert!(cf.min_cards.sentence.contains("size isn't known"), "{cf:?}");
    // A known size: one card holds it by its size.
    let w = fit::by_size(Some(GIB), Some(&c), FitMeasure::Beside, "not read".into());
    let cf = w.cards.unwrap();
    assert!(cf.by_size);
    assert_eq!(cf.min_cards.label, "1 of 2");
    assert_eq!(cf.free.state, FreeState::Now);
}
