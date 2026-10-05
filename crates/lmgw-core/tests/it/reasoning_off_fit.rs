//! A reasoning off fitted to the cloud model it goes to (model-capabilities
//! design §5.6): the protocol's own off where the model takes it, no control
//! where it has none, its lowest level where it cannot stop — decided from
//! its capabilities (the upstream catalog, an owner override), else from a
//! refusal: retried once with what it names, and as the last resort with no
//! control at all, on record, and remembered. No off ends in an error.
//!
//! Against wiremock upstreams that answer like the providers did when
//! probed (2026-10-04).

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{serve, Gw};

/// `my-model` → `tgt-model` on a generic (cloud) upstream at `base`, with
/// the owner's `capabilities_override` when given.
async fn setup(base: &str, protocol: Protocol, caps: Option<Value>) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "cloud".into(),
            protocol,
            kind: UpstreamKind::Generic,
            base_url: base.trim_end_matches('/').to_string(),
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
            capabilities_override: caps,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

fn openai_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "c1", "object": "chat.completion", "model": "tgt-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "OK"},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1}
    }))
}

fn openai_refusal(message: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({
        "error": {"message": message, "type": "invalid_request_error", "param": null}
    }))
}

const OPENAI_SSE: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"OK\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
);

fn gemini_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "candidates": [{"content": {"role": "model", "parts": [{"text": "OK"}]},
                        "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 1}
    }))
}

/// `gpt-4.1-nano`, as probed: any `reasoning_effort` is refused by name.
async fn without_reasoning(mock: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("reasoning_effort"))
        .respond_with(openai_refusal(
            "Unrecognized request argument supplied: reasoning_effort",
        ))
        .with_priority(1)
        .mount(mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_ok())
        .mount(mock)
        .await;
}

/// Every chat body the upstream was sent, in order.
async fn posted(mock: &MockServer) -> Vec<Value> {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// `(status, error_msg)` of every request row, oldest first.
async fn rows(state: &SharedState) -> Vec<(i64, Option<String>)> {
    sqlx::query_as("SELECT status, error_msg FROM request_logs ORDER BY id")
        .fetch_all(&state.db)
        .await
        .unwrap()
}

async fn chat(base: &Gw, headers: &[(&str, &str)], stream: bool) -> reqwest::Response {
    let mut rb = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "my-model", "stream": stream,
                      "messages": [{"role": "user", "content": "Reply with OK."}]}));
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    rb.send().await.unwrap()
}

fn ignored(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("x-lmgw-reasoning-ignored")
        .map(|v| v.to_str().unwrap().to_string())
}

// ---------------------------------------------------------------------------
// Nothing known: the protocol's off, corrected once by a refusal
// ---------------------------------------------------------------------------

/// The `gpt-4.1-nano` case: no catalog says anything, the off goes out as
/// `"none"`, the provider refuses the control by name — retried once
/// without it, the refused call on record, and the route remembers.
#[tokio::test]
async fn a_refused_off_is_retried_without_the_control_and_remembered() {
    let mock = MockServer::start().await;
    without_reasoning(&mock).await;
    let (state, base) = setup(&mock.uri(), Protocol::Openai, None).await;

    let resp = chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp).as_deref(), Some("enabled"));
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "OK");

    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[0]["reasoning_effort"], "none");
    assert!(sent[1].get("reasoning_effort").is_none(), "{}", sent[1]);
    let r = rows(&state).await;
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(r[0].0, 400);
    let note = r[0].1.as_deref().unwrap();
    assert!(note.contains("Unrecognized request argument"), "{note}");
    assert!(
        note.contains("retried once with no reasoning control"),
        "{note}"
    );
    assert_eq!(r[1], (200, None));

    // Remembered: the next off goes out without the control at once.
    let resp = chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp).as_deref(), Some("enabled"));
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 3);
    assert!(sent[2].get("reasoning_effort").is_none(), "{}", sent[2]);
    assert_eq!(rows(&state).await.len(), 3);
}

/// The `gpt-5-nano` case, streamed: the refusal lists the values the model
/// takes, and the least of them is what the retry sends.
#[tokio::test]
async fn a_refusal_listing_its_values_is_retried_with_the_least() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"reasoning_effort": "none"})))
        .respond_with(openai_refusal(
            "Unsupported value: 'reasoning_effort' does not support 'none' with this model. \
             Supported values are: 'minimal', 'low', 'medium', and 'high'.",
        ))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(OPENAI_SSE, "text/event-stream"),
        )
        .mount(&mock)
        .await;
    let (state, base) = setup(&mock.uri(), Protocol::Openai, None).await;

    let resp = chat(&base, &[("x-lmgw-reasoning", "off")], true).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp).as_deref(), Some("enabled"));
    assert!(resp.text().await.unwrap().contains("[DONE]"));
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["reasoning_effort"], "minimal");
    let r = rows(&state).await;
    assert_eq!(r[0].0, 400);
    assert!(
        r[0].1
            .as_deref()
            .unwrap()
            .contains("reasoning_effort: \"minimal\""),
        "{r:?}"
    );
}

/// The `gemini-flash-latest` case: the off goes out as the lowest Gemini
/// level, the model's least is above it and its refusal names the level —
/// one level up.
#[tokio::test]
async fn gemini_climbs_from_minimal_to_the_models_least_level() {
    let mock = MockServer::start().await;
    let generate = "/v1beta/models/tgt-model:generateContent";
    Mock::given(method("POST"))
        .and(path(generate))
        .and(body_partial_json(
            json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "minimal"}}}),
        ))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "code": 400, "status": "INVALID_ARGUMENT",
            "message": "Thinking level MINIMAL is not supported for this model. Please retry \
                        with other thinking level."
        }})))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path(generate))
        .respond_with(gemini_ok())
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri(), Protocol::Gemini, None).await;

    let resp = chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    assert_eq!(resp.status(), 200);
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2);
    let level = |b: &Value| b["generationConfig"]["thinkingConfig"].clone();
    assert_eq!(level(&sent[0]), json!({"thinkingLevel": "minimal"}));
    assert_eq!(level(&sent[1]), json!({"thinkingLevel": "low"}));

    // A budget of 0 is an off too, and takes what was learned.
    let resp = chat(&base, &[("x-lmgw-reasoning-budget", "0")], false).await;
    assert_eq!(resp.status(), 200);
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 3);
    assert_eq!(level(&sent[2]), json!({"thinkingLevel": "low"}));
}

/// Only off is fitted: a level asked of a model without reasoning is the
/// provider's refusal, passed on as it came, and nothing is retried.
#[tokio::test]
async fn an_explicit_level_on_a_model_without_reasoning_is_the_providers_error() {
    let mock = MockServer::start().await;
    without_reasoning(&mock).await;
    let (state, base) = setup(&mock.uri(), Protocol::Openai, None).await;

    let resp = chat(&base, &[("x-lmgw-reasoning-effort", "high")], false).await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unrecognized request argument supplied: reasoning_effort"),
        "{body}"
    );
    assert_eq!(posted(&mock).await.len(), 1);
    assert_eq!(rows(&state).await.len(), 1);
}

/// A refusal that does not name the control is tried once more without the
/// off — only that can tell whether the off was what it refused — and, when
/// it is refused again, that answer stands.
#[tokio::test]
async fn a_refusal_about_something_else_is_retried_without_the_off_and_stands() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_refusal("Request contains an invalid argument."))
        .mount(&mock)
        .await;
    let (state, base) = setup(&mock.uri(), Protocol::Openai, None).await;

    let resp = chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid argument"),
        "{body}"
    );
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[0]["reasoning_effort"], "none");
    assert!(sent[1].get("reasoning_effort").is_none(), "{}", sent[1]);
    let r = rows(&state).await;
    assert_eq!(
        r.iter().map(|r| r.0).collect::<Vec<_>>(),
        [400, 400],
        "{r:?}"
    );
    assert!(
        r[0].1
            .as_deref()
            .unwrap()
            .contains("retried with no reasoning control"),
        "{r:?}"
    );

    // Nothing was learned: the next off goes out as the off again.
    chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    assert_eq!(posted(&mock).await[2]["reasoning_effort"], "none");
}

/// A refusal lmgw can tell is about the context stands at once: without the
/// off it would be refused again.
#[tokio::test]
async fn a_context_refusal_is_not_retried() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_refusal(
            "This model's maximum context length is 1024 tokens. However, your messages \
             resulted in 5000 tokens.",
        ))
        .mount(&mock)
        .await;
    let (state, base) = setup(&mock.uri(), Protocol::Openai, None).await;
    let resp = chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    assert_eq!(resp.status(), 400);
    assert_eq!(posted(&mock).await.len(), 1);
    assert_eq!(rows(&state).await.len(), 1);
}

/// The owner's ruling (2026-10-04): a model that refuses every form of off
/// runs with its default reasoning rather than into an error. The off, then
/// the least level its refusal named, then no control — three sends, every
/// one on record — and the route keeps what answered.
#[tokio::test]
async fn every_off_refused_answers_with_no_control_and_is_remembered() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("reasoning_effort"))
        .respond_with(openai_refusal(
            "Unsupported value: 'reasoning_effort' does not support this value with this \
             model. Supported values are: 'minimal', 'low', 'medium', and 'high'.",
        ))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_ok())
        .mount(&mock)
        .await;
    let (state, base) = setup(&mock.uri(), Protocol::Openai, None).await;

    let resp = chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp).as_deref(), Some("enabled"));
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert_eq!(sent[0]["reasoning_effort"], "none");
    assert_eq!(sent[1]["reasoning_effort"], "minimal");
    assert!(sent[2].get("reasoning_effort").is_none(), "{}", sent[2]);
    let r = rows(&state).await;
    assert_eq!(
        r.iter().map(|r| r.0).collect::<Vec<_>>(),
        [400, 400, 200],
        "{r:?}"
    );
    let last_resort = r[1].1.as_deref().unwrap();
    assert!(
        last_resort.contains("retried with no reasoning control")
            && last_resort.contains("reasons as it does by default"),
        "{last_resort}"
    );

    // Remembered: no control at once.
    let resp = chat(&base, &[("x-lmgw-reasoning", "off")], true).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp).as_deref(), Some("enabled"));
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 4);
    assert!(sent[3].get("reasoning_effort").is_none(), "{}", sent[3]);
}

/// `gemini-flash-lite-latest` answers `thinkingBudget: 0` with a bare
/// "invalid argument" that names nothing. An owner override saying the model
/// can stop sends that budget; the refusal is retried with no thinking
/// config, which answers.
#[tokio::test]
async fn a_bare_refusal_of_the_budget_off_answers_with_no_thinking_config() {
    let mock = MockServer::start().await;
    let generate = "/v1beta/models/tgt-model:generateContent";
    Mock::given(method("POST"))
        .and(path(generate))
        .and(body_partial_json(
            json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 0}}}),
        ))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "code": 400, "status": "INVALID_ARGUMENT",
            "message": "Request contains an invalid argument."
        }})))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path(generate))
        .respond_with(gemini_ok())
        .mount(&mock)
        .await;
    let (_state, base) = setup(
        &mock.uri(),
        Protocol::Gemini,
        Some(json!({"capabilities": {"task": "chat",
            "reasoning": {"kind": "toggle", "can_disable": true}}})),
    )
    .await;

    let resp = chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp).as_deref(), Some("enabled"));
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2);
    let config = |b: &Value| b["generationConfig"].get("thinkingConfig").cloned();
    assert_eq!(config(&sent[0]), Some(json!({"thinkingBudget": 0})));
    assert_eq!(config(&sent[1]), None, "{}", sent[1]);

    // The facts still say the budget; the lesson says the provider refused
    // exactly that.
    chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 3);
    assert_eq!(config(&sent[2]), None, "{}", sent[2]);
}

// ---------------------------------------------------------------------------
// Known capabilities decide; a refusal still corrects them
// ---------------------------------------------------------------------------

/// Mount an OpenAI-shaped (Kilo/OpenRouter) catalog with `entry` for
/// `tgt-model`, and a chat route that refuses any `reasoning_effort` other
/// than `allowed` — so a decision the catalog should have made, made by the
/// retry instead, shows up as a second call.
async fn kilo_catalog(mock: &MockServer, entry: Value) {
    let mut entry = entry;
    entry["id"] = json!("tgt-model");
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [entry]})))
        .mount(mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_ok())
        .mount(mock)
        .await;
}

/// The catalog lists the levels the model takes and `none` is not one:
/// its least goes out, without a refusal first.
#[tokio::test]
async fn a_catalog_listing_levels_without_off_sends_the_least() {
    let mock = MockServer::start().await;
    kilo_catalog(
        &mock,
        json!({"supported_parameters": ["reasoning", "tools"],
               "opencode": {"variants": {"low": {}, "medium": {}, "high": {}}}}),
    )
    .await;
    let (state, base) = setup(&mock.uri(), Protocol::Openai, None).await;

    let resp = chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp).as_deref(), Some("enabled"));
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["reasoning_effort"], "low");
    assert_eq!(rows(&state).await, vec![(200, None)]);
}

/// A form the catalog chose is retried like any other when the provider
/// refuses it, and the lesson corrects the catalog for this route.
#[tokio::test]
async fn a_form_the_catalog_chose_is_retried_when_refused() {
    let mock = MockServer::start().await;
    without_reasoning(&mock).await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{
            "id": "tgt-model", "supported_parameters": ["reasoning"],
            "opencode": {"variants": {"low": {}, "high": {}}}
        }]})))
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri(), Protocol::Openai, None).await;

    let resp = chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    assert_eq!(resp.status(), 200);
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[0]["reasoning_effort"], "low");
    assert!(sent[1].get("reasoning_effort").is_none(), "{}", sent[1]);

    chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 3);
    assert!(sent[2].get("reasoning_effort").is_none(), "{}", sent[2]);
}

/// The catalog says the model can switch off: the protocol's own off, and
/// nothing reported.
#[tokio::test]
async fn a_catalog_that_can_switch_off_gets_the_off() {
    let mock = MockServer::start().await;
    kilo_catalog(
        &mock,
        json!({"supported_parameters": ["reasoning"],
               "opencode": {"variants": {"none": {}, "low": {}, "high": {}}}}),
    )
    .await;
    let (_state, base) = setup(&mock.uri(), Protocol::Openai, None).await;

    let resp = chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp), None);
    assert_eq!(posted(&mock).await[0]["reasoning_effort"], "none");
}

/// Gemini's catalog says the model does not think: no thinking config at
/// all, where the budget form used to go out.
#[tokio::test]
async fn a_gemini_model_that_does_not_think_gets_no_thinking_config() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [{
            "name": "models/tgt-model",
            "supportedGenerationMethods": ["generateContent"],
            "thinking": false
        }]})))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/tgt-model:generateContent"))
        .respond_with(gemini_ok())
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri(), Protocol::Gemini, None).await;

    let resp = chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp).as_deref(), Some("enabled"));
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0]["generationConfig"].get("thinkingConfig").is_none(),
        "{}",
        sent[0]
    );
}

/// The owner's override on the alias settles it where no catalog does: a
/// model declared `fixed` gets no control, and the upstream that would
/// refuse one is asked once.
#[tokio::test]
async fn an_owner_override_settles_it() {
    let mock = MockServer::start().await;
    without_reasoning(&mock).await;
    let (state, base) = setup(
        &mock.uri(),
        Protocol::Openai,
        Some(json!({"capabilities": {"task": "chat", "reasoning": {"kind": "fixed"}}})),
    )
    .await;

    let resp = chat(&base, &[("x-lmgw-reasoning", "off")], false).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(ignored(&resp).as_deref(), Some("enabled"));
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 1);
    assert!(sent[0].get("reasoning_effort").is_none(), "{}", sent[0]);
    assert_eq!(rows(&state).await, vec![(200, None)]);
}

// ---------------------------------------------------------------------------
// In-process callers: the Chat's text turn, /v1/responses
// ---------------------------------------------------------------------------

/// A Chat thread set to reasoning off, on a model without reasoning: the
/// turn answers, and its `done` says the off was not sent.
#[tokio::test]
async fn a_chat_turn_with_reasoning_off_answers_on_a_model_without_it() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("reasoning_effort"))
        .respond_with(openai_refusal(
            "Unrecognized request argument supplied: reasoning_effort",
        ))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(OPENAI_SSE, "text/event-stream"),
        )
        .mount(&mock)
        .await;
    let (state, base) = setup(&mock.uri(), Protocol::Openai, None).await;
    let client = base.client();
    let t: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({"model_alias": "my-model", "kind": "chat"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = t["id"].as_i64().unwrap();
    let r = client
        .post(format!("{base}/chat/api/threads/{tid}/settings"))
        .json(&json!({"reasoning_enabled": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let sse = client
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({"content": "Reply with OK."}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let done = sse
        .split("\n\n")
        .find(|r| r.contains("event: done"))
        .and_then(|r| r.lines().find_map(|l| l.strip_prefix("data:")))
        .map(|d| serde_json::from_str::<Value>(d.trim()).unwrap())
        .unwrap_or_else(|| panic!("no done frame: {sse}"));
    assert!(!sse.contains("event: error"), "{sse}");
    assert_eq!(done["reasoning_ignored"], json!(["enabled"]), "{done}");
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2);
    assert!(sent[1].get("reasoning_effort").is_none(), "{}", sent[1]);
    let r = rows(&state).await;
    assert_eq!(
        r.iter().map(|r| r.0).collect::<Vec<_>>(),
        [400, 200],
        "{r:?}"
    );
}

/// A Chat thread with reasoning off on a model that refuses every form of
/// off: the turn answers with the model's default reasoning, no error, and
/// its `done` says so in a sentence for the composer's line.
#[tokio::test]
async fn a_chat_turn_on_a_model_refusing_every_off_answers_and_says_so() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("reasoning_effort"))
        .respond_with(openai_refusal(
            "Unsupported value: 'reasoning_effort' does not support this value with this \
             model. Supported values are: 'low' and 'high'.",
        ))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    concat!(
                        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Hm.\"}}]}\n\n",
                        "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\n",
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                        "data: [DONE]\n\n"
                    ),
                    "text/event-stream",
                ),
        )
        .mount(&mock)
        .await;
    let (_state, base) = setup(&mock.uri(), Protocol::Openai, None).await;
    let client = base.client();
    let t: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({"model_alias": "my-model", "kind": "chat"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = t["id"].as_i64().unwrap();
    client
        .post(format!("{base}/chat/api/threads/{tid}/settings"))
        .json(&json!({"reasoning_enabled": false}))
        .send()
        .await
        .unwrap();
    let sse = client
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({"content": "Reply with OK."}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!sse.contains("event: error"), "{sse}");
    let done = sse
        .split("\n\n")
        .find(|r| r.contains("event: done"))
        .and_then(|r| r.lines().find_map(|l| l.strip_prefix("data:")))
        .map(|d| serde_json::from_str::<Value>(d.trim()).unwrap())
        .unwrap_or_else(|| panic!("no done frame: {sse}"));
    assert_eq!(done["reasoning_ignored"], json!(["enabled"]), "{done}");
    assert_eq!(
        done["reasoning_note"],
        "my-model refused every way lmgw has to switch reasoning off; it reasons as it does by \
         default"
    );
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert_eq!(sent[1]["reasoning_effort"], "low");
    let thread: Value = client
        .get(format!("{base}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let reply = thread["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        (reply["content"].as_str(), reply["reasoning"].as_str()),
        (Some("OK"), Some("Hm."))
    );
}

/// `/v1/responses` runs its turns in-process (`sample_once`): the same fit.
#[tokio::test]
async fn a_responses_turn_with_reasoning_off_answers_on_a_model_without_it() {
    let mock = MockServer::start().await;
    without_reasoning(&mock).await;
    let (_state, base) = setup(&mock.uri(), Protocol::Openai, None).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .header("x-lmgw-reasoning", "off")
        .json(&json!({"model": "my-model", "input": "Reply with OK."}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "completed", "{body}");
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2);
    assert!(sent[1].get("reasoning_effort").is_none(), "{}", sent[1]);
}
