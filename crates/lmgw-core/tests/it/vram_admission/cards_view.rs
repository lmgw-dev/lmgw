//! The ledger per card, shown (device-ledger design §10, WP2): `GET
//! /api/vram` gives each card lmgw's share there, the outside use there and
//! lmgw's models on it, and each model its cards — a split model on each of
//! its cards with its bytes there. The sums stay what they were.

use super::outside_vram_fallback::until_vram;
use super::*;

/// Three cards of two sizes: the chat model
/// split over cards 0 and 1, the embedder on card 2, another program on
/// card 1.
fn three_cards(f: &Fixture) {
    f.attribute(0);
    let mut w = f.world();
    w.cards = vec![
        ("RTX 6000 Ada".into(), 48 * GIB),
        ("RTX PRO 6000 Blackwell".into(), 96 * GIB),
        ("RTX 6000 Ada".into(), 48 * GIB),
    ];
    w.spread.insert(
        "chat-model".into(),
        vec![(0, Some(2 * GIB)), (1, Some(4 * GIB))],
    );
    w.spread
        .insert("embed-model".into(), vec![(2, Some(3 * GIB))]);
    w.outside_on = vec![(1, 20 * GIB)];
}

/// A card's figures and its models as `/api/vram` gives them.
fn on_card(v: &Value, i: usize) -> (Value, Value, Vec<(String, Value)>) {
    let d = &v["devices"][i];
    let models = d["resident"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                format!(
                    "{}/{}",
                    r["container"].as_str().unwrap(),
                    r["model"].as_str().unwrap()
                ),
                r["bytes"].clone(),
            )
        })
        .collect();
    (
        d["lmgw_share_bytes"].clone(),
        d["outside_share_bytes"].clone(),
        models,
    )
}

#[tokio::test]
async fn each_card_has_its_share_its_outside_use_and_its_models() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    three_cards(&f);
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(embed(&f.gateway).await.status(), 200);

    let v = until_vram(&f, "both models' shares are measured", |v| {
        v["lmgw_share_bytes"] == 9 * GIB
    })
    .await;
    // The sums stay, as sums.
    assert_eq!(v["capacity_bytes"], 192 * GIB, "{v}");
    assert_eq!(v["outside_share_bytes"], 20 * GIB, "{v}");
    let devices = v["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 3, "{v}");
    assert_eq!(devices[1]["id"], card_id(1).0, "{v}");
    assert_eq!(devices[1]["index"], 1);
    assert_eq!(devices[1]["name"], "RTX PRO 6000 Blackwell");

    let chat_on = |b: u64| ("chat/chat-model".to_string(), json!(b));
    assert_eq!(
        on_card(&v, 0),
        (json!(2 * GIB), json!(0), vec![chat_on(2 * GIB)]),
        "{v}"
    );
    assert_eq!(
        on_card(&v, 1),
        (json!(4 * GIB), json!(20 * GIB), vec![chat_on(4 * GIB)]),
        "{v}"
    );
    assert_eq!(
        on_card(&v, 2),
        (
            json!(3 * GIB),
            json!(0),
            vec![("aux/embed-model".to_string(), json!(3 * GIB))]
        ),
        "{v}"
    );
    for d in devices {
        assert!(d["share_unknown"].is_null(), "{d}");
        // Kept free per card where it is charged per card (WP4): with both
        // models up nothing is kept free, on any card.
        assert_eq!(
            d["kept_free"],
            json!({"starting_bytes": 0, "image_peak_bytes": 0, "pending_load_bytes": 0}),
            "{d}"
        );
    }

    let chat_cards = v["resident"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["model"] == "chat-model")
        .map(|r| r["cards"].clone())
        .unwrap();
    assert_eq!(
        chat_cards,
        json!([
            {"id": card_id(0).0, "bytes": 2 * GIB},
            {"id": card_id(1).0, "bytes": 4 * GIB},
        ]),
        "{v}"
    );
}

/// A card on which the driver lists one of lmgw's processes without a
/// figure is unknown on its own: the other cards keep their shares and
/// their models, the model on that card is there without its bytes, and the
/// sums are unknown with the card's reason, as the pooled figure was.
#[tokio::test]
async fn a_card_the_driver_cannot_size_is_unknown_alone() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    three_cards(&f);
    f.world()
        .spread
        .insert("embed-model".into(), vec![(2, None)]);
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(embed(&f.gateway).await.status(), 200);

    let v = until_vram(&f, "the chat model's cards are measured", |v| {
        v["devices"][0]["lmgw_share_bytes"] == 2 * GIB
    })
    .await;
    assert_eq!(on_card(&v, 1).0, json!(4 * GIB), "{v}");
    let (share, outside, models) = on_card(&v, 2);
    assert!(share.is_null() && outside.is_null(), "{v}");
    assert_eq!(
        models,
        vec![("aux/embed-model".to_string(), Value::Null)],
        "{v}"
    );
    let why = v["devices"][2]["share_unknown"]
        .as_str()
        .unwrap_or_default();
    assert!(why.contains("aux/embed-model"), "{v}");
    assert!(v["lmgw_share_bytes"].is_null(), "{v}");
    assert_eq!(v["external_trigger_reason"], why, "{v}");
}

/// On a machine of one card the card's figures are the machine's: its share
/// and outside use are the sums, what is kept free there is all of it, and
/// the model's one card is that card.
#[tokio::test]
async fn one_card_carries_the_machines_figures() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(GIB);
    assert_eq!(chat(&f.gateway).await.status(), 200);

    let v = until_vram(&f, "the chat model's share is measured", |v| {
        v["external_trigger_active"] == true
    })
    .await;
    let d = &v["devices"][0];
    assert_eq!(d["lmgw_share_bytes"], v["lmgw_share_bytes"], "{v}");
    assert_eq!(d["outside_share_bytes"], v["outside_share_bytes"], "{v}");
    assert_eq!(d["kept_free"], v["kept_free"], "{v}");
    assert_eq!(
        on_card(&v, 0).2,
        vec![("chat/chat-model".to_string(), json!(6 * GIB))],
        "{v}"
    );
    assert_eq!(
        v["resident"][0]["cards"],
        json!([{"id": card_id(0).0, "bytes": 6 * GIB}]),
        "{v}"
    );
}
