//! The gate's entries and the self-admin dispatcher's, with who, how and
//! what.

use super::*;
use lmgw_core::config::SelfAdmin;
use lmgw_core::state::AppState;

#[tokio::test]
async fn an_admin_write_names_its_account_its_way_in_and_its_row() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let session = turn_on(&gw).await;

    // The log starts with the switch's own entry (§5.11).
    let first = entries(&gw, 50).await;
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0]["action"], "accounts_switch");
    assert_eq!(first[0]["account_id"], 1);
    assert_eq!(first[0]["via"], "api");
    assert_eq!(first[0]["outcome"], 200);

    // A key made and deleted from a signed-in browser.
    let (status, made) = op_as(
        &gw,
        &session,
        "key_create",
        json!({ "name": "spike-wp7-key", "kind": "client" }),
    )
    .await;
    assert_eq!(status, 200, "{made}");
    let id = made["id"].as_i64().unwrap();
    let (status, _) = op_as(&gw, &session, "key_delete", json!({ "id": id })).await;
    assert_eq!(status, 200);
    // A read is not an entry.
    assert_eq!(op_as(&gw, &session, "account_list", json!({})).await.0, 200);

    let e = entries(&gw, 50).await;
    assert_eq!(e.len(), 3, "{e:?}");
    let (deleted, created) = (&e[0], &e[1]);
    assert_eq!(created["action"], "key_create");
    assert_eq!(
        created["target"],
        json!({ "name": "spike-wp7-key", "kind": "client" })
    );
    assert_eq!(created["account_id"], 1);
    assert!(
        created["principal"]
            .as_str()
            .unwrap()
            .starts_with("owner, session #"),
        "{created}"
    );
    assert_eq!(created["via"], "dashboard");
    assert_eq!(created["peer"], "127.0.0.1");
    assert_eq!(deleted["action"], "key_delete");
    assert_eq!(deleted["target"], json!({ "id": id }));
    // The key itself is in no entry.
    let key = made["key"].as_str().unwrap();
    assert!(!super::whole_log(&state).await.contains(key));
}

#[tokio::test]
async fn a_bearer_is_the_api_the_command_line_says_cli_and_a_refusal_keeps_its_status() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    turn_on(&gw).await;

    // The dashboard's key as a bearer.
    assert_eq!(
        op(&gw, "account_create", json!({ "login": "sam" })).await.0,
        200
    );
    // The command line's claim.
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/account_delete"))
        .header("x-lmgw-client", "cli")
        .json(&json!({ "id": 1 }))
        .send()
        .await
        .unwrap();
    let refused = resp.status().as_u16();
    assert!(refused >= 400, "account 1 cannot be deleted");

    let e = entries(&gw, 50).await;
    assert_eq!(e[1]["action"], "account_create");
    assert_eq!(e[1]["via"], "api");
    assert_eq!(e[1]["principal"], "owner, key owner:dashboard");
    assert_eq!(e[1]["target"], json!({ "login": "sam" }));
    assert_eq!(e[0]["action"], "account_delete");
    assert_eq!(e[0]["via"], "cli");
    assert_eq!(e[0]["outcome"], refused);
}

#[tokio::test]
async fn a_settings_save_names_the_fields_it_changed_with_their_values() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let session = turn_on(&gw).await;
    let (status, body) = op_as(
        &gw,
        &session,
        "settings_set_full",
        json!({ "accounts": { "session_days": 7 }, "hf_token": "hf_spike_wp7_0000" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let e = entries(&gw, 1).await;
    assert_eq!(e[0]["action"], "settings_set_full");
    assert_eq!(
        e[0]["target"]["fields"],
        json!({ "accounts.session_days": 7, "hf_token": "changed" })
    );
    assert!(!super::whole_log(&state).await.contains("hf_spike_wp7_0000"));
}

#[tokio::test]
async fn a_writing_tool_call_is_written_by_its_name_and_a_reading_one_is_not() {
    let state = AppState::init_for_tests().await.unwrap();
    with_settings(&state, |s| s.self_admin = SelfAdmin::Full).await;
    let gw = serve(state.clone()).await;
    turn_on(&gw).await;
    let before = count(&state).await;
    assert!(!mcp_call(&gw, "lmgw__status", json!({})).await);
    assert_eq!(count(&state).await, before, "a read is not an entry");
    assert!(!mcp_call(&gw, "lmgw__hold_set", json!({ "active": false })).await);
    let e = entries(&gw, 1).await;
    assert_eq!(e[0]["action"], "lmgw__hold_set");
    assert_eq!(e[0]["via"], "mcp");
    assert_eq!(e[0]["account_id"], 1);
    assert_eq!(e[0]["outcome"], 200);
    // `/mcp/admin` itself is not the gate's to write.
    assert_eq!(count(&state).await, before + 1);
}
