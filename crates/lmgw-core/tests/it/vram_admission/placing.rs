//! lmgw places on one card (device-ledger design §5, §6.1, §7.1, WP5): a
//! model goes on the card it leaves least free on and is started on that
//! card alone, by UUID; a machine of one card starts its containers with
//! the arguments as written; an eviction stops a model on the card that is
//! short, never one on another card.

use super::llama_fit_review::{argv, start};
use super::*;
use lmgw_core::runtime::argv::run_args_digest;
use lmgw_core::vram::charge::PLACEMENT_LABEL;
use lmgw_core::vram::headroom::LLAMA_MARGIN;

/// The cards of the machine as (name, total), in the driver's order.
fn cards(f: &Fixture, totals: &[u64]) {
    f.world().cards = totals
        .iter()
        .enumerate()
        .map(|(i, t)| (format!("card {i}"), *t))
        .collect();
}

/// `bytes` held by programs outside lmgw on card `index`.
fn outside_on(f: &Fixture, index: usize, bytes: u64) {
    f.world().outside_on.push((index, bytes));
}

/// The value of a label on a container's argv.
fn label(argv: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    argv.iter()
        .find_map(|a| a.strip_prefix(prefix.as_str()))
        .map(str::to_string)
}

/// The NVIDIA devices an argv passes, as written after `--device`.
fn devices(argv: &[String]) -> Vec<String> {
    argv.windows(2)
        .filter(|w| w[0] == "--device")
        .filter_map(|w| w[1].strip_prefix("nvidia.com/gpu="))
        .map(str::to_string)
        .collect()
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

/// An unpinned model on three cards goes on the card it leaves least free
/// on — not the emptiest — and on that card only: the container is passed
/// that card by UUID with `NVIDIA_VISIBLE_DEVICES` and `CUDA_DEVICE_ORDER`
/// in place of the class's `nvidia.com/gpu=all`, records it in its label,
/// and its run-args digest is the arguments' as written.
#[tokio::test]
async fn a_model_goes_on_the_tightest_card_and_on_it_alone() {
    let f = fixture(48 * GIB, 3 * GIB, GIB, 512).await;
    cards(&f, &[16 * GIB, 16 * GIB, 16 * GIB]);
    // Free: 1 GiB on card 0, 6 GiB on card 1, 16 GiB on card 2.
    outside_on(&f, 0, 15 * GIB);
    outside_on(&f, 1, 10 * GIB);
    start(&f, "chat-model").await.unwrap();

    let a = argv(&f, "chat-model");
    assert_eq!(devices(&a), vec![card_id(1).0.clone()], "{a:?}");
    assert!(
        a.iter()
            .any(|x| x == &format!("NVIDIA_VISIBLE_DEVICES={}", card_id(1).0)),
        "{a:?}"
    );
    assert!(
        a.iter().any(|x| x == "CUDA_DEVICE_ORDER=PCI_BUS_ID"),
        "{a:?}"
    );
    assert!(
        label(&a, PLACEMENT_LABEL)
            .unwrap()
            .starts_with(&format!("{}:", card_id(1).0)),
        "{a:?}"
    );
    let written = f.state.snapshot().settings.router.extra_run_args.clone();
    assert!(written.iter().any(|x| x == "nvidia.com/gpu=all"));
    assert_eq!(
        label(&a, "lmgw.run_args"),
        Some(run_args_digest(&written)),
        "the digest is the arguments' as written"
    );
    // What the driver shows: the model's process on card 1.
    let v = vram_status(&f.gateway).await;
    let used: Vec<u64> = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["used_bytes"].as_u64().unwrap())
        .collect();
    assert_eq!(used[0], 15 * GIB, "{v}");
    assert!(used[1] > 10 * GIB, "{v}");
    assert_eq!(used[2], 0, "{v}");
}

/// A machine of one card starts its containers with the arguments as
/// written (D17): the class's `nvidia.com/gpu=all`, no variables of lmgw's.
#[tokio::test]
async fn one_card_starts_with_the_arguments_as_written() {
    let f = fixture(16 * GIB, 3 * GIB, GIB, 512).await;
    start(&f, "chat-model").await.unwrap();
    let a = argv(&f, "chat-model");
    assert_eq!(devices(&a), vec!["all".to_string()], "{a:?}");
    assert!(
        !a.iter().any(
            |x| x.starts_with("NVIDIA_VISIBLE_DEVICES=") || x.starts_with("CUDA_DEVICE_ORDER=")
        ),
        "{a:?}"
    );
}

/// A row whose arguments name a card is placed on that card, and started
/// with them as written: its placement is every card it passes (D17).
#[tokio::test]
async fn a_row_that_names_its_card_keeps_its_arguments() {
    let f = fixture(32 * GIB, 3 * GIB, GIB, 512).await;
    cards(&f, &[16 * GIB, 16 * GIB]);
    let row = f
        .state
        .snapshot()
        .aux_models
        .iter()
        .find(|m| m.model_id == "embed-model")
        .cloned()
        .unwrap();
    store::update_aux_model(
        &f.state.db,
        row.id,
        &NewAuxModel {
            model_id: row.model_id,
            gguf_path: row.gguf_path,
            kind: row.kind,
            pooling: row.pooling,
            ctx_size: row.ctx_size,
            ubatch_size: row.ubatch_size,
            batch_size: row.batch_size,
            mmproj_path: row.mmproj_path,
            backend: row.backend,
            threads: row.threads,
            args: row.args,
            idle_seconds: row.idle_seconds,
            enabled: row.enabled,
            image: row.image,
            extra_run_args: Some(vec!["--device".into(), "nvidia.com/gpu=1".into()]),
            warm_start: row.warm_start,
            fallback: row.fallback,
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    start(&f, "embed-model").await.unwrap();
    let a = argv(&f, "embed-model");
    assert_eq!(devices(&a), vec!["1".to_string()], "{a:?}");
    assert!(
        !a.iter().any(|x| x.starts_with("NVIDIA_VISIBLE_DEVICES=")),
        "{a:?}"
    );
    assert!(label(&a, PLACEMENT_LABEL)
        .unwrap()
        .starts_with(&format!("{}:", card_id(1).0)));
}

/// An eviction stays on the card that is short (§6.1): the model on the
/// other card, idle and used longer ago, is left alone, and the one on the
/// card the request goes on is stopped.
#[tokio::test]
async fn an_eviction_stops_a_model_on_the_short_card_only() {
    let f = fixture(20 * GIB, 8 * GIB, 3 * GIB, 512).await;
    cards(&f, &[10 * GIB, 10 * GIB]);
    add_aux(&f, "embed-two", 2 * GIB).await;
    // Card 1 has 5 GiB free: the older idle model goes there, the tighter
    // card; the other on card 0.
    outside_on(&f, 1, 5 * GIB);
    start(&f, "embed-two").await.unwrap();
    assert_eq!(devices(&argv(&f, "embed-two")), vec![card_id(1).0.clone()]);
    start(&f, "embed-model").await.unwrap();
    assert_eq!(
        devices(&argv(&f, "embed-model")),
        vec![card_id(0).0.clone()]
    );

    // 8 GiB fit card 0 once its model is stopped, and card 1 never: the
    // least recently used model is card 1's, and it stays.
    let r = chat(&f.gateway).await;
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    let w = f.world();
    assert_eq!(w.stops, vec!["embed-model".to_string()], "{:?}", w.stops);
    assert!(w.loaded.contains("embed-two"));
    drop(w);
    assert_eq!(devices(&argv(&f, "chat-model")), vec![card_id(0).0.clone()]);
}

/// A start that waits commits to its card and gives the gate back (§5.5,
/// D15): while one waits for a busy model on card 0, a start that fits now
/// goes ahead at once instead of waiting behind it for the gate.
#[tokio::test]
async fn a_start_that_fits_is_not_held_up_by_one_that_waits() {
    let f = fixture(20 * GIB, 8 * GIB, 3 * GIB, 512).await;
    cards(&f, &[10 * GIB, 10 * GIB]);
    add_aux(&f, "embed-two", GIB).await;
    outside_on(&f, 1, 8 * GIB);
    start(&f, "chat-model").await.unwrap();
    assert_eq!(devices(&argv(&f, "chat-model")), vec![card_id(0).0.clone()]);
    // Busy with traffic lmgw does not see: never evicted, waited for.
    f.world().busy.insert("chat-model".into());

    let gw = f.gateway.clone();
    let waiter = tokio::spawn(async move { embed(&gw).await.status() });
    // Until it waits for room (its phase), with its claim on card 0.
    let mut waiting = false;
    for _ in 0..200 {
        let v = vram_status(&f.gateway).await;
        waiting = v["queue"].as_array().is_some_and(|q| {
            q.iter().any(|w| {
                w["phase"]["kind"] == "waiting_for_room" || w["phase"] == "waiting_for_room"
            })
        });
        if waiting {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(waiting, "{}", vram_status(&f.gateway).await);
    let started = std::time::Instant::now();
    let r = f
        .gateway
        .client()
        .post(format!("{}/v1/embeddings", f.gateway))
        .json(&json!({"model": "embed/embed-two", "input": "hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "held up for {:?}",
        started.elapsed()
    );
    assert!(!waiter.is_finished(), "the waiter still waits");
    // The waiter times out, as before: nothing it waited for left.
    let status = waiter.await.unwrap();
    assert_ne!(status, 200);
}

/// The engine's own cost on a card (§4.2, ruling 17): a llama.cpp model is
/// charged the stated default on a card that has learned none; once it runs
/// there alone, what its processes hold less lmgw's estimate of its buffers
/// is that card's cost, stored, charged from then on and shown in Settings.
#[tokio::test]
async fn the_engine_cost_is_charged_and_learned_per_card() {
    const MIB: u64 = 1024 * 1024;
    let f = fixture(16 * GIB, 6 * GIB, GIB, 512).await;
    f.state.vram.set_engine_default(640 * MIB);
    f.world().attribution = true;
    // It holds a GiB more than its file.
    f.world().size.insert("chat-model".into(), 7 * GIB);
    start(&f, "chat-model").await.unwrap();

    let card = card_id(0);
    let mut learned = None;
    for _ in 0..200 {
        let c = f.state.vram.engine_cost(&card);
        if c.learned_at.is_some() {
            learned = Some(c.bytes);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let learned = learned.expect("the engine's cost on card 0 was learned");
    let v = vram_status(&f.gateway).await;
    let r = v["resident"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["model"] == "chat-model")
        .cloned()
        .unwrap();
    let charged = r["estimated_bytes"].as_u64().unwrap();
    // Its buffers as estimated, and the cost learned on its card.
    assert!(learned > 0 && learned <= GIB, "{learned}");
    assert_eq!(charged, 7 * GIB, "the estimate and the learned cost: {v}");
    let stored = lmgw_core::store::list_vram_engine_costs(&f.state.db)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!((stored[0].0.clone(), stored[0].1), (card.clone(), learned));
    let now = f.state.vram.headroom_now(&f.state.snapshot());
    assert!(
        now.engine_sentence.contains("FakeGPU (card 0)") && now.engine_sentence.contains("learned"),
        "{}",
        now.engine_sentence
    );
    assert_eq!(now.cards[0].engine_bytes, learned);
}

/// The engine's cost is read once the model is ready and idle, not only
/// when a start settles (owner, 2026-11-18): a start whose own request is
/// still in flight then is read on the headroom's next periodic reading.
#[tokio::test]
async fn the_engine_cost_is_read_once_the_model_is_idle() {
    const MIB: u64 = 1024 * 1024;
    let f = fixture(16 * GIB, 6 * GIB, GIB, 512).await;
    f.state.vram.set_engine_default(640 * MIB);
    f.world().attribution = true;
    f.world().size.insert("chat-model".into(), 7 * GIB);
    // Started for a request held open: busy when the start settles.
    let route = f.state.snapshot().resolve("chat-model").unwrap();
    let hold = lmgw_core::vram::admit(&f.state, &route, "chat-model")
        .await
        .expect("the chat model is admitted")
        .expect("a managed local model gets a hold");
    let card = card_id(0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        f.state.vram.engine_cost(&card).learned_at,
        None,
        "not read while its request is in flight"
    );
    drop(hold);
    lmgw_core::vram::headroom::sample(&f.state).await;
    let mut learned = false;
    for _ in 0..200 {
        if f.state.vram.engine_cost(&card).learned_at.is_some() {
            learned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(learned, "read on the periodic reading once idle");
}

/// Until a card has learned its cost, a llama.cpp model is charged the
/// stated default there, beside its estimate.
#[tokio::test]
async fn a_card_without_a_learned_cost_is_charged_the_default() {
    const MIB: u64 = 1024 * 1024;
    let f = fixture(16 * GIB, 6 * GIB, GIB, 512).await;
    start(&f, "chat-model").await.unwrap();
    let before = vram_status(&f.gateway).await["estimated_resident_bytes"]
        .as_u64()
        .unwrap();
    let f2 = fixture(16 * GIB, 6 * GIB, GIB, 512).await;
    f2.state.vram.set_engine_default(640 * MIB);
    start(&f2, "chat-model").await.unwrap();
    let after = vram_status(&f2.gateway).await["estimated_resident_bytes"]
        .as_u64()
        .unwrap();
    assert_eq!(after, before + 640 * MIB);
    let now = f2.state.vram.headroom_now(&f2.state.snapshot());
    assert!(
        now.engine_sentence.contains("640 MiB (the CUDA default"),
        "{}",
        now.engine_sentence
    );
}

/// Set chat row `model_id`'s pin through its op.
async fn pin_chat(f: &Fixture, model_id: &str, cards: &[&str]) {
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

/// A pin (§8) places the model on its card, the roomier one included, and
/// starts it there by UUID in place of the class's every card; the row
/// reads it back; clearing it gives the choice back to lmgw.
#[tokio::test]
async fn a_pin_places_the_model_on_its_card() {
    let f = fixture(48 * GIB, 3 * GIB, GIB, 512).await;
    cards(&f, &[16 * GIB, 16 * GIB, 16 * GIB]);
    outside_on(&f, 0, 10 * GIB);
    pin_chat(&f, "chat-model", &[&card_id(2).0]).await;
    let snap = f.state.snapshot();
    assert_eq!(
        snap.gpu_pins
            .get(&(lmgw_core::runtime::Class::Chat, "chat-model".to_string())),
        Some(&vec![card_id(2)])
    );
    start(&f, "chat-model").await.unwrap();
    assert_eq!(devices(&argv(&f, "chat-model")), vec![card_id(2).0.clone()]);
    let got = crate::common::read_wire(
        lmgw_core::ops::local_model_get(&f.state, None, Some("chat-model"), None)
            .await
            .unwrap(),
    );
    assert_eq!(got["gpu_pin"], json!([card_id(2).0]), "{got}");

    let patch: lmgw_core::ops::LocalModelPatch = serde_json::from_value(json!({
        "action": "update", "model_id": "chat-model", "clear": "gpu_pin",
    }))
    .unwrap();
    lmgw_core::ops::local_model_set(&f.state, patch)
        .await
        .unwrap();
    assert!(f.state.snapshot().gpu_pins.is_empty());
}

/// A pinned card that is gone (§8): the start does not go to another card,
/// and says which card is missing.
#[tokio::test]
async fn a_pinned_card_that_is_gone_starts_nowhere_else() {
    let f = fixture(32 * GIB, 3 * GIB, GIB, 512).await;
    cards(&f, &[16 * GIB, 16 * GIB]);
    pin_chat(&f, "chat-model", &["GPU-gone"]).await;
    let _ = vram_status(&f.gateway).await;
    let err = start(&f, "chat-model").await;
    let text = format!("{err:?}");
    assert!(
        text.contains("pinned to a graphics card this computer does not list (GPU-gone)"),
        "{text}"
    );
    assert!(f.world().runs.is_empty(), "nothing started");
    let r = chat(&f.gateway).await;
    assert_eq!(r.status(), 503);
}

/// Run arguments that name a card by `NVIDIA_VISIBLE_DEVICES` are read as
/// a pin (§8.1): the model goes on that card, started by UUID; and a row's
/// own `CUDA_VISIBLE_DEVICES` is not passed, which its warnings say.
#[tokio::test]
async fn a_card_named_by_the_variable_is_a_pin() {
    let f = fixture(32 * GIB, 3 * GIB, GIB, 512).await;
    cards(&f, &[16 * GIB, 16 * GIB]);
    let mut s = f.state.snapshot().settings.clone();
    s.router.extra_run_args = vec![
        "--device".into(),
        "nvidia.com/gpu=all".into(),
        "-e".into(),
        "NVIDIA_VISIBLE_DEVICES=1".into(),
        "-e".into(),
        "CUDA_VISIBLE_DEVICES=0".into(),
    ];
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    start(&f, "chat-model").await.unwrap();
    let a = argv(&f, "chat-model");
    assert_eq!(devices(&a), vec![card_id(1).0.clone()], "{a:?}");
    assert!(!a.iter().any(|x| x == "CUDA_VISIBLE_DEVICES=0"), "{a:?}");
    let warnings = f
        .state
        .runtime()
        .list()
        .into_iter()
        .find(|e| e.model_id == "chat-model")
        .unwrap()
        .warnings;
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("CUDA_VISIBLE_DEVICES=0 was not passed") && w.contains("GPU pin")),
        "{warnings:?}"
    );
}

/// The context field's notice (§8.2): a chat row with no context on a
/// machine of several cards says lmgw keeps it on one card and what that
/// card holds of its own context now; a set context has none, and neither
/// does a machine of one card.
#[tokio::test]
async fn the_context_notice_says_one_card() {
    use lmgw_core::config::LlamaParams;
    const MIB: u64 = 1024 * 1024;
    let f = fixture(96 * MIB + LLAMA_MARGIN, 8 * MIB, 8 * MIB, 4).await;
    cards(&f, &[96 * MIB + LLAMA_MARGIN, 96 * MIB + LLAMA_MARGIN]);
    super::llama_fit_review::add_row(&f, "fit-model", LlamaParams::default()).await;
    let _ = vram_status(&f.gateway).await;
    start(&f, "fit-model").await.unwrap();
    let read = |f: &Fixture| {
        let st = f.state.clone();
        async move {
            crate::common::read_wire(
                lmgw_core::ops::local_model_get(&st, None, Some("fit-model"), None)
                    .await
                    .unwrap(),
            )
        }
    };
    let got = read(&f).await;
    let notice = got["context_notice"].as_str().unwrap_or_default();
    assert!(
        notice.starts_with(
            "No context set: the model's own is used (32,768 tokens). lmgw keeps this model on \
             one card."
        ) && notice.contains("Now that would be ")
            && notice.contains("tokens on card ")
            && notice.ends_with("To spread the model over several cards for more, set a context."),
        "{got}"
    );
    // Started: where it runs now comes first (§10).
    let line = got["command_line_note"].as_str().unwrap_or_default();
    assert!(
        line.starts_with("It runs on card ")
            && line.contains(" now. lmgw picks the card when it starts this model"),
        "{got}"
    );

    let one = fixture(96 * MIB + LLAMA_MARGIN, 8 * MIB, 8 * MIB, 4).await;
    super::llama_fit_review::add_row(&one, "fit-model", LlamaParams::default()).await;
    let _ = vram_status(&one.gateway).await;
    let got = read(&one).await;
    assert!(got["context_notice"].is_null(), "{got}");
    assert!(got["command_line_note"].is_null(), "{got}");
}
