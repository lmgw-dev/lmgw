//! Min. cards and Free VRAM over stated cards (device-ledger design WP12).

use super::*;
use crate::vram::nvml::DeviceId;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// A card of `total` GiB with `free` GiB free, 1 GiB kept free beside a
/// model, nothing kept last and no engine cost, nothing of lmgw's on it.
fn card(i: u32, total: u64, free: u64) -> Card {
    Card {
        id: DeviceId::new(format!("GPU-{i}")),
        index: i,
        total: total * GIB,
        free: free * GIB,
        headroom: GIB,
        margin: 0,
        last: 0,
        structural: GIB,
        engine: 0,
        holders: Vec::new(),
        pool: None,
    }
}

fn holder(model: &str, gib: u64, idle: bool) -> Holder {
    Holder {
        class: Class::Chat,
        model: model.into(),
        generation: gib,
        bytes: gib * GIB,
        idle,
        guest: false,
        idle_seconds: 10,
        elsewhere: Vec::new(),
    }
}

/// 40 layers of 1 GiB each, nothing else: a 40 GiB chat model.
fn forty() -> Arc<ByLayer> {
    Arc::new(ByLayer {
        first_gpu: 0,
        layers: vec![GIB; 40],
        last: 0,
        first: 0,
        each: 0,
        copy: 0,
        weights: Vec::new(),
        states: Vec::new(),
        drafter: false,
        pipeline: true,
        projector: false,
    })
}

fn indices(parts: &[CardPart]) -> Vec<u32> {
    parts.iter().map(|p| p.index).collect()
}

#[test]
fn a_model_one_card_holds_counts_one_the_largest_card() {
    let now = CardsNow::for_tests(vec![card(0, 24, 24), card(1, 48, 10)]);
    match now.min_cards(20 * GIB, Some(&forty())) {
        Min::One(p) => assert_eq!(p.index, 1),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_model_no_card_holds_counts_the_fewest_cards_its_split_needs() {
    let now = CardsNow::for_tests(vec![
        card(0, 24, 24),
        card(1, 24, 24),
        card(2, 24, 24),
        card(3, 24, 24),
    ]);
    match now.min_cards(40 * GIB, Some(&forty())) {
        Min::Split(parts) => {
            assert_eq!(parts.len(), 2);
            let layers: u64 = parts.iter().filter_map(|p| p.layers).sum();
            assert_eq!(layers, 40);
            assert!(parts.iter().all(|p| p.bytes + GIB <= 24 * GIB));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn larger_than_every_card_together_is_too_large() {
    let now = CardsNow::for_tests(vec![card(0, 16, 16), card(1, 16, 16)]);
    match now.min_cards(40 * GIB, Some(&forty())) {
        Min::TooLarge { largest, all, .. } => {
            assert_eq!(largest, 16 * GIB);
            assert_eq!(all, 32 * GIB);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn without_layers_to_split_by_the_count_is_by_size() {
    let now = CardsNow::for_tests(vec![card(0, 24, 24), card(1, 24, 24), card(2, 24, 24)]);
    assert_eq!(now.min_cards(40 * GIB, None), Min::About(2));
    assert!(matches!(
        now.min_cards(80 * GIB, None),
        Min::TooLarge { .. }
    ));
}

#[test]
fn free_vram_says_now_on_the_card_a_start_would_take() {
    let now = CardsNow::for_tests(vec![card(0, 24, 24), card(1, 24, 12)]);
    match now.free(10 * GIB, Some(forty()), 1) {
        // The tightest card that holds it (D7).
        Free::Now(parts) => assert_eq!(indices(&parts), vec![1]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn free_vram_names_the_idle_models_that_unload_first() {
    let mut c = card(0, 24, 4);
    c.holders.push(holder("chat-a", 18, true));
    let now = CardsNow::for_tests(vec![c]);
    match now.free(10 * GIB, None, 1) {
        Free::Unload(parts, names) => {
            assert_eq!(indices(&parts), vec![0]);
            assert_eq!(names, vec!["chat/chat-a".to_string()]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_busy_model_is_waited_for() {
    let mut c = card(0, 24, 4);
    c.holders.push(holder("chat-busy", 18, false));
    let now = CardsNow::for_tests(vec![c]);
    match now.free(10 * GIB, None, 1) {
        Free::Wait(_, names) => assert_eq!(names, vec!["chat/chat-busy".to_string()]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_split_that_fits_now_names_its_cards() {
    let now = CardsNow::for_tests(vec![
        card(0, 24, 4),
        card(1, 24, 22),
        card(2, 24, 22),
        card(3, 24, 2),
    ]);
    match now.free(40 * GIB, Some(forty()), 2) {
        Free::Now(parts) => assert_eq!(indices(&parts), vec![1, 2]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn short_says_what_it_needs_and_what_the_roomiest_card_has() {
    let mut c = card(1, 24, 6);
    c.holders.push(holder("other", 4, false));
    let now = CardsNow::for_tests(vec![card(0, 24, 3), c]);
    match now.free(20 * GIB, None, 1) {
        Free::Short {
            needs,
            has,
            ours,
            cards,
        } => {
            assert_eq!(needs, 20 * GIB);
            assert_eq!(has, 10 * GIB);
            assert_eq!(ours, 4 * GIB);
            assert_eq!(indices(&cards), vec![1]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_split_short_now_is_measured_on_as_many_cards_as_it_needs() {
    let now = CardsNow::for_tests(vec![card(0, 24, 10), card(1, 24, 12), card(2, 24, 3)]);
    match now.free(40 * GIB, Some(forty()), 2) {
        Free::Short {
            needs, has, cards, ..
        } => {
            // Its layers and a split's compute buffers on two cards.
            assert_eq!(
                needs,
                40 * GIB + forty().computes(2) + 2 * super::super::split::SPLIT_ENGINE_EXTRA
            );
            assert_eq!(has, 22 * GIB);
            assert_eq!(indices(&cards), vec![0, 1]);
        }
        other => panic!("{other:?}"),
    }
}
