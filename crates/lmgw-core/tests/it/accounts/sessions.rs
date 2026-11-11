//! Sessions (§5.2, ruling 19): their cookie, their lifetime, and what ends
//! them, with the streams they opened.

use std::time::Duration;

use serde_json::json;

use super::*;
use lmgw_core::state::AppState;

#[tokio::test]
async fn a_session_cookie_lasts_session_days_and_sign_out_deletes_its_row() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (status, _) = op(
        &gw,
        "settings_set",
        json!({ "accounts": { "session_days": 3 } }),
    )
    .await;
    assert_eq!(status, 200);
    let _ = account_with_password(&gw, "sam").await;
    let resp = browser()
        .post(format!("{gw}/api/session"))
        .json(&json!({ "login": "sam", "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    let cookie = set_cookies(&resp)
        .into_iter()
        .find(|(n, ..)| n == "lmgw_session")
        .unwrap();
    assert!(cookie.2.contains("Max-Age=259200"), "{}", cookie.2);
    assert!(cookie.2.contains("HttpOnly") && cookie.2.contains("SameSite=Strict"));
    assert!(!cookie.2.contains("Secure"), "plain http carries no Secure");
    let session = cookie.1;
    let before = rows(&state, "sessions").await;
    let resp = browser()
        .delete(format!("{gw}/api/session"))
        .header("cookie", format!("lmgw_session={session}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_eq!(rows(&state, "sessions").await, before - 1);
    assert_eq!(get_as(&gw, &session, "/api/status").await.0, 401);
}

#[tokio::test]
async fn sign_out_closes_the_event_stream_it_opened() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let session = turn_on(&gw).await;
    let events = browser()
        .get(format!("{gw}/api/events"))
        .header("cookie", format!("lmgw_session={session}"))
        .send()
        .await
        .unwrap();
    assert_eq!(events.status(), 200);
    let resp = browser()
        .delete(format!("{gw}/api/session"))
        .header("cookie", format!("lmgw_session={session}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let text = text_until_end(events, Duration::from_secs(10))
        .await
        .expect("the stream outlived its session");
    // Its last frame says why (D58), as the dashboard shows it.
    let frame = revoked_frame(&text).unwrap_or_else(|| panic!("no revoked frame: {text}"));
    let message = frame["message"].as_str().unwrap();
    assert!(
        message.contains("session") && message.contains("sign in again"),
        "{frame}"
    );
}

#[tokio::test]
async fn a_disable_ends_the_account_s_sessions_and_streams_and_names_the_account() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (sam, sams) = account_with_password(&gw, "sam").await;
    // An account does not disable itself (another account can).
    let (status, body) = op_as(
        &gw,
        &sams,
        "account_set",
        json!({ "id": sam, "enabled": false }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    let events = browser()
        .get(format!("{gw}/api/events"))
        .header("cookie", format!("lmgw_session={sams}"))
        .send()
        .await
        .unwrap();
    let (status, done) = op(&gw, "account_set", json!({ "id": sam, "enabled": false })).await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["sessions_ended"], 1);
    let text = text_until_end(events, Duration::from_secs(10))
        .await
        .unwrap();
    assert!(revoked_frame(&text).is_some(), "{text}");
    assert_eq!(get_as(&gw, &sams, "/api/status").await.0, 401);
    // Its door key is refused naming the account.
    let door = state.snapshot().account(sam).unwrap().door_key_id;
    let key = state
        .snapshot()
        .api_keys
        .iter()
        .find(|k| k.id == door)
        .and_then(|k| k.key_plain.as_ref().map(|s| s.expose().to_string()))
        .unwrap();
    let body: Value = browser()
        .get(format!("{gw}/api/status"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["code"], "account_disabled");
    assert!(body["message"].as_str().unwrap().contains("'sam'"));
    // Account 1 is not disabled either.
    assert_eq!(
        op(&gw, "account_set", json!({ "id": 1, "enabled": false }))
            .await
            .0,
        409
    );
}

#[tokio::test]
async fn password_changes_end_the_sessions_they_should() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (sam, first) = account_with_password(&gw, "sam").await;
    let (.., second) = sign_in(&gw, "sam", PASSWORD).await;
    let second = second.unwrap();
    // Sam's own change needs the current password and ends the other one.
    let (status, body) = op_as(
        &gw,
        &first,
        "account_password_set",
        json!({ "password": "a new long password" }),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (401, Some("password_wrong")),
        "{body}"
    );
    // A wrong current password counts on the account's name, as a failed
    // sign-in does (§5.4): the next try inside its delay is held.
    let change = json!({ "current": PASSWORD, "password": "a new long password" });
    let held = browser()
        .post(format!("{gw}/api/op/account_password_set"))
        .header("cookie", format!("lmgw_session={first}"))
        .json(&change)
        .send()
        .await
        .unwrap();
    assert_eq!(held.status(), 429);
    assert_eq!(
        held.headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("1"),
        "the wait in the header as the sign-in's 429 has it"
    );
    let body: Value = held.json().await.unwrap();
    assert_eq!(body["code"], "login_throttled", "{body}");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (status, body) = op_as(&gw, &first, "account_password_set", change).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(get_as(&gw, &first, "/api/status").await.0, 200);
    assert_eq!(get_as(&gw, &second, "/api/status").await.0, 401);
    // An admin setting it (ruling 21): no current password, every session ends.
    let (status, _) = op(
        &gw,
        "account_password_set",
        json!({ "id": sam, "password": "set by an admin!" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(get_as(&gw, &first, "/api/status").await.0, 401);
    // Eleven code points are refused, naming the minimum.
    let (status, body) = op(
        &gw,
        "account_password_set",
        json!({ "id": sam, "password": "short pass1" }),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        body["message"].as_str().unwrap().contains("at least 12"),
        "{body}"
    );
}

#[tokio::test]
async fn the_card_lists_and_ends_sessions_and_signs_out_everywhere() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (sam, a) = account_with_password(&gw, "sam").await;
    let (.., b) = sign_in(&gw, "sam", PASSWORD).await;
    let b = b.unwrap();
    let (_, list) = op_as(&gw, &a, "account_sessions", json!({ "id": sam })).await;
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(
        sessions
            .iter()
            .filter(|s| s["this_browser"] == true)
            .count(),
        1
    );
    assert_eq!(sessions[0]["peer"], "127.0.0.1");
    let other = sessions
        .iter()
        .find(|s| s["this_browser"] == false)
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        op_as(&gw, &a, "session_end", json!({ "id": other }))
            .await
            .0,
        200
    );
    assert_eq!(get_as(&gw, &b, "/api/status").await.0, 401);
    assert_eq!(get_as(&gw, &a, "/api/status").await.0, 200);
    let (status, _) = op_as(&gw, &a, "session_end", json!({ "account": sam })).await;
    assert_eq!(status, 200);
    assert_eq!(get_as(&gw, &a, "/api/status").await.0, 401);
}

#[tokio::test]
async fn a_new_sign_in_ends_the_session_the_browser_carried() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (_, first) = account_with_password(&gw, "sam").await;
    let resp = browser()
        .post(format!("{gw}/api/session"))
        .header("cookie", format!("lmgw_session={first}"))
        .json(&json!({ "login": "sam", "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let second = session_of(&resp).unwrap();
    assert_eq!(get_as(&gw, &first, "/api/status").await.0, 401);
    assert_eq!(get_as(&gw, &second, "/api/status").await.0, 200);
}

#[tokio::test]
async fn expired_sessions_are_not_listed_and_go_at_the_next_sign_in() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (sam, _) = account_with_password(&gw, "sam").await;
    sqlx::query(
        "INSERT INTO sessions (token_hash, account_id, last_seen_at, expires_at, peer)
              VALUES ('expired', ?1, '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z', '192.0.2.1')",
    )
    .bind(sam)
    .execute(&state.db)
    .await
    .unwrap();
    let (_, list) = op(&gw, "account_sessions", json!({ "id": sam })).await;
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1, "{list}");
    let (_, accounts) = op(&gw, "account_list", json!({})).await;
    let row = accounts["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == sam)
        .unwrap()
        .clone();
    assert_eq!(row["sessions"], 1, "{row}");
    assert_eq!(sign_in(&gw, "sam", PASSWORD).await.0, 204);
    let left: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE token_hash = 'expired'")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(left, 0);
}

/// A start whose account changed after its check (a password change, a
/// disable) is refused by its own insert and leaves no row (M6's race,
/// driven at the seam the race would reach).
#[tokio::test]
async fn a_start_checks_its_account_again_in_its_insert() {
    use lmgw_core::accounts::sessions::{start, Proof};
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (sam, _) = account_with_password(&gw, "sam").await;
    let before = rows(&state, "sessions").await;
    let stale = start(
        &state,
        sam,
        "192.0.2.1",
        "",
        Proof::Password("$argon2id$v=19$stale"),
    )
    .await
    .unwrap_err();
    assert_eq!(stale.code, "sign_in_superseded");
    assert_eq!(
        op(&gw, "account_set", json!({ "id": sam, "enabled": false }))
            .await
            .0,
        200
    );
    let disabled = start(&state, sam, "192.0.2.1", "", Proof::Credential)
        .await
        .unwrap_err();
    assert_eq!(disabled.code, "sign_in_superseded");
    assert_eq!(
        rows(&state, "sessions").await,
        before - 1,
        "sam's one went with the disable"
    );
}
