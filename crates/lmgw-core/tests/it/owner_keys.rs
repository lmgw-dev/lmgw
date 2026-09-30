//! The owner-key ops of the Keys page (principals design §3.12).
//!
//! An owner key is the one credential that is *kept*: minted with a prefix
//! that names it, stored in plaintext so the page can hand it back, rotated in
//! one write, and — for `owner:dashboard` alone — neither disabled nor
//! deleted, because a gateway whose only owner row is switched off has no
//! surface left to switch it back on.
//!
//! Driven over HTTP rather than against the functions, for `key_set.rs`'s
//! reason: the dashboard posts `/api/op/*`, and an op that is implemented but
//! not dispatched is a feature that is dead in exactly the way a direct unit
//! test cannot see.

use lmgw_core::agents::token::OWNER_DASHBOARD;
use lmgw_core::state::{AppState, SharedState};
use serde_json::{json, Value};

use crate::common;
use common::{serve, Gw};

/// A client presenting one specific credential — for the assertions about a
/// key that [`Gw`] does not hold (a rotated-away one, a disabled owner row).
fn bearer(key: &str) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {key}").parse().unwrap(),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

async fn op_as(client: &reqwest::Client, gw: &Gw, name: &str, body: Value) -> (u16, Value) {
    let resp = client
        .post(format!("{gw}/api/op/{name}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn op(gw: &Gw, name: &str, body: Value) -> (u16, Value) {
    op_as(&gw.client(), gw, name, body).await
}

/// `GET /api/status` is the cheapest `Admin` route: what it answers is what
/// the gate made of the credential that asked.
async fn status_as(client: &reqwest::Client, gw: &Gw) -> (u16, Value) {
    let resp = client.get(format!("{gw}/api/status")).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// One row of `GET /api/usage/keys`, by name.
async fn listed(gw: &Gw, name: &str) -> Value {
    let v: Value = gw
        .client()
        .get(format!("{gw}/api/usage/keys"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["keys"]
        .as_array()
        .unwrap_or_else(|| panic!("no keys array in {v}"))
        .iter()
        .find(|k| k["name"] == json!(name))
        .cloned()
        .unwrap_or_else(|| panic!("no key named '{name}' in {v}"))
}

async fn gateway() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

/// Create one owner key and hand back `(id, plaintext)`.
async fn make_owner(gw: &Gw, name: &str) -> (i64, String) {
    let (status, v) = op(gw, "key_create", json!({ "name": name, "kind": "owner" })).await;
    assert_eq!(status, 200, "{v}");
    (
        v["id"].as_i64().unwrap(),
        v["plaintext"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn an_owner_key_is_minted_named_and_listed_as_one() {
    let (_st, gw) = gateway().await;

    let (status, v) = op(
        &gw,
        "key_create",
        json!({ "name": "laptop", "kind": "owner" }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let plaintext = v["plaintext"].as_str().unwrap();
    // The prefix is the point: a string found in a config file says which of
    // the three kinds of credential it is.
    assert!(plaintext.starts_with("lmgw-owner-"), "{v}");
    assert_eq!(plaintext.len(), "lmgw-owner-".len() + 64, "{v}");
    // The `owner:` prefix is the server's to write.
    assert_eq!(v["name"], json!("owner:laptop"), "{v}");

    let row = listed(&gw, "owner:laptop").await;
    assert_eq!(row["kind"], json!("owner"), "{row}");
    assert_eq!(row["enabled"], json!(true), "{row}");
    // The list never carries a plaintext, whatever the row's kind.
    assert!(
        row.get("key").is_none() && row.get("plaintext").is_none(),
        "{row}"
    );

    // And it authenticates: every enabled owner row holds everything.
    let (status, _) = status_as(&bearer(plaintext), &gw).await;
    assert_eq!(status, 200);

    // A second row of that name is refused rather than rewriting the first.
    let (status, v) = op(
        &gw,
        "key_create",
        json!({ "name": "laptop", "kind": "owner" }),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("already exists"),
        "{v}"
    );
    // Including the two the seed owns.
    for taken in ["dashboard", "self-admin"] {
        let (status, v) = op(&gw, "key_create", json!({ "name": taken, "kind": "owner" })).await;
        assert_eq!(status, 400, "{v}");
    }
    // An empty name is refused before anything is minted.
    let (status, v) = op(&gw, "key_create", json!({ "name": "  ", "kind": "owner" })).await;
    assert_eq!(status, 400, "{v}");
}

/// The word the dashboard sends for the other kind, and the column's own
/// spelling, both mean a client key; an absent `kind` is what every caller
/// predating the owner principal sends.
#[tokio::test]
async fn the_client_kind_answers_to_both_its_names_and_to_none() {
    let (_st, gw) = gateway().await;

    for (name, args) in [
        ("plain", json!({ "name": "plain" })),
        ("worded", json!({ "name": "worded", "kind": "client" })),
        ("columned", json!({ "name": "columned", "kind": "key" })),
    ] {
        let (status, v) = op(&gw, "key_create", args).await;
        assert_eq!(status, 200, "{name}: {v}");
        assert!(v["plaintext"].as_str().unwrap().starts_with("lmgw-"), "{v}");
        assert!(
            !v["plaintext"].as_str().unwrap().starts_with("lmgw-owner-"),
            "{v}"
        );
        assert_eq!(listed(&gw, name).await["kind"], json!("key"), "{name}");
    }

    let (status, v) = op(&gw, "key_create", json!({ "name": "x", "kind": "wizard" })).await;
    assert_eq!(status, 400, "{v}");
    assert!(v["message"].as_str().unwrap().contains("wizard"), "{v}");
}

#[tokio::test]
async fn reveal_hands_back_an_owner_key_and_nothing_else() {
    let (_st, gw) = gateway().await;
    let (id, plaintext) = make_owner(&gw, "laptop").await;

    let (status, v) = op(&gw, "key_reveal", json!({ "id": id })).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["key"], json!(plaintext), "{v}");

    // The seeded rows too — this is how the Keys page's Copy works.
    let dash = listed(&gw, OWNER_DASHBOARD).await["id"].as_i64().unwrap();
    let (status, v) = op(&gw, "key_reveal", json!({ "id": dash })).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["key"], json!(gw.key), "{v}");

    // A client key is hashed and shown once, so there is nothing stored to
    // reveal — and saying so is not the same as pretending the row is missing.
    let (_, created) = op(&gw, "key_create", json!({ "name": "kilo" })).await;
    let client_id = created["id"].as_i64().unwrap();
    let (status, v) = op(&gw, "key_reveal", json!({ "id": client_id })).await;
    assert_eq!(status, 400, "{v}");
    assert!(v["message"].as_str().unwrap().contains("client key"), "{v}");

    let (status, v) = op(&gw, "key_reveal", json!({ "id": 9_999 })).await;
    assert_eq!(status, 400, "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("no key with id"),
        "{v}"
    );
}

#[tokio::test]
async fn rotating_the_door_replaces_every_session_including_the_caller_s() {
    let (_st, gw) = gateway().await;
    let old = gw.key.clone();
    let dash = listed(&gw, OWNER_DASHBOARD).await["id"].as_i64().unwrap();

    let (status, v) = op(&gw, "key_rotate", json!({ "id": dash })).await;
    assert_eq!(status, 200, "{v}");
    let new = v["key"].as_str().unwrap().to_string();
    assert!(new.starts_with("lmgw-owner-"), "{v}");
    assert_ne!(new, old, "{v}");

    // One write for hash and plaintext: the old value is dead the moment the
    // new one is live, and `key_reveal` agrees with what rotate returned.
    let (status, body) = status_as(&bearer(&old), &gw).await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(body["code"], json!("session_required"), "{body}");

    let (status, _) = status_as(&bearer(&new), &gw).await;
    assert_eq!(status, 200);

    let fresh = Gw {
        base: gw.base.clone(),
        key: new.clone(),
    };
    let (status, v) = op(&fresh, "key_reveal", json!({ "id": dash })).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["key"], json!(new), "{v}");

    // A client row has no plaintext to replace: rotation is not a way to
    // re-mint one behind the same id.
    let (_, created) = op(&fresh, "key_create", json!({ "name": "kilo" })).await;
    let (status, v) = op(
        &fresh,
        "key_rotate",
        json!({ "id": created["id"].as_i64().unwrap() }),
    )
    .await;
    assert_eq!(status, 400, "{v}");
}

/// Rotating a disabled row re-mints it and leaves it disabled — the self-admin
/// plane does not open itself by being rotated (§3.7).
#[tokio::test]
async fn rotation_carries_the_enabled_flag_across() {
    let (_st, gw) = gateway().await;
    let row = listed(&gw, "owner:self-admin").await;
    assert_eq!(row["enabled"], json!(false), "{row}");

    let (status, v) = op(&gw, "key_rotate", json!({ "id": row["id"] })).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        listed(&gw, "owner:self-admin").await["enabled"],
        json!(false)
    );
}

#[tokio::test]
async fn an_owner_row_owns_two_fields_and_the_door_owns_one() {
    let (_st, gw) = gateway().await;
    let (id, plaintext) = make_owner(&gw, "laptop").await;
    let dash = listed(&gw, OWNER_DASHBOARD).await["id"].as_i64().unwrap();

    // The note is the owner's to write.
    let (status, v) = op(&gw, "key_set", json!({ "id": id, "note": "the LAN box" })).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        listed(&gw, "owner:laptop").await["note"],
        json!("the LAN box")
    );
    let (status, v) = op(&gw, "key_set", json!({ "id": dash, "note": "this tab" })).await;
    assert_eq!(status, 200, "{v}");

    // The client-shaped half of the dialog is refused, field by field, with
    // its own code — including on the door.
    for patch in [
        json!({ "id": id, "rpm_limit": 10 }),
        json!({ "id": id, "tpm_limit": 10 }),
        json!({ "id": id, "concurrency_limit": 2 }),
        json!({ "id": id, "budget_micro": 1_000_000 }),
        json!({ "id": id, "budget_period": "day" }),
        json!({ "id": id, "scope_mode": "allow" }),
        json!({ "id": id, "scope_patterns": "gpt-*" }),
        json!({ "id": id, "tool_scope_mode": "allow" }),
        json!({ "id": id, "tool_scope_patterns": "docs__*" }),
        json!({ "id": id, "expires_at": "2027-01-01" }),
        json!({ "id": dash, "rpm_limit": 10 }),
    ] {
        let (status, v) = op(&gw, "key_set", patch.clone()).await;
        assert_eq!(status, 400, "{patch}: {v}");
        assert_eq!(v["code"], json!("refuse_owner"), "{patch}: {v}");
        assert_eq!(
            v["message"],
            json!("an owner key is not a client; it is not scoped, budgeted or rate-limited"),
            "{patch}: {v}"
        );
    }

    // Restating what the row already holds is not an attempt to change it —
    // the dialog posts a whole form, and a refusal has to mean something.
    let (status, v) = op(
        &gw,
        "key_set",
        json!({ "id": id, "rpm_limit": 0, "scope_mode": "all", "note": "the LAN box" }),
    )
    .await;
    assert_eq!(status, 200, "{v}");

    // The door keeps its enabled flag; every other owner row gives it up.
    let (status, v) = op(&gw, "key_set", json!({ "id": dash, "enabled": false })).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["code"], json!("refuse_owner"), "{v}");
    assert_eq!(
        v["message"],
        json!("the dashboard key is the door; rotate it instead"),
        "{v}"
    );
    assert_eq!(listed(&gw, OWNER_DASHBOARD).await["enabled"], json!(true));

    let (status, v) = op(&gw, "key_set", json!({ "id": id, "enabled": false })).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(listed(&gw, "owner:laptop").await["enabled"], json!(false));
    // A disabled owner row names itself rather than reading as a typo.
    let (status, body) = status_as(&bearer(&plaintext), &gw).await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(body["code"], json!("owner_key_disabled"), "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("laptop"),
        "{body}"
    );
}

#[tokio::test]
async fn the_door_cannot_be_deleted_and_the_others_can() {
    let (_st, gw) = gateway().await;
    let (id, _) = make_owner(&gw, "laptop").await;
    let dash = listed(&gw, OWNER_DASHBOARD).await["id"].as_i64().unwrap();

    let (status, v) = op(&gw, "key_delete", json!({ "id": dash })).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["code"], json!("refuse_owner"), "{v}");
    assert_eq!(
        v["message"],
        json!("the dashboard key is the door; rotate it instead"),
        "{v}"
    );
    // And it is still there, and still the session this test is holding.
    let (status, _) = status_as(&gw.client(), &gw).await;
    assert_eq!(status, 200);

    let (status, v) = op(&gw, "key_delete", json!({ "id": id })).await;
    assert_eq!(status, 200, "{v}");
    let (status, v) = op(&gw, "key_reveal", json!({ "id": id })).await;
    assert_eq!(status, 400, "{v}");

    // `owner:self-admin` is deletable on purpose: the next start seeds it
    // again, disabled, which is exactly what §3.7 means by "closed".
    let self_admin = listed(&gw, "owner:self-admin").await["id"]
        .as_i64()
        .unwrap();
    let (status, v) = op(&gw, "key_delete", json!({ "id": self_admin })).await;
    assert_eq!(status, 200, "{v}");
}
