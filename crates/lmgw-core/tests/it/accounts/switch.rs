//! The desktop's switch (§5.11, D29), and container mode, where accounts
//! are always on.

use serde_json::json;

use super::*;
use lmgw_core::config::deployment::{Deployment, StartFacts};
use lmgw_core::state::AppState;

#[tokio::test]
async fn on_hands_the_caller_a_session_and_other_owner_key_cookies_read_as_nobody() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    // A browser signed in before, with today's cookie.
    let (status, _) = get_as(&gw, &gw.key, "/api/status").await;
    assert_eq!(status, 200);

    let session = turn_on(&gw).await;
    assert!(session.starts_with("lmgw-session-"), "{session}");
    assert!(state.snapshot().accounts_on);
    let (status, me) = get_as(&gw, &session, "/api/session").await;
    assert_eq!(status, 200);
    assert_eq!(me["account"]["login"], "owner");
    assert_eq!(me["account"]["has_password"], false);
    assert!(me["expires_at"].is_string());

    // The owner-key cookie now reads as nobody.
    let (status, body) = get_as(&gw, &gw.key, "/api/status").await;
    assert_eq!(
        (status, body["code"].as_str()),
        (401, Some("session_required"))
    );
    // A bearer still works (the CLI's and scripts' path).
    assert_eq!(op(&gw, "account_list", json!({})).await.0, 200);
    // The banner's fact.
    let full: Value = gw
        .client()
        .get(format!("{gw}/api/settings-full"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(full["accounts_status"]["door_without_password"], true);
    assert_eq!(full["accounts_status"]["on"], true);
}

#[tokio::test]
async fn account_one_sets_its_password_and_login_without_a_current_one() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let session = turn_on(&gw).await;
    let (status, body) = op_as(
        &gw,
        &session,
        "account_password_set",
        json!({ "password": PASSWORD, "login": "Admin" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let one = state.snapshot().account(1).cloned().unwrap();
    assert_eq!(one.login, "admin");
    assert!(one.has_password);
    // The login changes only with the first password.
    let (status, body) = op_as(
        &gw,
        &session,
        "account_password_set",
        json!({ "current": PASSWORD, "password": "another long password", "login": "boss" }),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("login_fixed")),
        "{body}"
    );
    // Its own session survived its own change.
    assert_eq!(get_as(&gw, &session, "/api/status").await.0, 200);
    let (status, ..) = sign_in(&gw, "admin", PASSWORD).await;
    assert_eq!(status, 204);
}

#[tokio::test]
async fn off_ends_sessions_pauses_other_accounts_keys_and_keeps_everything_else() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let mine = turn_on(&gw).await;
    let (sam, sams) = account_with_password(&gw, "sam").await;
    let sam_door = state.snapshot().account(sam).unwrap().door_key_id;
    let sam_key = state
        .snapshot()
        .api_keys
        .iter()
        .find(|k| k.id == sam_door)
        .and_then(|k| k.key_plain.as_ref().map(|s| s.expose().to_string()))
        .unwrap();
    // A client key sam made keeps working through all of it.
    let (status, made) = op_as(&gw, &sams, "key_create", json!({ "name": "ci-bot" })).await;
    assert_eq!(status, 200, "{made}");
    let client_key = made["key"].as_str().unwrap().to_string();
    let accounts_before = rows(&state, "accounts").await;
    let known_before = rows(&state, "known_browsers").await;
    let (_, link) = op(&gw, "account_password_link", json!({ "id": sam })).await;

    // Off, from account 1's browser.
    let resp = browser()
        .post(format!("{gw}/api/op/accounts_switch"))
        .header("cookie", format!("lmgw_session={mine}"))
        .json(&json!({ "enabled": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // Account 1's browser gets today's door-key cookie.
    assert_eq!(session_of(&resp).as_deref(), Some(gw.key.as_str()));
    let done: Value = resp.json().await.unwrap();
    assert!(
        done["message"].as_str().unwrap().contains("paused"),
        "{done}"
    );

    assert_eq!(rows(&state, "sessions").await, 0);
    assert_eq!(rows(&state, "accounts").await, accounts_before);
    assert_eq!(rows(&state, "known_browsers").await, known_before);
    for s in [&mine, &sams] {
        assert_eq!(get_as(&gw, s, "/api/status").await.0, 401);
    }
    // Sam's door key is paused, naming the account; account 1's works.
    let resp = browser()
        .get(format!("{gw}/api/status"))
        .bearer_auth(&sam_key)
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "key_paused", "{body}");
    assert!(body["message"].as_str().unwrap().contains("'sam'"));
    assert_eq!(op(&gw, "hold_set", json!({ "active": false })).await.0, 200);
    let ok = browser()
        .get(format!("{gw}/v1/models"))
        .bearer_auth(&client_key)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200, "a client key sam made keeps working");
    // A set-password link is refused while off.
    let token = link["url"]
        .as_str()
        .unwrap()
        .split_once("#token=")
        .unwrap()
        .1;
    let resp = browser()
        .post(format!("{gw}/api/account/password"))
        .json(&json!({ "token": token, "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    // On again: sam's key works, the old password verifies, no old session.
    let _ = turn_on(&gw).await;
    let resp = browser()
        .get(format!("{gw}/api/status"))
        .bearer_auth(&sam_key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(sign_in(&gw, "sam", PASSWORD).await.0, 204);
    assert_eq!(get_as(&gw, &sams, "/api/status").await.0, 401);
}

#[tokio::test]
async fn another_account_switching_off_is_signed_out() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (_, sams) = account_with_password(&gw, "sam").await;
    let resp = browser()
        .post(format!("{gw}/api/op/accounts_switch"))
        .header("cookie", format!("lmgw_session={sams}"))
        .json(&json!({ "enabled": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(session_of(&resp).as_deref(), Some(""), "cleared");
}

#[tokio::test]
async fn container_mode_has_accounts_always_on_and_no_switch() {
    let state = AppState::init_for_tests().await.unwrap();
    state.set_start_for_tests(StartFacts {
        deployment: Deployment::Container,
        ..Default::default()
    });
    assert!(state.snapshot().accounts_on);
    let gw = serve(state.clone()).await;
    for on in [true, false] {
        let (status, body) = op(&gw, "accounts_switch", json!({ "enabled": on })).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (409, Some("accounts_always_on"))
        );
    }
    assert_eq!(op(&gw, "account_list", json!({})).await.0, 200);

    // Ruling 15's lines: a set-password link while account 1 has none, a
    // fresh one each start, replacing the last.
    lmgw_core::accounts::start_lines(&state, "https://lmgw.example").await;
    let first: String = sqlx::query_scalar("SELECT token_hash FROM account_tokens")
        .fetch_one(&state.db)
        .await
        .unwrap();
    lmgw_core::accounts::start_lines(&state, "https://lmgw.example").await;
    let second: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM account_tokens")
        .fetch_all(&state.db)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_ne!(second[0], first);
    // With a password, none.
    sqlx::query("DELETE FROM account_tokens")
        .execute(&state.db)
        .await
        .unwrap();
    let (status, body) = op(
        &gw,
        "account_password_set",
        json!({ "id": 1, "password": PASSWORD }),
    )
    .await;
    assert_eq!(
        status, 200,
        "plain http from this machine takes a password: {body}"
    );
    lmgw_core::accounts::start_lines(&state, "https://lmgw.example").await;
    assert_eq!(rows(&state, "account_tokens").await, 0);
}
