//! `place` over stated ledgers (device-ledger design §12, "Placement").

use super::*;
use crate::vram::plan::chat::by_layer::ByLayer;
use std::sync::Arc;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

fn id(i: u32) -> DeviceId {
    DeviceId::new(format!("GPU-{i}"))
}

/// A card with `total` and `free`, 1 GiB headroom, nothing kept last, 640 MiB of
/// engine, nothing of lmgw's on it.
fn card(i: u32, total: u64, free: u64) -> Card {
    Card {
        id: id(i),
        index: i,
        total,
        free,
        headroom: GIB,
        margin: 0,
        last: 0,
        structural: GIB,
        engine: 640 * MIB,
        holders: Vec::new(),
        pool: None,
    }
}

fn holder(model: &str, bytes: u64, idle: bool, idle_seconds: u64) -> Holder {
    Holder {
        class: Class::Chat,
        model: model.into(),
        generation: idle_seconds,
        bytes,
        idle,
        guest: false,
        idle_seconds,
        elsewhere: Vec::new(),
    }
}

fn with(mut c: Card, h: Holder) -> Card {
    c.holders.push(h);
    c
}

fn whole(bytes: u64) -> Ask {
    Ask {
        bytes,
        adapts: false,
        split: None,
    }
}

/// A chat model of `layers` layers of `layer` MiB each, `last` MiB on the
/// last card (the output layer), `first` on the first and `each` MiB of
/// compute on every card: what [`Ask::split`] carries.
fn by_layer(layers: usize, layer: u64, first: u64, last: u64, each: u64) -> Arc<ByLayer> {
    Arc::new(ByLayer {
        first_gpu: 0,
        layers: vec![layer * MIB; layers],
        last: last * MIB,
        first: first * MIB,
        each: each * MIB,
        copy: 0,
        weights: Vec::new(),
        states: Vec::new(),
        drafter: false,
        pipeline: true,
        projector: false,
    })
}

/// `cost` asked whole, splittable.
fn splittable(cost: Arc<ByLayer>) -> Ask {
    Ask {
        bytes: cost.on_cards(&[cost.layers.len() as u64]).unwrap()[0],
        adapts: false,
        split: Some(cost),
    }
}

/// The split `p` answered, and its victims' names.
fn split_of(p: &Placed) -> (&Split, Vec<&str>) {
    match p {
        Placed::Split { split, victims } => (split, names(victims)),
        other => panic!("not a split: {other:?}"),
    }
}

fn names(hs: &[Holder]) -> Vec<&str> {
    hs.iter().map(|h| h.model.as_str()).collect()
}

/// A four-card layout (test figures): three RTX 6000 Ada of 49140 MiB at 0, 1
/// and 3, an RTX PRO 6000 Blackwell of 97887 MiB at 2, other programs
/// holding most of them; free 7641, 12220, 22690 and 16143 MiB.
fn measured() -> Vec<Card> {
    vec![
        card(0, 49140 * MIB, 7641 * MIB),
        card(1, 49140 * MIB, 12220 * MIB),
        card(2, 97887 * MIB, 22690 * MIB),
        card(3, 49140 * MIB, 16143 * MIB),
    ]
}

/// A 4 GiB model goes on card 0, the card it leaves least free on, and
/// not over four cards.
#[test]
fn the_measured_machine_puts_a_small_model_on_the_tightest_card() {
    assert_eq!(
        place(&whole(4 * GIB), &measured(), Mode::Owner),
        Placed::Now(id(0))
    );
    // Too large for card 0 with its headroom and engine: card 1.
    assert_eq!(
        place(&whole(7 * GIB), &measured(), Mode::Owner),
        Placed::Now(id(1))
    );
}

/// A 24 GiB chat model with a set context: no card holds it, so it goes
/// over cards 2 and 3, the fewest that hold it, nothing of lmgw's being in
/// the way (§12): card 2, the roomiest, filled to its room, the rest and
/// the output layer on card 3. The same model with no set context goes on
/// card 2 alone, to be fitted to it (§5.3).
#[test]
fn the_measured_machine_splits_a_large_set_context_and_fits_an_adapting_one() {
    // 36 layers of 650 MiB, 900 MiB of output layer, 100 MiB of compute.
    let large = splittable(by_layer(36, 650, 0, 900, 100));
    assert!(large.bytes > 23 * GIB);
    let p = place(&large, &measured(), Mode::Owner);
    let (split, victims) = split_of(&p);
    assert!(victims.is_empty());
    assert_eq!(split.cards, vec![id(2), id(3)]);
    // Card 2 has 22690 − 640 MiB less its 1 GiB headroom of room, which
    // a split keeps while its other card has room (WP13), and less the
    // 64 MiB a split card's engine holds more: 31 layers and its compute,
    // a split's first card's (222 MiB for the one-card 100: 1.58 times it
    // and 64 MiB, `by_layer`); card 3 every other card's (174: 1.58 times
    // it and 16).
    assert_eq!(split.counts, vec![31, 5]);
    assert_eq!(split.entries(), vec![31, 6], "the output layer is card 3's");
    assert_eq!(
        split.bytes,
        vec![(31 * 650 + 222) * MIB, (5 * 650 + 174 + 900) * MIB]
    );
    assert_eq!(split.engine, vec![(640 + 64) * MIB, (640 + 64) * MIB]);
    // No context set: no split to ask for (ruling 15).
    let adapting = Ask {
        adapts: true,
        split: None,
        ..large.clone()
    };
    assert_eq!(
        place(&adapting, &measured(), Mode::Owner),
        Placed::Roomiest(id(2))
    );
    // A set context whose layers --fit could move: split before it is
    // fitted smaller on one card (D6).
    let layers_adapt = Ask {
        adapts: true,
        ..large
    };
    assert_eq!(
        split_of(&place(&layers_adapt, &measured(), Mode::Owner))
            .0
            .cards,
        vec![id(2), id(3)]
    );
}

/// The fewest cards (§5.4): where the two roomiest cards do not hold it,
/// the third roomiest joins them; the cards are in the bus's order, the
/// first card holds the first card's parts (a drafter, a projector).
#[test]
fn three_cards_where_two_do_not_hold_it() {
    // 60 layers of 600 MiB: 36000 MiB, more than cards 2 and 3 have.
    let mut cost = (*by_layer(60, 600, 1000, 900, 100)).clone();
    cost.drafter = true;
    let p = place(&splittable(Arc::new(cost)), &measured(), Mode::Owner);
    let (split, _) = split_of(&p);
    assert_eq!(split.cards, vec![id(1), id(2), id(3)]);
    assert_eq!(split.counts.iter().sum::<u64>(), 60);
    assert!(split.drafter);
    // Card 2 and card 3, the roomiest, are filled to their room beside
    // their 1 GiB headroom (34 and 22 layers; WP13: kept while a third card
    // has room), and card 1 takes the rest, beside the drafter's 1000 MiB,
    // the first card's on the bus.
    assert_eq!(split.counts, vec![4, 34, 22]);
    assert_eq!(split.entries(), vec![4, 34, 23]);
    assert_eq!(split.bytes[0], (4 * 600 + 222 + 1000) * MIB);
    for (j, c) in [1usize, 2, 3].iter().enumerate() {
        let room = measured()[*c].free - 640 * MIB;
        assert!(split.bytes[j] <= room, "card {c}: {split:?}");
    }
}

/// Evict before splitting (ruling 5): a model one card holds once an idle
/// model there is stopped is not split; one no card holds even then is
/// split once idle models of its cards are stopped (step 4), the ones each
/// card needs gone and no more.
#[test]
fn one_card_after_evicting_before_a_split_and_a_split_after_evicting() {
    let cost = by_layer(20, 500, 0, 500, 100);
    // 10600 MiB in all.
    let a = with(card(0, 16 * GIB, 4 * GIB), holder("a", 8 * GIB, true, 50));
    let b = card(1, 16 * GIB, 8 * GIB);
    match place(
        &splittable(cost.clone()),
        &[a.clone(), b.clone()],
        Mode::Owner,
    ) {
        Placed::Evict { card, victims } => {
            assert_eq!(card, id(0));
            assert_eq!(names(&victims), ["a"]);
        }
        other => panic!("{other:?}"),
    }
    // Neither card holds it alone even empty of lmgw's: split once card 0's
    // idle models are stopped for its part, least recently used first;
    // card 1's idle "c" is not needed for its part and stays.
    let big = by_layer(40, 500, 0, 500, 100);
    let a = with(
        with(card(0, 16 * GIB, 4 * GIB), holder("a", 8 * GIB, true, 50)),
        holder("b", 2 * GIB, true, 10),
    );
    let b = with(card(1, 16 * GIB, 12 * GIB), holder("c", GIB, true, 90));
    let p = place(&splittable(big), &[a, b], Mode::Owner);
    let (split, victims) = split_of(&p);
    assert_eq!(split.cards, vec![id(0), id(1)]);
    assert_eq!(
        split.counts,
        vec![24, 16],
        "card 0 filled to its room beside its headroom, kept (WP13), and a \
         split's first card's compute buffer"
    );
    assert_eq!(victims, ["a", "b"], "{split:?}");
}

/// A busy model in the way (ruling 14): a model that fits over several
/// cards now does not wait for it to leave one.
#[test]
fn a_split_now_before_waiting_for_a_busy_model() {
    let cost = by_layer(20, 500, 0, 500, 100);
    let busy = with(
        card(0, 16 * GIB, 7 * GIB),
        holder("busy", 9 * GIB, false, 1),
    );
    let other = card(1, 16 * GIB, 7 * GIB);
    let p = place(
        &splittable(cost.clone()),
        &[busy.clone(), other],
        Mode::Owner,
    );
    let (split, victims) = split_of(&p);
    assert!(victims.is_empty());
    assert_eq!(split.cards, vec![id(0), id(1)]);
    // Without a split it waits for the busy model on card 0.
    let one = Ask {
        split: None,
        ..splittable(cost)
    };
    match place(&one, &[busy, card(1, 16 * GIB, 7 * GIB)], Mode::Owner) {
        Placed::Wait { card, .. } => assert_eq!(card, id(0)),
        other => panic!("{other:?}"),
    }
}

/// The pinned-row bug (§12): a row pinned to a card with 7.5 GiB free is
/// placed on that card and nowhere else, whatever the machine has free in
/// all: its candidates are its pin.
#[test]
fn a_pinned_row_is_placed_on_its_card_alone() {
    let pin = vec![card(0, 49140 * MIB, 7641 * MIB)];
    let adapting = Ask {
        bytes: 24 * GIB,
        adapts: true,
        split: None,
    };
    assert_eq!(place(&adapting, &pin, Mode::Owner), Placed::Roomiest(id(0)));
}

/// Equal room left: the lower index.
#[test]
fn a_tie_goes_to_the_lower_index() {
    let cards = vec![card(1, 16 * GIB, 8 * GIB), card(0, 16 * GIB, 8 * GIB)];
    assert_eq!(
        place(&whole(4 * GIB), &cards, Mode::Owner),
        Placed::Now(id(0))
    );
}

/// What a start keeps free on a card is its headroom while something of
/// lmgw's is there, what it keeps last (its floor) once nothing is (§5.6,
/// WP13: the learned room and the margin given up) — and a card
/// that holds it with its headroom is taken first.
#[test]
fn an_empty_card_keeps_its_floor_alone() {
    let mut empty = card(0, 16 * GIB, 5 * GIB);
    empty.margin = 100 * MIB;
    empty.last = 100 * MIB;
    let busy = with(
        card(1, 16 * GIB, 5 * GIB),
        holder("other", 2 * GIB, false, 1),
    );
    // 4 GiB + 640 MiB engine + 100 MiB floor fits the empty card; with
    // the headroom it fits neither.
    assert_eq!(
        place(&whole(4 * GIB), &[busy.clone(), empty.clone()], Mode::Owner),
        Placed::Now(id(0))
    );
    let roomy = card(2, 16 * GIB, 6 * GIB);
    assert_eq!(
        place(&whole(4 * GIB), &[busy, empty, roomy], Mode::Owner),
        Placed::Now(id(2)),
        "a card that holds it with its headroom first"
    );
}

/// Step 2: the card where the fewest idle victims make room, then the
/// victims used longest ago; the victims are that card's alone, least
/// recently used first.
#[test]
fn evicting_on_one_card_with_the_fewest_victims() {
    let two = with(
        with(card(0, 16 * GIB, GIB), holder("a", 3 * GIB, true, 50)),
        holder("b", 3 * GIB, true, 10),
    );
    let one = with(card(1, 16 * GIB, 2 * GIB), holder("c", 5 * GIB, true, 5));
    let p = place(&whole(5 * GIB), &[two.clone(), one.clone()], Mode::Owner);
    assert_eq!(
        p,
        Placed::Evict {
            card: id(1),
            victims: vec![holder("c", 5 * GIB, true, 5)]
        }
    );
    // An idle model on another card is left alone: only card 0's go.
    let p = place(&whole(5 * GIB), std::slice::from_ref(&two), Mode::Owner);
    match p {
        Placed::Evict { card, victims } => {
            assert_eq!(card, id(0));
            assert_eq!(names(&victims), ["a", "b"], "least recently used first");
        }
        other => panic!("{other:?}"),
    }
    // Equal counts: the card whose victims were used longest ago.
    let older = with(card(2, 16 * GIB, 2 * GIB), holder("d", 5 * GIB, true, 90));
    match place(&whole(5 * GIB), &[one, older], Mode::Owner) {
        Placed::Evict { card, .. } => assert_eq!(card, id(2)),
        other => panic!("{other:?}"),
    }
}

/// `h`, which could be started again on card `to` now with `spare` left
/// there, taking `takes` (§6.2).
fn movable(mut h: Holder, to: u32, spare: u64, takes: u64) -> Holder {
    h.elsewhere.push(Spare {
        id: id(to),
        index: to,
        spare,
        takes,
    });
    h
}

/// Step 2's second tie (§5.2, §6.2): of two cards that each need one
/// victim, the one whose victim could be started again on another card now
/// — though it was used more recently — and the others' order where
/// neither or both could. Victims that could each move but not together
/// onto the one card that has room are no move; and a victim is never
/// counted as moving onto the card it makes room on.
#[test]
fn evicting_prefers_victims_that_can_move() {
    let stays = with(card(0, 16 * GIB, 2 * GIB), holder("old", 5 * GIB, true, 90));
    let moves = with(
        card(1, 16 * GIB, 2 * GIB),
        movable(holder("young", 5 * GIB, true, 5), 2, GIB, 5 * GIB),
    );
    match place(
        &whole(5 * GIB),
        &[stays.clone(), moves.clone()],
        Mode::Owner,
    ) {
        Placed::Evict { card, victims } => {
            assert_eq!(card, id(1), "the victim that can move, though younger");
            assert_eq!(names(&victims), ["young"]);
        }
        other => panic!("{other:?}"),
    }
    // Neither can move: the victim used longest ago, as before.
    let neither = with(
        card(1, 16 * GIB, 2 * GIB),
        holder("young", 5 * GIB, true, 5),
    );
    match place(&whole(5 * GIB), &[stays.clone(), neither], Mode::Owner) {
        Placed::Evict { card, .. } => assert_eq!(card, id(0)),
        other => panic!("{other:?}"),
    }
    // Fewer victims still come first: one that stays beats two that move.
    let two = with(
        with(
            card(2, 16 * GIB, GIB),
            movable(holder("x", 3 * GIB, true, 1), 3, 8 * GIB, 3 * GIB),
        ),
        movable(holder("y", 3 * GIB, true, 2), 3, 8 * GIB, 3 * GIB),
    );
    match place(&whole(5 * GIB), &[two, stays.clone()], Mode::Owner) {
        Placed::Evict { card, .. } => assert_eq!(card, id(0)),
        other => panic!("{other:?}"),
    }
    // Two victims that each fit card 3 but not both: they do not all move,
    // and the older victims win.
    let both = with(
        with(
            card(4, 16 * GIB, GIB),
            movable(holder("p", 3 * GIB, true, 1), 3, 2 * GIB, 3 * GIB),
        ),
        movable(holder("q", 3 * GIB, true, 2), 3, 2 * GIB, 3 * GIB),
    );
    let older_two = with(
        with(card(5, 16 * GIB, GIB), holder("r", 3 * GIB, true, 80)),
        holder("s", 3 * GIB, true, 70),
    );
    match place(&whole(5 * GIB), &[both, older_two], Mode::Owner) {
        Placed::Evict { card, .. } => assert_eq!(card, id(5)),
        other => panic!("{other:?}"),
    }
    // A victim whose only other card is the one it makes room on does not
    // move.
    let onto_itself = with(
        card(1, 16 * GIB, 2 * GIB),
        movable(holder("young", 5 * GIB, true, 5), 1, 8 * GIB, 5 * GIB),
    );
    match place(&whole(5 * GIB), &[stays, onto_itself], Mode::Owner) {
        Placed::Evict { card, .. } => assert_eq!(card, id(0)),
        other => panic!("{other:?}"),
    }
}

/// A guest stops only guests, and never waits: with only an owner's idle
/// model in the way it goes to the roomiest card, where it is told no.
#[test]
fn a_guest_stops_only_guests() {
    let mut guest = holder("guest", 5 * GIB, true, 5);
    guest.guest = true;
    let owners = with(card(0, 16 * GIB, 2 * GIB), holder("own", 5 * GIB, true, 50));
    let guests = with(card(1, 16 * GIB, 2 * GIB), guest.clone());
    assert_eq!(
        place(&whole(5 * GIB), &[owners.clone(), guests], Mode::Guest),
        Placed::Evict {
            card: id(1),
            victims: vec![guest]
        }
    );
    assert_eq!(
        place(&whole(5 * GIB), &[owners], Mode::Guest),
        Placed::Roomiest(id(0))
    );
}

/// Step 5: an owner's start waits on the card where a busy model leaving
/// makes room, and not on one it could never fit.
#[test]
fn waiting_for_a_busy_model_on_one_card() {
    let busy = with(card(0, 16 * GIB, GIB), holder("busy", 8 * GIB, false, 1));
    let small = card(1, 4 * GIB, 3 * GIB);
    assert_eq!(
        place(&whole(6 * GIB), &[small.clone(), busy.clone()], Mode::Owner),
        Placed::Wait {
            card: id(0),
            on: vec![holder("busy", 8 * GIB, false, 1)]
        }
    );
    // Nothing of lmgw's to wait for anywhere: the roomiest card.
    assert_eq!(
        place(
            &whole(6 * GIB),
            &[small, card(2, 8 * GIB, 5 * GIB)],
            Mode::Owner
        ),
        Placed::Roomiest(id(2))
    );
}

/// No candidate: nowhere.
#[test]
fn no_candidate_is_nowhere() {
    assert_eq!(place(&whole(GIB), &[], Mode::Owner), Placed::Nowhere);
}

/// `c` keeping `whole` free while it can make room another way, `floor`
/// once its learned room is given up (WP13): no margin to give up.
fn keeps(c: Card, whole: u64, floor: u64) -> Card {
    keeps3(c, whole, floor, floor)
}

/// `c` keeping `whole` free while it can make room another way, `margin`
/// once its learned room is given up and `floor` once the margin is too.
fn keeps3(mut c: Card, whole: u64, margin: u64, floor: u64) -> Card {
    c.headroom = whole;
    c.margin = margin;
    c.last = floor;
    c
}

/// WP13: the learned room is given up last. A single model evicts an idle
/// model on another card before it takes an empty card's learned room, and
/// takes it where nothing can be evicted.
#[test]
fn a_model_evicts_elsewhere_before_it_takes_the_learned_room() {
    // Card 0 holds it beside its floor (512 MiB), not beside its whole
    // headroom (3 GiB); card 1 holds it whole once its idle "a" is stopped.
    let empty = keeps(card(0, 16 * GIB, 6 * GIB), 3 * GIB, 512 * MIB);
    let idle = keeps(
        with(card(1, 16 * GIB, 4 * GIB), holder("a", 6 * GIB, true, 50)),
        3 * GIB,
        512 * MIB,
    );
    match place(&whole(4 * GIB), &[empty.clone(), idle], Mode::Owner) {
        Placed::Evict { card, victims } => {
            assert_eq!(card, id(1));
            assert_eq!(names(&victims), ["a"]);
        }
        other => panic!("{other:?}"),
    }
    // "a" busy: nothing to evict, so card 0's learned room is taken.
    let busy = keeps(
        with(card(1, 16 * GIB, 4 * GIB), holder("a", 6 * GIB, false, 50)),
        3 * GIB,
        512 * MIB,
    );
    assert_eq!(
        place(&whole(4 * GIB), &[empty, busy], Mode::Owner),
        Placed::Now(id(0))
    );
}

/// WP13: a split puts its layers on another card before it takes a card's
/// learned room, and takes it where no more cards are there; a model one
/// card holds by giving up the learned room is not split instead (D6).
#[test]
fn a_split_takes_another_card_before_the_learned_room() {
    // 10.6 GiB of layers: two cards hold it beside their floors, three
    // beside their whole headroom.
    let cost = by_layer(20, 500, 0, 500, 100);
    let three: Vec<Card> = (0..3)
        .map(|i| keeps(card(i, 16 * GIB, 8 * GIB), 3 * GIB, 0))
        .collect();
    let p = place(&splittable(cost.clone()), &three, Mode::Owner);
    let (split, _) = split_of(&p);
    assert_eq!(
        split.cards,
        vec![id(0), id(1), id(2)],
        "the learned room kept"
    );
    let p = place(&splittable(cost.clone()), &three[..2], Mode::Owner);
    let (split, _) = split_of(&p);
    assert_eq!(
        split.cards,
        vec![id(0), id(1)],
        "taken where nothing else holds it"
    );
    // One card holds it beside its floor: on that card, not split over two
    // cards that would keep their learned room.
    let roomy = keeps(card(0, 16 * GIB, 13 * GIB), 3 * GIB, 0);
    assert_eq!(
        place(
            &splittable(cost),
            &[roomy, three[1].clone(), three[2].clone()],
            Mode::Owner
        ),
        Placed::Now(id(0))
    );
}

/// The margin given up last (WP13, owner 2026-11-17): a card keeps its
/// floor and the start's margin while the model can go on another card,
/// evict an idle model or take an empty card's learned room instead, and
/// its floor alone ([`Card::last`]) only where nothing else holds it.
#[test]
fn the_margin_is_kept_while_another_way_exists() {
    let m = 327 * MIB;
    // Fixed mode (no learned room): card 0 holds 4 GiB beside its 1 GiB
    // floor but not beside the floor and the margin.
    let fixed = |c: Card| keeps3(c, GIB + m, GIB + m, GIB);
    let tight = fixed(card(0, 16 * GIB, 4 * GIB + 640 * MIB + GIB + m / 2));
    // Card 1 holds it beside both once its idle "a" is stopped.
    let idle = fixed(with(
        card(1, 16 * GIB, 2 * GIB),
        holder("a", 4 * GIB, true, 50),
    ));
    match place(&whole(4 * GIB), &[tight.clone(), idle], Mode::Owner) {
        Placed::Evict { card, victims } => {
            assert_eq!(card, id(1));
            assert_eq!(names(&victims), ["a"]);
        }
        other => panic!("{other:?}"),
    }
    // Card 2 holds it beside both now: there.
    let roomy = fixed(card(2, 16 * GIB, 8 * GIB));
    assert_eq!(
        place(&whole(4 * GIB), &[tight.clone(), roomy], Mode::Owner),
        Placed::Now(id(2))
    );
    // Card 3 keeps 3 GiB of learned room and holds it beside its floor and
    // margin without that room: its room is given up before card 0's
    // margin, though card 0 is the tighter fit (D7).
    let learned = keeps3(
        card(3, 16 * GIB, 4 * GIB + 640 * MIB + GIB + m + 200 * MIB),
        GIB + 3 * GIB + m,
        GIB + m,
        GIB,
    );
    assert_eq!(
        place(&whole(4 * GIB), &[tight.clone(), learned], Mode::Owner),
        Placed::Now(id(3))
    );
    // Nothing else: card 0, its margin given up.
    assert_eq!(
        place(&whole(4 * GIB), std::slice::from_ref(&tight), Mode::Owner),
        Placed::Now(id(0))
    );
    // Its floor is never given up: one byte less, and it waits for room.
    let short = fixed(card(0, 16 * GIB, 4 * GIB + 640 * MIB + GIB - 1));
    assert_eq!(
        place(&whole(4 * GIB), &[short], Mode::Owner),
        Placed::Roomiest(id(0))
    );
}

/// A model moved to make room keeps its margin on its new card (WP13,
/// judgement call 4): a card that holds it beside its floor alone, not
/// beside its floor and margin, is no move, though a start there would give
/// the margin up; with room for the margin it moves there.
#[test]
fn a_move_keeps_its_margin() {
    let m = 327 * MIB;
    let fixed = |c: Card| keeps3(c, GIB + m, GIB + m, GIB);
    let tight = fixed(card(1, 16 * GIB, 4 * GIB + 640 * MIB + GIB + m / 2));
    assert_eq!(
        place(&whole(4 * GIB), std::slice::from_ref(&tight), Mode::Owner),
        Placed::Now(id(1)),
        "a start gives the margin up there"
    );
    assert_eq!(
        super::repack::move_to(&whole(4 * GIB), vec![tight]),
        None,
        "a move does not"
    );
    let room = fixed(card(1, 16 * GIB, 4 * GIB + 640 * MIB + GIB + m));
    assert_eq!(
        super::repack::move_to(&whole(4 * GIB), vec![room]),
        Some(id(1))
    );
}

/// A start that never evicts or waits (`Mode::Still`): an idle model on
/// card 0 is no room, so the empty card 1 takes it with its learned room
/// given up — where an owner's start evicts on card 0 instead — and with
/// its margin given up too where nothing else holds it; a card it fits on
/// only after the idle model left is the roomiest, not an eviction.
#[test]
fn a_start_that_never_evicts_takes_an_empty_card_before_an_idle_model() {
    let m = 327 * MIB;
    let idle = keeps3(
        with(card(0, 16 * GIB, 4 * GIB), holder("a", 6 * GIB, true, 50)),
        GIB + m,
        GIB + m,
        GIB,
    );
    // Card 1 holds it beside its floor and margin, not beside its 3 GiB of
    // learned room as well.
    let learned = keeps3(
        card(1, 16 * GIB, 4 * GIB + 640 * MIB + GIB + m + 200 * MIB),
        GIB + 3 * GIB + m,
        GIB + m,
        GIB,
    );
    let both = [idle.clone(), learned.clone()];
    assert!(matches!(
        place(&whole(4 * GIB), &both, Mode::Owner),
        Placed::Evict { .. }
    ));
    assert_eq!(
        place(&whole(4 * GIB), &both, Mode::Still),
        Placed::Now(id(1))
    );
    // Card 1 holds it beside its floor alone: there, the margin given up.
    let tight = keeps3(
        card(1, 16 * GIB, 4 * GIB + 640 * MIB + GIB + m / 2),
        GIB + 3 * GIB + m,
        GIB + m,
        GIB,
    );
    assert_eq!(
        place(&whole(4 * GIB), &[idle.clone(), tight], Mode::Still),
        Placed::Now(id(1))
    );
    // Nothing holds it now: the roomiest by free memory, never a victim.
    let short = card(1, 16 * GIB, 3 * GIB);
    assert_eq!(
        place(&whole(4 * GIB), &[idle, short], Mode::Still),
        Placed::Roomiest(id(0))
    );
}

/// A set-context model larger than either card, while a busy model holds
/// card 0 (owner, 2026-11-18): no split fits now or after evicting, so
/// `place` answers the roomiest card, which can never hold it; the split it
/// waits for is over both cards, once the busy model has left.
#[test]
fn a_model_only_a_split_holds_waits_for_the_split_not_the_roomiest_card() {
    let busy = with(
        card(0, 10 * GIB, 3 * GIB),
        holder("busy", 6 * GIB, false, 5),
    );
    let cards = [busy, card(1, 10 * GIB, 9 * GIB)];
    let ask = splittable(by_layer(12, 1000, 0, 200, 100));
    assert!(matches!(
        place(&ask, &cards, Mode::Owner),
        Placed::Roomiest(_)
    ));
    assert_eq!(split_once_left(&ask, &cards), Some(vec![id(0), id(1)]));
    // One that does not split has nothing to wait for there.
    assert_eq!(split_once_left(&whole(12 * GIB), &cards), None);
    // Nor one too large for every card even with lmgw's models gone.
    let huge = splittable(by_layer(30, 1000, 0, 200, 100));
    assert_eq!(split_once_left(&huge, &cards), None);
}

/// A split card's engine holds more than one card's (measured on a test
/// machine, 2026-11-18: up to 61 MiB): each part of a split is charged its card's
/// engine cost and [`split::SPLIT_ENGINE_EXTRA`], 64 MiB; a start on one
/// card is charged the card's engine cost alone.
#[test]
fn a_split_card_s_engine_holds_64_mib_more() {
    assert_eq!(split::SPLIT_ENGINE_EXTRA, 64 * MIB);
    let large = splittable(by_layer(36, 650, 0, 900, 100));
    let p = place(&large, &measured(), Mode::Owner);
    let (sp, _) = split_of(&p);
    assert_eq!(sp.engine, vec![(640 + 64) * MIB; 2]);
    for (j, (_, charged)) in sp.charged().iter().enumerate() {
        assert_eq!(*charged, sp.bytes[j] + (640 + 64) * MIB);
    }
    // One card: the card's engine cost, as before.
    let c = card(0, 16 * GIB, 16 * GIB);
    assert_eq!(c.needs(&whole(4 * GIB), GIB), 4 * GIB + 640 * MIB + GIB);
}
