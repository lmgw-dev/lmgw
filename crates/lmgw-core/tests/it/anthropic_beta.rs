//! The client's `anthropic-beta` header reaches an Anthropic upstream — as
//! one header, merged with any the upstream row carries — from every
//! chat-shaped route, and goes nowhere else.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{serve, Gw};

/// One alias, `m`, on an upstream of `protocol` at `base` with the row's own
/// `extra_headers`.
async fn world(base: &str, protocol: Protocol, extra_headers: Vec<(String, String)>) -> Gw {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "beta-up".into(),
            protocol,
            kind: UpstreamKind::Generic,
            base_url: base.trim_end_matches('/').to_string(),
            api_key: Some("sk-up".into()),
            extra_headers,
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "m".into(),
            upstream_id: up_id,
            upstream_model_id: "tgt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    serve(state).await
}

/// An Anthropic provider: `/v1/messages` and its `count_tokens`.
async fn anthropic_mock() -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "model": "tgt", "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 1}
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 7})))
        .mount(&mock)
        .await;
    mock
}

/// Every `anthropic-beta` line of every request the mock received at `p`.
async fn betas_at(mock: &MockServer, p: &str) -> Vec<Vec<String>> {
    mock.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == p)
        .map(|r| {
            r.headers
                .get_all("anthropic-beta")
                .iter()
                .map(|v| v.to_str().unwrap().to_string())
                .collect()
        })
        .collect()
}

fn messages_body() -> Value {
    json!({"model": "m", "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]})
}

#[tokio::test]
async fn messages_forward_the_clients_flags_as_one_header() {
    let mock = anthropic_mock().await;
    let gw = world(&mock.uri(), Protocol::Anthropic, vec![]).await;

    // Two lines, a repeat and stray spaces: one line upstream, each flag once.
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .header(
            "anthropic-beta",
            "context-1m-2025-08-07, interleaved-thinking-2025-05-14",
        )
        .header("anthropic-beta", "context-1m-2025-08-07,effort-2025-11-24")
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    assert_eq!(
        betas_at(&mock, "/v1/messages").await,
        vec![vec![
            "context-1m-2025-08-07,interleaved-thinking-2025-05-14,effort-2025-11-24".to_string()
        ]]
    );
}

/// A row that already sends a beta (Settings → the upstream's extra headers)
/// keeps it; the client's are added to the same line, not a second one.
#[tokio::test]
async fn the_rows_own_beta_header_is_merged_not_doubled() {
    let mock = anthropic_mock().await;
    let gw = world(
        &mock.uri(),
        Protocol::Anthropic,
        vec![
            ("Anthropic-Beta".into(), "row-flag-1,shared-flag".into()),
            ("x-extra".into(), "kept".into()),
        ],
    )
    .await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "shared-flag,client-flag")
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // Without a client header the row's flags still go, alone.
    let resp = client
        .post(format!("{gw}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    assert_eq!(
        betas_at(&mock, "/v1/messages").await,
        vec![
            vec!["row-flag-1,shared-flag,client-flag".to_string()],
            vec!["row-flag-1,shared-flag".to_string()],
        ]
    );
    let received = mock.received_requests().await.unwrap();
    assert!(
        received.iter().all(|r| r.headers["x-extra"] == "kept"),
        "the row's other extra headers still go"
    );
}

/// The header names its protocol, not the client's dialect: an OpenAI-shaped
/// request to an Anthropic alias carries it too, and so do
/// `/v1/responses` and `/v1/messages/count_tokens`.
#[tokio::test]
async fn every_chat_shaped_route_forwards_it() {
    let mock = anthropic_mock().await;
    let gw = world(&mock.uri(), Protocol::Anthropic, vec![]).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .header("anthropic-beta", "from-chat")
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let resp = client
        .post(format!("{gw}/v1/responses"))
        .header("anthropic-beta", "from-responses")
        .json(&json!({"model": "m", "input": "hi", "store": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let resp = client
        .post(format!("{gw}/v1/messages/count_tokens"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "from-count")
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    assert_eq!(
        betas_at(&mock, "/v1/messages").await,
        vec![
            vec!["from-chat".to_string()],
            vec!["from-responses".to_string()]
        ]
    );
    assert_eq!(
        betas_at(&mock, "/v1/messages/count_tokens").await,
        vec![vec!["from-count".to_string()]]
    );
}

/// Another protocol has no such header: an Anthropic client on an
/// OpenAI-compatible alias sends none upstream.
#[tokio::test]
async fn a_non_anthropic_upstream_gets_no_beta_header() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "model": "tgt",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .expect(1)
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Openai, vec![]).await;

    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "context-1m-2025-08-07")
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        betas_at(&mock, "/chat/completions").await,
        vec![Vec::<String>::new()]
    );
}

/// A value that is not a header value is refused by name, not dropped.
#[tokio::test]
async fn an_unsendable_beta_value_is_a_400() {
    let mock = anthropic_mock().await;
    let gw = world(&mock.uri(), Protocol::Anthropic, vec![]).await;

    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .header(
            "anthropic-beta",
            reqwest::header::HeaderValue::from_bytes(b"flag-\xff").unwrap(),
        )
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error", "Anthropic's dialect: {body}");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("anthropic-beta"), "{msg}");
    assert!(
        betas_at(&mock, "/v1/messages").await.is_empty(),
        "nothing was sent"
    );
}
