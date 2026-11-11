//! A desktop with accounts off writes nothing; the switch is the log's
//! first entry going on and its last going off (§5.11).

use super::*;
use lmgw_core::state::AppState;

#[tokio::test]
async fn nothing_is_written_while_accounts_are_off_but_the_switch_both_ways() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let (status, made) = op(&gw, "key_create", json!({ "name": "spike-wp7-off" })).await;
    assert_eq!(status, 200, "{made}");
    assert_eq!(count(&state).await, 0);

    turn_on(&gw).await;
    assert_eq!(count(&state).await, 1);
    let (status, _) = op(&gw, "accounts_switch", json!({ "enabled": false })).await;
    assert_eq!(status, 200);
    assert_eq!(count(&state).await, 2);
    let (status, _) = op(&gw, "key_delete", json!({ "id": made["id"] })).await;
    assert_eq!(status, 200);
    assert_eq!(count(&state).await, 2, "accounts are off again");

    // The kept log is readable while off, its last entry the switch.
    let e = entries(&gw, 50).await;
    assert_eq!(e[0]["action"], "accounts_switch");
    assert_eq!(e[1]["action"], "accounts_switch");
    let (_, page) = op(&gw, "audit_list", json!({})).await;
    assert_eq!(page["total"], 2);
    assert!(page["bytes"].as_u64().unwrap() > 0, "{page}");
}
