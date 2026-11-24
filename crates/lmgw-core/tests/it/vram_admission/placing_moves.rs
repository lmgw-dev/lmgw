//! Repacking (device-ledger design §6.2, WP7): an idle model stopped for
//! room on the card a larger one needs is started again on another card
//! that holds it now, as old as it was; the request that needed the room
//! does not wait for it; a moved model is not moved again before a request
//! has used it; a pinned one moves only among its pin's cards.

use super::llama_fit_review::{argv, start};
use super::*;
use lmgw_core::runtime::registry::RuntimeState;

/// `n` cards of 10 GiB, named "card 0", "card 1", ….
fn cards(f: &Fixture, n: u32) {
    f.world().cards = (0..n).map(|i| (format!("card {i}"), 10 * GIB)).collect();
}

/// Card 1 with `card_1` held by other programs, and every card after it
/// full.
fn outside(f: &Fixture, n: u32, card_1: u64) {
    f.world().outside_on = std::iter::once((1, card_1))
        .chain((2..n as usize).map(|i| (i, 10 * GIB)))
        .collect();
}

/// The NVIDIA devices an argv passes, as written after `--device`.
fn devices(argv: &[String]) -> Vec<String> {
    argv.windows(2)
        .filter(|w| w[0] == "--device")
        .filter_map(|w| w[1].strip_prefix("nvidia.com/gpu="))
        .map(str::to_string)
        .collect()
}

/// `model_id`'s registry entry, where it has one.
fn entry_of(f: &Fixture, model_id: &str) -> Option<lmgw_core::runtime::registry::RuntimeView> {
    f.state
        .runtime()
        .list()
        .into_iter()
        .find(|e| e.model_id == model_id)
}

/// Until `model_id` is ready, for up to 10 s.
async fn ready(f: &Fixture, model_id: &str) -> lmgw_core::runtime::registry::RuntimeView {
    for _ in 0..500 {
        if let Some(e) = entry_of(f, model_id).filter(|e| e.state == RuntimeState::Ready) {
            return e;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{model_id} did not start again: {:?}", f.world().runs);
}

/// On `n` cards: `embed-model` (2 GiB) started on card 0 while card 1 is
/// all but full; then card 1 has 4 GiB free, which holds it and not
/// `chat-model` (8 GiB), and the cards after it none; and `embed-model`
/// idle for over two seconds. Its age then.
async fn idle_on_card_0(f: &Fixture, n: u32) -> u64 {
    cards(f, n);
    outside(f, n, 9 * GIB);
    start(f, "embed-model").await.unwrap();
    assert_eq!(devices(&argv(f, "embed-model")), vec![card_id(0).0.clone()]);
    outside(f, n, 6 * GIB);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let age = entry_of(f, "embed-model").unwrap().last_used_age_seconds;
    assert!(age >= 2, "{age}");
    age
}

/// Set aux row `model_id`'s pin through its op.
async fn pin_aux(f: &Fixture, model_id: &str, cards: &[&str]) {
    let patch: lmgw_core::ops::AuxModelPatch = serde_json::from_value(json!({
        "action": "update",
        "model_id": model_id,
        "gpu_pin": cards,
    }))
    .unwrap();
    lmgw_core::ops::aux_model_set(&f.state, patch)
        .await
        .unwrap();
}

/// One more embedding row of `bytes`, in the fixture's models dir.
async fn add_aux(f: &Fixture, model_id: &str, bytes: u64) {
    let file = format!("{model_id}.gguf");
    crate::support::sparse::create(f._models_dir.path().join(&file), bytes);
    f.world().size.insert(model_id.to_string(), bytes);
    store::insert_aux_model(
        &f.state.db,
        &NewAuxModel {
            model_id: model_id.into(),
            gguf_path: file,
            kind: AuxKind::Embed,
            pooling: None,
            ctx_size: None,
            ubatch_size: None,
            batch_size: None,
            mmproj_path: None,
            backend: None,
            threads: None,
            args: vec![],
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
}

/// The wait's words on several cards (device-ledger design §10, WP8): a
/// caller that follows its own wait hears the eviction named with the card
/// it makes room on and that its victim starts again on another card, and
/// the queue's stage says the same.
#[tokio::test]
async fn a_followed_wait_names_the_card_it_makes_room_on_and_the_move() {
    use lmgw_core::vram::stage_watch::{follow, WaitPhase};

    let f = fixture(20 * GIB, 8 * GIB, 2 * GIB, 512).await;
    idle_on_card_0(&f, 2).await;
    let route = f.state.snapshot().resolve("chat-model").unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = follow(tx, lmgw_core::vram::admit(&f.state, &route, "chat-model"))
        .await
        .unwrap();
    assert!(hold.is_some(), "admitted");
    let mut phases = Vec::new();
    while let Ok(p) = rx.try_recv() {
        phases.push(p);
    }
    let card_0 = lmgw_api_types::WaitCard {
        id: card_id(0),
        index: 0,
        name: "card 0".into(),
    };
    assert!(
        phases.contains(&WaitPhase::Evicting {
            victim: "aux/embed-model".into(),
            cards: vec![card_0],
            moves: true,
        }),
        "{phases:?}"
    );
    drop(hold);
    ready(&f, "embed-model").await;
}

/// A refusal on several cards names the card it was judged against
/// (device-ledger design §10, WP8): an embedding model larger than every
/// card is measured against the largest.
#[tokio::test]
async fn too_large_names_the_largest_card_it_may_use() {
    let f = fixture(20 * GIB, 8 * GIB, 2 * GIB, 512).await;
    cards(&f, 2);
    add_aux(&f, "big-embed", 12 * GIB).await;
    let why = start(&f, "big-embed").await.unwrap_err();
    assert!(
        why.contains("card 0 (card 0), the largest card it may use, holds 10.0 GiB in total"),
        "{why}"
    );
}

/// The verification of WP7: with an idle model on the card a larger one
/// needs, the idle one is running on another card afterwards, as old as it
/// was. The request is answered without waiting for the move; the moved
/// container is handed card 1 by UUID; its warnings say where it came from
/// and for whom; its age is the one it had, older than the model it made
/// room for, until a request uses it.
#[tokio::test]
async fn an_idle_model_in_the_way_moves_to_another_card_as_old_as_it_was() {
    let f = fixture(20 * GIB, 8 * GIB, 2 * GIB, 512).await;
    let before = idle_on_card_0(&f, 2).await;

    let r = chat(&f.gateway).await;
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    assert_eq!(devices(&argv(&f, "chat-model")), vec![card_id(0).0.clone()]);

    let moved = ready(&f, "embed-model").await;
    {
        let w = f.world();
        assert_eq!(w.stops, vec!["embed-model".to_string()], "{:?}", w.stops);
        assert_eq!(
            w.runs,
            vec!["embed-model", "chat-model", "embed-model"],
            "{:?}",
            w.runs
        );
        assert!(w.loaded.contains("embed-model") && w.loaded.contains("chat-model"));
    }
    let a = argv(&f, "embed-model");
    assert_eq!(devices(&a), vec![card_id(1).0.clone()], "{a:?}");
    assert!(
        a.iter()
            .any(|x| x == &format!("NVIDIA_VISIBLE_DEVICES={}", card_id(1).0)),
        "{a:?}"
    );
    assert!(
        moved
            .warnings
            .iter()
            .any(|w| w
                == "Moved from card 0 (card 0) to card 1 (card 1) to make room for chat-model"),
        "{:?}",
        moved.warnings
    );
    // As old as it was: its idle age goes on from where it stood, and it
    // is still older than the model it made room for.
    assert!(
        moved.last_used_age_seconds >= before,
        "{} < {before}",
        moved.last_used_age_seconds
    );
    let chat_age = entry_of(&f, "chat-model").unwrap().last_used_age_seconds;
    assert!(moved.last_used_age_seconds > chat_age);
    // What the driver shows: each on its own card.
    let v = vram_status(&f.gateway).await;
    let used: Vec<u64> = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["used_bytes"].as_u64().unwrap())
        .collect();
    assert_eq!(used, vec![8 * GIB, 8 * GIB], "{v}");

    // A request makes it young again.
    let r = embed(&f.gateway).await;
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    let e = entry_of(&f, "embed-model").unwrap();
    assert!(
        e.last_used_age_seconds < before,
        "{}",
        e.last_used_age_seconds
    );
    assert_eq!(f.world().runs.len(), 3, "served where it was moved to");
}

/// Three cards: `embed-model` moved from card 0 to card 1 for
/// `chat-model` (the verification's start), then card 2 emptied and
/// `embed-small` (3 GiB) pinned to card 1, where it fits only once
/// `embed-model` is stopped; card 2 would hold `embed-model`.
async fn moved_then_card_2_free(f: &Fixture) {
    add_aux(f, "embed-small", 3 * GIB).await;
    pin_aux(f, "embed-small", &[&card_id(1).0]).await;
    idle_on_card_0(f, 3).await;
    let r = chat(&f.gateway).await;
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    ready(f, "embed-model").await;
    assert_eq!(devices(&argv(f, "embed-model")), vec![card_id(1).0.clone()]);
    f.world().outside_on = vec![(1, 6 * GIB)];
}

/// `embed-small`, asked for.
async fn embed_small(f: &Fixture) -> reqwest::Response {
    f.gateway
        .client()
        .post(format!("{}/v1/embeddings", f.gateway))
        .json(&json!({"model": "embed/embed-small", "input": "hello"}))
        .send()
        .await
        .unwrap()
}

/// Not again before it has served (D21): a moved model no request has used
/// is an ordinary victim — stopped for room again, and not started again,
/// though another card holds it.
#[tokio::test]
async fn a_moved_model_is_not_moved_again_before_it_has_served() {
    let f = fixture_n(30 * GIB, 8 * GIB, 2 * GIB, 512, 2).await;
    moved_then_card_2_free(&f).await;
    let r = embed_small(&f).await;
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    assert_eq!(
        devices(&argv(&f, "embed-small")),
        vec![card_id(1).0.clone()]
    );
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let w = f.world();
    assert_eq!(
        w.stops,
        vec!["embed-model".to_string(), "embed-model".to_string()],
        "{:?}",
        w.stops
    );
    assert!(!w.loaded.contains("embed-model"), "{:?}", w.runs);
    assert_eq!(
        w.runs,
        vec!["embed-model", "chat-model", "embed-model", "embed-small"],
        "{:?}",
        w.runs
    );
}

/// Once a request has used it, a moved model moves again: stopped for room
/// on card 1, it is started on card 2.
#[tokio::test]
async fn a_moved_model_that_served_moves_again() {
    let f = fixture_n(30 * GIB, 8 * GIB, 2 * GIB, 512, 2).await;
    moved_then_card_2_free(&f).await;
    let r = embed(&f.gateway).await;
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    let r = embed_small(&f).await;
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    for _ in 0..500 {
        if devices(&argv(&f, "embed-model")) == vec![card_id(2).0.clone()] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let moved = ready(&f, "embed-model").await;
    assert_eq!(
        devices(&argv(&f, "embed-model")),
        vec![card_id(2).0.clone()]
    );
    assert!(
        moved
            .warnings
            .iter()
            .any(|w| w
                == "Moved from card 1 (card 1) to card 2 (card 2) to make room for embed-small"),
        "{:?}",
        moved.warnings
    );
    assert_eq!(
        f.world().runs,
        vec![
            "embed-model",
            "chat-model",
            "embed-model",
            "embed-small",
            "embed-model"
        ]
    );
}

/// A pinned model moves only among its pin's cards (§6.2): pinned to card
/// 0, it is stopped for room there and stays stopped, though card 1 would
/// hold it.
#[tokio::test]
async fn a_pinned_model_moves_only_within_its_pin() {
    let f = fixture(20 * GIB, 8 * GIB, 2 * GIB, 512).await;
    pin_aux(&f, "embed-model", &[&card_id(0).0]).await;
    idle_on_card_0(&f, 2).await;
    let r = chat(&f.gateway).await;
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let w = f.world();
    assert_eq!(w.stops, vec!["embed-model".to_string()], "{:?}", w.stops);
    assert!(!w.loaded.contains("embed-model"));
    assert_eq!(w.runs, vec!["embed-model", "chat-model"], "{:?}", w.runs);
}

/// A machine of one card moves nothing: the eviction is what it was.
#[tokio::test]
async fn one_card_moves_nothing() {
    let f = fixture(10 * GIB, 8 * GIB, 2 * GIB, 512).await;
    start(&f, "embed-model").await.unwrap();
    let r = chat(&f.gateway).await;
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let w = f.world();
    assert_eq!(w.stops, vec!["embed-model".to_string()]);
    assert_eq!(w.runs, vec!["embed-model", "chat-model"]);
}
