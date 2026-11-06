//! A catalog (models-list) read is bounded by its upstream's own
//! `timeout_ms`, the bound its sends have, and by no constant of lmgw's
//! (the owner's answer of 2026-10-25, openai egress design §14.2): a short
//! timeout ends a slow read, a longer one lets it answer.

use std::time::{Duration, Instant};

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewUpstream};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// How long the mock takes to list its models.
const SLOW: Duration = Duration::from_millis(700);

/// An `openai_compatible` row with `timeout_ms` in front of a mock that lists
/// one model after [`SLOW`].
async fn slow_catalog(timeout_ms: u64) -> (MockServer, SharedState, i64) {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"object": "list", "data": [{"id": "m1", "object": "model"}]}))
                .set_delay(SLOW),
        )
        .mount(&mock)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "slow".into(),
            protocol: Protocol::OpenaiCompatible,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms,
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
    (mock, state, id)
}

#[tokio::test]
async fn a_catalog_read_ends_at_the_upstream_s_own_timeout() {
    let (_mock, state, id) = slow_catalog(100).await;
    let up = state.snapshot().upstreams[&id].clone();
    let started = Instant::now();
    let read = lmgw_core::catalog::fetch_models(&state, &up).await;
    assert!(read.is_err(), "{read:?}");
    assert!(
        started.elapsed() < SLOW,
        "ended at the row's 100 ms, not at the answer: {:?}",
        started.elapsed()
    );
}

/// A timeout longer than the answer takes, and none at all (`0`), let it
/// answer.
#[tokio::test]
async fn a_longer_timeout_or_none_lets_the_catalog_answer() {
    for timeout_ms in [5_000, 0] {
        let (_mock, state, id) = slow_catalog(timeout_ms).await;
        let up = state.snapshot().upstreams[&id].clone();
        let models = lmgw_core::catalog::fetch_models(&state, &up)
            .await
            .unwrap_or_else(|e| panic!("{timeout_ms}: {e}"));
        assert_eq!(models.len(), 1, "{timeout_ms}");
        assert_eq!(models[0].id, "m1");
    }
}

/// `/v1/models` reads its expose-all upstreams' catalogs at once: two slow
/// ones cost the listing one wait, not two.
#[tokio::test]
async fn v1_models_reads_its_upstreams_at_once() {
    let state = AppState::init_for_tests().await.unwrap();
    let mut mocks = Vec::new();
    for (name, model) in [("slow-a", "ma"), ("slow-b", "mb")] {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"object": "list", "data": [{"id": model}]}))
                    .set_delay(SLOW),
            )
            .mount(&mock)
            .await;
        store::insert_upstream(
            &state.db,
            &NewUpstream {
                name: name.into(),
                protocol: Protocol::OpenaiCompatible,
                kind: UpstreamKind::Generic,
                base_url: format!("{}/v1", mock.uri()),
                api_key: None,
                extra_headers: vec![],
                timeout_ms: 5_000,
                enabled: true,
                expose_all: true,
                expose_prefix: String::new(),
                supports_responses: false,
                catalog_all_modalities: true,
            },
        )
        .await
        .unwrap();
        mocks.push(mock);
    }
    state.reload_snapshot().await.unwrap();
    let gw = crate::common::serve(state).await;
    let started = Instant::now();
    let listed: serde_json::Value = gw
        .client()
        .get(format!("{gw}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let elapsed = started.elapsed();
    let names: Vec<&str> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["id"].as_str())
        .collect();
    assert!(names.contains(&"ma") && names.contains(&"mb"), "{listed}");
    assert!(elapsed < SLOW * 2, "one wait, not two: {elapsed:?}");
}

/// The dashboard's upstream Test reads the catalog under the same bound:
/// the row's `timeout_ms` ends it, a longer one or none lets it connect.
#[tokio::test]
async fn the_upstream_test_s_catalog_read_has_the_row_s_timeout() {
    for (timeout_ms, connects) in [(100, false), (5_000, true), (0, true)] {
        let (_mock, state, id) = slow_catalog(timeout_ms).await;
        let gw = crate::common::serve(state).await;
        let started = Instant::now();
        let resp = gw
            .client()
            .post(format!("{gw}/api/op/upstream_test"))
            .json(&json!({"id": id}))
            .send()
            .await
            .unwrap();
        let elapsed = started.elapsed();
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap();
        assert_eq!(status == 200, connects, "{timeout_ms}: {status} {body}");
        if !connects {
            assert!(elapsed < SLOW, "ended at the row's 100 ms: {elapsed:?}");
        }
    }
}
