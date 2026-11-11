//! The remembered quota state (openai egress design §14.6): a hop on an
//! upstream remembered out of quota that has a fallback is skipped to it
//! at resolve, before anything is sent; the re-check; Check now, an edit of
//! the row and the views.

use std::time::Duration;

use super::*;

/// The upstream row id `g.cloud(alias)` made.
async fn upstream_of(g: &Gpu, alias: &str) -> i64 {
    sqlx::query_scalar("SELECT upstream_id FROM models WHERE alias = ?1")
        .bind(alias)
        .fetch_one(&g.state.db)
        .await
        .unwrap()
}

fn remember(g: &Gpu, upstream: i64, next_check: Duration) {
    g.state.upstream_quota.set(
        upstream,
        "cloud-a",
        Some("insufficient_quota"),
        "You exceeded your current quota.",
        next_check,
    );
}

/// The chat calls the cloud mocks got.
async fn chat_calls(g: &Gpu) -> usize {
    g.cloud_requests()
        .await
        .iter()
        .filter(|r| r.ends_with("/chat/completions"))
        .count()
}

#[tokio::test]
async fn a_remembered_quota_skips_to_the_fallback_before_anything_is_sent() {
    let (g, gw) = world().await;
    g.cloud("a", None).await;
    g.cloud("b", None).await;
    let up = upstream_of(&g, "a").await;
    remember(&g, up, Duration::from_secs(300));

    // Without a fallback the request still goes to the upstream: its client
    // gets the provider's own answer.
    let resp = ask(&gw, "a").await;
    assert_eq!(resp.status(), 200);
    assert!(h(&resp, "x-lmgw-fallback").is_none());
    assert_eq!(chat_calls(&g).await, 1);
    assert!(g.state.upstream_quota.get(up).is_none(), "a 2xx clears it");

    remember(&g, up, Duration::from_secs(300));
    set_fallback(&g.state, "models", "a", Some("b")).await;
    answered_along(
        ask(&gw, "a").await,
        "a",
        r#""b";reason=quota"#,
        "from the cloud",
    )
    .await;
    assert_eq!(chat_calls(&g).await, 2, "only b was asked");

    // Embeddings keep the quota error in WP10: no skip for them.
    assert!(!lmgw_core::gate::RouteCheck::Embeddings.quota_falls_back());
}

/// Once the next check has passed, one request goes to the upstream again;
/// a quota again sets the next check, that request falls back, and the
/// next one is skipped without a send.
#[tokio::test]
async fn the_recheck_lets_one_request_through() {
    let (g, gw) = world().await;
    let a = platform(&g, "a", 429, quota_body(), &[]).await;
    g.cloud("b", None).await;
    set_fallback(&g.state, "models", "a", Some("b")).await;
    let up = upstream_of(&g, "a").await;
    remember(&g, up, Duration::ZERO);

    for _ in 0..2 {
        answered_along(
            ask(&gw, "a").await,
            "a",
            r#""b";reason=quota"#,
            "from the cloud",
        )
        .await;
    }
    assert_eq!(
        a.received_requests().await.unwrap().len(),
        1,
        "the re-check only"
    );
    let e = g.state.upstream_quota.get(up).unwrap();
    assert!(e.next_check > e.since, "set again by the re-check's quota");
}

/// Check now forgets it; the views show it until then; an edit of the row
/// forgets it too; the interval is a setting of at least one second.
#[tokio::test]
async fn check_now_an_edit_and_the_views() {
    let (g, gw) = world().await;
    g.cloud("a", None).await;
    let up = upstream_of(&g, "a").await;
    remember(&g, up, Duration::from_secs(300));

    let ups: Value = gw
        .client()
        .get(format!("{gw}/api/upstreams"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = ups["upstreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == up)
        .unwrap()
        .clone();
    assert_eq!(row["quota"]["code"], "insufficient_quota", "{row}");
    assert!(row["quota"]["next_check"].as_i64().unwrap() > row["quota"]["since"].as_i64().unwrap());
    let status = lmgw_core::ops::status(&g.state).await.unwrap();
    assert_eq!(status.upstreams_out_of_quota.len(), 1);
    assert_eq!(status.upstreams_out_of_quota[0].id, up);

    let v: Value = gw
        .client()
        .post(format!("{gw}/api/op/upstream_quota_clear"))
        .json(&json!({"id": up}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("no longer remembered"),
        "{v}"
    );
    assert!(g.state.upstream_quota.get(up).is_none());
    let v: Value = gw
        .client()
        .post(format!("{gw}/api/op/upstream_quota_clear"))
        .json(&json!({"id": up}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v["message"].as_str().unwrap().contains("was not"), "{v}");

    remember(&g, up, Duration::from_secs(300));
    lmgw_core::ops::upstream_set(
        &g.state,
        serde_json::from_value(json!({"action": "update", "id": up, "timeout_ms": 6000})).unwrap(),
        lmgw_core::ops::RowWriter::Tool,
    )
    .await
    .unwrap();
    assert!(
        g.state.upstream_quota.get(up).is_none(),
        "an edit forgets it"
    );

    let refused = lmgw_core::ops::settings_set(
        &g.state,
        serde_json::from_value(json!({"quota_recheck_seconds": 0})).unwrap(),
    )
    .await
    .unwrap_err()
    .message;
    assert!(refused.contains("at least 1"), "{refused}");
    lmgw_core::ops::settings_set(
        &g.state,
        serde_json::from_value(json!({"quota_recheck_seconds": 60})).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(g.state.snapshot().settings.quota_recheck_seconds, 60);
}
