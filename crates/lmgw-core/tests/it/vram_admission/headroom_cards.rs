//! Headroom per card (device-ledger design §9.1, §12, WP3 and WP13): the
//! sampler reads each card's outside use, the floor and the learned room of
//! one card are that card's alone and apply from its first reading, a card
//! nobody else uses keeps nothing, the decisions read the sum, and
//! forgetting one card leaves the others' readings, in memory and stored.

use super::*;
use lmgw_core::store::OutsideHour;

const MIB: u64 = 1024 * 1024;

/// Three cards, another program on card 1.
fn three_cards(f: &Fixture, outside: u64) {
    f.attribute(0);
    let mut w = f.world();
    w.cards = vec![
        ("RTX 6000 Ada".into(), 48 * GIB),
        ("RTX PRO 6000 Blackwell".into(), 96 * GIB),
        ("RTX 6000 Ada".into(), 48 * GIB),
    ];
    w.outside_on = vec![(1, outside)];
}

/// A day of flat readings at `bytes` on card `index`: learned, at no growth.
fn learned_flat(f: &Fixture, index: u32, bytes: u64, rise: u64) {
    let now = super::llama_fit_review::now_hour();
    f.state.vram.seed_outside_hours(
        &card_id(index),
        (1..25)
            .map(|h| OutsideHour {
                hour: now - h,
                low_bytes: bytes,
                high_bytes: bytes + if h == 3 { rise } else { 0 },
                rise_bytes: if h == 3 { rise } else { 0 },
            })
            .collect(),
    );
}

#[tokio::test]
async fn each_card_learns_its_own_outside_use_and_forgets_it_alone() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    three_cards(&f, 20 * GIB);

    // Two readings, card 1's outside use rising by 3 GiB between them.
    let first = lmgw_core::vram::headroom::sample(&f.state).await;
    assert_eq!(first, Some(20 * GIB), "the readings of the three cards");
    f.world().outside_on = vec![(1, 23 * GIB)];
    lmgw_core::vram::headroom::sample(&f.state).await;

    let now = f.state.vram.headroom_now(&f.state.snapshot());
    let seen: Vec<_> = now
        .cards
        .iter()
        .map(|c| {
            (
                c.index,
                c.name.as_str(),
                c.most_bytes,
                c.at_rest_bytes,
                c.floor_bytes,
                c.room_bytes,
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            (0, "RTX 6000 Ada", Some(0), Some(0), 0, 0),
            (
                1,
                "RTX PRO 6000 Blackwell",
                Some(23 * GIB),
                Some(20 * GIB),
                512 * MIB,
                0
            ),
            (2, "RTX 6000 Ada", Some(0), Some(0), 0, 0),
        ],
        "{now:?}"
    );
    // From the first readings: the floor on the card another program uses,
    // nothing on the others; no learned room while it holds its most.
    assert_eq!(now.bytes, 512 * MIB);
    assert_eq!(vram_status(&f.gateway).await["headroom_bytes"], 512 * MIB);
    // It lets go of 2 GiB: that is card 1's learned room now.
    f.world().outside_on = vec![(1, 21 * GIB)];
    lmgw_core::vram::headroom::sample(&f.state).await;
    let now = f.state.vram.headroom_now(&f.state.snapshot());
    assert_eq!(
        (now.cards[1].room_bytes, now.bytes),
        (2 * GIB, 2 * GIB + 512 * MIB),
        "{now:?}"
    );
    let stored = store::list_vram_outside_hours(&f.state.db, 0)
        .await
        .unwrap();
    assert_eq!(stored.len(), 3, "{stored:?}");

    // Card 1 forgotten: its readings go, in memory and stored; the others'
    // stay.
    let resp = f
        .gateway
        .client()
        .post(format!("{}/api/op/headroom_forget", f.gateway))
        .json(&json!({ "card": card_id(1).0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let after: Value = resp.json().await.unwrap();
    assert_eq!(after["cards"][1]["most_bytes"], Value::Null, "{after}");
    assert_eq!(
        after["cards"][1]["floor_bytes"],
        512 * MIB,
        "the whole floor: {after}"
    );
    assert_eq!(after["cards"][1]["room_bytes"], 0, "{after}");
    assert_eq!(after["cards"][0]["most_bytes"], 0, "{after}");
    assert_eq!(after["cards"][2]["hours_seen"], 1, "{after}");
    let stored = store::list_vram_outside_hours(&f.state.db, 0)
        .await
        .unwrap();
    assert!(
        stored.iter().all(|(d, _)| *d != card_id(1)) && stored.len() == 2,
        "{stored:?}"
    );

    // A card that is neither listed nor has readings is refused.
    let resp = f
        .gateway
        .client()
        .post(format!("{}/api/op/headroom_forget", f.gateway))
        .json(&json!({ "card": "GPU-not-here" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("RTX PRO 6000 Blackwell (card 1)"),
        "{body}"
    );
}

/// A day of readings on every card: the decisions read the sum of the
/// cards' floors and learned room, and a card nobody else uses adds
/// nothing.
#[tokio::test]
async fn the_decisions_read_the_sum_of_the_cards_headrooms() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    three_cards(&f, 20 * GIB);
    // The cards as a device read lists them.
    vram_status(&f.gateway).await;
    learned_flat(&f, 0, 0, 0);
    learned_flat(&f, 1, 20 * GIB, 2 * GIB);
    learned_flat(&f, 2, 0, 0);
    let now = f.state.vram.headroom_now(&f.state.snapshot());
    assert_eq!(
        (now.bytes, now.floor_bytes, now.room_bytes),
        (2 * GIB + 512 * MIB, 512 * MIB, 2 * GIB),
        "{now:?}"
    );
    assert_eq!(now.at_rest_bytes, Some(20 * GIB));
    assert_eq!(
        vram_status(&f.gateway).await["headroom_bytes"],
        2 * GIB + 512 * MIB
    );
    assert!(
        now.sentence.starts_with(
            "Headroom: auto, from what other programs did on each card: a floor of 512 MiB"
        ),
        "{}",
        now.sentence
    );
}

/// A start that never evicts or waits (an operator's, as a warm or boot
/// start) goes on a card with nothing of lmgw's on it, that card's learned
/// room given up, rather than being refused for an idle model on another
/// card that only an eviction would make room beside (WP13): before, it
/// was placed beside the idle embedder on card 0 and refused there.
#[tokio::test]
async fn a_start_that_never_evicts_takes_an_empty_cards_learned_room() {
    let f = fixture(20 * GIB, 6 * GIB, 4 * GIB, 512).await;
    f.world().cards = vec![("card 0".into(), 10 * GIB), ("card 1".into(), 10 * GIB)];
    // Card 1 all but full: the embedder goes on card 0, which then holds
    // the chat model only once the embedder is stopped.
    f.world().outside_on = vec![(1, 9 * GIB)];
    let devices = |model_id: &str| -> Vec<String> {
        super::llama_fit_review::argv(&f, model_id)
            .windows(2)
            .filter(|w| w[0] == "--device")
            .filter_map(|w| w[1].strip_prefix("nvidia.com/gpu=").map(str::to_string))
            .collect()
    };
    super::llama_fit_review::start(&f, "embed-model")
        .await
        .unwrap();
    assert_eq!(devices("embed-model"), vec![card_id(0).0.clone()]);
    // Card 1: 2 GiB of other programs that rose by 2 GiB within the day:
    // 2 GiB of learned room beside its 512 MiB floor, which the chat model
    // does not fit beside; card 0: nobody else, nothing kept.
    f.world().outside_on = vec![(1, 2 * GIB)];
    learned_flat(&f, 0, 0, 0);
    learned_flat(&f, 1, 2 * GIB, 2 * GIB);
    let now = f.state.vram.headroom_now(&f.state.snapshot());
    assert_eq!(
        (now.cards[1].floor_bytes, now.cards[1].room_bytes),
        (512 * MIB, 2 * GIB),
        "{now:?}"
    );

    super::llama_fit_review::start(&f, "chat-model")
        .await
        .unwrap();
    assert_eq!(devices("chat-model"), vec![card_id(1).0.clone()]);
    assert!(f.stops().is_empty(), "{:?}", f.stops());
}

/// A ladder climb reads the rules of the card its new rung goes on, as
/// admission does (owner, 2026-11-18), not the sum over every card: card 1
/// has 8 GiB of learned room, card 0 none and no floor (nobody else uses
/// it). A climb on card 0 that fits by card 0's own rules goes ahead
/// without evicting the idle embedding model there — with card 1's room
/// summed in, it evicted it and then went ahead warned.
#[tokio::test]
async fn a_climb_reads_its_own_cards_rules_not_every_cards() {
    let f = ladder_fixture(32 * GIB, 0).await;
    f.attribute(0);
    f.world().cards = vec![("card 0".into(), 16 * GIB), ("card 1".into(), 16 * GIB)];
    f.world().outside_on = vec![(1, 14 * GIB)];
    lmgw_core::vram::headroom::sample(&f.state).await;
    f.world().outside_on = vec![(1, 6 * GIB)];
    lmgw_core::vram::headroom::sample(&f.state).await;
    let now = f.state.vram.headroom_now(&f.state.snapshot());
    assert_eq!(
        (now.cards[0].floor_bytes, now.cards[0].room_bytes),
        (0, 0),
        "{now:?}"
    );
    assert_eq!(now.cards[1].room_bytes, 8 * GIB, "{now:?}");

    // The idle embedding model and the ladder's base both on card 0.
    super::llama_fit_review::start(&f, "embed-model")
        .await
        .unwrap();
    let hold = ladder_hold(&f).await;
    let on =
        |model: &str| super::placing_splits::devices(&super::llama_fit_review::argv(&f, model));
    assert_eq!(on("embed-model"), vec![card_id(0).0.clone()]);
    assert_eq!(on(LADDER), vec![card_id(0).0.clone()]);

    // 8 GiB rung on card 0: 11 GiB free there and the base's 2 GiB.
    let climbed = climb_to_top(&f, &hold).await;
    assert!(climbed.is_ok(), "{climbed:?}");
    assert_eq!(
        ladder_runs(&f).last().map(String::as_str),
        Some("ladder-top.gguf")
    );
    assert!(
        !f.stops().iter().any(|s| s == "embed-model"),
        "nothing evicted: {:?}",
        f.stops()
    );
    assert_eq!(on(LADDER), vec![card_id(0).0.clone()], "on its card");
}
