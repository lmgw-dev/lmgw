//! Keys and accounts (§5.1): owner keys belong to the account that made
//! them, door keys to their account, and "made by" outlives its account.

use serde_json::json;

use super::*;
use lmgw_core::state::AppState;

async fn keys(gw: &Gw, session: &str) -> Vec<Value> {
    let (status, body) = get_as(gw, session, "/api/usage/keys").await;
    assert_eq!(status, 200, "{body}");
    body["keys"].as_array().unwrap().clone()
}

#[tokio::test]
async fn owner_keys_attach_to_their_maker_and_door_keys_stay_their_account_s() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let mine = turn_on(&gw).await;
    let (sam, sams) = account_with_password(&gw, "sam").await;
    let sam_door = state.snapshot().account(sam).unwrap().door_key_id;
    assert_eq!(
        state
            .snapshot()
            .api_keys
            .iter()
            .find(|k| k.id == sam_door)
            .unwrap()
            .name,
        format!("owner:account-{sam}"),
        "door keys are named by id"
    );

    // Sam's owner key belongs to sam.
    let (status, made) = op_as(
        &gw,
        &sams,
        "key_create",
        json!({ "name": "script", "kind": "owner" }),
    )
    .await;
    assert_eq!(status, 200, "{made}");
    let k = state
        .snapshot()
        .api_keys
        .iter()
        .find(|k| k.name == "owner:script")
        .cloned()
        .unwrap();
    assert_eq!(k.account_id, Some(sam));
    // The reserved prefix is the server's.
    let (status, body) = op_as(
        &gw,
        &sams,
        "key_create",
        json!({ "name": "account-9", "kind": "owner" }),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    // Another account's door key: no reveal, rotate or set; no delete for
    // any door key.
    for name in ["key_reveal", "key_rotate", "key_set"] {
        let (status, body) = op_as(&gw, &mine, name, json!({ "id": sam_door })).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (409, Some("door_key")),
            "{name}: {body}"
        );
        assert!(body["message"].as_str().unwrap().contains("account 'sam'"));
    }
    assert_eq!(
        op_as(&gw, &sams, "key_reveal", json!({ "id": sam_door }))
            .await
            .0,
        200
    );
    let (status, body) = op_as(&gw, &sams, "key_delete", json!({ "id": sam_door })).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("door_key")),
        "{body}"
    );
    // Any other owner key stays every account's (ruling 6).
    assert_eq!(
        op_as(&gw, &mine, "key_reveal", json!({ "id": k.id }))
            .await
            .0,
        200
    );

    // A client key sam made reads "made by sam", and after sam's delete
    // "made by a deleted account (sam)", and keeps working.
    let (_, client) = op_as(&gw, &sams, "key_create", json!({ "name": "ci" })).await;
    let row = keys(&gw, &mine)
        .await
        .into_iter()
        .find(|r| r["name"] == "ci")
        .unwrap();
    assert_eq!(row["made_by"], "sam");
    let door = keys(&gw, &mine)
        .await
        .into_iter()
        .find(|r| r["id"] == sam_door)
        .unwrap();
    assert_eq!(door["door_of"], "sam");
    assert_eq!(door["yours"], false);
    // Sam cannot delete itself; account 1 can delete sam.
    assert_eq!(
        op_as(&gw, &sams, "account_delete", json!({ "id": sam }))
            .await
            .0,
        409
    );
    assert_eq!(
        op_as(&gw, &mine, "account_delete", json!({ "id": 1 }))
            .await
            .0,
        409
    );
    assert_eq!(
        op_as(&gw, &mine, "account_delete", json!({ "id": sam }))
            .await
            .0,
        200
    );
    let row = keys(&gw, &mine)
        .await
        .into_iter()
        .find(|r| r["name"] == "ci")
        .unwrap();
    assert_eq!(row["made_by"], "a deleted account (sam)");
    assert!(
        !state
            .snapshot()
            .api_keys
            .iter()
            .any(|k| k.account_id == Some(sam) && k.kind.as_str() == "owner"),
        "its owner keys went with it"
    );
    let resp = browser()
        .get(format!("{gw}/v1/models"))
        .bearer_auth(client["key"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(get_as(&gw, &sams, "/api/status").await.0, 401);
}

/// A credential that belongs to no account (an owner row the seed has not
/// attached) acts as no account: what needs one refuses, never account 1.
#[tokio::test]
async fn a_credential_of_no_account_is_refused_what_needs_one() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let key = format!("lmgw-owner-{}", "ab".repeat(32));
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, key_plain, enabled, kind)
              VALUES ('owner:loose', ?1, ?2, 1, 'owner')",
    )
    .bind(lmgw_core::config::hash_api_key(&key))
    .bind(&key)
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let call = |name: &'static str, args: Value| {
        let (gw, key) = (gw.clone(), key.clone());
        async move {
            let resp = browser()
                .post(format!("{gw}/api/op/{name}"))
                .bearer_auth(&key)
                .json(&args)
                .send()
                .await
                .unwrap();
            let status = resp.status().as_u16();
            (status, resp.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    let (status, body) = call(
        "account_password_set",
        json!({ "password": "a long enough password" }),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (403, Some("no_account")),
        "{body}"
    );
    let (status, body) = call("accounts_switch", json!({ "enabled": false })).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (403, Some("no_account")),
        "{body}"
    );
    // Nothing it lists is "you".
    let (_, list) = call("account_list", json!({})).await;
    assert!(list["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|a| a["you"] == false));
}
