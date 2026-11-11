//! The desktop with accounts off (ruling 17): nothing changed. The tray's
//! nonce setting the `owner:dashboard` cookie is
//! `session_login::a_nonce_is_exchanged_exactly_once`, and the start's
//! login line `main_tls::scheme::the_login_line_names_the_bound_listener`;
//! both run with the switch at its default.

use serde_json::json;

use super::*;
use lmgw_core::state::AppState;

#[tokio::test]
async fn with_the_switch_at_its_default_the_desktop_is_today_s() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;

    // `GET /api/session` says accounts are off and carries no account.
    let s: Value = gw
        .client()
        .get(format!("{gw}/api/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["accounts_on"], false);
    assert!(s["account"].is_null(), "{s}");

    // The key's sign-in sets today's cookie, the key itself, and no
    // sessions row.
    let resp = browser()
        .post(format!("{gw}/api/session"))
        .json(&json!({ "token": gw.key }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_eq!(session_of(&resp).as_deref(), Some(gw.key.as_str()));
    let (status, _) = get_as(&gw, &gw.key, "/api/status").await;
    assert_eq!(status, 200, "an owner-key cookie is honoured");
    assert_eq!(rows(&state, "sessions").await, 0);

    // A password sign-in is not there to take.
    let (status, body, _) = sign_in(&gw, "owner", PASSWORD).await;
    assert_eq!((status, body["code"].as_str()), (409, Some("accounts_off")));

    // Every account op but the switch answers 409 accounts_off.
    for name in [
        "account_list",
        "account_create",
        "account_set",
        "account_delete",
        "account_password_link",
        "account_password_set",
        "account_sessions",
        "session_end",
        "account_login_link",
    ] {
        let (status, body) = op(&gw, name, json!({ "id": 1 })).await;
        assert_eq!(status, 409, "{name}: {body}");
        assert_eq!(body["code"], "accounts_off", "{name}");
    }
    // Require API key stays off.
    assert!(!state.snapshot().settings.auth_enabled);

    // A key made here reads no "made by": the desktop has one owner.
    let (status, body) = op(&gw, "key_create", json!({ "name": "ci" })).await;
    assert_eq!(status, 200, "{body}");
    let (status, keys) = get_as(&gw, &gw.key, "/api/usage/keys").await;
    assert_eq!(status, 200, "{keys}");
    let row = keys["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["name"] == "ci")
        .unwrap()
        .clone();
    assert!(row["made_by"].is_null(), "{row}");
}

#[tokio::test]
async fn every_settings_save_path_refuses_the_switch_s_key() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    for name in ["settings_set", "settings_set_full"] {
        let (status, body) = op(&gw, name, json!({ "accounts": { "enabled": true } })).await;
        assert_eq!(status, 400, "{name}: {body}");
        assert_eq!(body["code"], "accounts_switch_only", "{name}: {body}");
        assert!(body["message"]
            .as_str()
            .unwrap()
            .contains("accounts_switch"));
    }
    assert!(!state.snapshot().settings.accounts.enabled);
    // The lifetime is an ordinary setting.
    let (status, body) = op(
        &gw,
        "settings_set",
        json!({ "accounts": { "session_days": 7 } }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(state.snapshot().settings.accounts.session_days, 7);
}
