//! A unary in-process call dropped before its answer (document-conversion
//! WP6 review S8): until 2026-11-19 `sample_once` opened the in-flight gauge
//! and nothing closed it when the call's future was dropped, so every
//! cancelled call left the titlebar's count one too high for good. The
//! guard closes it for every caller; the `499` row is for the callers that
//! ask ([`Dropped::Row`]).

use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::*;
use crate::store::{query_logs, LogFilter};

/// An upstream that answers after five seconds, and a route to it.
async fn slow_route() -> (MockServer, Route) {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "id": "c", "object": "chat.completion", "created": 0, "model": "m",
                    "choices": [{"index": 0, "finish_reason": "stop",
                                 "message": {"role": "assistant", "content": "late"}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                }))
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&mock)
        .await;
    let route = Route {
        upstream: Upstream {
            id: 5,
            name: "example-up".into(),
            protocol: Protocol::OpenaiCompatible,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            llama: None,
        },
        upstream_model: "m".into(),
        param_defaults: Default::default(),
        fallback: None,
    };
    (mock, route)
}

fn hello() -> ChatRequest {
    ChatRequest {
        model_alias: "example-model".into(),
        messages: vec![Message::text(Role::User, "hello")],
        params: Default::default(),
        tools: vec![],
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        response_format: None,
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
        context_management: None,
        compaction: None,
        thinking_display: None,
        cache_control: None,
    }
}

async fn rows(state: &SharedState, proto: &str) -> Vec<crate::store::RequestLogRow> {
    let all = query_logs(
        &state.db,
        &LogFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    all.into_iter()
        .filter(|r| r.ingress_proto == proto)
        .collect()
}

/// Every caller: the gauge is closed, and (as before) no row is written.
#[tokio::test]
async fn a_dropped_call_closes_the_gauge_it_opened() {
    let (_mock, route) = slow_route().await;
    let state = AppState::init_for_tests().await.unwrap();
    let ir = hello();
    let call = sample_once(
        &state,
        None,
        &route,
        None,
        &ir,
        "mcp-sampling",
        None,
        Duration::from_secs(30),
    );
    // The caller gives up while the upstream still works on it.
    let dropped = tokio::time::timeout(Duration::from_millis(400), call).await;
    assert!(dropped.is_err(), "the call was to be cut mid-flight");
    assert_eq!(state.telemetry.stats().active_requests, 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(rows(&state, "mcp-sampling").await.is_empty());
    assert_eq!(state.telemetry.stats().active_requests, 0);
}

/// A caller that asks for the row: `499`, under its key, no tokens.
#[tokio::test]
async fn a_dropped_call_leaves_its_row_when_asked_to() {
    let (_mock, route) = slow_route().await;
    let state = AppState::init_for_tests().await.unwrap();
    let ir = hello();
    let call = sample_once_dropping(
        &state,
        None,
        &route,
        None,
        &ir,
        "documents-page",
        KeyRef::named(Some("example-agent".into())),
        Duration::from_secs(30),
        None,
        None,
        Dropped::Row,
    );
    assert!(tokio::time::timeout(Duration::from_millis(400), call)
        .await
        .is_err());
    let mut written = Vec::new();
    for _ in 0..300 {
        written = rows(&state, "documents-page").await;
        if !written.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(written.len(), 1, "{written:?}");
    let row = &written[0];
    assert_eq!(
        (row.status, row.error_kind.as_deref()),
        (499, Some("client_disconnected"))
    );
    assert_eq!(row.client_key.as_deref(), Some("example-agent"));
    assert_eq!(row.requested_alias, "example-model");
    assert_eq!((row.prompt_tokens, row.cost_micro), (None, None), "{row:?}");
    assert_eq!(state.telemetry.stats().active_requests, 0);

    // A call that is answered writes its own row, and the guard none.
    let (quick, mut route) = slow_route().await;
    quick.reset().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&quick)
        .await;
    route.upstream.id = 6;
    let failed = sample_once_dropping(
        &state,
        None,
        &route,
        None,
        &ir,
        "documents-page",
        KeyRef::named(Some("example-agent".into())),
        Duration::from_secs(30),
        None,
        None,
        Dropped::Row,
    )
    .await;
    assert!(failed.is_err());
    tokio::time::sleep(Duration::from_millis(200)).await;
    let all = rows(&state, "documents-page").await;
    assert_eq!(all.len(), 2, "{all:?}");
    assert_eq!(all.iter().filter(|r| r.status == 499).count(), 1);
    assert_eq!(state.telemetry.stats().active_requests, 0);
}
