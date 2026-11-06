//! An in-process unary turn whose platform answer is a 2xx whole Response
//! with `status: "failed"` (openai egress design §7.2.3): the caller gets
//! OpenAI's error, and the turn's row counts the usage and tier the
//! Response reported (decision 22).

use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::*;
use crate::store::{query_logs, LogFilter};

#[tokio::test]
async fn a_failed_whole_response_s_row_keeps_its_usage() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "resp_f", "object": "response", "status": "failed", "model": "gpt",
            "service_tier": "default", "output": [],
            "error": {"code": "server_error", "message": "The model broke."},
            "usage": {"input_tokens": 30, "output_tokens": 4, "total_tokens": 34},
        })))
        .mount(&mock)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    let route = Route {
        upstream: Upstream {
            id: 5,
            name: "platform".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: true,
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
        stream: false,
        passthrough: Default::default(),
        response_format: None,
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
        context_management: None,
        compaction: None,
        thinking_display: None,
    };
    let e = sample_once(
        &state,
        None,
        &route,
        None,
        &ir,
        "mcp-sampling",
        None,
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind(), "upstream");
    assert_eq!(
        e.to_string(),
        "the upstream's response failed: The model broke. (server_error)"
    );
    let rows = query_logs(
        &state.db,
        &LogFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let row = rows
        .iter()
        .find(|r| r.ingress_proto == "mcp-sampling")
        .expect("the turn's row");
    assert_eq!(row.status, 502, "{row:?}");
    assert_eq!(row.error_kind.as_deref(), Some("upstream"));
    assert_eq!(
        (row.prompt_tokens, row.completion_tokens),
        (Some(30), Some(4)),
        "{row:?}"
    );
    assert_eq!(row.service_tier.as_deref(), Some("default"), "{row:?}");
}
