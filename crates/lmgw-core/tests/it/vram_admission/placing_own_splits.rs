//! A row's own split (device-ledger design §7.2, rulings 11 and 16, WP11):
//! a chat row with its own `--tensor-split` pinned to as many cards as the
//! split has entries starts on those cards, handed by UUID, with its split
//! as written and each card charged its share; the same row unpinned, or
//! pinned to another number of cards, is placed as any row and started
//! without its split, `--main-gpu` and `--split-mode` beside it, and says
//! so; an aux row's own split is dropped with a warning that aux models run
//! on one card. Boot adopts either kind of container as its start rendered
//! it.

use super::llama_fit_review::{argv, flag, start};
use super::placing_pins::{pin_chat, read};
use super::placing_splits::{add_row, devices, label, set_context, write_model};
use super::unheld_containers::{entry, forget_everything, pass};
use super::*;
use lmgw_core::runtime::argv::{FITTED_LABEL, HANDED_LABEL, OWN_SPLIT_LABEL};
use lmgw_core::vram::charge::PLACEMENT_LABEL;
use lmgw_core::vram::headroom::LLAMA_MARGIN;
use lmgw_core::vram::CardCharge;

const MIB: u64 = 1024 * 1024;

/// The row's own split, and `--main-gpu` beside it.
const OWN: &[&str] = &["--tensor-split", "1,1", "--main-gpu", "0"];

/// Three cards of 192 MiB, 30 MiB of each held by other programs: each card
/// holds [`write_model`]'s model whole (about 58 MiB) beside the 4 MiB kept
/// free, and a split's part with its larger compute buffer (`by_layer`). A
/// ledger read first, so lmgw knows the machine has several cards.
async fn three_cards() -> Fixture {
    let f = fixture(576 * MIB + LLAMA_MARGIN, 8 * MIB, 8 * MIB, 4).await;
    f.world().cards = (0..3)
        .map(|i| (format!("card {i}"), 192 * MIB + LLAMA_MARGIN))
        .collect();
    f.world().outside_on = vec![(0, 30 * MIB), (1, 30 * MIB), (2, 30 * MIB)];
    vram_status(&f.gateway).await;
    f
}

/// What the driver shows used on each card.
async fn used(f: &Fixture) -> Vec<u64> {
    let v = vram_status(&f.gateway).await;
    v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["used_bytes"].as_u64().unwrap())
        .collect()
}

/// The verification of WP11 (§13): a chat row with `--tensor-split 1,1`
/// pinned to two cards starts on those two, handed by UUID in the bus's
/// order, with its split and `--main-gpu` as written and nothing of lmgw's
/// split beside them; each card is charged its share — five of the eight
/// layers on the first, three and the output layer on the second, as
/// llama.cpp divides `1,1` — within what was free there beside the 4 MiB
/// kept free, and the driver shows each share on its card. Boot adopts the
/// container with its split.
#[tokio::test]
async fn a_row_s_own_split_on_its_pin_starts_on_those_cards_as_written() {
    let f = three_cards().await;
    f.world().reports = true;
    add_row(&f, "own-split", set_context(), OWN).await;
    pin_chat(&f, "own-split", &[&card_id(1).0, &card_id(2).0]).await;
    start(&f, "own-split").await.unwrap();
    let a = argv(&f, "own-split");

    assert_eq!(
        devices(&a),
        vec![card_id(1).0.clone(), card_id(2).0.clone()],
        "{a:?}"
    );
    assert!(
        a.iter()
            .any(|x| x == &format!("NVIDIA_VISIBLE_DEVICES={},{}", card_id(1).0, card_id(2).0)),
        "{a:?}"
    );
    assert!(
        a.iter().any(|x| x == "CUDA_DEVICE_ORDER=PCI_BUS_ID"),
        "{a:?}"
    );
    assert!(label(&a, HANDED_LABEL).is_some(), "{a:?}");
    assert_eq!(label(&a, OWN_SPLIT_LABEL).as_deref(), Some("kept"), "{a:?}");
    // The row's split as written, and nothing of lmgw's.
    assert_eq!(flag(&a, "--tensor-split").as_deref(), Some("1,1"), "{a:?}");
    assert_eq!(flag(&a, "--main-gpu").as_deref(), Some("0"), "{a:?}");
    assert_eq!(flag(&a, "--split-mode"), None, "{a:?}");
    assert_eq!(flag(&a, "--fit"), None, "{a:?}");
    assert_eq!(label(&a, FITTED_LABEL), None, "{a:?}");

    // Each card its share: 5 layers of 6 MiB and a compute buffer on card
    // 1; 3 layers, the output layer and a compute buffer on card 2.
    let recorded = label(&a, PLACEMENT_LABEL).unwrap();
    let charge = CardCharge::parse(&recorded).unwrap();
    let parts = charge.split_parts().expect("a part on each card");
    assert_eq!(parts[0].0, card_id(1));
    assert_eq!(parts[1].0, card_id(2));
    assert!(parts[0].1 >= 30 * MIB, "{parts:?}");
    assert!(parts[1].1 >= 20 * MIB, "{parts:?}");
    // Two layers more on card 1, the output layer (2 MiB and its norm's
    // few bytes) on card 2, and the first card's larger compute buffer on
    // a pipeline (`by_layer`: 64 MiB on top against 16).
    let diff = parts[0].1 - parts[1].1;
    let first = 2 * 6 * MIB - 2 * MIB + 48 * MIB;
    assert!((first - 64 * 1024..=first).contains(&diff), "{parts:?}");
    for (_, b) in &parts {
        assert!(*b <= (162 - 4) * MIB, "within what was free: {parts:?}");
    }
    assert_eq!(
        used(&f).await,
        vec![30 * MIB, 30 * MIB + parts[0].1, 30 * MIB + parts[1].1]
    );

    // The start says the split is kept, and what each card holds.
    let warnings = entry(&f, "own-split").unwrap().warnings;
    assert!(
        warnings.iter().any(|w| w.starts_with(
            "Its own --tensor-split 1,1 is kept: lmgw starts it on the 2 cards of its pin"
        ) && w.contains("5 layers on card 1 (card 1)")
            && w.contains("3 layers and the output layer on card 2 (card 2)")),
        "{warnings:?}"
    );

    // The editor's line says so too.
    let got = read(&f, "own-split").await;
    let line = got["command_line_note"].as_str().unwrap_or_default();
    assert!(
        line.contains("lmgw starts it on all of them")
            && line.contains("Its own --tensor-split 1,1 is kept"),
        "{got}"
    );

    // Boot adopts it as it was started: the split, and its parts.
    forget_everything(&f);
    assert!(pass(&f).await, "adopted");
    let e = entry(&f, "own-split").expect("adopted");
    assert!(e.adopted);
    assert_eq!(f.world().runs.len(), 1, "not started again");
    assert_eq!(e.card_charge.and_then(|c| c.label()), Some(recorded));
}

/// The same row unpinned (§7.2): placed as any row — on one card, which
/// holds it whole — and started without its split and `--main-gpu`; the
/// start's warnings and the editor's line say a split of its own needs a
/// pin of two cards. Boot adopts the container started without them.
#[tokio::test]
async fn the_same_row_unpinned_starts_on_one_card_without_its_split() {
    let f = three_cards().await;
    f.world().reports = true;
    add_row(&f, "own-split", set_context(), OWN).await;
    start(&f, "own-split").await.unwrap();
    let a = argv(&f, "own-split");
    assert_eq!(devices(&a).len(), 1, "{a:?}");
    assert!(label(&a, HANDED_LABEL).is_some(), "{a:?}");
    assert_eq!(label(&a, OWN_SPLIT_LABEL), None, "{a:?}");
    for gone in ["--tensor-split", "--main-gpu", "--split-mode"] {
        assert_eq!(flag(&a, gone), None, "{gone}: {a:?}");
    }
    let charge = CardCharge::parse(&label(&a, PLACEMENT_LABEL).unwrap()).unwrap();
    assert_eq!(charge.cards().len(), 1);

    let warnings = entry(&f, "own-split").unwrap().warnings;
    assert!(
        warnings.iter().any(|w| w.contains(
            "Its llama-server arguments' --tensor-split 1,1 --main-gpu 0 were not passed: a \
             split of its own needs a GPU pin of 2 cards, and it has none, so lmgw placed it as \
             it places any model; to keep the split, pin it to 2 cards"
        )),
        "{warnings:?}"
    );
    let got = read(&f, "own-split").await;
    let line = got["command_line_note"].as_str().unwrap_or_default();
    assert!(
        line.contains(
            "Its own --tensor-split 1,1 needs a GPU pin of 2 cards, and it has none: lmgw starts \
             it without --tensor-split 1,1 --main-gpu 0 and places it as it places any model. To \
             keep the split, pin it to 2 cards."
        ),
        "{got}"
    );

    forget_everything(&f);
    assert!(pass(&f).await, "adopted");
    assert!(entry(&f, "own-split").is_some_and(|e| e.adopted));
    assert_eq!(f.world().runs.len(), 1, "not started again");
}

/// A pin of another size (§7.2): pinned to one card, a split of two
/// entries is not passed, and the warning says what the pin is.
#[tokio::test]
async fn a_pin_of_another_size_starts_without_the_split() {
    let f = three_cards().await;
    add_row(&f, "own-split", set_context(), OWN).await;
    pin_chat(&f, "own-split", &[&card_id(2).0]).await;
    start(&f, "own-split").await.unwrap();
    let a = argv(&f, "own-split");
    assert_eq!(devices(&a), vec![card_id(2).0.clone()], "{a:?}");
    assert_eq!(flag(&a, "--tensor-split"), None, "{a:?}");
    assert_eq!(flag(&a, "--main-gpu"), None, "{a:?}");
    let warnings = entry(&f, "own-split").unwrap().warnings;
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("needs a GPU pin of 2 cards, and it is pinned to 1")),
        "{warnings:?}"
    );
}

/// A pin written in the run arguments counts as one (§8.1): a row whose
/// `NVIDIA_VISIBLE_DEVICES` names two cards keeps its split of two
/// entries on them.
#[tokio::test]
async fn a_run_argument_pin_keeps_the_split() {
    let f = three_cards().await;
    write_model(&f, "own-split.gguf");
    f.world().size.insert("own-split".into(), 50 * MIB);
    store::insert_local_model(
        &f.state.db,
        &NewLocalModel {
            model_id: "own-split".into(),
            gguf_path: "own-split.gguf".into(),
            params: set_context(),
            args: OWN.iter().map(|a| a.to_string()).collect(),
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: Some(
                [
                    "--device",
                    "nvidia.com/gpu=all",
                    "-e",
                    "NVIDIA_VISIBLE_DEVICES=0,2",
                ]
                .map(String::from)
                .to_vec(),
            ),
            warm_start: false,
            fallback: None,
            capabilities_override: None,
            ladder: vec![],
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    start(&f, "own-split").await.unwrap();
    let a = argv(&f, "own-split");
    assert_eq!(
        devices(&a),
        vec![card_id(0).0.clone(), card_id(2).0.clone()],
        "{a:?}"
    );
    assert_eq!(flag(&a, "--tensor-split").as_deref(), Some("1,1"), "{a:?}");
    assert_eq!(label(&a, OWN_SPLIT_LABEL).as_deref(), Some("kept"), "{a:?}");
}

/// Ruling 16: an aux row's own split is not passed, with a warning that
/// aux models run on one card, and the row starts on one card.
#[tokio::test]
async fn an_aux_row_s_own_split_is_dropped() {
    let f = three_cards().await;
    let file = "split-embed.gguf";
    crate::support::sparse::create(f._models_dir.path().join(file), 8 * MIB);
    f.world().size.insert("split-embed".into(), 8 * MIB);
    store::insert_aux_model(
        &f.state.db,
        &NewAuxModel {
            model_id: "split-embed".into(),
            gguf_path: file.into(),
            kind: AuxKind::Embed,
            pooling: None,
            ctx_size: None,
            ubatch_size: None,
            batch_size: None,
            mmproj_path: None,
            backend: None,
            threads: None,
            args: ["-ts", "1,1", "-sm", "layer"].map(String::from).to_vec(),
            idle_seconds: 0,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            fallback: None,
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    start(&f, "split-embed").await.unwrap();
    let a = argv(&f, "split-embed");
    assert_eq!(devices(&a).len(), 1, "{a:?}");
    assert_eq!(flag(&a, "-ts"), None, "{a:?}");
    assert_eq!(flag(&a, "-sm"), None, "{a:?}");
    let warnings = entry(&f, "split-embed").unwrap().warnings;
    assert!(
        warnings.iter().any(|w| w.contains(
            "Its llama-server arguments' -ts 1,1 -sm layer were not passed: aux models run on \
             one card"
        )),
        "{warnings:?}"
    );
}

/// An unpinned row's own split gives way to lmgw's (§7.2, §5.2 step 3):
/// where no card holds its set context, lmgw splits it over the fewest
/// cards that hold it, with a split of its own in place of the row's.
#[tokio::test]
async fn an_unpinned_row_no_card_holds_is_split_by_lmgw() {
    let f = super::placing_splits::four_cards().await;
    super::placing_splits::add_big(&f, "own-split", set_context(), OWN).await;
    start(&f, "own-split").await.unwrap();
    let a = argv(&f, "own-split");
    assert_eq!(
        devices(&a),
        vec![card_id(1).0.clone(), card_id(2).0.clone()],
        "{a:?}"
    );
    assert_eq!(flag(&a, "--split-mode").as_deref(), Some("layer"), "{a:?}");
    assert_ne!(flag(&a, "--tensor-split").as_deref(), Some("1,1"), "{a:?}");
    assert_eq!(
        a.iter().filter(|x| *x == "--tensor-split").count(),
        1,
        "{a:?}"
    );
    assert_eq!(flag(&a, "--main-gpu"), None, "{a:?}");
    let warnings = entry(&f, "own-split").unwrap().warnings;
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("lmgw splits it over 2 cards")
                && w.contains("a split of its own needs a GPU pin of 2 cards")),
        "{warnings:?}"
    );
}

/// A row's own split kept on its pin under a declared budget over two cards
/// (owner, 2026-11-18): its parts together, the engine's cost included, fit
/// the budget as an lmgw split's do. Under 200 MiB each part fits a card
/// alone but not their sum: refused, naming the budget. Under 400 MiB it
/// starts on its pin as written.
#[tokio::test]
async fn under_a_budget_an_own_split_s_parts_together_fit_it() {
    for (budget_mb, starts) in [(200u64, false), (400, true)] {
        let f = fixture(2 * (192 * MIB + LLAMA_MARGIN), 8 * MIB, 8 * MIB, 4).await;
        f.world().cards = (0..2)
            .map(|i| (format!("card {i}"), 192 * MIB + LLAMA_MARGIN))
            .collect();
        vram_status(&f.gateway).await;
        let mut s = f.state.snapshot().settings.clone();
        s.vram.budget_mb = budget_mb;
        store::save_settings(&f.state.db, &s).await.unwrap();
        f.state.reload_snapshot().await.unwrap();
        add_row(&f, "own-split", set_context(), OWN).await;
        pin_chat(&f, "own-split", &[&card_id(0).0, &card_id(1).0]).await;
        match (start(&f, "own-split").await, starts) {
            (Ok(_), true) => {
                let a = argv(&f, "own-split");
                assert_eq!(flag(&a, "--tensor-split").as_deref(), Some("1,1"), "{a:?}");
                assert_eq!(
                    devices(&a),
                    vec![card_id(0).0.clone(), card_id(1).0.clone()],
                    "{a:?}"
                );
            }
            (Err(why), false) => {
                assert!(why.contains("GPU memory budget"), "{why}");
                assert!(argv(&f, "own-split").is_empty(), "not started");
            }
            (other, _) => panic!("budget {budget_mb} MiB: {other:?}"),
        }
    }
}
