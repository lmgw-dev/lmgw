//! The early refusal reads only cached catalogs (structured-output design
//! §11.21): a stale entry answers at once, a catalog never read is unknown,
//! and neither waits on the provider's model list, which here takes a minute.

use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::refusal;
use crate::config::{Protocol, UpstreamKind};
use crate::ir::FormatKind;
use crate::state::{AppState, SharedState};
use crate::store::{self, NewAlias, NewUpstream};

/// An Anthropic upstream named `name` at `base` with alias `name` onto
/// `tgt`; its id.
async fn anthropic_alias(state: &SharedState, name: &str, base: &str) -> i64 {
    let id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: name.into(),
            protocol: Protocol::Anthropic,
            kind: UpstreamKind::Generic,
            base_url: base.into(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 120_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: name.into(),
            upstream_id: id,
            upstream_model_id: "tgt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    id
}

fn catalog(delay: Duration) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(json!({
            "data": [{"id": "tgt", "type": "model", "display_name": "Example",
                      "capabilities": {"structured_outputs": {"supported": true}}}],
            "has_more": false
        }))
        .set_delay(delay)
}

/// What [`refusal`] answers, within a few seconds or not at all.
async fn judged(state: &SharedState, name: &str) -> Option<&'static str> {
    tokio::time::timeout(
        Duration::from_secs(5),
        refusal(state, name, FormatKind::JsonObject),
    )
    .await
    .expect("the early refusal waited on a catalog read")
    .map(|e| e.kind())
}

#[tokio::test]
async fn a_stale_catalog_answers_at_once_and_a_cold_one_is_unknown() {
    let state = AppState::init_for_tests().await.unwrap();

    // Read once, then stale; the provider now takes a minute to list.
    let warm = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(catalog(Duration::ZERO))
        .up_to_n_times(1)
        .mount(&warm)
        .await;
    let id = anthropic_alias(&state, "warm", &warm.uri()).await;
    let up = state.snapshot().upstreams[&id].clone();
    crate::catalog::upstream_models(&state, &up).await.unwrap();
    state.catalog.expire_for_tests(&up).await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(catalog(Duration::from_secs(60)))
        .mount(&warm)
        .await;
    // Anthropic has no JSON-object mode: the stale entry says so.
    assert_eq!(
        judged(&state, "warm").await,
        Some("response_format_unsupported")
    );

    // Never read: unknown, so the request goes (the egress backstop and the
    // upstream answer it).
    let cold = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(catalog(Duration::from_secs(60)))
        .mount(&cold)
        .await;
    anthropic_alias(&state, "cold", &cold.uri()).await;
    assert_eq!(judged(&state, "cold").await, None);
}
