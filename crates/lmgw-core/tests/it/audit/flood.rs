//! What a request with no credential costs the log (D91, the WP7 review's
//! first finding): a flood from one client is one entry a minute, its
//! answer never waits for the write, and a throttled attempt is one entry
//! per hold.

use std::time::{Duration, Instant};

use futures::StreamExt;

use super::*;
use crate::accounts::{browser, sign_in_from, trust_loopback_proxy};
use lmgw_core::state::AppState;

/// `(rows, their counts summed)` of `action`.
async fn rows_and_times(state: &SharedState, action: &str) -> (i64, i64) {
    sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(count), 0) FROM audit_log WHERE action = ?1")
        .bind(action)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

/// Wait up to ten seconds for `action`'s counts to reach `times`.
async fn until_times(state: &SharedState, action: &str, times: i64) -> (i64, i64) {
    let mut got = (0, 0);
    for _ in 0..200 {
        got = rows_and_times(state, action).await;
        if got.1 >= times {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    got
}

#[tokio::test]
async fn a_flood_of_garbage_logins_is_one_entry_a_minute() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    turn_on(&gw).await;
    let client = browser();
    let started = Instant::now();
    let url = format!("{gw}/api/session/login?token=spike-wp7-garbage");
    let sends = (0..1000).map(|_| {
        let req = client.get(&url).send();
        async move { req.await.unwrap().status().as_u16() }
    });
    let statuses: Vec<u16> = futures::stream::iter(sends)
        .buffer_unordered(64)
        .collect()
        .await;
    assert!(
        statuses.iter().all(|s| (300..400).contains(s)),
        "{statuses:?}"
    );
    let (rows, times) = until_times(&state, "sign-in failed", 1000).await;
    assert_eq!(times, 1000, "every attempt is counted");
    let minutes = started.elapsed().as_secs() / 60 + 2;
    assert!(
        rows as u64 <= minutes,
        "{rows} rows for one client's {times} attempts in {:?}",
        started.elapsed()
    );
    let e = with_action(&gw, "sign-in failed").await;
    assert_eq!(e[0]["peer"], "127.0.0.1");
    assert_eq!(e[0]["target"], json!({ "with": "key" }));
    assert!(e[0]["last_ts"].is_string(), "{}", e[0]);
    assert!(!super::whole_log(&state).await.contains("spike-wp7-garbage"));
}

#[tokio::test]
async fn a_failed_sign_ins_answer_does_not_wait_for_its_entry() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    turn_on(&gw).await;
    // Every connection of the pool held: a write would wait for one.
    let max = state.db.options().get_max_connections();
    let mut held = Vec::new();
    for _ in 0..max {
        held.push(state.db.acquire().await.unwrap());
    }
    let answer = tokio::time::timeout(
        Duration::from_secs(2),
        browser()
            .get(format!("{gw}/api/session/login?nonce=spike-wp7-nonce"))
            .send(),
    )
    .await
    .expect("the answer came while the database was busy")
    .unwrap();
    assert!(answer.status().is_redirection());
    drop(held);
    let (rows, times) = until_times(&state, "sign-in failed", 1).await;
    assert_eq!((rows, times), (1, 1), "the entry landed after the answer");
}

#[tokio::test]
async fn a_throttled_sign_in_is_one_entry_per_hold() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    turn_on(&gw).await;
    trust_loopback_proxy(&state).await;
    let wrong = "wrong wrong wrong";
    assert_eq!(sign_in_from(&gw, "192.0.2.20", "owner", wrong).await.0, 401);
    for _ in 0..5 {
        assert_eq!(sign_in_from(&gw, "192.0.2.20", "owner", wrong).await.0, 429);
    }
    assert_eq!(until_times(&state, "sign-in throttled", 1).await, (1, 1));
    // Nothing more lands later: the four other 429s wrote nothing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rows_and_times(&state, "sign-in throttled").await, (1, 1));
    assert_eq!(until_times(&state, "sign-in failed", 1).await, (1, 1));
}

#[cfg_attr(
    windows,
    ignore = "Windows resets a connection closed with unread bytes and the reset loses the refusal before the client reads it; only a declared oversize /v1 body is drained before its 413 on Windows yet (Windows design §5)"
)]
#[tokio::test]
async fn an_op_body_over_the_routes_limit_is_its_413() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    turn_on(&gw).await;
    let before = count(&state).await;
    let big = format!(r#"{{"name":"spike-wp7","pad":"{}"}}"#, "x".repeat(3 << 20));
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/key_create"))
        .header("content-type", "application/json")
        .body(big)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
    assert_eq!(count(&state).await, before, "a refused body did nothing");
}
