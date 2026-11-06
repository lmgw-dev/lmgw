//! An exhausted quota and an unreachable upstream at the chat send (openai
//! egress design §14.4, §14.7, §14.8).

use std::time::Duration;

use super::*;

/// An `openai_compatible` alias `alias` whose mock answers a chat unary or
/// streamed with "from the cloud"; the mock.
async fn answering(g: &Gpu, alias: &str) -> MockServer {
    let mock = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(wiremock::matchers::body_partial_json(json!({"stream": true})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"from the cloud\"}}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            ),
            "text/event-stream",
        ))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "created": 0, "model": "gpt",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "from the cloud"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })))
        .mount(&mock)
        .await;
    upstream_alias(
        g,
        alias,
        &format!("{}/v1", mock.uri()),
        Protocol::OpenaiCompatible,
    )
    .await;
    mock
}

async fn chats(mock: &MockServer) -> usize {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .count()
}

/// The refused attempt's row and the answer's: the first is the quota,
/// handed to `to`, with no usage; the second is the fallback's answer.
fn two_rows(rows: &[store::RequestLogRow], status: i64, to: &str) {
    assert_eq!(rows.len(), 2, "{rows:#?}");
    let (refused, answer) = (&rows[0], &rows[1]);
    assert_eq!(refused.status, status, "{refused:?}");
    assert_eq!(refused.error_kind.as_deref(), Some("insufficient_quota"));
    let msg = refused.error_msg.as_deref().unwrap();
    assert!(
        msg.contains(&format!("; handed to fallback '{to}' (quota)")),
        "{msg}"
    );
    assert_eq!(refused.upstream_name.as_deref(), Some("platform-a"));
    assert_eq!(
        refused.fallback_reason, None,
        "the name the client asked for"
    );
    assert_eq!(refused.cost_micro.unwrap_or(0), 0);
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(
        answer.upstream_name.as_deref(),
        Some(&*format!("platform-{to}"))
    );
    assert_eq!(answer.fallback_reason.as_deref(), Some("quota"));
}

#[tokio::test]
async fn a_quota_on_a_unary_chat_is_answered_by_the_fallback_and_remembered() {
    let (g, gw) = world().await;
    let a = platform(&g, "a", 429, quota_body(), &[]).await;
    let _b = answering(&g, "b").await;
    set_fallback(&g.state, "models", "a", Some("b")).await;

    answered_along(
        ask(&gw, "a").await,
        "a",
        r#""b";reason=quota"#,
        "from the cloud",
    )
    .await;
    two_rows(&rows(&g, "a", 2).await, 429, "b");
    let up: i64 = sqlx::query_scalar("SELECT id FROM upstreams WHERE name = 'platform-a'")
        .fetch_one(&g.state.db)
        .await
        .unwrap();
    let e = g.state.upstream_quota.get(up).expect("remembered");
    assert_eq!(e.code.as_deref(), Some("insufficient_quota"));

    // Remembered: the next request is skipped before anything is sent, with
    // no row of its own for the skipped hop.
    answered_along(
        ask(&gw, "a").await,
        "a",
        r#""b";reason=quota"#,
        "from the cloud",
    )
    .await;
    assert_eq!(chats(&a).await, 1, "a was asked once");
    assert_eq!(rows(&g, "a", 3).await.len(), 3);
}

#[tokio::test]
async fn a_quota_on_a_stream_and_on_messages_is_answered_by_the_fallback() {
    let (g, gw) = world().await;
    let _m1 = platform(&g, "a", 429, quota_body(), &[]).await;
    let _m2 = answering(&g, "b").await;
    set_fallback(&g.state, "models", "a", Some("b")).await;

    let resp = gw
        .client()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": "a", "stream": true,
                       "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(h(&resp, "x-lmgw-fallback").as_deref(), Some("b"));
    assert_eq!(
        h(&resp, "x-lmgw-fallback-chain").as_deref(),
        Some(r#""b";reason=quota"#)
    );
    let text = resp.text().await.unwrap();
    assert!(text.contains("from the cloud"), "{text}");
    two_rows(&rows(&g, "a", 2).await, 429, "b");

    // `/v1/messages`: the refused attempt's row is Anthropic's 402. Forgotten
    // first, so the quota is met at the send again.
    let up: i64 = sqlx::query_scalar("SELECT id FROM upstreams WHERE name = 'platform-a'")
        .fetch_one(&g.state.db)
        .await
        .unwrap();
    g.state.upstream_quota.clear(up, "platform-a", "test");
    let resp = gw
        .client()
        .post(format!("{gw}/v1/messages"))
        .json(&json!({"model": "a", "max_tokens": 64,
                       "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(h(&resp, "x-lmgw-fallback-reason").as_deref(), Some("quota"));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["content"][0]["text"], "from the cloud", "{v}");
    let all = rows(&g, "a", 4).await;
    assert_eq!(all[2].status, 402, "{:?}", all[2]);
    assert_eq!(all[2].error_kind.as_deref(), Some("insufficient_quota"));
}

/// A rate limit is not a quota: it reaches the client as OpenAI sent it,
/// and nothing is remembered or handed on.
#[tokio::test]
async fn a_rate_limit_reaches_the_client() {
    let (g, gw) = world().await;
    let body = json!({"error": {"message": "Rate limit reached for requests.",
                                "type": "requests", "param": null,
                                "code": "rate_limit_exceeded"}});
    let _m3 = platform(&g, "a", 429, body, &[]).await;
    let b = answering(&g, "b").await;
    set_fallback(&g.state, "models", "a", Some("b")).await;

    let resp = ask(&gw, "a").await;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap();
    assert_eq!(status, 429, "{v}");
    assert_eq!(v["error"]["code"], "rate_limit_exceeded", "{v}");
    assert_eq!(chats(&b).await, 0);
    assert!(g.state.snapshot().upstreams.values().all(|u| g
        .state
        .upstream_quota
        .get(u.id)
        .is_none()));
}

/// Without a fallback the quota error stands, its note naming where one
/// would be set; the upstream's `Retry-After` is when it is asked again.
#[tokio::test]
async fn without_a_fallback_the_quota_names_where_one_would_be_set() {
    let (g, gw) = world().await;
    let _m4 = platform(&g, "a", 429, quota_body(), &[("retry-after", "120")]).await;

    let resp = ask(&gw, "a").await;
    assert_eq!(resp.status(), 429);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "insufficient_quota", "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("retrying will not help"), "{msg}");
    assert!(msg.ends_with("'a' has no fallback configured"), "{msg}");

    let up: i64 = sqlx::query_scalar("SELECT id FROM upstreams WHERE name = 'platform-a'")
        .fetch_one(&g.state.db)
        .await
        .unwrap();
    let e = g
        .state
        .upstream_quota
        .get(up)
        .expect("remembered all the same");
    let after = e.next_check - e.since;
    assert!((115..=125).contains(&after), "Retry-After first: {after}");
}

/// A → B → C, A and B out of quota: C answers, each refused attempt has its
/// row, and the chain header names both hops.
#[tokio::test]
async fn a_chain_of_quotas_is_answered_by_its_end() {
    let (g, gw) = world().await;
    let _m5 = platform(&g, "a", 429, quota_body(), &[]).await;
    let _m6 = platform(&g, "b", 429, quota_body(), &[]).await;
    let _m7 = answering(&g, "c").await;
    set_fallback(&g.state, "models", "a", Some("b")).await;
    set_fallback(&g.state, "models", "b", Some("c")).await;

    answered_along(
        ask(&gw, "a").await,
        "a",
        r#""b";reason=quota, "c";reason=quota"#,
        "from the cloud",
    )
    .await;
    let all = rows(&g, "a", 3).await;
    assert_eq!(
        all.iter()
            .map(|r| (r.status, r.upstream_name.clone().unwrap_or_default()))
            .collect::<Vec<_>>(),
        [
            (429, "platform-a".to_string()),
            (429, "platform-b".to_string()),
            (200, "platform-c".to_string())
        ]
    );
    assert_eq!(
        all[1].fallback_reason.as_deref(),
        Some("quota"),
        "B was reached on a quota"
    );
}

/// A local fallback reached on a quota under the hold is held too: its own
/// fallback answers (reasons `quota`, then `hold`); without one, `gpu_hold`
/// naming the chain.
#[tokio::test]
async fn a_local_fallback_under_the_hold_goes_on_to_its_own() {
    let (g, gw) = world().await;
    let _m8 = platform(&g, "a", 429, quota_body(), &[]).await;
    let _m9 = answering(&g, "c").await;
    g.model("m", 4 * GIB).await;
    set_fallback(&g.state, "models", "a", Some("m")).await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();

    let resp = ask(&gw, "a").await;
    assert_eq!(resp.status(), 503);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "gpu_hold", "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("'m'") && msg.contains("(tried: a → m)"),
        "{msg}"
    );
    // The answer's row is on the hop that refused, `m`, with its reason —
    // not on the cloud route the request left (review B6).
    let all = rows(&g, "a", 2).await;
    assert_eq!(all[1].status, 503, "{:?}", all[1]);
    assert_eq!(
        all[1].upstream_name.as_deref(),
        Some(lmgw_core::config::ROUTER_UPSTREAM_NAME),
        "{:?}",
        all[1]
    );
    assert_eq!(all[1].fallback_reason.as_deref(), Some("quota"));

    set_fallback(&g.state, "local_models", "m", Some("c")).await;
    answered_along(
        ask(&gw, "a").await,
        "a",
        r#""m";reason=quota, "c";reason=hold"#,
        "from the cloud",
    )
    .await;
    assert!(g.runs().is_empty(), "{:?}", g.runs());
}

/// An upstream nothing answers on: its fallback answers (reason
/// `unreachable`) with the attempt's own `502` row; without one, the `502`
/// as before, of kind `unreachable`.
#[tokio::test]
async fn an_unreachable_upstream_is_answered_by_its_fallback() {
    let (g, gw) = world().await;
    let (port, _held) = crate::common::refusing_port();
    upstream_alias(
        &g,
        "a",
        &format!("http://127.0.0.1:{port}/v1"),
        Protocol::OpenaiCompatible,
    )
    .await;

    let resp = ask(&gw, "a").await;
    assert_eq!(resp.status(), 502);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "unreachable", "{v}");

    let _m10 = answering(&g, "b").await;
    set_fallback(&g.state, "models", "a", Some("b")).await;
    answered_along(
        ask(&gw, "a").await,
        "a",
        r#""b";reason=unreachable"#,
        "from the cloud",
    )
    .await;
    let all = rows(&g, "a", 3).await;
    assert_eq!(all[1].status, 502, "{:?}", all[1]);
    assert_eq!(all[1].error_kind.as_deref(), Some("unreachable"));
    assert!(all[1]
        .error_msg
        .as_deref()
        .unwrap()
        .contains("; handed to fallback 'b' (unreachable)"));
    assert_eq!(all[2].fallback_reason.as_deref(), Some("unreachable"));
}

/// The unary read on a cloud row runs detached so its row survives a
/// hang-up; the hand-over it decides does not (review B3): a client that
/// left before the quota came back starts no local fallback, and the
/// attempt's row says the client left before it was handed on.
#[tokio::test]
async fn a_client_that_left_before_the_quota_starts_no_local_fallback() {
    let (g, gw) = world().await;
    let mock = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_json(quota_body())
                .set_delay(Duration::from_millis(600)),
        )
        .mount(&mock)
        .await;
    let up = upstream_alias(&g, "a", &mock.uri(), Protocol::Openai).await;
    g.model("m", 4 * GIB).await;
    set_fallback(&g.state, "models", "a", Some("m")).await;

    let gone = gw
        .client()
        .post(format!("{gw}/v1/chat/completions"))
        .timeout(Duration::from_millis(150))
        .json(&json!({"model": "a", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await;
    assert!(gone.is_err(), "the client left first");
    let row = rows(&g, "a", 1).await.remove(0);
    assert_eq!(
        row.error_kind.as_deref(),
        Some("insufficient_quota"),
        "{row:?}"
    );
    let msg = row.error_msg.as_deref().unwrap();
    assert!(
        msg.contains("the client left before it was handed to 'a''s fallback"),
        "{msg}"
    );
    assert!(g.runs().is_empty(), "nothing started: {:?}", g.runs());
    assert!(
        g.state.upstream_quota.get(up).is_some(),
        "the quota is remembered all the same"
    );
}

/// A `2xx` from an upstream remembered out of quota (its re-check) clears
/// what was remembered.
#[tokio::test]
async fn a_success_clears_what_was_remembered() {
    let (g, gw) = world().await;
    let _m11 = answering(&g, "a").await;
    let up: i64 = sqlx::query_scalar("SELECT id FROM upstreams WHERE name = 'platform-a'")
        .fetch_one(&g.state.db)
        .await
        .unwrap();
    g.state
        .upstream_quota
        .set(up, "platform-a", None, "no credit", Duration::ZERO);
    assert_eq!(ask(&gw, "a").await.status(), 200);
    assert!(g.state.upstream_quota.get(up).is_none());
}

/// Any `2xx` from that upstream clears it, whatever the request class
/// (§14.6, review B8): an embedding is one.
#[tokio::test]
async fn a_success_of_another_class_clears_it_too() {
    let (g, gw) = world().await;
    let mock = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "model": "gpt-test",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}],
            "usage": {"prompt_tokens": 2, "total_tokens": 2},
        })))
        .mount(&mock)
        .await;
    let up = upstream_alias(&g, "e", &mock.uri(), Protocol::Openai).await;
    g.state.upstream_quota.set(
        up,
        "platform-e",
        None,
        "no credit",
        Duration::from_secs(600),
    );
    let resp = gw
        .client()
        .post(format!("{gw}/v1/embeddings"))
        .json(&json!({"model": "e", "input": "hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(g.state.upstream_quota.get(up).is_none());
}

/// However long the upstream says to wait, and however long the setting
/// is, a quota is remembered and handed on, never a panic (review B4): a
/// `retry-after-ms` of 10^25 and `quota_recheck_seconds = u64::MAX`.
#[tokio::test]
async fn a_retry_after_or_a_setting_past_the_clock_is_taken_as_it_is() {
    let (g, gw) = world().await;
    let mut s = g.state.snapshot().settings.clone();
    s.quota_recheck_seconds = u64::MAX;
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
    let _a = platform(&g, "a", 429, quota_body(), &[("retry-after-ms", "1e25")]).await;
    let _c = platform(&g, "c", 429, quota_body(), &[]).await;
    let _b = answering(&g, "b").await;
    set_fallback(&g.state, "models", "a", Some("b")).await;
    set_fallback(&g.state, "models", "c", Some("b")).await;
    for alias in ["a", "c"] {
        answered_along(
            ask(&gw, alias).await,
            alias,
            r#""b";reason=quota"#,
            "from the cloud",
        )
        .await;
        let up: i64 = sqlx::query_scalar("SELECT id FROM upstreams WHERE name = ?1")
            .bind(format!("platform-{alias}"))
            .fetch_one(&g.state.db)
            .await
            .unwrap();
        let e = g.state.upstream_quota.get(up).expect("remembered");
        assert_eq!(e.next_check, i64::MAX, "{alias}");
    }
}
