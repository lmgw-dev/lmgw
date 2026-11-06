//! Native `/v1/responses` (openai egress design §14.4): an exhausted quota
//! and an unreachable upstream on a platform row handed to the fallback,
//! which is served as if the client had named it — here a compatible row,
//! so the synthesized loop.

use super::*;

/// A platform alias `alias` whose `/responses` answers `status` and `body`.
async fn native(g: &Gpu, alias: &str, status: u16, body: Value) -> MockServer {
    let mock = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&mock)
        .await;
    upstream_alias(g, alias, &mock.uri(), Protocol::Openai).await;
    mock
}

async fn respond(gw: &Gw, model: &str) -> reqwest::Response {
    gw.client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": model, "input": "hi"}))
        .send()
        .await
        .unwrap()
}

/// The text a Response object answered with.
async fn answered_text(resp: reqwest::Response) -> String {
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    let v: Value = resp.json().await.unwrap();
    v["output"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|o| o["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["text"].as_str().map(String::from))
        .collect()
}

#[tokio::test]
async fn a_native_quota_is_answered_by_the_fallback_and_remembered() {
    let (g, gw) = world().await;
    let a = native(&g, "a", 429, quota_body()).await;
    g.cloud("b", None).await;
    set_fallback(&g.state, "models", "a", Some("b")).await;

    let resp = respond(&gw, "a").await;
    assert_eq!(h(&resp, "x-lmgw-fallback").as_deref(), Some("b"));
    assert_eq!(
        h(&resp, "x-lmgw-fallback-chain").as_deref(),
        Some(r#""b";reason=quota"#)
    );
    assert_eq!(answered_text(resp).await, "from the cloud");
    let all = rows(&g, "a", 2).await;
    assert_eq!(all[0].status, 429, "{:?}", all[0]);
    assert_eq!(all[0].error_kind.as_deref(), Some("insufficient_quota"));
    assert!(all[0]
        .error_msg
        .as_deref()
        .unwrap()
        .contains("; handed to fallback 'b' (quota)"));
    assert_eq!(
        all[1].fallback_reason.as_deref(),
        Some("quota"),
        "{:?}",
        all[1]
    );

    // Remembered: the next run never reaches the platform — not even for
    // the facts an off is fitted by, which only a hop that sends reads.
    let resp = respond(&gw, "a").await;
    assert_eq!(h(&resp, "x-lmgw-fallback").as_deref(), Some("b"));
    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .header("x-lmgw-reasoning", "off")
        .json(&json!({"model": "a", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(h(&resp, "x-lmgw-fallback").as_deref(), Some("b"));
    let reached: Vec<String> = a
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect();
    assert_eq!(reached, ["POST /responses"]);
}

/// Without a fallback the quota stands, its note naming where one would be
/// set.
#[tokio::test]
async fn without_a_fallback_the_native_quota_stands() {
    let (g, gw) = world().await;
    let _a = native(&g, "a", 429, quota_body()).await;
    let resp = respond(&gw, "a").await;
    assert_eq!(resp.status(), 429);
    let v: Value = resp.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.ends_with("'a' has no fallback configured"), "{msg}");
}

/// A platform row nothing answers on: its fallback answers, reason
/// `unreachable`.
#[tokio::test]
async fn an_unreachable_native_upstream_is_answered_by_its_fallback() {
    let (g, gw) = world().await;
    let (port, _held) = crate::common::refusing_port();
    upstream_alias(
        &g,
        "a",
        &format!("http://127.0.0.1:{port}"),
        Protocol::Openai,
    )
    .await;
    g.cloud("b", None).await;
    set_fallback(&g.state, "models", "a", Some("b")).await;

    let resp = respond(&gw, "a").await;
    assert_eq!(
        h(&resp, "x-lmgw-fallback-reason").as_deref(),
        Some("unreachable")
    );
    assert_eq!(answered_text(resp).await, "from the cloud");
    let all = rows(&g, "a", 2).await;
    assert_eq!(all[0].status, 502, "{:?}", all[0]);
    assert_eq!(all[0].error_kind.as_deref(), Some("unreachable"));
}

/// A quota whose fallback cannot be tried after all (here one written past
/// the save check: a name that resolves to nothing) leaves one row, the
/// hop's error with the clause — not a "handed to fallback" row beside it
/// (review B5).
#[tokio::test]
async fn a_walk_that_ends_leaves_the_one_row() {
    let (g, gw) = world().await;
    let _a = native(&g, "a", 429, quota_body()).await;
    set_fallback(&g.state, "models", "a", Some("gone")).await;
    let resp = respond(&gw, "a").await;
    assert_eq!(resp.status(), 429);
    let v: Value = resp.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("(fallback 'gone' does not resolve)"), "{msg}");
    let all = rows(&g, "a", 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let all_now = rows(&g, "a", 1).await;
    assert_eq!(all_now.len(), 1, "{all_now:#?}");
    assert!(
        !all[0]
            .error_msg
            .as_deref()
            .unwrap()
            .contains("handed to fallback"),
        "{:?}",
        all[0]
    );
}

/// Along a chain, a native hop without a fallback names the chain it was
/// tried along (§14.8, review B5): "'b' has no fallback configured (tried:
/// a → b)".
#[tokio::test]
async fn a_native_quota_at_the_chains_end_names_the_chain() {
    let (g, gw) = world().await;
    let _a = native(&g, "a", 429, quota_body()).await;
    let _b = native(&g, "b", 429, quota_body()).await;
    set_fallback(&g.state, "models", "a", Some("b")).await;
    let resp = respond(&gw, "a").await;
    assert_eq!(resp.status(), 429);
    let v: Value = resp.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.ends_with("'b' has no fallback configured (tried: a → b)"),
        "{msg}"
    );
}

/// A refused parameter is retried before any hand-on decision (api-compat
/// design §9): the retry without it meets the exhausted quota, which hands
/// the request on as any send's would. The fallback gets the client's body
/// and answers under its own headers, naming nothing the hop left out.
#[tokio::test]
async fn a_refused_parameter_is_retried_before_the_hop_hands_on() {
    let (g, gw) = world().await;
    let mock = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(wiremock::matchers::body_partial_json(
            json!({"temperature": 0.5}),
        ))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "message": "Unsupported parameter: 'temperature' is not supported with this model.",
            "type": "invalid_request_error", "param": "temperature", "code": null}})))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(429).set_body_json(quota_body()))
        .with_priority(2)
        .mount(&mock)
        .await;
    upstream_alias(&g, "a", &mock.uri(), Protocol::Openai).await;
    g.cloud("b", None).await;
    set_fallback(&g.state, "models", "a", Some("b")).await;

    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": "a", "input": "hi", "temperature": 0.5}))
        .send()
        .await
        .unwrap();
    assert_eq!(h(&resp, "x-lmgw-fallback").as_deref(), Some("b"));
    assert_eq!(h(&resp, "x-lmgw-params-refused"), None);
    assert_eq!(answered_text(resp).await, "from the cloud");
    let sent = mock.received_requests().await.unwrap();
    assert_eq!(sent.len(), 2, "the refusal, then the retry");
    let retry: Value = serde_json::from_slice(&sent[1].body).unwrap();
    assert!(retry.get("temperature").is_none(), "{retry}");
    let all = rows(&g, "a", 3).await;
    assert_eq!(all[0].status, 400, "{:?}", all[0]);
    assert!(all[0]
        .error_msg
        .as_deref()
        .unwrap()
        .contains("the model refused temperature; lmgw retried once without it"));
    assert_eq!(all[1].status, 429, "{:?}", all[1]);
    assert!(all[1]
        .error_msg
        .as_deref()
        .unwrap()
        .contains("; handed to fallback 'b' (quota)"));
    assert_eq!(all[2].fallback_reason.as_deref(), Some("quota"));
    assert_eq!(
        g.cloud_bodies().await[0]["temperature"],
        0.5,
        "the fallback gets the client's body"
    );
}

/// A refused off is retried before any hand-on decision too
/// (model-capabilities design §5.6): the retry at the model's least level
/// meets the exhausted quota and is handed on. The fallback answers under
/// its own headers: the hop's fitted off is not named on it.
#[tokio::test]
async fn a_refused_off_is_retried_before_the_hop_hands_on() {
    let (g, gw) = world().await;
    let mock = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(wiremock::matchers::body_partial_json(
            json!({"reasoning": {"effort": "none"}}),
        ))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "message": "Unsupported value: 'reasoning.effort' does not support 'none' with \
                        this model. Supported values are: 'minimal', 'low', 'medium', and \
                        'high'.",
            "type": "invalid_request_error", "param": "reasoning.effort",
            "code": "unsupported_value"}})))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(429).set_body_json(quota_body()))
        .with_priority(2)
        .mount(&mock)
        .await;
    upstream_alias(&g, "a", &mock.uri(), Protocol::Openai).await;
    g.cloud("b", None).await;
    set_fallback(&g.state, "models", "a", Some("b")).await;

    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .header("x-lmgw-reasoning", "off")
        .json(&json!({"model": "a", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(h(&resp, "x-lmgw-fallback").as_deref(), Some("b"));
    assert_eq!(h(&resp, "x-lmgw-reasoning-ignored"), None);
    assert_eq!(answered_text(resp).await, "from the cloud");
    // The fit's catalog lookup is a GET of its own.
    let sent: Vec<_> = mock
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/responses")
        .collect();
    assert_eq!(sent.len(), 2, "the refusal, then the retry");
    let retry: Value = serde_json::from_slice(&sent[1].body).unwrap();
    assert_eq!(retry["reasoning"]["effort"], "minimal", "{retry}");
    let all = rows(&g, "a", 3).await;
    assert_eq!(all[0].status, 400, "{:?}", all[0]);
    assert!(all[0]
        .error_msg
        .as_deref()
        .unwrap()
        .contains(r#"lmgw retried once with reasoning.effort: "minimal""#));
    assert_eq!(all[1].status, 429, "{:?}", all[1]);
    assert!(all[1]
        .error_msg
        .as_deref()
        .unwrap()
        .contains("; handed to fallback 'b' (quota)"));
    assert_eq!(all[2].fallback_reason.as_deref(), Some("quota"));
}
