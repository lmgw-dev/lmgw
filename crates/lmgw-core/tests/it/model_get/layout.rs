//! Get it asks (device-ledger design WP12): on a computer of two cards
//! where no single card holds the file at its full context and a split
//! over both does, `model_get` without a layout answers with the choices
//! and downloads nothing; with `layout: split` it creates the row at the
//! full context, which lmgw then splits where no card holds it.

use std::sync::{Arc, Mutex};

use lmgw_api_types::model_find::{GetLayout, MinCardsState, Sign};
use lmgw_core::vram::{GpuMemory, GpuProbe};

use super::*;
use crate::support::fake_machine::{card_id, Machine};

/// A probe whose cards a test states, and states again.
struct Cards(Mutex<Machine>);

impl GpuProbe for Cards {
    fn devices(&self) -> Result<Vec<GpuMemory>, String> {
        Ok(self.0.lock().unwrap().devices())
    }

    fn source(&self) -> String {
        "stated cards".into()
    }
}

/// The world of [`world_on`], read from stated cards with no budget, and
/// with the chat class's engine image here (its `--help` lists a split's
/// flags).
async fn world_on_cards(machine: Machine) -> (World, Arc<Cards>) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::init_for_tests().await.unwrap();
    let podman = Arc::new(Podman::default());
    *podman.engine.lock().unwrap() = Some(Engine {
        here: true,
        ..Engine::default()
    });
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        podman.clone(),
        reqwest::Client::new(),
        Arc::new(|| Ok(1)),
    )));
    let mut settings = Settings::default();
    settings.router.models_dir = dir.path().join("chat").display().to_string();
    settings.aux_router.models_dir = dir.path().join("aux").display().to_string();
    settings.router.image = "demo/llama-server:split".into();
    settings.aux_router.image = String::new();
    settings.vram.budget_mb = 0;
    settings.vram.headroom_mb = 32;
    store::save_settings(&state.db, &settings).await.unwrap();
    let cards = Arc::new(Cards(Mutex::new(machine)));
    state.vram.set_probe(cards.clone());
    state.reload_snapshot().await.unwrap();
    let gw = common::serve(state.clone()).await;
    (
        World {
            state,
            gw,
            podman,
            dir,
        },
        cards,
    )
}

/// Two cards of `each` bytes.
fn two(each: u64) -> Machine {
    Machine::new()
        .card(card_id(0), "Demo card A", each)
        .card(card_id(1), "Demo card B", each)
}

#[tokio::test]
async fn a_file_only_a_split_holds_is_asked_about_and_set_up_as_chosen() {
    let _env = common::process_env_lock().await;
    // First on one large card: what the file takes at its full context.
    let (w, cards) = world_on_cards(Machine::one_card("Demo big", 64 << 30)).await;
    let (_hub, _cdn_server, _cdn) = hub(&[chat_repo()], Cdn::default()).await;
    let plan = lmgw_core::modelinfo::local_model_plan_of(
        &w.state,
        None,
        Some(REPO),
        Some(WEIGHTS),
        Some("chat"),
    )
    .await
    .unwrap();
    let uses = plan["fit"]["whole_card"]["uses_bytes"].as_u64().unwrap();
    assert!(uses > 64 * MIB, "{plan}");
    let one = plan["fit"]["whole_card"]["cards"].clone();
    assert_eq!(one["min_cards"]["state"], "one_card", "{one}");
    assert!(one["choices"].as_array().is_none_or(Vec::is_empty), "{one}");

    // Two cards, each 70 % of it and a start's margin, which rides on each
    // card of a split (in every mode since the device ledger's WP13): only
    // a split over both holds it.
    *cards.0.lock().unwrap() = two(uses * 7 / 10 + lmgw_core::vram::headroom::LLAMA_MARGIN);
    let asked = get(&w.gw, json!({"repo": REPO, "file": WEIGHTS})).await;
    assert!(
        asked.get_id.is_none() && asked.job_id.is_none(),
        "{asked:?}"
    );
    assert!(asked.downloads.is_empty(), "{asked:?}");
    let layouts: Vec<GetLayout> = asked.choices.iter().map(|c| c.layout).collect();
    assert!(layouts.contains(&GetLayout::Split), "{asked:?}");
    let split = asked
        .choices
        .iter()
        .find(|c| c.layout == GetLayout::Split)
        .unwrap();
    assert_eq!(split.ctx, 131_072);
    assert_eq!(split.cards.len(), 2, "{split:?}");
    assert!(
        split.sentence.starts_with("Split over cards 0 and 1"),
        "{split:?}"
    );
    assert!(asked.message.contains("layout"), "{}", asked.message);
    assert!(
        store::list_hf_models(&w.state.db).await.unwrap().is_empty(),
        "nothing is downloaded before the person chooses"
    );

    // The file list's marks say the same.
    let marks = lmgw_core::modelinfo::local_model_plan_of(
        &w.state,
        None,
        Some(REPO),
        Some(WEIGHTS),
        Some("chat"),
    )
    .await
    .unwrap();
    let c: lmgw_api_types::model_find::CardsFit =
        serde_json::from_value(marks["fit"]["beside"]["cards"].clone()).unwrap();
    assert_eq!(c.min_cards.state, MinCardsState::Holds, "{c:?}");
    assert_eq!(c.min_cards.label, "2 of 2");
    assert_eq!(c.free.sign, Sign::Ok, "{c:?}");
    assert!(c.asks());

    // Chosen: split. The row takes the full context.
    let started = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "layout": "split"}),
    )
    .await;
    assert_eq!(started.layout, Some(GetLayout::Split));
    assert!(started.choices.is_empty());
    let (row, done, error) = ended(&w.state, started.get_id.unwrap()).await;
    assert_eq!(row.status, "done", "{error:?}");
    assert_eq!(row.layout.as_deref(), Some("split"));
    let done = done.expect("a result");
    assert_eq!(done.layout, Some(GetLayout::Split));
    assert_eq!(done.ctx_size, Some(131_072));
    assert!(done.note.is_none(), "{done:?}");
    let snap = w.state.snapshot();
    let created = snap
        .local_models
        .iter()
        .find(|m| m.id == done.row_id)
        .unwrap();
    assert_eq!(created.params.ctx_size, Some(131_072));
    assert!(w.podman.starts().is_empty(), "{:?}", w.podman.starts());
}

/// A layout of experts in RAM for a model without experts is refused
/// before anything is downloaded.
#[tokio::test]
async fn experts_in_ram_for_a_model_without_experts_is_refused() {
    let _env = common::process_env_lock().await;
    let (w, _cards) = world_on_cards(two(64 << 30)).await;
    let (_hub, _cdn_server, _cdn) = hub(&[chat_repo()], Cdn::default()).await;
    let (status, body) = op(
        &w.gw,
        "model_get",
        json!({"repo": REPO, "file": WEIGHTS, "layout": "experts_in_ram"}),
    )
    .await;
    assert_ne!(status, 200, "{body}");
    assert!(
        body.to_string().contains("not a mixture-of-experts model"),
        "{body}"
    );
    assert!(store::list_hf_models(&w.state.db).await.unwrap().is_empty());
}
