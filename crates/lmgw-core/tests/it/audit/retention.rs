//! Retention (§5.8, ruling 18): a change of it is written before it
//! prunes, so a shorter retention leaves its own entry behind.

use super::*;
use lmgw_core::state::AppState;

#[tokio::test]
async fn a_shorter_retention_is_written_before_it_removes_the_old_entries() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let session = turn_on(&gw).await;
    // Entries from long ago, as a log kept with "keep every entry" has.
    for ts in ["2020-01-01T00:00:00Z", "2021-06-01T00:00:00Z"] {
        sqlx::query(
            "INSERT INTO audit_log (ts, principal, via, peer, action, outcome)
             VALUES (?1, 'owner, key owner:dashboard', 'api', '192.0.2.10', 'key_create', 200)",
        )
        .bind(ts)
        .execute(&state.db)
        .await
        .unwrap();
    }
    assert_eq!(count(&state).await, 3);

    let (status, body) = op_as(
        &gw,
        &session,
        "settings_set_full",
        json!({ "accounts": { "audit_days": 30 } }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let e = entries(&gw, 50).await;
    let actions: Vec<&str> = e.iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert_eq!(actions, ["settings_set_full", "accounts_switch"]);
    assert_eq!(
        e[0]["target"]["fields"],
        json!({ "accounts.audit_days": 30 })
    );
    let (_, page) = op(&gw, "audit_list", json!({})).await;
    assert_eq!(page["keep_days"], 30);
    assert_eq!(page["total"], 2);

    // Out of range is refused by name.
    let (status, body) = op_as(
        &gw,
        &session,
        "settings_set_full",
        json!({ "accounts": { "audit_days": 3651 } }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("Keep audit entries"),
        "{body}"
    );
}

#[tokio::test]
async fn the_card_pages_through_the_log() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    turn_on(&gw).await;
    for n in 0..4 {
        let (status, _) = op(
            &gw,
            "key_create",
            json!({ "name": format!("spike-wp7-{n}") }),
        )
        .await;
        assert_eq!(status, 200);
    }
    let (_, first) = op(&gw, "audit_list", json!({ "count": 3 })).await;
    assert_eq!(first["entries"].as_array().unwrap().len(), 3);
    assert_eq!(first["more"], true);
    assert_eq!(first["total"], 5);
    let last = first["entries"][2]["id"].clone();
    let (_, second) = op(&gw, "audit_list", json!({ "count": 3, "before": last })).await;
    assert_eq!(second["entries"].as_array().unwrap().len(), 2);
    assert_eq!(second["more"], false);
    assert_eq!(second["entries"][1]["action"], "accounts_switch");
    assert_eq!(op(&gw, "audit_list", json!({ "count": 0 })).await.0, 400);
}
