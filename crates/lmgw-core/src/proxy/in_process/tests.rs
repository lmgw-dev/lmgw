//! A route the caller refuses ([`PerRoute::request`]) gets nothing: no chat
//! call and no catalog read for the reasoning fit, which would otherwise
//! run first (voice-audio-input WP2 review #8: a heard turn's audio refused
//! on a route the tool loop was re-routed to whose model cannot take it).

use std::time::Duration;

use wiremock::MockServer;

use super::*;
use crate::config::{Protocol, Upstream, UpstreamKind};
use crate::ir::{Message, ReasoningControl, Role, StreamDelta};
use crate::state::AppState;

/// Refuses every route, as the chat runner refuses one whose model cannot
/// take a heard turn's audio.
struct Refuses;

#[async_trait::async_trait]
impl PerRoute for Refuses {
    async fn request(
        &self,
        _route: &Route,
        _hold: Option<&crate::vram::LocalHold>,
        _rerouted: Option<&crate::gate::GateHeaders>,
        _ir: &ChatRequest,
    ) -> Result<Option<ChatRequest>, GatewayError> {
        Err(GatewayError::InvalidRequest {
            code: "audio_input_unsupported",
            message: "gpt does not take audio input".into(),
        })
    }
}

struct Discard;

impl crate::agent::DeltaSink for Discard {
    fn on_delta(&mut self, _: &StreamDelta) {}
}

#[tokio::test]
async fn a_route_the_caller_refuses_gets_no_catalog_read() {
    let mock = MockServer::start().await;
    let state = AppState::init_for_tests().await.unwrap();
    let route = Route {
        upstream: Upstream {
            id: 5,
            name: "cloud".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            llama: None,
        },
        upstream_model: "gpt".into(),
        param_defaults: Default::default(),
        fallback: None,
    };
    // Reasoning off, as a voice turn asks: the fit would read the catalog.
    let ir = ChatRequest {
        model_alias: "m".into(),
        messages: vec![Message::text(Role::User, "hello")],
        params: crate::ir::Params {
            reasoning: Some(ReasoningControl {
                enabled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        },
        tools: vec![],
        tool_choice: None,
        stream: true,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    };
    let refused = stream_once_on(
        &state,
        None,
        &route,
        None,
        &ir,
        "chat",
        KeyRef::default(),
        Duration::from_secs(5),
        &mut Discard,
        Some((&Refuses, None)),
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code(), "audio_input_unsupported");
    let reached: Vec<String> = mock
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect();
    assert!(reached.is_empty(), "{reached:?}");
}

/// Never answers, as a capability check waiting on a provider's catalog.
struct Hangs;

#[async_trait::async_trait]
impl PerRoute for Hangs {
    async fn request(
        &self,
        _route: &Route,
        _hold: Option<&crate::vram::LocalHold>,
        _rerouted: Option<&crate::gate::GateHeaders>,
        _ir: &ChatRequest,
    ) -> Result<Option<ChatRequest>, GatewayError> {
        std::future::pending().await
    }
}

/// A sink whose consumer stopped.
struct Stopped(crate::proxy::StopSignal);

impl crate::agent::DeltaSink for Stopped {
    fn on_delta(&mut self, _: &StreamDelta) {}
    fn stop(&self) -> Option<crate::proxy::StopSignal> {
        Some(self.0.clone())
    }
}

/// Voice-audio-input review V9: the caller's per-route check is raced
/// against the consumer's stop — a stop while it waits ends the call at
/// once, `canceled`, and nothing is sent.
#[tokio::test]
async fn a_stop_ends_a_call_whose_per_route_check_waits() {
    let mock = MockServer::start().await;
    let state = AppState::init_for_tests().await.unwrap();
    let route = Route {
        upstream: Upstream {
            id: 5,
            name: "cloud".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            llama: None,
        },
        upstream_model: "gpt".into(),
        param_defaults: Default::default(),
        fallback: None,
    };
    let ir = ChatRequest {
        model_alias: "m".into(),
        messages: vec![Message::text(Role::User, "hello")],
        params: Default::default(),
        tools: vec![],
        tool_choice: None,
        stream: true,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    };
    let (handle, signal) = crate::proxy::stop_pair();
    handle.stop();
    let ended = tokio::time::timeout(
        Duration::from_secs(5),
        stream_once_on(
            &state,
            None,
            &route,
            None,
            &ir,
            "chat",
            KeyRef::default(),
            Duration::from_secs(60),
            &mut Stopped(signal),
            Some((&Hangs, None)),
        ),
    )
    .await
    .expect("the stop ends it, not the check");
    let e = ended.unwrap_err();
    assert!(crate::proxy::is_canceled(&e), "{e}");
    assert!(mock
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}

/// Raises its own stop on the first delta — a consumer that stopped the
/// stream after the upstream answered — and keeps the cost its row told it
/// (`DeltaSink::billed_cost`).
struct StopsOnFirstDelta {
    handle: crate::proxy::StopHandle,
    signal: crate::proxy::StopSignal,
    cost: Option<crate::pricing::RowCost>,
}

impl StopsOnFirstDelta {
    fn new() -> Self {
        let (handle, signal) = crate::proxy::stop_pair();
        Self {
            handle,
            signal,
            cost: None,
        }
    }
}

impl crate::agent::DeltaSink for StopsOnFirstDelta {
    fn on_delta(&mut self, _: &StreamDelta) {
        self.handle.stop();
    }
    fn stop(&self) -> Option<crate::proxy::StopSignal> {
        Some(self.signal.clone())
    }
    fn billed_cost(&mut self, row: crate::pricing::RowCost) {
        self.cost = Some(row);
    }
}

/// The `requests` audit (billable-units §4.5): a stream its consumer
/// stopped after the upstream's 2xx counts its request, so a per-request
/// fee applies to its `canceled` row; one stopped before anything went out
/// stays unknown. The caller's meter is told the row's cost either way.
#[tokio::test]
async fn a_stream_stopped_after_its_answer_counts_its_request() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let mock = MockServer::start().await;
    let sse = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"choices": [{"delta": {"role": "assistant", "content": "Hallo"}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "stop"}],
                           "usage": {"prompt_tokens": 7, "completion_tokens": 3}}),
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&mock)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    crate::store::upsert_price(
        &state.db,
        crate::config::PriceScope::Alias,
        "m",
        crate::config::PriceUnit::PerRequest,
        &crate::pricing::Prices {
            source: crate::pricing::PriceSource::Manual,
            ..Default::default()
        },
        Some(0.005),
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let route = Route {
        upstream: Upstream {
            id: 5,
            name: "cloud".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            llama: None,
        },
        upstream_model: "gpt".into(),
        param_defaults: Default::default(),
        fallback: None,
    };
    let ir = ChatRequest {
        model_alias: "m".into(),
        messages: vec![Message::text(Role::User, "hello")],
        params: Default::default(),
        tools: vec![],
        tool_choice: None,
        stream: true,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    };
    let newest = || async {
        crate::store::query_logs(
            &state.db,
            &crate::store::LogFilter {
                alias: Some("m".into()),
                limit: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .remove(0)
    };

    let mut sink = StopsOnFirstDelta::new();
    let e = stream_once_on(
        &state,
        None,
        &route,
        None,
        &ir,
        "agent",
        KeyRef::default(),
        Duration::from_secs(5),
        &mut sink,
        None,
    )
    .await
    .unwrap_err();
    assert!(crate::proxy::is_canceled(&e), "{e}");
    let row = newest().await;
    assert_eq!(row.error_kind.as_deref(), Some("canceled"), "{row:?}");
    assert_eq!(row.status, 200);
    assert_eq!(
        row.cost_micro,
        Some(5_000),
        "answered, then stopped: {row:?}"
    );
    assert_eq!(row.cost_units_micro, Some(5_000));
    assert_eq!(
        sink.cost,
        Some(crate::pricing::RowCost::Priced(5_000)),
        "the meter is told the row's cost"
    );

    // Stopped before anything went out: nothing was answered.
    let mut sink = StopsOnFirstDelta::new();
    sink.handle.stop();
    let e = stream_once_on(
        &state,
        None,
        &route,
        None,
        &ir,
        "agent",
        KeyRef::default(),
        Duration::from_secs(5),
        &mut sink,
        None,
    )
    .await
    .unwrap_err();
    assert!(crate::proxy::is_canceled(&e), "{e}");
    let row = newest().await;
    assert_eq!(row.error_kind.as_deref(), Some("canceled"), "{row:?}");
    assert_eq!(row.cost_micro, None, "{row:?}");
    assert_eq!(row.price_per_request, Some(0.005));
    assert_eq!(
        sink.cost,
        Some(crate::pricing::RowCost::Unpriced),
        "unknown, and said so: a stop is an `ok` row, a gap in any total"
    );
}
