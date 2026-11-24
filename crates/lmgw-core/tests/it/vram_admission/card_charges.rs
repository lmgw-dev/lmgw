//! Charges per card (device-ledger design §3.2, §3.3, §4.1, WP4): a start
//! is charged on the cards it sees — the card its arguments name, else
//! every card as a group — records that charge on its container in a
//! label, and boot adopts it with it. A container without the label is
//! charged on the cards its arguments pass, and says so where that is a
//! group; one whose recorded card is gone is charged nowhere. The sums the
//! decisions read are what they were.

use super::llama_fit_review::{argv, start};
use super::unheld_containers::{entry, forget_everything, pass};
use super::*;
use lmgw_core::vram::charge::PLACEMENT_LABEL;
use lmgw_core::vram::CardCharge;

/// The `lmgw.placement` label a container was started with.
fn label(argv: &[String]) -> Option<String> {
    let prefix = format!("{PLACEMENT_LABEL}=");
    argv.iter()
        .find_map(|a| a.strip_prefix(prefix.as_str()))
        .map(str::to_string)
}

/// `model`'s resident in `/api/vram`.
fn resident(v: &Value, model: &str) -> Value {
    v["resident"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["model"] == model)
        .cloned()
        .unwrap_or(Value::Null)
}

/// `n` cards of 16 GiB each.
fn cards(f: &Fixture, n: u32) {
    f.world().cards = (0..n).map(|i| (format!("card {i}"), 16 * GIB)).collect();
}

/// Set `chat-model`'s own run arguments, as an edit does.
async fn chat_run_args(f: &Fixture, args: &[&str]) {
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
            args: row.args,
            idle_seconds: row.idle_seconds,
            enabled: row.enabled,
            public: row.public,
            image: row.image,
            extra_run_args: Some(args.iter().map(|a| a.to_string()).collect()),
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

/// On one card a start is charged on that card, records it in its label
/// beside the others, and is adopted with it; what is kept free on the card
/// is the machine's.
#[tokio::test]
async fn a_start_records_its_charge_and_boot_adopts_it() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().reports = true;
    start(&f, "chat-model").await.unwrap();
    let v = vram_status(&f.gateway).await;
    let r = resident(&v, "chat-model");
    let bytes = r["estimated_bytes"].as_u64().unwrap();
    assert_eq!(
        r["charge"],
        json!([{"cards": [card_id(0).0], "bytes": bytes}]),
        "{v}"
    );
    assert_eq!(
        label(&argv(&f, "chat-model")),
        Some(format!("{}:{bytes}", card_id(0).0))
    );
    assert_eq!(v["devices"][0]["kept_free"], v["kept_free"], "{v}");

    forget_everything(&f);
    assert!(pass(&f).await, "adopted");
    let e = entry(&f, "chat-model").expect("adopted");
    assert!(e.adopted);
    assert_eq!(
        e.card_charge,
        Some(CardCharge::over(vec![card_id(0)], bytes))
    );
    let v = vram_status(&f.gateway).await;
    assert_eq!(resident(&v, "chat-model")["estimated_bytes"], bytes, "{v}");
    let note = resident(&v, "chat-model")["note"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !note.contains("no longer lists") && !note.contains("does not record its cards"),
        "{note}"
    );
}

/// A row whose arguments name a card is charged on that card; one that
/// passes every card is placed on one of them (device-ledger design WP5):
/// the card it leaves least free on, which is the named row's, and is
/// charged there alone.
#[tokio::test]
async fn a_named_card_is_the_charge_and_a_placed_model_is_on_one_card() {
    let f = fixture(48 * GIB, 6 * GIB, 3 * GIB, 512).await;
    cards(&f, 3);
    chat_run_args(&f, &["--device", "nvidia.com/gpu=1"]).await;
    start(&f, "chat-model").await.unwrap();
    start(&f, "embed-model").await.unwrap();

    let v = vram_status(&f.gateway).await;
    let chat = resident(&v, "chat-model");
    assert_eq!(
        chat["charge"],
        json!([{"cards": [card_id(1).0], "bytes": chat["estimated_bytes"]}]),
        "{v}"
    );
    let embed = resident(&v, "embed-model");
    assert_eq!(
        embed["charge"],
        json!([{"cards": [card_id(1).0], "bytes": embed["estimated_bytes"]}]),
        "{v}"
    );
    assert_eq!(
        label(&argv(&f, "embed-model")),
        Some(format!("{}:{}", card_id(1).0, embed["estimated_bytes"]))
    );
    // Once both are up, nothing is kept free on any card but their margins
    // until their first request has run, on the card they are on (in
    // every mode since WP13).
    for d in v["devices"].as_array().unwrap() {
        let starting = match d["id"] == card_id(1).0 {
            true => 2 * lmgw_core::vram::headroom::LLAMA_MARGIN,
            false => 0,
        };
        assert_eq!(
            d["kept_free"],
            json!({"starting_bytes": starting, "image_peak_bytes": 0, "pending_load_bytes": 0}),
            "{v}"
        );
    }
    // The sums are what they were.
    let sum: u64 = v["resident"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["estimated_bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(v["estimated_resident_bytes"], sum, "{v}");
}

/// A container whose recorded card the machine no longer lists stays
/// adopted, is charged nowhere, and says which card is gone.
#[tokio::test]
async fn a_recorded_card_that_is_gone_is_charged_nowhere() {
    let f = fixture(32 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().reports = true;
    cards(&f, 2);
    chat_run_args(&f, &["--device", "nvidia.com/gpu=1"]).await;
    start(&f, "chat-model").await.unwrap();
    assert!(label(&argv(&f, "chat-model"))
        .unwrap()
        .starts_with(&card_id(1).0));

    cards(&f, 1);
    forget_everything(&f);
    assert!(pass(&f).await, "adopted");
    assert!(entry(&f, "chat-model").is_some_and(|e| e.adopted));
    let v = vram_status(&f.gateway).await;
    let r = resident(&v, "chat-model");
    assert_eq!(r["estimated_bytes"], 0, "{v}");
    assert_eq!(r["charge"], json!([]), "{v}");
    let note = r["note"].as_str().unwrap_or_default();
    assert!(
        note.contains(&card_id(1).0)
            && note.contains("no longer lists")
            && note.contains("nowhere"),
        "{note}"
    );
}

/// A container started without the label (an lmgw before charges per card)
/// is adopted and charged its estimate on the cards its arguments pass, as
/// a group, and the resident says that it is not what it holds on each —
/// until the driver says what its processes hold on each card, which it is
/// then charged (device-ledger design §3.3, WP5).
#[tokio::test]
async fn a_container_without_the_label_is_charged_as_a_group_then_by_what_it_holds() {
    let f = fixture(48 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.world().reports = true;
    cards(&f, 3);
    start(&f, "chat-model").await.unwrap();
    let before = resident(&vram_status(&f.gateway).await, "chat-model");
    let on = before["charge"][0]["cards"][0]
        .as_str()
        .unwrap()
        .to_string();
    {
        let mut w = f.world();
        let argv = w.argv.get_mut("chat-model").unwrap();
        let at = argv
            .iter()
            .position(|a| a.starts_with(PLACEMENT_LABEL))
            .expect("the label");
        argv.drain(at - 1..=at);
    }

    forget_everything(&f);
    assert!(pass(&f).await, "adopted");
    assert_eq!(entry(&f, "chat-model").unwrap().card_charge, None);
    let v = vram_status(&f.gateway).await;
    let r = resident(&v, "chat-model");
    let every: Vec<String> = (0..3).map(|i| card_id(i).0).collect();
    assert_eq!(
        r["charge"],
        json!([{"cards": every, "bytes": before["estimated_bytes"]}]),
        "{v}"
    );
    let note = r["note"].as_str().unwrap_or_default();
    assert!(note.contains("does not record its cards"), "{note}");

    // The driver lists its processes: charged what they hold, on the card
    // they are on.
    f.world().attribution = true;
    f.state.vram.cache_pids(&f.state);
    let mut last = Value::Null;
    for _ in 0..100 {
        let v = vram_status(&f.gateway).await;
        last = resident(&v, "chat-model");
        if last["charge"]
            .as_array()
            .is_some_and(|c| c.len() == 1 && c[0]["cards"] == json!([on]))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        last["charge"],
        json!([{"cards": [on], "bytes": 6 * GIB}]),
        "{last}"
    );
    let note = last["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("Charged what its processes hold on each card they are on")
            && !note.contains("does not record its cards"),
        "{note}"
    );
}
