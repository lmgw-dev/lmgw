use std::time::Instant;

use super::*;
use crate::config::{Protocol, UpstreamKind};
use crate::ir::{ChatRequest, StreamDelta, Timings, Usage};
use crate::ir::{ContentPart, FinishReason, Message, Role};
use crate::state::AppState;
use crate::store::{insert_alias, insert_upstream, query_logs, LogFilter, NewAlias, NewUpstream};
use crate::telemetry::RequestClass;
use crate::telemetry::TelemetryBus;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn usage_delta(completion_tokens: u64) -> StreamDelta {
    StreamDelta::Usage(Usage {
        prompt_tokens: Some(7),
        completion_tokens: Some(completion_tokens),
        ..Default::default()
    })
}

fn timings_delta(predicted_n: u64) -> StreamDelta {
    StreamDelta::Timings(Timings {
        prompt_n: 7,
        prompt_ms: 1.0,
        prompt_per_second: 7.0,
        predicted_n,
        predicted_ms: 10.0,
        predicted_per_second: 30.0,
        cache_n: None,
        draft_n: None,
        draft_n_accepted: None,
    })
}

/// How many output tokens a batch sequence publishes to the live rate.
fn published(batches: &[Vec<StreamDelta>]) -> u64 {
    let bus = TelemetryBus::new();
    let mut p = StreamProgress::default();
    for b in batches {
        p.observe(b, &bus);
    }
    p.counted
}

/// The fallback source: upstreams that only stream text (the OpenAI shape)
/// are counted one token per non-empty content chunk — near-exact against
/// llama-server, which emits a chunk per token. Empty chunks and non-content
/// frames (the `role` opener, `Stop`) must not inflate it.
#[test]
fn stream_progress_counts_one_token_per_content_chunk() {
    assert_eq!(
        published(&[
            vec![StreamDelta::TextDelta("Hel".into())],
            vec![StreamDelta::TextDelta("lo".into())],
            vec![StreamDelta::TextDelta(String::new())],
            vec![StreamDelta::ReasoningDelta("hmm".into())],
            vec![StreamDelta::ToolCallArgsDelta {
                index: 0,
                fragment: "{\"a\":".into(),
            }],
            vec![StreamDelta::Stop(FinishReason::Stop)],
        ]),
        4,
        "3 content chunks + 1 tool-args chunk; the empty one and Stop don't count"
    );
}

/// The preferred source: a cumulative count reported *while generating*
/// (Anthropic `message_delta`, Gemini `usageMetadata`, llama.cpp per-token
/// `timings`) replaces the chunk approximation without double counting, and
/// never rewinds it when it lags behind.
#[test]
fn stream_progress_prefers_live_exact_counts() {
    assert_eq!(
        published(&[
            vec![StreamDelta::TextDelta("a".into())],
            vec![StreamDelta::TextDelta("b".into()), usage_delta(40)],
            vec![StreamDelta::TextDelta("c".into())],
            vec![timings_delta(60)],
        ]),
        60,
        "exact counts win and are cumulative, not additive"
    );
    assert_eq!(
        published(&[
            vec![StreamDelta::TextDelta("a".into())],
            vec![StreamDelta::TextDelta("b".into())],
            vec![usage_delta(1)],
        ]),
        2,
        "a lagging exact count must not rewind the total"
    );
}

/// The terminal frame's usage is deliberately ignored: on the common
/// upstream that reports tokens only at the end, adopting it would dump the
/// whole correction into one instant and spike the displayed rate just as
/// the stream finishes. Whether it rides along with `Stop` (Anthropic's
/// `message_delta`, llama.cpp's final `timings`) or arrives in a chunk after
/// it (OpenAI `stream_options.include_usage`), it is out.
#[test]
fn stream_progress_ignores_the_terminal_usage_correction() {
    assert_eq!(
        published(&[
            vec![StreamDelta::TextDelta("a".into())],
            vec![usage_delta(300), StreamDelta::Stop(FinishReason::Stop)],
        ]),
        1,
    );
    assert_eq!(
        published(&[
            vec![StreamDelta::TextDelta("a".into())],
            vec![StreamDelta::Stop(FinishReason::Stop)],
            vec![usage_delta(300)],
        ]),
        1,
    );
    assert_eq!(
        published(&[
            vec![StreamDelta::TextDelta("a".into())],
            vec![StreamDelta::Error("upstream died".into())],
            vec![StreamDelta::TextDelta("b".into())],
        ]),
        1,
        "nothing after a mid-stream error is live generation",
    );
}

/// Each stream publishes the *difference* since its last report, so two
/// concurrent streams — one chunk-counted, one reporting exact cumulative
/// counts — add up on the shared counter instead of clobbering each other.
#[test]
fn stream_progress_publishes_deltas_to_the_shared_bus() {
    let bus = TelemetryBus::new();
    let (mut a, mut b) = (StreamProgress::default(), StreamProgress::default());
    for _ in 0..10 {
        a.observe(&[StreamDelta::TextDelta("x".into())], &bus);
        b.observe(&[usage_delta(b.counted + 3)], &bus);
    }
    assert_eq!(a.counted, 10, "one per content chunk");
    assert_eq!(b.counted, 30, "cumulative exact counts, 3 at a time");
    assert_eq!(
        bus.stream_tokens_total(),
        40,
        "both streams' deltas land on the one counter, summed not overwritten"
    );
}

/// End-to-end observability test for the §8 sampling helper: drive
/// `sample_once` against a mock OpenAI upstream and assert it (a) returns the
/// completion and (b) writes a `request_logs` row with
/// `ingress_proto = "mcp-sampling"` and the upstream's **real** token counts
/// — the concrete proof of the §8 "logs like any request" / §10
/// "counts in stats" claims. This exercises the same path
/// `GatewayClientHandler::create_message` uses, without needing a live
/// southbound MCP server.
#[tokio::test]
async fn sample_once_writes_mcp_sampling_log_with_real_tokens() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "sampled answer" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 11, "completion_tokens": 7 }
        })))
        .mount(&mock)
        .await;

    let state = AppState::init_for_tests().await.unwrap();
    let up_id = insert_upstream(
        &state.db,
        &NewUpstream {
            supports_responses: false,
            name: "sampling-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: mock.uri(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
        },
    )
    .await
    .unwrap();
    insert_alias(
        &state.db,
        &NewAlias {
            alias: "sampler".into(),
            upstream_id: up_id,
            upstream_model_id: "target-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let route = state.snapshot().resolve("sampler").unwrap();
    let ir = ChatRequest {
        model_alias: "sampler".into(),
        messages: vec![Message::text(Role::User, "hello")],
        params: Default::default(),
        tools: vec![],
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    };

    let completion = sample_once(
        &state,
        None,
        &route,
        None,
        &ir,
        "mcp-sampling",
        None,
        std::time::Duration::from_secs(30),
    )
    .await
    .expect("sample_once should succeed against the mock");

    // The completion came back with the upstream's text + usage.
    let text: String = completion
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "sampled answer");
    assert_eq!(completion.usage.prompt_tokens, Some(11));
    assert_eq!(completion.usage.completion_tokens, Some(7));

    // Observability claim (§8): a request_logs row exists, labeled
    // `mcp-sampling`, with the real token counts and a 200 status.
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
        .expect("a mcp-sampling request_logs row must be written");
    assert_eq!(row.requested_alias, "sampler");
    assert_eq!(row.upstream_name.as_deref(), Some("sampling-up"));
    assert_eq!(row.upstream_model.as_deref(), Some("target-model"));
    assert_eq!(row.status, 200);
    assert_eq!(row.prompt_tokens, Some(11));
    assert_eq!(row.completion_tokens, Some(7));
    assert!(row.error_kind.is_none());
    // It is NOT an mcp tools/call row — mcp_tool stays NULL (it's LLM traffic).
    assert!(row.mcp_tool.is_none());

    // §10 inverse of the M3 `mcp` exclusion: `mcp-sampling` DOES count in the
    // telemetry aggregates (real LLM traffic, real tokens).
    let stats = state.telemetry.stats();
    assert_eq!(stats.total_requests, 1, "mcp-sampling must count in stats");
    assert_eq!(stats.total_errors, 0);
    assert_eq!(stats.prompt_tokens, 11);
    assert_eq!(stats.completion_tokens, 7);
}

/// A failing upstream still writes a (non-200) `mcp-sampling` row and returns
/// an error — sampling errors are observable and surface to the caller (§14).
#[tokio::test]
async fn sample_once_logs_failures() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&mock)
        .await;

    let state = AppState::init_for_tests().await.unwrap();
    let up_id = insert_upstream(
        &state.db,
        &NewUpstream {
            supports_responses: false,
            name: "down-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: mock.uri(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
        },
    )
    .await
    .unwrap();
    insert_alias(
        &state.db,
        &NewAlias {
            alias: "dead".into(),
            upstream_id: up_id,
            upstream_model_id: "m".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let route = state.snapshot().resolve("dead").unwrap();
    let ir = ChatRequest {
        model_alias: "dead".into(),
        messages: vec![Message::text(Role::User, "hi")],
        params: Default::default(),
        tools: vec![],
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    };

    let err = sample_once(
        &state,
        None,
        &route,
        None,
        &ir,
        "mcp-sampling",
        None,
        std::time::Duration::from_secs(30),
    )
    .await;
    assert!(err.is_err(), "a 500 upstream must surface as an error");

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
        .expect("a failed sampling call still writes a row");
    assert!(row.status >= 400, "a failure must log a non-200 status");
    assert!(row.error_kind.is_some());
}

/// M4-review regression guard (§8 "one path instead of three copies"). The
/// Chat tab and Workflows classifier both log through `record_in_process`
/// (the DRY migration off their old `record_chat_call`/`record_llm_call`
/// copies). This asserts a migrated `"chat"`/`"workflow"` row still carries
/// the expected shape — `ingress_proto`, `streamed`, `status`, real tokens,
/// upstream labels, and a NULL `mcp_tool` (it's LLM traffic, not a
/// `tools/call`). Cheap insurance the shared recorder stays
/// behavior-preserving. Mirrors `sample_once_writes_mcp_sampling_log_with_real_tokens`.
#[tokio::test]
async fn record_in_process_chat_and_workflow_rows_have_expected_shape() {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = insert_upstream(
        &state.db,
        &NewUpstream {
            supports_responses: false,
            name: "inproc-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: "http://127.0.0.1:1".into(), // never dialed; we log directly
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
        },
    )
    .await
    .unwrap();
    insert_alias(
        &state.db,
        &NewAlias {
            alias: "inproc".into(),
            upstream_id: up_id,
            upstream_model_id: "the-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let route = state.snapshot().resolve("inproc").unwrap();

    // A "chat" row: streamed = true (the chat path is SSE), real tokens, 200.
    record_in_process(
        InProcessLog {
            key: KeyRef::default(),
            ingress_proto: "chat",
            alias: "inproc",
            route: &route,
            started: Instant::now(),
            streamed: true,
            class: RequestClass::Chat,
            timings: None,
            max_tokens_clamped: None,
            fallback: None,
            rung: None,
            degraded: None,
        },
        200,
        Some(12),
        Usage {
            prompt_tokens: Some(31),
            completion_tokens: Some(9),
            ..Default::default()
        },
        None,
        &state,
    )
    .await;

    // A "workflow" row: non-streamed, an error, to cover that branch too.
    record_in_process(
        InProcessLog {
            key: KeyRef::named(Some("key-a".into())),
            ingress_proto: "workflow",
            alias: "inproc",
            route: &route,
            started: Instant::now(),
            streamed: false,
            class: RequestClass::Chat,
            timings: None,
            max_tokens_clamped: None,
            fallback: None,
            rung: None,
            degraded: None,
        },
        500,
        None,
        Usage::default(),
        Some(("upstream_error", "boom".into())),
        &state,
    )
    .await;

    let rows = query_logs(
        &state.db,
        &LogFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let chat = rows
        .iter()
        .find(|r| r.ingress_proto == "chat")
        .expect("a chat row must be written");
    assert!(chat.streamed, "chat is the SSE path → streamed = true");
    assert_eq!(chat.status, 200);
    assert_eq!(chat.requested_alias, "inproc");
    assert_eq!(chat.upstream_name.as_deref(), Some("inproc-up"));
    assert_eq!(chat.upstream_model.as_deref(), Some("the-model"));
    assert_eq!(chat.egress_proto.as_deref(), Some("openai"));
    assert_eq!(chat.prompt_tokens, Some(31));
    assert_eq!(chat.completion_tokens, Some(9));
    assert!(chat.error_kind.is_none());
    // It's LLM traffic, not a tools/call — mcp_tool stays NULL.
    assert!(chat.mcp_tool.is_none());

    let wf = rows
        .iter()
        .find(|r| r.ingress_proto == "workflow")
        .expect("a workflow row must be written");
    assert!(!wf.streamed, "this workflow call was non-streamed");
    assert_eq!(wf.status, 500);
    assert_eq!(wf.error_kind.as_deref(), Some("upstream_error"));
    assert_eq!(wf.client_key.as_deref(), Some("key-a"));
    assert!(wf.mcp_tool.is_none());

    // Both are real LLM traffic → they count in stats (unlike `mcp` rows).
    let stats = state.telemetry.stats();
    assert_eq!(stats.total_requests, 2, "chat + workflow both count");
    assert_eq!(stats.total_errors, 1, "the 500 workflow row is an error");
    assert_eq!(stats.prompt_tokens, 31);
    assert_eq!(stats.completion_tokens, 9);
}

/// The in-process single call (`sample_once`: agent runs, knowledge and
/// quickdoc helpers) on a fallback that cannot see sends the placeholder
/// (`gate::fallback_images`, decided in `fit_chat` on every send): the
/// route comes marked from `Snapshot::usable_fallback`, as every fallback's.
#[tokio::test]
async fn sample_once_on_a_blind_fallback_sends_the_placeholder() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-1", "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1}
        })))
        .mount(&mock)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = insert_upstream(
        &state.db,
        &NewUpstream {
            supports_responses: false,
            name: "blind-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: mock.uri(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
        },
    )
    .await
    .unwrap();
    insert_alias(
        &state.db,
        &NewAlias {
            alias: "blind".into(),
            upstream_id: up_id,
            upstream_model_id: "text-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(serde_json::json!({"capabilities": {
                "task": "chat", "endpoints": ["/v1/chat/completions"],
                "vision": false, "source": "owner",
            }})),
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let route = state.snapshot().usable_fallback("blind").unwrap();
    assert_eq!(route.fallback.as_deref(), Some("blind"));
    let ir = ChatRequest {
        model_alias: "local".into(),
        messages: vec![Message {
            role: Role::User,
            content: vec![
                ContentPart::text("what is this"),
                ContentPart::Image {
                    mime: "image/png".into(),
                    source: crate::ir::ImageSource::Base64 {
                        data: "iVBORw0KGgo=".into(),
                    },
                },
            ],
        }],
        params: Default::default(),
        tools: vec![],
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    };
    sample_once(
        &state,
        None,
        &route,
        Some(crate::gate::FallbackReason::Hold),
        &ir,
        "agent",
        None,
        std::time::Duration::from_secs(30),
    )
    .await
    .expect("the fallback answers");
    let reqs = mock.received_requests().await.unwrap();
    let sent = reqs
        .iter()
        .rev()
        .find(|r| r.url.path() == "/chat/completions")
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .expect("a chat call");
    assert!(
        sent.contains("omitted: the answering model cannot see images"),
        "{sent}"
    );
    assert!(!sent.contains("iVBORw0KGgo="), "{sent}");
}
