//! Set-password links (§5.5) and one-time sign-in links (§5.6, §5.7).

use serde_json::json;

use super::*;
use lmgw_core::state::AppState;

#[tokio::test]
async fn a_set_password_link_works_once_and_never_for_one_s_own_session() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let mine = turn_on(&gw).await;
    let (status, link) = op(&gw, "account_create", json!({ "login": "sam" })).await;
    assert_eq!(status, 200);
    assert!(link["url"]
        .as_str()
        .unwrap()
        .contains("/set-password#token="));
    assert!(link["validity"]
        .as_str()
        .unwrap()
        .starts_with("valid until "));
    let token = link["url"]
        .as_str()
        .unwrap()
        .split_once("#token=")
        .unwrap()
        .1
        .to_string();
    let post = |password: &'static str| {
        let (gw, token) = (gw.clone(), token.clone());
        async move {
            browser()
                .post(format!("{gw}/api/account/password"))
                .json(&json!({ "token": token, "password": password }))
                .send()
                .await
                .unwrap()
        }
    };
    let short = post("eleven char").await;
    assert_eq!(short.status(), 400, "a short password keeps the link");
    let ok = post(PASSWORD).await;
    assert_eq!(ok.status(), 204);
    assert!(session_of(&ok).is_some(), "the link signs this browser in");
    let again = post(PASSWORD).await;
    assert_eq!(again.status(), 401);
    let body: Value = again.json().await.unwrap();
    assert_eq!(body["code"], "link_invalid");

    // A link for one's own account from a session is refused; the
    // dashboard key's bearer (the command line) may mint one.
    let (status, body) = op_as(&gw, &mine, "account_password_link", json!({ "id": 1 })).await;
    assert_eq!((status, body["code"].as_str()), (409, Some("account_self")));
    assert_eq!(
        op(&gw, "account_password_link", json!({ "id": 1 })).await.0,
        200
    );
    // A second link replaces the first.
    let (_, a) = op(&gw, "account_password_link", json!({ "id": 2 })).await;
    let (_, b) = op(&gw, "account_password_link", json!({ "id": 2 })).await;
    assert_ne!(a["url"], b["url"]);
    let stale = a["url"].as_str().unwrap().split_once("#token=").unwrap().1;
    let resp = browser()
        .post(format!("{gw}/api/account/password"))
        .json(&json!({ "token": stale, "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn a_login_link_signs_its_account_in_once_and_expires() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (sam, _) = account_with_password(&gw, "sam").await;
    let (status, link) = op(&gw, "account_login_link", json!({ "id": sam })).await;
    assert_eq!(status, 200, "{link}");
    let path = link["url"]
        .as_str()
        .unwrap()
        .split_once("/api/")
        .unwrap()
        .1
        .to_string();
    let open = || {
        let (gw, path) = (gw.clone(), path.clone());
        async move {
            browser()
                .get(format!("{gw}/api/{path}"))
                .send()
                .await
                .unwrap()
        }
    };
    let first = open().await;
    assert_eq!(first.status(), 302);
    let session = session_of(&first).unwrap();
    assert!(
        session.starts_with("lmgw-session-"),
        "a session, never a key"
    );
    let (_, me) = get_as(&gw, &session, "/api/session").await;
    assert_eq!(me["account"]["login"], "sam");
    let second = open().await;
    assert_eq!(
        second.headers()["location"].to_str().unwrap(),
        "/?login=invalid",
        "spent"
    );
    // Expired.
    state
        .login_nonces
        .set_ttl(std::time::Duration::from_millis(0));
    let (_, link) = op(&gw, "account_login_link", json!({ "id": sam })).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let path = link["url"]
        .as_str()
        .unwrap()
        .split_once("/api/")
        .unwrap()
        .1
        .to_string();
    let late = browser()
        .get(format!("{gw}/api/{path}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        late.headers()["location"].to_str().unwrap(),
        "/?login=invalid"
    );
}

#[tokio::test]
async fn the_tray_s_nonce_signs_account_one_into_a_session_with_accounts_on() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let nonce = lmgw_core::web::session::mint_login_nonce(&state);
    let resp = browser()
        .get(format!("{gw}/api/session/login?nonce={nonce}"))
        .send()
        .await
        .unwrap();
    let session = session_of(&resp).unwrap();
    assert!(session.starts_with("lmgw-session-"));
    // The key link too: a session of the key's account, never the key.
    let resp = browser()
        .get(format!("{gw}/api/session/login?token={}", gw.key))
        .send()
        .await
        .unwrap();
    let session = session_of(&resp).unwrap();
    assert!(session.starts_with("lmgw-session-"), "{session}");
    let (_, me) = get_as(&gw, &session, "/api/session").await;
    assert_eq!(me["account"]["id"], 1);
}

fn token_of(link: &Value) -> String {
    link["url"]
        .as_str()
        .unwrap()
        .split_once("#token=")
        .unwrap()
        .1
        .to_string()
}

async fn submit(gw: &Gw, token: &str, password: &str) -> (u16, Value) {
    let resp = browser()
        .post(format!("{gw}/api/account/password"))
        .json(&json!({ "token": token, "password": password }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn two_submits_of_one_link_set_one_password() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (_, link) = op(&gw, "account_create", json!({ "login": "kim" })).await;
    let token = token_of(&link);
    let (a, b) = tokio::join!(
        submit(&gw, &token, "the first long password"),
        submit(&gw, &token, "the second long password"),
    );
    let mut statuses = [a.0, b.0];
    statuses.sort_unstable();
    assert_eq!(statuses, [204, 401], "{a:?} {b:?}");
    let refused = if a.0 == 401 { &a.1 } else { &b.1 };
    assert_eq!(refused["code"], "link_invalid");
    // Whichever won, its password is the one that signs in.
    let won = if a.0 == 204 {
        "the first long password"
    } else {
        "the second long password"
    };
    assert_eq!(sign_in(&gw, "kim", won).await.0, 204);
}

#[tokio::test]
async fn a_disabled_account_s_link_is_refused_and_kept() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (_, link) = op(&gw, "account_create", json!({ "login": "kim" })).await;
    let kim = link["account_id"].as_i64().unwrap();
    let token = token_of(&link);
    let set = |enabled: bool| op(&gw, "account_set", json!({ "id": kim, "enabled": enabled }));
    assert_eq!(set(false).await.0, 200);
    let (status, body) = submit(&gw, &token, PASSWORD).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (401, Some("account_disabled")),
        "{body}"
    );
    assert_eq!(set(true).await.0, 200);
    assert_eq!(submit(&gw, &token, PASSWORD).await.0, 204);
}

#[tokio::test]
async fn a_session_cannot_mint_a_one_time_sign_in_link() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let mine = turn_on(&gw).await;
    let (sam, _) = account_with_password(&gw, "sam").await;
    let (status, body) = op_as(&gw, &mine, "account_login_link", json!({ "id": sam })).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (403, Some("login_link_cli_only")),
        "{body}"
    );
    // The command line's bearer may.
    assert_eq!(
        op(&gw, "account_login_link", json!({ "id": sam })).await.0,
        200
    );
}

#[tokio::test]
async fn with_accounts_off_only_account_one_s_nonce_signs_in() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (sam, _) = account_with_password(&gw, "sam").await;
    assert_eq!(
        op(&gw, "accounts_switch", json!({ "enabled": false }))
            .await
            .0,
        200
    );
    let open = |nonce: String| {
        let gw = gw.clone();
        async move {
            browser()
                .get(format!("{gw}/api/session/login?nonce={nonce}"))
                .send()
                .await
                .unwrap()
        }
    };
    let theirs = open(lmgw_core::web::session::mint_login_nonce_for(&state, sam)).await;
    assert_eq!(
        theirs.headers()["location"].to_str().unwrap(),
        "/?login=invalid"
    );
    assert!(session_of(&theirs).is_none());
    let shell = open(lmgw_core::web::session::mint_login_nonce(&state)).await;
    assert_eq!(shell.headers()["location"].to_str().unwrap(), "/");
    assert!(session_of(&shell).is_some(), "the door key's cookie");
}
