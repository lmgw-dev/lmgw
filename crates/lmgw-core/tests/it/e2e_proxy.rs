//! End-to-end proxy tests (§15) against a wiremock upstream: translated
//! output in both directions, a correct `request_logs` row, and a live-feed
//! telemetry event.

use lmgw_core::config::Protocol;
use lmgw_core::config::UpstreamKind;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use lmgw_core::telemetry::Event;
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common;
use common::{serve, Gw};

async fn setup(upstream_base: &str, protocol: Protocol) -> (SharedState, Gw) {
    setup_kind(upstream_base, protocol, UpstreamKind::Generic).await
}

pub(crate) async fn setup_kind(
    upstream_base: &str,
    protocol: Protocol,
    kind: UpstreamKind,
) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol,
            kind,
            base_url: upstream_base.trim_end_matches('/').to_string(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
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
            alias: "my-model".into(),
            upstream_id: up_id,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    // Serve on an ephemeral port.
    let gw = serve(state.clone()).await;
    (state, gw)
}

#[tokio::test]
async fn openai_to_openai_unary_with_log_and_event() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"model": "tgt-model"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-up", "object": "chat.completion", "model": "tgt-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "pong"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 34}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let (state, base) = setup(&mock.uri(), Protocol::Openai).await;

    // Auth on + a named key, so the request/log/SSE frame all carry a real
    // client_key — "which key made this call", visible end to end (the P8
    // parity gap: the old UI's log detail page showed this, the new plane
    // dropped it from `dto::RequestRow` and `RequestSummary` entirely).
    let mut settings = state.snapshot().settings.clone();
    settings.auth_enabled = true;
    store::save_settings(&state.db, &settings).await.unwrap();
    store::insert_api_key(
        &state.db,
        "client-a",
        &lmgw_core::config::hash_api_key("lmgw-client-a"),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let mut rx = state.telemetry.subscribe();

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth("lmgw-client-a")
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["model"], "my-model"); // alias echoed, not upstream model
    assert_eq!(body["choices"][0]["message"]["content"], "pong");
    assert_eq!(body["usage"]["total_tokens"], 46);

    // live-feed event
    let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let Event::Request(summary) = ev else {
        panic!("expected request event")
    };
    assert_eq!(summary.requested_alias, "my-model");
    assert_eq!(summary.status, 200);
    assert_eq!(summary.prompt_tokens, Some(12));
    // The key name (never the key itself) rides the SSE frame too, not just
    // the stored row — the Traffic page's live tail needs it without a
    // round-trip to /api/logs.
    assert_eq!(summary.client_key.as_deref(), Some("client-a"));

    // request_logs row
    let logs = store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    assert_eq!(logs.len(), 1);
    let log = &logs[0];
    assert_eq!(log.requested_alias, "my-model");
    assert_eq!(log.upstream_model.as_deref(), Some("tgt-model"));
    assert_eq!(log.ingress_proto, "openai");
    assert_eq!(log.egress_proto.as_deref(), Some("openai"));
    assert_eq!(log.status, 200);
    assert_eq!(log.completion_tokens, Some(34));
    assert!(!log.streamed);
    assert_eq!(log.client_key.as_deref(), Some("client-a"));

    // `/api/logs` (the Traffic page's fetch, on the same dashboard-plane
    // router — no auth gate) carries it too.
    let logs_json: Value = base
        .client()
        .get(format!("{base}/api/logs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(logs_json["logs"][0]["client_key"], json!("client-a"));
}

/// The cache tiers are most of the story of what an Anthropic turn cost, so
/// they have to survive the whole trip: provider JSON -> IR -> stored row ->
/// live frame and `/api/logs`. Without them a Traffic row that cost 8x its
/// neighbour looks identical to it.
#[tokio::test]
async fn anthropic_cache_tiers_reach_the_live_frame_and_the_logs_api() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt-model",
            "content": [{"type": "text", "text": "pong"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 12, "output_tokens": 34,
                "cache_read_input_tokens": 600, "cache_creation_input_tokens": 200
            }
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let (state, base) = setup(&mock.uri(), Protocol::Anthropic).await;
    let mut rx = state.telemetry.subscribe();

    let body: Value = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Anthropic's three counters are disjoint; the IR's `prompt_tokens` is the
    // total the OpenAI dialect promises, with the cache read as a detail of it.
    assert_eq!(body["usage"]["prompt_tokens"], 812);
    assert_eq!(body["usage"]["prompt_tokens_details"]["cached_tokens"], 600);

    let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let Event::Request(summary) = ev else {
        panic!("expected request event")
    };
    assert_eq!(summary.prompt_tokens, Some(812));
    assert_eq!(summary.cached_in_tokens, Some(600));
    assert_eq!(summary.cache_write_tokens, Some(200));

    let logs_json: Value = base
        .client()
        .get(format!("{base}/api/logs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = &logs_json["logs"][0];
    assert_eq!(row["prompt_tokens"], json!(812));
    assert_eq!(row["cached_in_tokens"], json!(600));
    assert_eq!(row["cache_write_tokens"], json!(200));
}

#[tokio::test]
async fn anthropic_ingress_to_openai_egress_translation() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        // system message must be translated into OpenAI messages[0]
        .and(body_partial_json(json!({
            "model": "tgt-model",
            "messages": [{"role": "system", "content": "be brief"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "tgt-model",
            "choices": [{"message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let (_state, base) = setup(&mock.uri(), Protocol::Openai).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "my-model",
            "max_tokens": 64,
            "system": "be brief",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["model"], "my-model");
    assert_eq!(body["content"][0]["text"], "ok");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["usage"]["input_tokens"], 5);
}

/// An `input_audio` content part (model-capabilities design §6) survives the
/// round trip to an OpenAI-shaped upstream byte-for-byte: raw base64 data,
/// `format` lowercased from whatever the client sent.
#[tokio::test]
async fn an_input_audio_part_reaches_the_upstream_verbatim() {
    let mock = chat_mock().await;
    let (_state, base) = setup(&mock.uri(), Protocol::Openai).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "my-model",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "input_audio", "input_audio": {"data": "QUFB", "format": "WAV"}}
            ]}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let sent = sent_body(&mock).await;
    assert_eq!(
        sent["messages"][0]["content"][1],
        json!({"type": "input_audio", "input_audio": {"data": "QUFB", "format": "wav"}})
    );
}

#[tokio::test]
async fn streaming_openai_passthrough_translates_and_logs() {
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n"
    );
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock)
        .await;

    let (state, base) = setup(&mock.uri(), Protocol::Openai).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "my-model", "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let text = resp.text().await.unwrap();
    assert!(text.contains("\"content\":\"Hel\""));
    assert!(text.contains("\"finish_reason\":\"stop\""));
    assert!(text.trim_end().ends_with("data: [DONE]"));

    // wait for the pump task to write the log
    let mut tries = 0;
    let log = loop {
        let logs = store::query_logs(&state.db, &Default::default())
            .await
            .unwrap();
        if let Some(l) = logs.first() {
            break l.clone();
        }
        tries += 1;
        assert!(tries < 50, "log row never appeared");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert!(log.streamed);
    assert_eq!(log.status, 200);
    assert_eq!(log.prompt_tokens, Some(3));
    assert_eq!(log.completion_tokens, Some(2));
    assert!(log.ttfb_ms.is_some());
    assert!(log.error_kind.is_none());
}

#[tokio::test]
async fn anthropic_client_streaming_from_openai_upstream() {
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock)
        .await;

    let (_state, base) = setup(&mock.uri(), Protocol::Openai).await;
    let text = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "my-model", "max_tokens": 16, "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Anthropic event framing produced from an OpenAI upstream stream
    assert!(text.contains("event: message_start"));
    assert!(text.contains("\"text\":\"Hi\""));
    assert!(text.contains("event: message_delta"));
    assert!(text.contains("\"stop_reason\":\"end_turn\""));
    assert!(text.contains("event: message_stop"));
}

#[tokio::test]
async fn upstream_error_is_normalized_and_logged() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"message": "slow down", "type": "rate_limit_error"}
        })))
        .mount(&mock)
        .await;

    let (state, base) = setup(&mock.uri(), Protocol::Openai).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "x"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "api_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("slow down"));

    let logs = store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    assert_eq!(logs[0].status, 429);
    assert_eq!(logs[0].error_kind.as_deref(), Some("upstream"));
    assert!(logs[0].error_msg.as_deref().unwrap().contains("slow down"));
}

#[tokio::test]
async fn unknown_alias_is_404_in_client_shape() {
    let (state, base) = setup("http://127.0.0.1:1", Protocol::Openai).await;

    // OpenAI shape
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "ghost", "messages": [{"role": "user", "content": "x"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unknown_alias");

    // Anthropic shape
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&json!({"model": "ghost", "max_tokens": 5,
                      "messages": [{"role": "user", "content": "x"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "not_found_error");

    let logs = store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    assert_eq!(logs.len(), 2);
}

/// A legacy completion without `model` is the 400 every other ingress gives,
/// logged — not a 404 for an alias named `?`, which is what routing the
/// missing name used to answer.
#[tokio::test]
async fn legacy_completion_without_model_is_400() {
    let (state, base) = setup("http://127.0.0.1:1", Protocol::Openai).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/completions"))
        .json(&json!({"prompt": "Once upon a time"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "bad_request", "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing 'model'"),
        "{body}"
    );
    let logs = store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].status, 400);
    assert_eq!(logs[0].error_kind.as_deref(), Some("bad_request"));
    assert_eq!(state.telemetry.stats().active_requests, 0);
}

#[tokio::test]
async fn models_endpoint_serves_both_shapes() {
    let (_state, base) = setup("http://127.0.0.1:1", Protocol::Openai).await;
    let client = reqwest::Client::new();

    let openai: Value = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(openai["object"], "list");
    assert_eq!(openai["data"][0]["id"], "my-model");

    let anthropic: Value = client
        .get(format!("{base}/v1/models"))
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(anthropic["data"][0]["type"], "model");
    assert_eq!(anthropic["has_more"], false);
}

#[tokio::test]
async fn auth_blocks_v1_when_enabled() {
    let (state, base) = setup("http://127.0.0.1:1", Protocol::Openai).await;

    // enable auth + create a key
    let mut settings = state.snapshot().settings.clone();
    settings.auth_enabled = true;
    store::save_settings(&state.db, &settings).await.unwrap();
    store::insert_api_key(
        &state.db,
        "test",
        &lmgw_core::config::hash_api_key("lmgw-key-1"),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let resp = client
        .get(format!("{base}/v1/models"))
        .bearer_auth("lmgw-key-1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // x-api-key works too
    let resp = client
        .get(format!("{base}/v1/models"))
        .header("x-api-key", "lmgw-key-1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // UI stays open
    let resp = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn embeddings_passthrough() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .and(body_partial_json(
            json!({"model": "tgt-model", "input": ["hello"]}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "model": "tgt-model",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}],
            "usage": {"prompt_tokens": 2, "total_tokens": 2}
        })))
        .mount(&mock)
        .await;

    let (_state, base) = setup(&mock.uri(), Protocol::Openai).await;
    let body: Value = reqwest::Client::new()
        .post(format!("{base}/v1/embeddings"))
        .json(&json!({"model": "my-model", "input": "hello"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["model"], "my-model");
    let v = body["data"][0]["embedding"][1].as_f64().unwrap();
    assert!((v - 0.2).abs() < 1e-6, "embedding value {v}");
}

#[tokio::test]
async fn count_tokens_openai_generic_uses_tiktoken() {
    // Real-OpenAI-style upstream (kind=Generic): no /tokenize endpoint, so the
    // count is computed locally — no upstream call is made.
    let (_state, base) = setup("http://127.0.0.1:1", Protocol::Openai).await;
    let body: Value = reqwest::Client::new()
        .post(format!("{base}/v1/count_tokens"))
        .json(&json!({"model": "my-model", "input": "hello world"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["model"], "my-model"); // alias echoed, uniform schema
    assert!(
        body["tokens"].as_u64().unwrap() >= 2,
        "got {}",
        body["tokens"]
    );
}

#[tokio::test]
async fn count_tokens_anthropic_native_endpoint() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .and(body_partial_json(json!({
            "model": "tgt-model",
            "messages": [{"role": "user", "content": "count me"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 11})))
        .expect(1)
        .mount(&mock)
        .await;

    let (_state, base) = setup(&mock.uri(), Protocol::Anthropic).await;
    let body: Value = reqwest::Client::new()
        .post(format!("{base}/v1/count_tokens"))
        .json(&json!({"model": "my-model", "input": "count me"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Same response schema regardless of backend.
    assert_eq!(body["model"], "my-model");
    assert_eq!(body["tokens"], 11);
}

#[tokio::test]
async fn count_tokens_llama_server_forwards_model() {
    // llama-server upstream: count via the native /tokenize endpoint. The
    // upstream model must be forwarded so a router-mode server can pick a
    // backend (regression: it was previously omitted → "model name is missing
    // from the request"). /tokenize lives at the server root, not under /v1.
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .and(body_partial_json(json!({
            "model": "tgt-model",
            "content": "Hallo Welt"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tokens": [1, 2, 3]})))
        .expect(1)
        .mount(&mock)
        .await;

    let (_state, base) =
        setup_kind(&mock.uri(), Protocol::LlamaCpp, UpstreamKind::LlamaServer).await;
    let body: Value = reqwest::Client::new()
        .post(format!("{base}/v1/count_tokens"))
        .json(&json!({"model": "my-model", "input": "Hallo Welt"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["model"], "my-model"); // alias echoed, uniform schema
    assert_eq!(body["tokens"], 3); // count = number of returned token ids
}

#[tokio::test]
async fn count_tokens_unknown_alias_is_404() {
    let (_state, base) = setup("http://127.0.0.1:1", Protocol::Openai).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/count_tokens"))
        .json(&json!({"model": "ghost", "input": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unknown_alias");
}

#[tokio::test]
async fn anthropic_egress_end_to_end() {
    // Gateway → Anthropic-protocol upstream (wiremock playing Anthropic).
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"model": "tgt-model"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "message", "model": "tgt-model", "role": "assistant",
            "content": [{"type": "text", "text": "bonjour"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 9, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let (_state, base) = setup(&mock.uri(), Protocol::Anthropic).await;
    // OpenAI client → Anthropic upstream
    let body: Value = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "salut"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "bonjour");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["prompt_tokens"], 9);
}

/// The trace a reasoning model produced is replayed to the upstream on the
/// next request, in the one spelling llama-server reads (`reasoning_content`),
/// whichever API shape the client speaks. Before this the gateway dropped it
/// at ingress and `--reasoning-preserve` did nothing for anyone behind it.
#[tokio::test]
async fn assistant_reasoning_is_replayed_to_the_upstream() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-up", "object": "chat.completion", "model": "tgt-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "because"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .expect(2)
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri(), Protocol::Openai).await;
    let client = reqwest::Client::new();

    // OpenAI-shaped client: `reasoning_content` on the assistant turn.
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "my-model",
            "messages": [
                {"role": "user", "content": "pick a colour"},
                {"role": "assistant", "content": "Teal", "reasoning_content": "teal is unusual"},
                {"role": "user", "content": "why?"}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Anthropic-shaped client: the unsigned thinking block this gateway hands
    // out, sent back the way an Anthropic SDK does.
    let resp = client
        .post(format!("{base}/v1/messages"))
        .json(&json!({
            "model": "my-model",
            "max_tokens": 50,
            "messages": [
                {"role": "user", "content": "pick a colour"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "teal is unusual"},
                    {"type": "text", "text": "Teal"}
                ]},
                {"role": "user", "content": "why?"}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let seen = mock.received_requests().await.unwrap();
    assert_eq!(seen.len(), 2);
    for req in &seen {
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let asst = &body["messages"][1];
        assert_eq!(asst["role"], "assistant", "{body}");
        assert_eq!(asst["content"], "Teal", "{body}");
        assert_eq!(asst["reasoning_content"], "teal is unusual", "{body}");
    }
}

// ---------------------------------------------------------------------------
// Reasoning control plane (model-capabilities design §5)
// ---------------------------------------------------------------------------

/// A chat mock that answers anything and records what it was sent.
async fn chat_mock() -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "model": "tgt-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&mock)
        .await;
    mock
}

async fn messages_mock() -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt-model",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&mock)
        .await;
    mock
}

/// The first request body the mock was *posted* — skipping the catalog's own
/// `GET /v1/models`, which an Anthropic route makes before the call.
async fn sent_body(mock: &MockServer) -> Value {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .find(|r| r.method == wiremock::http::Method::POST)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .expect("no request posted to the mock")
}

/// Headers outrank the body (§5.2) — which is the whole point of them: a
/// client driving lmgw through someone else's SDK can steer reasoning without
/// a field for it.
#[tokio::test]
async fn a_reasoning_header_beats_the_body_field() {
    let mock = chat_mock().await;
    let (_state, base) =
        setup_kind(&mock.uri(), Protocol::LlamaCpp, UpstreamKind::LlamaServer).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-lmgw-reasoning-effort", "high")
        .json(&json!({
            "model": "my-model",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "low",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let sent = sent_body(&mock).await;
    assert_eq!(sent["reasoning_effort"], "high");
    assert_eq!(sent["chat_template_kwargs"]["enable_thinking"], true);
}

/// Off, on a llama-server route, is the template kwarg — not a level the
/// template would have to know.
#[tokio::test]
async fn reasoning_off_reaches_a_llama_server_as_a_template_kwarg() {
    let mock = chat_mock().await;
    let (_state, base) =
        setup_kind(&mock.uri(), Protocol::LlamaCpp, UpstreamKind::LlamaServer).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-lmgw-reasoning", "off")
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // Nothing was ignored — this route can say all of it.
    assert!(resp.headers().get("x-lmgw-reasoning-ignored").is_none());

    let sent = sent_body(&mock).await;
    assert_eq!(sent["chat_template_kwargs"]["enable_thinking"], false);
    assert!(sent.get("reasoning_effort").is_none());
}

/// A contradiction inside the header tier has no lower tier to defer to, so it
/// is refused by name rather than resolved by coin flip.
#[tokio::test]
async fn contradicting_headers_are_a_400_in_the_callers_dialect() {
    let mock = chat_mock().await;
    let (_state, base) = setup(&mock.uri(), Protocol::Openai).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .header("x-lmgw-reasoning", "on")
        .header("x-lmgw-reasoning-effort", "none")
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("x-lmgw-reasoning"), "{msg}");
    assert!(msg.contains("x-lmgw-reasoning-effort"), "{msg}");

    // Same refusal, Anthropic-shaped, for an Anthropic client.
    let resp = client
        .post(format!("{base}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .header("x-lmgw-reasoning", "on")
        .header("x-lmgw-reasoning-budget", "0")
        .json(&json!({
            "model": "my-model", "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("x-lmgw-reasoning-budget"));

    // Neither reached the upstream.
    assert!(mock.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_malformed_reasoning_header_is_a_400_naming_it() {
    let mock = chat_mock().await;
    let (_state, base) = setup(&mock.uri(), Protocol::Openai).await;
    let client = reqwest::Client::new();

    for (header, value, needle) in [
        ("x-lmgw-reasoning-budget", "lots", "x-lmgw-reasoning-budget"),
        ("x-lmgw-reasoning-budget", "-5", "x-lmgw-reasoning-budget"),
        ("x-lmgw-reasoning", "maybe", "x-lmgw-reasoning"),
        ("x-lmgw-reasoning-effort", "  ", "x-lmgw-reasoning-effort"),
    ] {
        let resp = client
            .post(format!("{base}/v1/chat/completions"))
            .header(header, value)
            .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{header}: {value}");
        let body: Value = resp.json().await.unwrap();
        assert!(
            body["error"]["message"].as_str().unwrap().contains(needle),
            "{header}: {value} → {body}"
        );
    }
}

/// A control the route cannot express is named on the response, never dropped
/// in silence (§5.3).
#[tokio::test]
async fn an_ignored_control_is_reported_on_the_response() {
    let mock = chat_mock().await;
    let (_state, base) = setup(&mock.uri(), Protocol::Openai).await; // generic kind
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .header("x-lmgw-reasoning-budget", "4096")
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // Both: asking for a budget is asking for thinking to be on, and this route
    // sends neither — so neither is claimed to have been sent.
    assert_eq!(resp.headers()["x-lmgw-reasoning-ignored"], "enabled,budget");
    let sent = sent_body(&mock).await;
    assert!(sent.get("reasoning_budget_tokens").is_none());

    // An effort *is* expressible there, so nothing is reported.
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .header("x-lmgw-reasoning-effort", "high")
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-lmgw-reasoning-ignored").is_none());
}

/// §5.4: the hidden 4096 stays only as a last resort, and says so.
#[tokio::test]
async fn anthropic_max_tokens_falls_back_to_4096_and_says_so() {
    let mock = messages_mock().await;
    let (_state, base) = setup(&mock.uri(), Protocol::Anthropic).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lmgw-max-tokens-defaulted"], "4096");
    assert_eq!(sent_body(&mock).await["max_tokens"], 4096);
}

/// The catalog's maximum is for **streamed** requests only.
#[tokio::test]
async fn anthropic_max_tokens_comes_from_the_catalog_only_when_streaming() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "tgt-model", "max_tokens": 64000, "type": "model"}]
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt-model",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&mock)
        .await;
    // A streamed answer, because the catalog's maximum only applies there.
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse, "text/event-stream"),
        )
        .with_priority(1)
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri(), Protocol::Anthropic).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "my-model", "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // Still stamped: 64000 is lmgw's choice, not the client's, and a cap the
    // client cannot see is exactly the hidden limit this gateway refuses.
    assert_eq!(resp.headers()["x-lmgw-max-tokens-defaulted"], "64000");
    resp.text().await.unwrap();
    assert_eq!(sent_body(&mock).await["max_tokens"], 64000);

    // The same request unstreamed gets the conservative number instead: the
    // provider rejects a non-streamed call whose cap could outrun its limit.
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lmgw-max-tokens-defaulted"], "4096");
    let unary: Value = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap())
        .find(|b| b.get("stream").is_none())
        .expect("an unstreamed call");
    assert_eq!(unary["max_tokens"], 4096);
}

/// An Anthropic client's own `thinking` block used to be dropped on the floor;
/// now it crosses to whatever protocol answers — here, back out as Anthropic.
#[tokio::test]
async fn an_anthropic_thinking_block_reaches_an_anthropic_upstream() {
    let mock = messages_mock().await;
    let (_state, base) = setup(&mock.uri(), Protocol::Anthropic).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "my-model", "max_tokens": 1000,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "enabled", "budget_tokens": 8000},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let sent = sent_body(&mock).await;
    assert_eq!(sent["thinking"]["type"], "enabled");
    assert_eq!(sent["thinking"]["budget_tokens"], 8000);
    // The cap the API requires to exceed the budget, raised for the client.
    assert_eq!(sent["max_tokens"], 9024);
}

/// The annotation headers go out before the first SSE event, exactly like
/// `x-lmgw-fallback` — a stream is where a silently dropped control would be
/// hardest to notice.
#[tokio::test]
async fn an_ignored_control_is_reported_on_a_stream_too() {
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri(), Protocol::Openai).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-lmgw-reasoning", "on")
        .json(&json!({
            "model": "my-model", "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lmgw-reasoning-ignored"], "enabled");
    assert!(resp.text().await.unwrap().contains("[DONE]"));
}

// ---------------------------------------------------------------------------
// Reasoning control plane — review follow-ups
// ---------------------------------------------------------------------------

/// Point an alias at `upstream_base` with a reasoning default of its own.
async fn setup_with_alias_reasoning(
    upstream_base: &str,
    protocol: Protocol,
    kind: UpstreamKind,
    reasoning: lmgw_core::ir::ReasoningControl,
) -> (SharedState, Gw) {
    let (state, base) = setup_kind(upstream_base, protocol, kind).await;
    let aliases = store::list_aliases(&state.db).await.unwrap();
    let cur = aliases.into_iter().find(|a| a.alias == "my-model").unwrap();
    store::update_alias(
        &state.db,
        cur.id,
        &NewAlias {
            alias: cur.alias,
            upstream_id: cur.upstream_id,
            upstream_model_id: cur.upstream_model_id,
            param_overrides: lmgw_core::ir::Params {
                reasoning: Some(reasoning),
                ..Default::default()
            },
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    (state, base)
}

/// An alias configured off must not erase the level the caller just asked for:
/// the header is the higher tier, and asking for `high` is asking for thinking.
#[tokio::test]
async fn a_header_effort_overrides_an_alias_that_is_off() {
    let mock = chat_mock().await;
    let (_state, base) = setup_with_alias_reasoning(
        &mock.uri(),
        Protocol::LlamaCpp,
        UpstreamKind::LlamaServer,
        lmgw_core::ir::ReasoningControl {
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-lmgw-reasoning-effort", "high")
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let sent = sent_body(&mock).await;
    assert_eq!(sent["reasoning_effort"], "high");
    assert_eq!(sent["chat_template_kwargs"]["enable_thinking"], true);
}

/// Same rule one tier down: the body's level beats the alias's "off".
#[tokio::test]
async fn a_body_effort_overrides_an_alias_that_is_off() {
    let mock = chat_mock().await;
    let (_state, base) = setup_with_alias_reasoning(
        &mock.uri(),
        Protocol::LlamaCpp,
        UpstreamKind::LlamaServer,
        lmgw_core::ir::ReasoningControl {
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "my-model",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "low",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(sent_body(&mock).await["reasoning_effort"], "low");
}

/// The header tier must be self-consistent in *both* directions.
#[tokio::test]
async fn an_off_header_contradicting_a_level_or_budget_is_a_400() {
    let mock = chat_mock().await;
    let (_state, base) = setup(&mock.uri(), Protocol::Openai).await;
    let client = reqwest::Client::new();

    for (header, value) in [
        ("x-lmgw-reasoning-effort", "high"),
        ("x-lmgw-reasoning-budget", "2048"),
    ] {
        let resp = client
            .post(format!("{base}/v1/chat/completions"))
            .header("x-lmgw-reasoning", "off")
            .header(header, value)
            .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{header}: {value}");
        let body: Value = resp.json().await.unwrap();
        let msg = body["error"]["message"].as_str().unwrap();
        assert!(msg.contains("'off' contradicts"), "{msg}");
        assert!(msg.contains(header), "{msg}");
    }

    // `off` with an effort of `none` agrees with itself and is accepted.
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .header("x-lmgw-reasoning", "off")
        .header("x-lmgw-reasoning-effort", "none")
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// The headers are enforced only where they mean something. A route with no
/// reasoning control to get wrong — the model list, embeddings — must not
/// start failing because a client sends the header to everything.
#[tokio::test]
async fn a_malformed_header_does_not_break_routes_that_ignore_it() {
    let mock = chat_mock().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "model": "tgt-model",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}],
            "usage": {"prompt_tokens": 2, "total_tokens": 2}
        })))
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri(), Protocol::Openai).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/v1/models"))
        .header("x-lmgw-reasoning", "maybe")
        .header("x-lmgw-reasoning-budget", "lots")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "the model list has no reasoning control"
    );

    let resp = client
        .post(format!("{base}/v1/embeddings"))
        .header("x-lmgw-reasoning-budget", "lots")
        .json(&json!({"model": "my-model", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // The chat routes still refuse it, in their own dialect.
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .header("x-lmgw-reasoning-budget", "lots")
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

/// `chat_template_kwargs.enable_thinking` is a llama.cpp template variable, not
/// a control any cloud provider has. It must never turn into an Anthropic
/// `thinking: {type: "disabled"}` the client never asked for — on that route it
/// is simply a key the IR carries nowhere.
#[tokio::test]
async fn enable_thinking_is_a_llama_only_control() {
    let llama = chat_mock().await;
    let (_state, base) =
        setup_kind(&llama.uri(), Protocol::LlamaCpp, UpstreamKind::LlamaServer).await;
    let client = reqwest::Client::new();
    let body = json!({
        "model": "my-model",
        "messages": [{"role": "user", "content": "hi"}],
        "chat_template_kwargs": {"enable_thinking": false},
    });

    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        sent_body(&llama).await["chat_template_kwargs"]["enable_thinking"],
        false
    );

    // The same body on an Anthropic route: forwarded nowhere, and inventing no
    // control.
    let cloud = messages_mock().await;
    let (_state, base) = setup(&cloud.uri(), Protocol::Anthropic).await;
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = sent_body(&cloud).await;
    assert!(sent.get("thinking").is_none(), "{sent}");
    assert!(sent.get("output_config").is_none(), "{sent}");
    assert!(resp.headers().get("x-lmgw-reasoning-ignored").is_none());
}

/// Raising the cap to fit a thinking budget is lmgw's decision, so it says so.
#[tokio::test]
async fn a_raised_max_tokens_is_reported() {
    let mock = messages_mock().await;
    let (_state, base) = setup(&mock.uri(), Protocol::Anthropic).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "my-model", "max_tokens": 1000,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "enabled", "budget_tokens": 8000},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lmgw-max-tokens-raised"], "9024");
    assert_eq!(sent_body(&mock).await["max_tokens"], 9024);
    // The client set the cap itself, so nothing was *defaulted*.
    assert!(resp.headers().get("x-lmgw-max-tokens-defaulted").is_none());
}

/// A budget no `u32` can hold is refused by name rather than wrapped into a
/// small, wrong cap.
#[tokio::test]
async fn an_absurd_budget_is_refused_by_name() {
    let mock = messages_mock().await;
    let (_state, base) = setup(&mock.uri(), Protocol::Anthropic).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "my-model", "max_tokens": 1000,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "enabled", "budget_tokens": 9223372036854775807i64},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("does not fit"));
}

/// A `reasoning_effort` lmgw cannot read is still the client's field: it
/// reaches the upstream exactly as sent.
#[tokio::test]
async fn an_unparseable_reasoning_effort_reaches_the_upstream_verbatim() {
    let mock = chat_mock().await;
    let (_state, base) = setup(&mock.uri(), Protocol::Openai).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "my-model",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": null,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = sent_body(&mock).await;
    assert!(sent["reasoning_effort"].is_null());
    assert!(sent.as_object().unwrap().contains_key("reasoning_effort"));
}

/// A client that spoke OpenRouter's dialect gets it back reconciled — and no
/// scalar beside it.
#[tokio::test]
async fn an_openrouter_object_is_reconciled_without_a_scalar() {
    let mock = chat_mock().await;
    let (_state, base) = setup(&mock.uri(), Protocol::Openai).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "my-model",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning": {"effort": "low", "exclude": true},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-lmgw-reasoning-ignored").is_none());
    let sent = sent_body(&mock).await;
    assert_eq!(sent["reasoning"]["effort"], "low");
    assert_eq!(sent["reasoning"]["exclude"], true);
    assert!(sent.get("reasoning_effort").is_none(), "{sent}");

    // A header override rewrites the object, still without a scalar.
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .header("x-lmgw-reasoning-effort", "high")
        .json(&json!({
            "model": "my-model",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning": {"effort": "low", "exclude": true},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent: Value = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap())
        .find(|b| b.pointer("/reasoning/effort") == Some(&json!("high")))
        .expect("the overridden call");
    assert_eq!(sent["reasoning"]["exclude"], true);
    assert!(sent.get("reasoning_effort").is_none(), "{sent}");
}

/// `timeout_ms = 0` means **the maximum possible** — no deadline of lmgw's own
/// — and this is the end-to-end proof it reaches reqwest that way. Until the
/// per-class ceilings landed, every call site spelled
/// `Duration::from_millis(timeout_ms.max(1))`, so a cleared field became a
/// one-millisecond deadline and *every* request failed instantly: the exact
/// inversion of what the owner asked for.
///
/// The mock answers after 1.5 s, comfortably past anything a stray floor would
/// produce, and the assertion is simply that the answer arrives.
#[tokio::test]
async fn a_zero_timeout_upstream_is_unbounded_not_instantly_expired() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(1500))
                .set_body_json(json!({
                    "id": "chatcmpl-up", "object": "chat.completion", "model": "tgt-model",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "slow"},
                                 "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let (state, base) = setup_with_timeout(&mock.uri(), 0).await;
    let _ = &state;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], json!("slow"));
}

/// The other half: a ceiling that *is* set still bites, and says so as a
/// timeout rather than as a transport error of some other name.
#[tokio::test]
async fn a_set_timeout_still_cuts_a_slow_upstream_off() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(30))
                .set_body_json(json!({"id": "never"})),
        )
        .mount(&mock)
        .await;

    let (state, base) = setup_with_timeout(&mock.uri(), 300).await;
    let _ = &state;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "my-model", "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.to_lowercase().contains("timeout") || msg.to_lowercase().contains("timed out"),
        "{v}"
    );
}

/// [`setup`] with the upstream's `timeout_ms` under the test's control.
async fn setup_with_timeout(upstream_base: &str, timeout_ms: u64) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: upstream_base.trim_end_matches('/').to_string(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms,
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
            alias: "my-model".into(),
            upstream_id: up_id,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let gw = serve(state.clone()).await;
    (state, gw)
}
