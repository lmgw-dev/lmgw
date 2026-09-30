//! Per-key policy over real HTTP (usage-analytics design §4).
//!
//! The point of these is the *shape* of each refusal, not that a counter
//! increments: a client that cannot tell "you are over your budget until the
//! 1st" from "the provider is busy, retry" will retry forever, and the whole
//! feature turns into an outage with extra steps.

use lmgw_core::config::{hash_api_key, ApiKey, BudgetPeriod, KeyPolicy, ScopeMode};
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

const KEY: &str = "lmgw-test-key";

/// A gateway with auth on and one key carrying `policy`.
async fn serve(policy: KeyPolicy) -> (SharedState, String) {
    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.auth_enabled = true;
    store::save_settings(&state.db, &settings).await.unwrap();

    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, scope_mode, scope_patterns,
             budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit, expires_at)
         VALUES ('agent', ?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(hash_api_key(KEY))
    .bind(policy.scope_mode.as_str())
    .bind(&policy.scope_patterns)
    .bind(policy.budget_micro)
    .bind(policy.budget_period.as_str())
    .bind(policy.rpm_limit)
    .bind(policy.tpm_limit)
    .bind(policy.concurrency_limit)
    .bind(policy.expires_at.clone())
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (state, base)
}

/// An upstream + a `streamy` alias pointed at it.
async fn seed_upstream(state: &SharedState, upstream_base: &str) {
    sqlx::query(
        "INSERT INTO upstreams (name, protocol, kind, base_url, extra_headers, timeout_ms,
             enabled, expose_all, expose_prefix, created_at, updated_at, supports_responses)
         VALUES ('slow', 'openai', 'generic', ?1, '[]', 20000, 1, 0, '',
                 datetime('now'), datetime('now'), 0)",
    )
    .bind(upstream_base.trim_end_matches('/'))
    .execute(&state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO models (alias, upstream_id, upstream_model_id, param_overrides, enabled,
             created_at, updated_at)
         VALUES ('streamy', (SELECT id FROM upstreams WHERE name='slow'), 'tgt', '{}', 1,
                 datetime('now'), datetime('now'))",
    )
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

async fn chat(base: &str, alias: &str) -> (u16, Value, Option<String>) {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("authorization", format!("Bearer {KEY}"))
        .json(&json!({"model": alias, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    (
        status,
        resp.json().await.unwrap_or(Value::Null),
        retry_after,
    )
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn an_alias_outside_the_scope_is_403_key_scope() {
    let (_s, base) = serve(KeyPolicy {
        scope_mode: ScopeMode::Allow,
        scope_patterns: "claude-*".into(),
        ..Default::default()
    })
    .await;

    let (status, err, _) = chat(&base, "gpt-5-mini").await;
    assert_eq!(status, 403);
    assert_eq!(code(&err), "key_scope");
    let msg = err["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("gpt-5-mini"),
        "the message names the alias: {msg}"
    );
    assert!(msg.contains("agent"), "and the key: {msg}");

    // An in-scope alias gets past the policy plane — it then fails on routing,
    // which is a different error entirely and proves the check let it through.
    let (status, err, _) = chat(&base, "claude-opus-5").await;
    assert_ne!(code(&err), "key_scope");
    assert_eq!(status, 404, "unknown alias, not a policy refusal");
}

#[tokio::test]
async fn a_rate_limit_is_429_with_a_real_retry_after() {
    let (_s, base) = serve(KeyPolicy {
        rpm_limit: 2,
        ..Default::default()
    })
    .await;

    for _ in 0..2 {
        let (_, err, _) = chat(&base, "whatever").await;
        assert_ne!(code(&err), "key_rate", "the first two are within the limit");
    }
    let (status, err, retry_after) = chat(&base, "whatever").await;
    assert_eq!(status, 429);
    assert_eq!(code(&err), "key_rate");
    let secs: u64 = retry_after
        .expect("Retry-After present")
        .parse()
        .expect("Retry-After is a number");
    assert!((1..=60).contains(&secs), "Retry-After was {secs}");
}

#[tokio::test]
async fn an_exhausted_budget_is_403_and_carries_no_retry_after() {
    // 429 would tell every SDK to retry with backoff a condition that will not
    // clear until the 1st of next month.
    let (state, base) = serve(KeyPolicy {
        budget_micro: 1_000_000,
        budget_period: BudgetPeriod::Month,
        ..Default::default()
    })
    .await;

    // Spend the budget: one priced row against this key in the current bucket.
    sqlx::query(
        "INSERT INTO usage_hourly (bucket_utc, key_id, alias, upstream_id, class, outcome,
             requests, cost_micro)
         VALUES (strftime('%Y-%m-%dT%H','now'),
                 (SELECT id FROM api_keys WHERE name='agent'), 'a', 1, 'chat', 'ok', 1, 2000000)",
    )
    .execute(&state.db)
    .await
    .unwrap();

    let (status, err, retry_after) = chat(&base, "whatever").await;
    assert_eq!(status, 403, "not 429 — a budget does not clear on retry");
    assert_eq!(code(&err), "key_budget");
    assert!(retry_after.is_none(), "nothing to wait for");
    let msg = err["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("2.00"), "states what was spent: {msg}");
    assert!(msg.contains("1.00"), "and the budget it passed: {msg}");
}

#[tokio::test]
async fn an_expired_key_is_401() {
    let (_s, base) = serve(KeyPolicy {
        expires_at: Some("2020-01-01".into()),
        ..Default::default()
    })
    .await;
    let (status, err, _) = chat(&base, "whatever").await;
    assert_eq!(status, 401);
    assert_eq!(code(&err), "key_expired");
}

#[tokio::test]
async fn a_refusal_is_logged_like_any_other_traffic() {
    // "Why did my agent stop working" has to be answerable from the Logs page,
    // not only from the client's side of the connection.
    let (state, base) = serve(KeyPolicy {
        scope_mode: ScopeMode::Allow,
        scope_patterns: "claude-*".into(),
        ..Default::default()
    })
    .await;
    chat(&base, "gpt-5-mini").await;

    let rows = store::query_logs(
        &state.db,
        &store::LogFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let row = rows
        .iter()
        .find(|r| r.error_kind.as_deref() == Some("key_scope"))
        .expect("the refusal was logged");
    assert_eq!(row.requested_alias, "gpt-5-mini");
    assert_eq!(row.status, 403);
    assert_eq!(row.client_key.as_deref(), Some("agent"));

    // …and it rolls up as a refusal, not as an upstream error: a month of
    // deliberate refusals must not read as an outage.
    let totals = store::usage_totals(
        &state.db,
        &store::UsageFilter {
            from: "2000-01-01T00".into(),
            to: "2999-01-01T00".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(totals.refusals, 1);
    assert_eq!(totals.errors, 0);
}

#[tokio::test]
async fn an_internal_identity_is_not_a_credential() {
    // The internal:* rows exist so gateway-internal spend is visible and
    // budgetable. They carry an empty hash, and an empty hash must never open
    // the door — hashing "" produces a real hex string, so this is not
    // self-evidently safe and is worth pinning.
    let (state, base) = serve(KeyPolicy::default()).await;
    let internal: Vec<ApiKey> = state
        .snapshot()
        .api_keys
        .iter()
        .filter(|k| k.name.starts_with("internal:"))
        .cloned()
        .collect();
    assert!(!internal.is_empty(), "the migration seeded them");
    assert!(internal.iter().all(|k| k.key_hash.is_empty()));

    for candidate in ["", &hash_api_key("")] {
        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/chat/completions"))
            .header("authorization", format!("Bearer {candidate}"))
            .json(&json!({"model": "x", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 401, "candidate {candidate:?}");
    }
}

#[tokio::test]
async fn a_middleware_refusal_is_logged_and_rolled_up_too() {
    // key_rate and key_expired are decided before any handler runs. Without a
    // row there, the refusal class most likely to be hit in a loop is the one
    // class Logs, Traffic and the Usage page can never show.
    let (state, base) = serve(KeyPolicy {
        rpm_limit: 1,
        ..Default::default()
    })
    .await;
    chat(&base, "whatever").await;
    let (status, _, _) = chat(&base, "whatever").await;
    assert_eq!(status, 429);

    let rows = store::query_logs(
        &state.db,
        &store::LogFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let row = rows
        .iter()
        .find(|r| r.error_kind.as_deref() == Some("key_rate"))
        .expect("the rate-limit refusal was logged");
    assert_eq!(row.status, 429);
    assert_eq!(row.client_key.as_deref(), Some("agent"));

    let totals = store::usage_totals(
        &state.db,
        &store::UsageFilter {
            from: "2000-01-01T00".into(),
            to: "2999-01-01T00".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(totals.refusals >= 1, "and it rolls up as a refusal");
}

#[tokio::test]
async fn a_concurrency_slot_outlives_a_streamed_body() {
    // `next.run()` returns when the response is *built*; a streamed chat
    // response is built in microseconds and then drained for minutes by a
    // spawned task. The guard has to live in the body, or a limit of 1 admits
    // fifty simultaneous streams while looking correct in a non-streaming test.
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    // An upstream whose SSE body arrives slowly enough to still be open when
    // the second request is made.
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(1500))
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: [DONE]\n\n"),
        )
        .mount(&mock)
        .await;

    let (state, base) = serve(KeyPolicy {
        concurrency_limit: 1,
        ..Default::default()
    })
    .await;
    seed_upstream(&state, &mock.uri()).await;

    let base2 = base.clone();
    let first = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{base2}/v1/chat/completions"))
            .header("authorization", format!("Bearer {KEY}"))
            .json(&json!({"model": "streamy", "messages": [], "stream": true}))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
    });
    // Let the first request take the slot.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let (status, err, _) = chat(&base, "streamy").await;
    assert_eq!(status, 429, "the slot was still held: {err}");
    assert_eq!(code(&err), "key_rate");

    let _ = first.await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (_, err, _) = chat(&base, "streamy").await;
    assert_ne!(code(&err), "key_rate", "the slot came back with the body");
}

#[tokio::test]
async fn the_log_filters_a_click_through_needs_actually_filter() {
    // They used to be ignored silently, so a link built from them returned the
    // unfiltered head — which looks exactly like an answer.
    let (state, base) = serve(KeyPolicy {
        scope_mode: ScopeMode::Allow,
        scope_patterns: "nothing-*".into(),
        ..Default::default()
    })
    .await;
    chat(&base, "refused-alias").await;

    let key_id: i64 = sqlx::query_scalar("SELECT id FROM api_keys WHERE name='agent'")
        .fetch_one(&state.db)
        .await
        .unwrap();

    for (f, expect) in [
        (
            store::LogFilter {
                key_id: Some(key_id),
                limit: 10,
                ..Default::default()
            },
            1,
        ),
        (
            store::LogFilter {
                key_id: Some(key_id + 9_999),
                limit: 10,
                ..Default::default()
            },
            0,
        ),
        (
            store::LogFilter {
                error_kind: Some("key_scope".into()),
                limit: 10,
                ..Default::default()
            },
            1,
        ),
        (
            store::LogFilter {
                error_kind: Some("gpu_hold".into()),
                limit: 10,
                ..Default::default()
            },
            0,
        ),
        (
            store::LogFilter {
                class: Some("chat".into()),
                limit: 10,
                ..Default::default()
            },
            1,
        ),
        (
            store::LogFilter {
                class: Some("audio".into()),
                limit: 10,
                ..Default::default()
            },
            0,
        ),
    ] {
        let n = store::query_logs(&state.db, &f).await.unwrap().len();
        assert_eq!(n, expect, "filter {f:?} returned {n} rows");
    }
}
