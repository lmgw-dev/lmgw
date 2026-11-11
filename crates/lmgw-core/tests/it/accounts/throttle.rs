//! The throttle through the sign-in route (§5.4, D13, D64): a failure is
//! answered before its bookkeeping, concurrent attempts meet each other's
//! delay, and a known browser is held by its own pair only.

use std::sync::Arc;

use serde_json::json;

use super::*;
use lmgw_core::state::AppState;

#[tokio::test]
async fn a_failure_is_answered_without_a_snapshot_reload() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let (sam, _) = account_with_password(&gw, "sam").await;
    let before = state.snapshot();
    assert_eq!(sign_in(&gw, "sam", "wrong wrong wrong").await.0, 401);
    // The count lands after the answer, in the store the card reads.
    assert_eq!(failed_logins(&gw, sam, 1).await, 1);
    assert!(
        Arc::ptr_eq(&before, &state.snapshot()),
        "a failed sign-in reloaded the snapshot, which an unknown name's never does"
    );
}

#[tokio::test]
async fn concurrent_attempts_inside_an_open_window_are_refused() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let _ = account_with_password(&gw, "sam").await;
    let attempts = (0..8).map(|_| sign_in(&gw, "sam", "wrong wrong wrong"));
    let statuses: Vec<u16> = futures::future::join_all(attempts)
        .await
        .into_iter()
        .map(|(status, ..)| status)
        .collect();
    assert_eq!(
        statuses.iter().filter(|s| **s == 401).count(),
        1,
        "one verification, the rest held: {statuses:?}"
    );
    assert!(
        statuses.iter().all(|s| *s == 401 || *s == 429),
        "{statuses:?}"
    );
}

#[tokio::test]
async fn a_known_browser_signs_in_through_a_spray_from_its_own_address() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let _ = account_with_password(&gw, "sam").await;
    let password = |cookie: Option<&str>, password: &str| {
        let mut req = browser()
            .post(format!("{gw}/api/session"))
            .json(&json!({ "login": "sam", "password": password }));
        if let Some(c) = cookie {
            req = req.header("cookie", format!("lmgw_known={c}"));
        }
        req.send()
    };
    let good = password(None, PASSWORD).await.unwrap();
    assert_eq!(good.status(), 204);
    let known = set_cookies(&good)
        .into_iter()
        .find(|(n, ..)| n == "lmgw_known")
        .expect("a known-browser cookie")
        .1;
    // A spray against the name from the very same address (loopback).
    assert_eq!(sign_in(&gw, "sam", "wrong wrong wrong").await.0, 401);
    assert_eq!(sign_in(&gw, "sam", "wrong wrong wrong").await.0, 429);
    // The known browser is held by its own pair, which the spray never
    // touched.
    let mine = password(Some(&known), PASSWORD).await.unwrap();
    assert_eq!(mine.status(), 204);
    // A known cookie of another account is no known browser for this one.
    let stranger = password(Some("not-a-known-token"), PASSWORD).await.unwrap();
    assert_eq!(stranger.status(), 429);
}
