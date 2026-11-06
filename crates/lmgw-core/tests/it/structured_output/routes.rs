//! End to end against wiremock upstreams: the early refusal (§7), the
//! classified refusals per route (§8, §9) — streamed and native included —
//! and the reasoning-off retry that leaves them alone.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{serve, Gw};

/// `m` → `tgt` on a cloud upstream of `protocol` at `base`, with the owner's
/// capabilities override when given.
pub(super) async fn world(
    base: &str,
    protocol: Protocol,
    supports_responses: bool,
    caps: Option<Value>,
) -> (SharedState, Gw) {
    world_on(
        base,
        protocol,
        UpstreamKind::Generic,
        supports_responses,
        caps,
    )
    .await
}

/// [`world`] on an upstream of `kind` — an external llama.cpp row is
/// `LlamaCpp` + `LlamaServer`.
pub(super) async fn world_on(
    base: &str,
    protocol: Protocol,
    kind: UpstreamKind,
    supports_responses: bool,
    caps: Option<Value>,
) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "cloud".into(),
            protocol,
            kind,
            base_url: base.trim_end_matches('/').to_string(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses,
            catalog_all_modalities: true,
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
            capabilities_override: caps,
            fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

/// A captured refusal (`tests/fixtures/structured_output/<name>.json`) as an
/// upstream answer.
pub(super) fn captured(name: &str) -> (ResponseTemplate, String) {
    let p = format!(
        "{}/tests/fixtures/structured_output/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
    let message = v["body"]["error"]["message"].as_str().unwrap().to_string();
    (
        ResponseTemplate::new(v["status"].as_u64().unwrap() as u16).set_body_json(&v["body"]),
        message,
    )
}

pub(super) fn openai_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "c1", "object": "chat.completion", "model": "tgt",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "{\"ok\":true}"},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3}
    }))
}

/// An OpenAI-protocol catalog that lists `tgt` and says nothing about it.
pub(super) async fn openai_catalog(mock: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "data": [{"id": "tgt", "object": "model"}]
        })))
        .mount(mock)
        .await;
}

pub(super) async fn posted(mock: &MockServer) -> Vec<Value> {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

pub(super) fn schema() -> Value {
    json!({"type": "object", "properties": {"ok": {"type": "boolean"}},
           "required": ["ok"], "additionalProperties": false})
}

pub(super) fn chat_body(rf: Option<Value>, stream: bool) -> Value {
    let mut b = json!({"model": "m", "stream": stream, "max_tokens": 16,
                       "messages": [{"role": "user", "content": "Reply with ok."}]});
    if let Some(rf) = rf {
        b["response_format"] = rf;
    }
    b
}

pub(super) fn json_schema() -> Value {
    json!({"type": "json_schema", "json_schema": {"name": "answer", "strict": true,
                                                  "schema": schema()}})
}

pub(super) async fn post(gw: &Gw, route: &str, body: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}{route}"))
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

pub(super) fn responses_body(format: Value) -> Value {
    json!({"model": "m", "input": "Reply with ok.", "max_output_tokens": 16,
           "text": {"format": format}})
}

pub(super) fn messages_body() -> Value {
    json!({"model": "m", "max_tokens": 16,
           "messages": [{"role": "user", "content": "Reply with ok."}],
           "output_config": {"format": {"type": "json_schema", "schema": schema()}}})
}

/// `GET /v1/models`: the listing reads every catalog, which the early
/// refusal then reads from the cache (§11.21).
pub(super) async fn list_models(gw: &Gw) -> Value {
    gw.client()
        .get(format!("{gw}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

pub(super) async fn newest_error_kind(state: &SharedState) -> Option<String> {
    let rows: Vec<(Option<String>,)> =
        sqlx::query_as("SELECT error_kind FROM request_logs ORDER BY id DESC LIMIT 1")
            .fetch_all(&state.db)
            .await
            .unwrap();
    rows.into_iter().next().and_then(|r| r.0)
}

pub(super) fn assert_format_error(v: &Value, code: &str, param: &str) {
    assert_eq!(v["error"]["code"], code, "{v}");
    assert_eq!(v["error"]["param"], param, "{v}");
    assert_eq!(v["error"]["type"], "invalid_request_error", "{v}");
}

// ---------------------------------------------------------------------------
// §7: the early refusal
// ---------------------------------------------------------------------------

/// A flag the owner set to `false` refuses that type on every route before
/// anything goes upstream; another type, whose flag is absent, goes.
#[tokio::test]
async fn an_explicit_false_flag_refuses_before_any_upstream_call() {
    let mock = MockServer::start().await;
    openai_catalog(&mock).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_ok())
        .mount(&mock)
        .await;
    let caps = json!({"capabilities": {"structured_output": {"json_schema": false}}});
    let (state, gw) = world(&mock.uri(), Protocol::OpenaiCompatible, false, Some(caps)).await;
    list_models(&gw).await;

    let (status, v) = post(
        &gw,
        "/v1/chat/completions",
        chat_body(Some(json_schema()), false),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_format_error(&v, "response_format_unsupported", "response_format");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("'m' does not produce json_schema") && msg.contains("capabilities override"),
        "{msg}"
    );
    assert_eq!(
        newest_error_kind(&state).await.as_deref(),
        Some("response_format_unsupported")
    );

    let (status, v) = post(
        &gw,
        "/v1/responses",
        responses_body(json!({"type": "json_schema", "name": "answer", "schema": schema()})),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_format_error(&v, "response_format_unsupported", "text.format");

    let (status, v) = post(&gw, "/v1/messages", messages_body()).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["error"]["type"], "invalid_request_error", "{v}");

    let (status, v) = post(
        &gw,
        "/v1/completions",
        json!({"model": "m", "prompt": "Reply with ok.", "max_tokens": 16,
               "response_format": json_schema()}),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_format_error(&v, "response_format_unsupported", "response_format");

    assert!(posted(&mock).await.is_empty(), "nothing went upstream");

    // json_object's flag is absent: unknown, so the upstream decides.
    let (status, v) = post(
        &gw,
        "/v1/chat/completions",
        chat_body(Some(json!({"type": "json_object"})), false),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["response_format"], json!({"type": "json_object"}));
}

/// An Anthropic row publishes `json_object: false` — refused at once — and
/// takes a JSON schema as `output_config.format`.
#[tokio::test]
async fn an_anthropic_row_refuses_json_object_and_translates_json_schema() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "tgt", "type": "model", "display_name": "Example",
                      "capabilities": {"structured_outputs": {"supported": true}}}],
            "has_more": false
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "model": "tgt", "role": "assistant",
            "content": [{"type": "text", "text": "{\"ok\":true}"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 3}
        })))
        .mount(&mock)
        .await;
    let (_state, gw) = world(&mock.uri(), Protocol::Anthropic, false, None).await;

    let models = list_models(&gw).await;
    let entry = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "m")
        .unwrap()
        .clone();
    assert_eq!(
        entry["capabilities"]["structured_output"],
        json!({"json_schema": true, "json_object": false}),
        "{entry}"
    );

    let (status, v) = post(
        &gw,
        "/v1/chat/completions",
        chat_body(Some(json!({"type": "json_object"})), false),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_format_error(&v, "response_format_unsupported", "response_format");
    assert!(posted(&mock).await.is_empty());

    let (status, v) = post(
        &gw,
        "/v1/chat/completions",
        chat_body(Some(json_schema()), false),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["choices"][0]["message"]["content"], "{\"ok\":true}");
    let sent = posted(&mock).await;
    assert_eq!(
        sent[0]["output_config"]["format"],
        json!({"type": "json_schema", "schema": schema()}),
        "{}",
        sent[0]
    );
}

// ---------------------------------------------------------------------------
// §9: classified upstream refusals, per route
// ---------------------------------------------------------------------------

/// OpenAI's captured schema refusal, on every route an OpenAI-shaped upstream
/// serves: the code, the param of the route, OpenAI's own message — streamed
/// and synthesized-Responses included — and generic for a request that asked
/// for no JSON.
#[tokio::test]
async fn an_openai_schema_refusal_answers_invalid_json_schema_on_every_route() {
    let mock = MockServer::start().await;
    openai_catalog(&mock).await;
    let (refusal, message) = captured("openai_invalid_schema");
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(refusal)
        .mount(&mock)
        .await;
    let (state, gw) = world(&mock.uri(), Protocol::OpenaiCompatible, false, None).await;

    for stream in [false, true] {
        let (status, v) = post(
            &gw,
            "/v1/chat/completions",
            chat_body(Some(json_schema()), stream),
        )
        .await;
        assert_eq!(status, 400, "stream {stream}: {v}");
        assert_format_error(&v, "invalid_json_schema", "response_format");
        assert_eq!(v["error"]["message"], message.as_str());
        assert_eq!(
            newest_error_kind(&state).await.as_deref(),
            Some("invalid_json_schema")
        );
    }

    let (status, v) = post(
        &gw,
        "/v1/responses",
        responses_body(json!({"type": "json_schema", "name": "answer", "schema": schema()})),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_format_error(&v, "invalid_json_schema", "text.format");
    assert_eq!(v["error"]["message"], message.as_str());
    // Streamed, the run is already answering: it ends `response.failed`
    // (§11.15), with the nearest code of OpenAI's enum (api-compat design
    // §9 finding 8) and the refusal's text.
    let mut streamed =
        responses_body(json!({"type": "json_schema", "name": "answer", "schema": schema()}));
    streamed["stream"] = json!(true);
    let sse = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&streamed)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let failed = sse
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .find(|e| e["type"] == "response.failed")
        .unwrap_or_else(|| panic!("no response.failed in {sse}"));
    assert_eq!(
        failed["response"]["error"]["code"], "invalid_prompt",
        "{failed}"
    );
    assert_eq!(
        failed["response"]["error"]["message"],
        message.as_str(),
        "{failed}"
    );

    let (status, v) = post(&gw, "/v1/messages", messages_body()).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["type"], "invalid_request_error", "{v}");
    assert_eq!(v["error"]["message"], message.as_str());

    // No JSON asked: the same body is not read as a format refusal.
    let (status, v) = post(&gw, "/v1/chat/completions", chat_body(None, false)).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "upstream", "{v}");
    assert!(v["error"]["param"].is_null(), "{v}");
}

/// A native Responses upstream's refusal (its own `param: text.format.schema`)
/// answers with the route's spelling.
#[tokio::test]
async fn a_native_responses_refusal_is_classified_too() {
    let mock = MockServer::start().await;
    openai_catalog(&mock).await;
    let (refusal, message) = captured("openai_responses_invalid_schema");
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(refusal)
        .mount(&mock)
        .await;
    let (_state, gw) = world(&mock.uri(), Protocol::OpenaiCompatible, true, None).await;
    let (status, v) = post(
        &gw,
        "/v1/responses",
        responses_body(json!({"type": "json_schema", "name": "answer", "schema": schema()})),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_format_error(&v, "invalid_json_schema", "text.format");
    assert_eq!(v["error"]["message"], message.as_str());
}

/// An OpenAI model without the asked type answers `response_format_unsupported`.
#[tokio::test]
async fn an_openai_model_without_the_type_answers_response_format_unsupported() {
    let mock = MockServer::start().await;
    openai_catalog(&mock).await;
    let (refusal, message) = captured("openai_json_schema_unsupported");
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(refusal)
        .mount(&mock)
        .await;
    let (_state, gw) = world(&mock.uri(), Protocol::OpenaiCompatible, false, None).await;
    let (status, v) = post(
        &gw,
        "/v1/chat/completions",
        chat_body(Some(json_schema()), false),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_format_error(&v, "response_format_unsupported", "response_format");
    assert_eq!(v["error"]["message"], message.as_str());
}

/// `/v1/completions` forwards the body as it came and reads llama.cpp's
/// converter refusal by its text.
#[tokio::test]
async fn legacy_completions_classify_llama_cpps_refusal() {
    let mock = MockServer::start().await;
    openai_catalog(&mock).await;
    let (refusal, message) = captured("llama_conversion_failed");
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(refusal)
        .mount(&mock)
        .await;
    let (_state, gw) = world(&mock.uri(), Protocol::OpenaiCompatible, false, None).await;
    let (status, v) = post(
        &gw,
        "/v1/completions",
        json!({"model": "m", "prompt": "Reply with ok.", "max_tokens": 16,
               "json_schema": {"type": "nonsense"}}),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_format_error(&v, "invalid_json_schema", "json_schema");
    assert_eq!(v["error"]["message"], message.as_str());
}

/// Anthropic's and Gemini's captured schema refusals, through their own
/// egresses.
#[tokio::test]
async fn anthropic_and_gemini_schema_refusals_answer_invalid_json_schema() {
    let mock = MockServer::start().await;
    let (refusal, message) = captured("anthropic_invalid_type");
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(refusal)
        .mount(&mock)
        .await;
    let (_state, gw) = world(&mock.uri(), Protocol::Anthropic, false, None).await;
    let (status, v) = post(
        &gw,
        "/v1/chat/completions",
        chat_body(Some(json_schema()), false),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_format_error(&v, "invalid_json_schema", "response_format");
    assert_eq!(v["error"]["message"], message.as_str());
    let (status, v) = post(&gw, "/v1/messages", messages_body()).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["type"], "invalid_request_error", "{v}");
    assert_eq!(v["error"]["message"], message.as_str());

    let mock = MockServer::start().await;
    let (refusal, message) = captured("gemini_unrecognized_type_nested");
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1beta/models/tgt:generateContent$"))
        .respond_with(refusal)
        .mount(&mock)
        .await;
    let (_state, gw) = world(&mock.uri(), Protocol::Gemini, false, None).await;
    let (status, v) = post(
        &gw,
        "/v1/chat/completions",
        chat_body(Some(json_schema()), false),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_format_error(&v, "invalid_json_schema", "response_format");
    assert_eq!(v["error"]["message"], message.as_str());
}

/// A refusal lmgw cannot classify keeps the generic code with the
/// upstream's message, even with JSON asked.
#[tokio::test]
async fn an_unknown_refusal_stays_upstream() {
    let mock = MockServer::start().await;
    openai_catalog(&mock).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "message": "Invalid value for 'temperature'.", "type": "invalid_request_error",
            "param": "temperature", "code": null}})))
        .mount(&mock)
        .await;
    let (_state, gw) = world(&mock.uri(), Protocol::OpenaiCompatible, false, None).await;
    let (status, v) = post(
        &gw,
        "/v1/chat/completions",
        chat_body(Some(json_schema()), false),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "upstream", "{v}");
    assert!(v["error"]["param"].is_null(), "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Invalid value for 'temperature'."),
        "{v}"
    );
}

/// A refused reasoning off is retried — but not when the refusal is about
/// the schema: the same request would be refused again (§11.9). The
/// unclassified twin proves the setup does retry.
#[tokio::test]
async fn a_schema_refusal_is_never_retried_without_the_off() {
    for (classified, sends) in [(true, 1), (false, 2)] {
        let mock = MockServer::start().await;
        let refusal = if classified {
            captured("openai_invalid_schema").0
        } else {
            ResponseTemplate::new(400).set_body_json(json!({"error": {
                "message": "Unrecognized request argument supplied: reasoning_effort",
                "type": "invalid_request_error", "param": null}}))
        };
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(refusal)
            .mount(&mock)
            .await;
        let (_state, gw) = world(&mock.uri(), Protocol::OpenaiCompatible, false, None).await;
        let resp = gw
            .client()
            .post(format!("{gw}/v1/chat/completions"))
            .header("x-lmgw-reasoning", "off")
            .json(&chat_body(Some(json_schema()), false))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let v: Value = resp.json().await.unwrap();
        let sent = posted(&mock).await;
        assert_eq!(sent.len(), sends, "classified {classified}: {v}");
        assert_eq!(sent[0]["reasoning_effort"], "none");
        if classified {
            assert_eq!(v["error"]["code"], "invalid_json_schema", "{v}");
        }
    }
}

/// A `text.format` type lmgw does not model has no chat spelling for the
/// synthesized loop: refused by name, nothing sent (§11.3).
#[tokio::test]
async fn an_unknown_text_format_type_is_refused_on_the_synthesized_loop() {
    let mock = MockServer::start().await;
    openai_catalog(&mock).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_ok())
        .mount(&mock)
        .await;
    let (_state, gw) = world(&mock.uri(), Protocol::OpenaiCompatible, false, None).await;
    let (status, v) = post(
        &gw,
        "/v1/responses",
        responses_body(json!({"type": "grammar", "syntax": "lark", "definition": "start: \"a\""})),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_format_error(&v, "response_format_unsupported", "text.format");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("'grammar'"),
        "{v}"
    );
    assert!(posted(&mock).await.is_empty());
}
