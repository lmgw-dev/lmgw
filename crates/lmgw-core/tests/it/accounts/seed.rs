//! Account 1 and the owner rows (§5.1): made by the owner-key seed, on a
//! fresh database and on one from before accounts.

use lmgw_core::agents::token::{seed_owner_keys, OWNER_DASHBOARD, OWNER_SELF_ADMIN};
use lmgw_core::state::AppState;
use serde_json::json;

use super::{op, serve, turn_on};

#[tokio::test]
async fn a_fresh_database_gets_account_one_with_the_dashboard_key_as_its_door() {
    let state = AppState::init_for_tests().await.unwrap();
    let snap = state.snapshot();
    let one = snap.account(1).expect("account 1");
    assert_eq!(one.login, "owner");
    assert!(!one.has_password);
    let door = snap
        .api_keys
        .iter()
        .find(|k| k.id == one.door_key_id)
        .unwrap();
    assert_eq!(door.name, OWNER_DASHBOARD);
    for name in [OWNER_DASHBOARD, OWNER_SELF_ADMIN] {
        let k = snap.api_keys.iter().find(|k| k.name == name).unwrap();
        assert_eq!(k.account_id, Some(1), "{name}");
    }
    // Accounts stay off on a desktop by default (ruling 17).
    assert!(!snap.accounts_on);
}

#[tokio::test]
async fn owner_rows_without_an_account_are_attached_and_a_reseeded_self_admin_too() {
    let state = AppState::init_for_tests().await.unwrap();
    // A database from before accounts: owner rows with no account.
    sqlx::query("UPDATE api_keys SET account_id = NULL WHERE kind = 'owner'")
        .execute(&state.db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM api_keys WHERE name = ?1")
        .bind(OWNER_SELF_ADMIN)
        .execute(&state.db)
        .await
        .unwrap();
    seed_owner_keys(&state).await.unwrap();
    let snap = state.snapshot();
    assert_eq!(
        snap.accounts.len(),
        1,
        "seeding again makes no second account"
    );
    for k in snap.api_keys.iter().filter(|k| k.kind.as_str() == "owner") {
        assert_eq!(k.account_id, Some(1), "{}", k.name);
    }
    assert!(snap.api_keys.iter().any(|k| k.name == OWNER_SELF_ADMIN));
}

#[tokio::test]
async fn a_deleted_account_s_id_is_never_handed_out_again() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let made = |login: &'static str| {
        let gw = gw.clone();
        async move {
            let (status, link) = op(&gw, "account_create", json!({ "login": login })).await;
            assert_eq!(status, 200, "{link}");
            link["account_id"].as_i64().unwrap()
        }
    };
    let _ = made("kim").await;
    let newest = made("sam").await;
    assert_eq!(
        op(&gw, "account_delete", json!({ "id": newest })).await.0,
        200
    );
    let next = made("lee").await;
    assert!(next > newest, "id {newest} came back as {next}");
}
