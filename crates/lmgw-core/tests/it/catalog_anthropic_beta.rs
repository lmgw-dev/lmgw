//! An Anthropic row's catalog is read with the on-demand compaction beta
//! flag (gateway design §6.1); an Anthropic-compatible upstream that refuses
//! the flag with a 4xx is asked once more without it and keeps its catalog.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewUpstream};
use serde_json::json;
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BETA: &str = "compact-2026-09-04";

fn listing() -> serde_json::Value {
    json!({"data": [{"id": "claude-x", "type": "model"}], "has_more": false})
}

async fn read(mock: &MockServer) -> Result<Vec<String>, String> {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "anthropic-like".into(),
            protocol: Protocol::Anthropic,
            kind: UpstreamKind::Generic,
            base_url: mock.uri(),
            api_key: Some("k".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let up = state.snapshot().upstreams[&id].clone();
    lmgw_core::catalog::fetch_models(&state, &up)
        .await
        .map(|models| models.into_iter().map(|m| m.id).collect())
}

/// The beta flag in the request's headers, per request the mock got.
async fn betas(mock: &MockServer) -> Vec<Option<String>> {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            r.headers
                .get("anthropic-beta")
                .map(|v| v.to_str().unwrap().to_string())
        })
        .collect()
}

#[tokio::test]
async fn an_upstream_that_takes_the_flag_is_asked_once_with_it() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("anthropic-beta", BETA))
        .respond_with(ResponseTemplate::new(200).set_body_json(listing()))
        .mount(&mock)
        .await;
    assert_eq!(read(&mock).await.unwrap(), ["claude-x"]);
    assert_eq!(betas(&mock).await, [Some(BETA.to_string())]);
}

#[tokio::test]
async fn an_upstream_that_refuses_the_flag_is_asked_again_without_it() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header_exists("anthropic-beta"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": "unknown beta"},
        })))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(listing()))
        .mount(&mock)
        .await;
    assert_eq!(read(&mock).await.unwrap(), ["claude-x"]);
    assert_eq!(betas(&mock).await, [Some(BETA.to_string()), None]);
}

/// A refusal of the retry too is the read's error, after exactly one retry.
#[tokio::test]
async fn a_refused_retry_is_the_read_s_error() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&mock)
        .await;
    let err = read(&mock).await.unwrap_err();
    assert!(err.contains("401"), "{err}");
    assert_eq!(betas(&mock).await, [Some(BETA.to_string()), None]);
}
