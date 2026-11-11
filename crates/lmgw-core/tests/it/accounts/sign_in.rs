//! The password sign-in (§5.2, §5.4, D13, D14, D26).

use serde_json::json;

use super::*;
use lmgw_core::state::AppState;

#[tokio::test]
async fn unknown_disabled_and_passwordless_names_answer_as_a_wrong_password_does() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (sam, _) = account_with_password(&gw, "sam").await;
    let (status, ok, session) = sign_in(&gw, "SAM", PASSWORD).await;
    assert_eq!(status, 204, "{ok}");
    assert!(session.is_some());
    // A known-browser cookie beside the session.
    let (_, link) = op(&gw, "account_create", json!({ "login": "kim" })).await;
    assert_eq!(link["login"], "kim");
    assert_eq!(
        op(&gw, "account_set", json!({ "id": sam, "enabled": false }))
            .await
            .0,
        200
    );
    // Each from its own name and address, so no throttle stands between
    // them.
    trust_loopback_proxy(&state).await;
    // And names that are no sign-in name at all: outside the alphabet, past
    // the 64 characters (D13: looked up as nothing, verified as any other).
    let long = "a".repeat(65);
    let names = ["nobody", "sam", "kim", "owner", "zoë", "sam k", &long];
    for (i, login) in names.into_iter().enumerate() {
        let (status, body) =
            sign_in_from(&gw, &format!("192.0.2.{}", i + 10), login, PASSWORD).await;
        assert_eq!(status, 401, "{login}: {body}");
        assert_eq!(body["code"], "login_invalid", "{login}");
        assert_eq!(body["message"], "the sign-in name or password is wrong");
    }
}

#[tokio::test]
async fn a_failure_holds_the_next_attempt_visibly_and_counts() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (sam, _) = account_with_password(&gw, "sam").await;
    assert_eq!(sign_in(&gw, "sam", "wrong wrong wrong").await.0, 401);
    let resp = browser()
        .post(format!("{gw}/api/session"))
        .json(&json!({ "login": "sam", "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    assert!(resp.headers().get("retry-after").is_some());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "login_throttled");
    assert!(
        body["message"].as_str().unwrap().contains("try again in"),
        "{body}"
    );
    assert_eq!(failed_logins(&gw, sam, 1).await, 1);
    // An unknown name is throttled as a known one, from another address.
    trust_loopback_proxy(&state).await;
    assert_eq!(
        sign_in_from(&gw, "198.51.100.7", "ghost", PASSWORD).await.0,
        401
    );
    let (status, body) = sign_in_from(&gw, "198.51.100.7", "ghost", PASSWORD).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (429, Some("login_throttled"))
    );
}

#[tokio::test]
async fn a_password_over_plain_http_from_another_machine_is_refused() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let _ = account_with_password(&gw, "sam").await;
    // A trusted proxy on loopback that forwards a client from elsewhere:
    // proxied is never loopback (D51), and the scheme is http.
    let mut s = state.snapshot().settings.clone();
    s.trusted_proxies = vec!["127.0.0.1/32".into()];
    lmgw_core::store::save_settings(&state.db, &s)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let resp = browser()
        .post(format!("{gw}/api/session"))
        .header("x-forwarded-for", "192.0.2.5")
        .json(&json!({ "login": "sam", "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "login_insecure", "{body}");
    // The same proxy saying the client used https is a secure transport.
    let resp = browser()
        .post(format!("{gw}/api/session"))
        .header("x-forwarded-for", "192.0.2.5")
        .header("x-forwarded-proto", "https")
        .json(&json!({ "login": "sam", "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let cookies = set_cookies(&resp);
    let session = cookies
        .iter()
        .find(|(n, ..)| n == "__Host-lmgw_session")
        .unwrap();
    assert!(session.2.contains("; Secure"), "{}", session.2);
    assert!(cookies.iter().any(|(n, ..)| n == "__Host-lmgw_known"));
}

#[tokio::test]
async fn a_body_past_the_named_limit_is_refused() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let big = "x".repeat(lmgw_core::web::session::SESSION_BODY_LIMIT + 1);
    let (status, ..) = sign_in(&gw, "owner", &big).await;
    assert_eq!(status, 413);
}
