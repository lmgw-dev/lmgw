//! What an enrolment says of its device (its time, address, `client` and
//! `device_name`; device link design §3.6, *changed 2026-10-23*) belongs to
//! the key it came with. A key waiting beside a confirmed one keeps its own,
//! which the owner's Confirm moves onto the row; a re-pair nobody confirms
//! leaves the confirmed device's line as it was.

use lmgw_client::requests::DeviceSelf;
use serde_json::{json, Value};

use super::{world, World};
use crate::support::device_pair;

const ANNA: DeviceSelf<'static> = DeviceSelf {
    name: Some("Anna's phone"),
    client: Some("Kai mobile 0.1.0"),
    app_id: None,
};

const STRANGER: DeviceSelf<'static> = DeviceSelf {
    name: Some("Stranger's phone"),
    client: Some("Other client 9.9"),
    app_id: None,
};

/// A device `phone` paired and confirmed as `ANNA`: its id, its enrolment
/// and its row's pairing.
async fn anna(w: &World) -> (i64, device_pair::Enrolled, Value) {
    let link = device_pair::mint(
        &w.state,
        &w.gw,
        "key_create",
        json!({ "kind": "device", "name": "phone", "local_only": true }),
    )
    .await;
    let (spki, private) = device_pair::key();
    let enrolled = device_pair::enrol_key_with(&link, spki, private, ANNA).await;
    device_pair::confirm(&w.gw, &enrolled).await;
    let pairing = w.row("phone").await["pairing"].clone();
    (enrolled.credentials.device_id, enrolled, pairing)
}

/// A new key enrolling with a fresh token beside the confirmed one.
async fn re_pair(w: &World, id: i64, device: DeviceSelf<'_>) -> device_pair::Enrolled {
    let link = device_pair::mint(&w.state, &w.gw, "device_token", json!({ "id": id })).await;
    let (spki, private) = device_pair::key();
    device_pair::enrol_key_with(&link, spki, private, device).await
}

/// The fields of a row's pairing that say how its key enrolled.
fn said(pairing: &Value) -> [&Value; 4] {
    [
        &pairing["enrolled_at"],
        &pairing["enrolled_from"],
        &pairing["client"],
        &pairing["device_name"],
    ]
}

/// A key waiting beside the confirmed one shows its own self-description
/// (state `enrolled`, the confirmed one's beside it as `kept_*`); a new
/// token that ends the wait restores the confirmed device's line, and a
/// Confirm moves the newcomer's onto the row.
#[tokio::test]
async fn an_unconfirmed_re_pair_does_not_overwrite_the_confirmed_devices_line() {
    let w = world().await;
    let (id, _, original) = anna(&w).await;
    assert_eq!(original["device_name"], json!("Anna's phone"), "{original}");
    assert_eq!(original["client"], json!("Kai mobile 0.1.0"), "{original}");

    // The stranger's key waits: its values show, Anna's are the kept ones.
    let _stranger = re_pair(&w, id, STRANGER).await;
    let pairing = w.row("phone").await["pairing"].clone();
    assert_eq!(pairing["state"], json!("enrolled"), "{pairing}");
    assert_eq!(pairing["device_name"], json!("Stranger's phone"));
    assert_eq!(pairing["client"], json!("Other client 9.9"));
    assert_eq!(pairing["kept_device_name"], json!("Anna's phone"));
    assert_eq!(pairing["kept_client"], json!("Kai mobile 0.1.0"));

    // Nobody confirms it and the owner makes a new token: the line is the
    // confirmed device's again, to the second.
    device_pair::mint(&w.state, &w.gw, "device_token", json!({ "id": id })).await;
    let pairing = w.row("phone").await["pairing"].clone();
    assert_eq!(pairing["state"], json!("confirmed"), "{pairing}");
    assert_eq!(said(&pairing), said(&original), "{pairing}");
    assert_eq!(pairing["kept_device_name"], Value::Null, "{pairing}");
    assert_eq!(pairing["kept_client"], Value::Null, "{pairing}");

    // This time the newcomer is confirmed: its values move over.
    let newcomer = re_pair(&w, id, STRANGER).await;
    let (s, v) = w
        .op(
            "device_confirm",
            json!({ "id": id, "confirm_code": newcomer.credentials.confirm_code }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let pairing = w.row("phone").await["pairing"].clone();
    assert_eq!(pairing["state"], json!("confirmed"), "{pairing}");
    assert_eq!(pairing["device_name"], json!("Stranger's phone"));
    assert_eq!(pairing["client"], json!("Other client 9.9"));
    assert_eq!(pairing["kept_device_name"], Value::Null, "{pairing}");
}

/// The same key enrolling again (a renewal) says its name afresh on the
/// row, and a name it leaves out is none.
#[tokio::test]
async fn a_renewal_says_the_device_name_afresh() {
    let w = world().await;
    let (id, enrolled, _) = anna(&w).await;
    let renamed = DeviceSelf {
        name: Some("Anna's Pixel"),
        ..ANNA
    };
    let link = device_pair::mint(&w.state, &w.gw, "device_token", json!({ "id": id })).await;
    device_pair::enrol_key_with(
        &link,
        enrolled.spki.clone(),
        enrolled.identity.1.clone_key(),
        renamed,
    )
    .await;
    let pairing = w.row("phone").await["pairing"].clone();
    assert_eq!(pairing["state"], json!("confirmed"), "{pairing}");
    assert_eq!(pairing["device_name"], json!("Anna's Pixel"), "{pairing}");
    assert_eq!(pairing["kept_device_name"], Value::Null, "{pairing}");

    let link = device_pair::mint(&w.state, &w.gw, "device_token", json!({ "id": id })).await;
    let unnamed = DeviceSelf { name: None, ..ANNA };
    device_pair::enrol_key_with(
        &link,
        enrolled.spki.clone(),
        enrolled.identity.1.clone_key(),
        unnamed,
    )
    .await;
    let pairing = w.row("phone").await["pairing"].clone();
    assert_eq!(pairing["device_name"], Value::Null, "{pairing}");
}
