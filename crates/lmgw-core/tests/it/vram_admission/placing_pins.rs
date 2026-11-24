//! The rest of WP5 (device-ledger design §5.7, §7.1, §8, §8.1, §8.2): a
//! pinned row says its card under the command line; a pin applies without
//! a decision; llama-server's own flags that number a card are not passed
//! where lmgw hands the engine its cards, and boot adopts the container
//! started without them; the audio class's device index is read once as a
//! pin; a start that lowered a context to one card of several says so in
//! the context field's words; a climb whose rung its card cannot hold goes
//! on another card (§5.6). And the realtime budget per card (§3.1).

use super::llama_fit_review::{argv, start};
use super::unheld_containers::{entry, forget_everything, pass};
use super::*;
use lmgw_core::runtime::argv::HANDED_LABEL;
use lmgw_core::runtime::Class;
use lmgw_core::vram::headroom::LLAMA_MARGIN;

/// The cards of the machine, 16 GiB each, named "card <i>".
fn cards(f: &Fixture, n: u32) {
    f.world().cards = (0..n).map(|i| (format!("card {i}"), 16 * GIB)).collect();
}

/// The NVIDIA devices an argv passes, as written after `--device`.
fn devices(argv: &[String]) -> Vec<String> {
    argv.windows(2)
        .filter(|w| w[0] == "--device")
        .filter_map(|w| w[1].strip_prefix("nvidia.com/gpu="))
        .map(str::to_string)
        .collect()
}

/// The value of label `name` on an argv.
fn label(argv: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    argv.iter()
        .find_map(|a| a.strip_prefix(prefix.as_str()))
        .map(str::to_string)
}

/// Set chat row `model_id`'s pin through its op.
pub(super) async fn pin_chat(f: &Fixture, model_id: &str, cards: &[&str]) {
    let patch: lmgw_core::ops::LocalModelPatch = serde_json::from_value(json!({
        "action": "update",
        "model_id": model_id,
        "gpu_pin": cards,
    }))
    .unwrap();
    lmgw_core::ops::local_model_set(&f.state, patch)
        .await
        .unwrap();
}

/// Set `chat-model`'s llama-server arguments, as an edit does.
async fn chat_args(f: &Fixture, args: &[&str]) {
    let row = f
        .state
        .snapshot()
        .local_models
        .iter()
        .find(|m| m.model_id == "chat-model")
        .cloned()
        .unwrap();
    store::update_local_model(
        &f.state.db,
        row.id,
        &NewLocalModel {
            model_id: row.model_id,
            gguf_path: row.gguf_path,
            params: row.params,
            args: args.iter().map(|a| a.to_string()).collect(),
            idle_seconds: row.idle_seconds,
            enabled: row.enabled,
            public: row.public,
            image: row.image,
            extra_run_args: row.extra_run_args,
            warm_start: row.warm_start,
            fallback: row.fallback,
            capabilities_override: row.capabilities_override,
            ladder: row.ladder,
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// The read of chat row `model_id`.
pub(super) async fn read(f: &Fixture, model_id: &str) -> Value {
    crate::common::read_wire(
        lmgw_core::ops::local_model_get(&f.state, None, Some(model_id), None)
            .await
            .unwrap(),
    )
}

/// A pinned row's line under the command line names its card by name and
/// index and says what happens where it is gone (§7.1, §8); a pin of a card
/// the machine does not list says that.
#[tokio::test]
async fn a_pinned_row_names_its_card_under_the_command_line() {
    let f = fixture(32 * GIB, 3 * GIB, GIB, 512).await;
    cards(&f, 2);
    let _ = vram_status(&f.gateway).await;
    pin_chat(&f, "chat-model", &[&card_id(1).0]).await;
    let got = read(&f, "chat-model").await;
    let line = got["command_line_note"].as_str().unwrap_or_default();
    assert!(
        line.starts_with(
            "Pinned to card 1 (card 1): lmgw starts it on card 1 (card 1) alone, in place of \
             the graphics cards these arguments name."
        ) && line.contains("Where that card is gone, it does not start on another card"),
        "{got}"
    );

    pin_chat(&f, "chat-model", &["GPU-gone"]).await;
    let got = read(&f, "chat-model").await;
    let line = got["command_line_note"].as_str().unwrap_or_default();
    assert!(
        line.contains("a card this computer does not list (GPU-gone)")
            && line.contains("this computer lists none of them"),
        "{got}"
    );
}

/// A pin applies without a decision (§5.7): with admission off, a pinned
/// row is started on its pin's card by UUID, and the container records
/// the cards it was handed.
#[tokio::test]
async fn a_pin_applies_with_admission_off() {
    let f = fixture(32 * GIB, 3 * GIB, GIB, 512).await;
    cards(&f, 2);
    let mut s = f.state.snapshot().settings.clone();
    s.vram.enabled = false;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    pin_chat(&f, "chat-model", &[&card_id(1).0]).await;
    start(&f, "chat-model").await.unwrap();
    let a = argv(&f, "chat-model");
    assert_eq!(devices(&a), vec![card_id(1).0.clone()], "{a:?}");
    assert_eq!(label(&a, HANDED_LABEL), Some(card_id(1).0.clone()), "{a:?}");
    assert!(
        a.iter()
            .any(|x| x == &format!("NVIDIA_VISIBLE_DEVICES={}", card_id(1).0)),
        "{a:?}"
    );

    // Unpinned, it starts with the arguments as written, as before.
    let f = fixture(32 * GIB, 3 * GIB, GIB, 512).await;
    cards(&f, 2);
    let mut s = f.state.snapshot().settings.clone();
    s.vram.enabled = false;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    start(&f, "chat-model").await.unwrap();
    let a = argv(&f, "chat-model");
    assert_eq!(devices(&a), vec!["all".to_string()], "{a:?}");
    assert_eq!(label(&a, HANDED_LABEL), None, "{a:?}");
}

/// llama-server's own flags that number a card (§8.1) are not passed where
/// lmgw hands the engine its card, the start's warnings say so, and boot
/// adopts the container started without them rather than finding its
/// command changed.
#[tokio::test]
async fn the_engine_s_own_card_flags_go_where_lmgw_hands_the_card() {
    let f = fixture(32 * GIB, 3 * GIB, GIB, 512).await;
    f.world().reports = true;
    cards(&f, 2);
    chat_args(&f, &["--main-gpu", "1", "-sm", "none", "--no-warmup"]).await;
    start(&f, "chat-model").await.unwrap();
    let a = argv(&f, "chat-model");
    assert_eq!(devices(&a).len(), 1, "{a:?}");
    assert!(label(&a, HANDED_LABEL).is_some(), "{a:?}");
    assert!(!a.iter().any(|x| x == "--main-gpu" || x == "-sm"), "{a:?}");
    assert!(a.iter().any(|x| x == "--no-warmup"), "{a:?}");
    let warnings = entry(&f, "chat-model").unwrap().warnings;
    assert!(
        warnings.iter().any(|w| w
            .contains("Its llama-server arguments' --main-gpu 1 -sm none was not passed")
            && w.contains("GPU pin")),
        "{warnings:?}"
    );

    forget_everything(&f);
    assert!(pass(&f).await, "adopted");
    assert!(entry(&f, "chat-model").is_some_and(|e| e.adopted));
    assert_eq!(f.world().runs.len(), 1, "not started again");
}

/// On a machine of one card nothing is handed, and the row's own flags
/// stay as written (D17).
#[tokio::test]
async fn one_card_keeps_the_engine_s_own_flags() {
    let f = fixture(16 * GIB, 3 * GIB, GIB, 512).await;
    chat_args(&f, &["--main-gpu", "0"]).await;
    start(&f, "chat-model").await.unwrap();
    let a = argv(&f, "chat-model");
    assert!(a.iter().any(|x| x == "--main-gpu"), "{a:?}");
    assert_eq!(label(&a, HANDED_LABEL), None, "{a:?}");
}

/// The audio class's device index is read once as a pin (§8.1): on a
/// device read that lists its card, every audio row without a pin is
/// pinned to it, and the index goes to 0.
#[tokio::test]
async fn the_audio_device_index_becomes_a_pin() {
    let f = fixture(32 * GIB, 3 * GIB, GIB, 512).await;
    cards(&f, 2);
    super::audio_residency::add_audio_model(&f, "voice", GIB, GIB, None).await;
    let mut s = f.state.snapshot().settings.clone();
    s.audio.device = 1;
    s.audio.backend = "cuda".into();
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    let _ = vram_status(&f.gateway).await;
    let mut pinned = None;
    for _ in 0..100 {
        let snap = f.state.snapshot();
        if snap.settings.audio.device == 0 {
            pinned = snap
                .gpu_pins
                .get(&(Class::Audio, "voice".to_string()))
                .cloned();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(pinned, Some(vec![card_id(1)]));
}

/// A start that lowered a row's context to one card of several says so in
/// the words of the context field's notice (§8.2).
#[tokio::test]
async fn the_start_that_fits_one_card_says_it_as_the_editor_does() {
    use lmgw_core::config::LlamaParams;
    use lmgw_core::vram::partial::llama::CONTEXT_FITTED;
    const MIB: u64 = 1024 * 1024;
    let f = fixture(96 * MIB + LLAMA_MARGIN, 8 * MIB, 8 * MIB, 4).await;
    f.world().cards = vec![
        ("card 0".into(), 96 * MIB + LLAMA_MARGIN),
        ("card 1".into(), 96 * MIB + LLAMA_MARGIN),
    ];
    super::llama_fit_review::add_row(&f, "fit-model", LlamaParams::default()).await;
    let _ = vram_status(&f.gateway).await;
    start(&f, "fit-model").await.unwrap();
    let warnings = entry(&f, "fit-model").unwrap().warnings;
    assert!(
        warnings.iter().any(|w| w.starts_with(&format!(
            "{CONTEXT_FITTED}: No context set: the model's own is used (32,768 tokens). lmgw \
             keeps this model on one card. Where that context does not fit one card, the \
             largest that fits one card is used. Now that would be "
        )) && w
            .contains("To spread the model over several cards for more, set a context.")),
        "{warnings:?}"
    );
}

/// A climb whose new rung does not fit its own card goes on another card
/// that holds it now (§5.6, §5.2 step 1), started there by UUID, rather
/// than evicting or waiting on its own.
#[tokio::test]
async fn a_climb_that_does_not_fit_its_card_goes_on_another() {
    use super::ladder_admission::{climb_to_top, ladder_fixture, ladder_hold, ladder_view, LADDER};
    let f = ladder_fixture(32 * GIB, 0).await;
    f.world().cards = vec![("card 0".into(), 6 * GIB), ("card 1".into(), 16 * GIB)];
    let _ = vram_status(&f.gateway).await;
    let hold = ladder_hold(&f).await;
    // The base goes on the tighter card.
    assert_eq!(devices(&argv(&f, LADDER)), vec![card_id(0).0.clone()]);
    climb_to_top(&f, &hold).await.expect("card 1 holds it");
    hold.sync().await.unwrap();
    assert_eq!(ladder_view(&f).rung.unwrap().rung, 3);
    assert_eq!(devices(&argv(&f, LADDER)), vec![card_id(1).0.clone()]);
    let e = entry(&f, LADDER).unwrap();
    assert_eq!(e.card_charge.map(|c| c.cards()), Some(vec![card_id(1)]));
}

/// The realtime cascade of `chat-model` and `audio/ears` on `f`.
async fn cascade(f: &Fixture) -> lmgw_api_types::realtime::RealtimeBudget {
    lmgw_core::ops::realtime_budget(
        &f.state,
        lmgw_api_types::realtime::RealtimeBudgetArgs {
            default_model: Some("chat-model".into()),
            asr_alias: Some("audio/ears".into()),
            tts_alias: Some(String::new()),
            barge_in_check: Some("duration".into()),
            barge_in_check_alias: None,
        },
    )
    .await
    .unwrap()
}

/// A fixture of two 8 GiB cards with `chat` bytes of chat model and a
/// 3 GiB speech-to-text model, which needs as much again beside it as its
/// margin until its residency is learned (in every mode since WP13).
async fn two_cards(chat: u64) -> Fixture {
    let f = fixture(16 * GIB, chat, GIB, 512).await;
    f.world().cards = vec![("card 0".into(), 8 * GIB), ("card 1".into(), 8 * GIB)];
    super::audio_residency::add_audio_model(&f, "ears", 3 * GIB, 3 * GIB, None).await;
    let _ = vram_status(&f.gateway).await;
    f
}

/// The realtime budget on a machine of several cards places the cascade
/// model by model (device-ledger design §3.1): two models that each fit a
/// card fit on two, each stage naming its card; a model no single card
/// holds is too large, however much the cards hold together.
#[tokio::test]
async fn the_realtime_budget_places_its_models_per_card() {
    let f = two_cards(6 * GIB).await;
    let b = cascade(&f).await;
    assert_eq!(b.verdict, "fits", "{}", b.summary);
    let on: Vec<Option<String>> = b
        .stages
        .iter()
        .filter(|s| s.stage == "chat" || s.stage == "asr")
        .map(|s| s.card.clone())
        .collect();
    assert_eq!(on.len(), 2, "{b:?}");
    assert!(on.iter().all(Option::is_some), "{b:?}");
    assert_ne!(on[0], on[1], "one card each: {b:?}");
    assert!(b.summary.contains("placed one by one"), "{}", b.summary);

    // 9 GiB of chat: no card of 8 holds it, though the two hold 16.
    let f = two_cards(9 * GIB).await;
    let b = cascade(&f).await;
    assert_eq!(b.verdict, "too_large", "{}", b.summary);
    assert!(
        b.summary.contains("chat/chat-model finds no card"),
        "{}",
        b.summary
    );
}
