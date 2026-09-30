//! The compatibility counters (api-docs design §5, §7.3): `POST
//! /v1/messages/count_tokens`, `POST /tokenize`, and `/v1/count_tokens`'s new
//! scope check and approximation header. WP3.
//!
//! Every backend branch §5.2 names is driven over real HTTP against a
//! wiremock upstream — or, for a local llama.cpp chat row, the `gpu_world`
//! containers — with and without `x-lmgw-count-approximate`, plus each
//! route's refusals in its own client's dialect.

use lmgw_core::config::{hash_api_key, Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use lmgw_core::telemetry::Event;
use serde_json::{json, Value};
use wiremock::matchers::{any, body_json, body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{self, Gw};
use crate::support::gpu_world::{Gpu, GIB};

/// The counters on a local row's containers: admission, starts, media.
mod local;

const APPROX: &str = "x-lmgw-count-approximate";

/// A gateway with one alias, `my-model`, on an upstream of `protocol`/`kind`
/// at `base` whose model is `upstream_model` — `e2e_proxy.rs`'s setup, with
/// the model name free, since the tiktoken guess turns on it.
async fn gateway(
    base: &str,
    protocol: Protocol,
    kind: UpstreamKind,
    upstream_model: &str,
) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol,
            kind,
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
            upstream_model_id: upstream_model.into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = common::serve(state.clone()).await;
    (state, gw)
}

async fn post(gw: &Gw, route: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gw}{route}"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn head<'r>(resp: &'r reqwest::Response, name: &str) -> Option<&'r str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

/// The request the Anthropic SDKs' `messages.count_tokens()` sends.
fn sdk_body(model: &str) -> Value {
    json!({
        "model": model,
        "system": "Be brief.",
        "tools": [{"name": "get_weather", "description": "Weather by city",
                   "input_schema": {"type": "object",
                                    "properties": {"city": {"type": "string"}}}}],
        "messages": [{"role": "user", "content": "Weather in Oslo?"}],
    })
}

// ---------------------------------------------------------------------------
// POST /v1/messages/count_tokens, per backend (§5.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn messages_count_passes_through_to_anthropic() {
    let mock = MockServer::start().await;
    // The client's body verbatim — its server tool too, which the IR cannot
    // carry — with only `model` rewritten, and the egress's own auth.
    let server_tool = json!({"type": "web_search_20250305", "name": "web_search"});
    let mut sent = sdk_body("tgt-model");
    sent["thinking"] = json!({"type": "enabled", "budget_tokens": 2048});
    sent["tools"]
        .as_array_mut()
        .unwrap()
        .push(server_tool.clone());
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .and(header("x-api-key", "sk-up"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(body_json(&sent))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 421})))
        .expect(1)
        .mount(&mock)
        .await;
    let (_state, gw) = gateway(
        &mock.uri(),
        Protocol::Anthropic,
        UpstreamKind::Generic,
        "tgt-model",
    )
    .await;

    let mut body = sent.clone();
    body["model"] = json!("my-model");
    let resp = post(&gw, "/v1/messages/count_tokens", body).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(head(&resp, APPROX), None, "Anthropic's own count is exact");
    let got: Value = resp.json().await.unwrap();
    assert_eq!(got, json!({"input_tokens": 421}));
}

/// The reasoning headers mean on this route what they mean on `/v1/messages`:
/// the count is made for the thinking lmgw would send, not the client's.
#[tokio::test]
async fn messages_count_on_anthropic_renders_the_reasoning_header() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .and(body_partial_json(json!({"thinking": {"type": "disabled"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 9})))
        .expect(1)
        .mount(&mock)
        .await;
    let (_state, gw) = gateway(
        &mock.uri(),
        Protocol::Anthropic,
        UpstreamKind::Generic,
        "tgt-model",
    )
    .await;

    let mut body = sdk_body("my-model");
    body["thinking"] = json!({"type": "enabled", "budget_tokens": 2048});
    body["output_config"] = json!({"effort": "high"});
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/messages/count_tokens"))
        .header("x-lmgw-reasoning", "off")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent: Value =
        serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent["thinking"], json!({"type": "disabled"}));
    assert!(sent.get("output_config").is_none(), "{sent}");

    // A malformed header is the 400 `/v1/messages` gives, in its dialect.
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/messages/count_tokens"))
        .header("x-lmgw-reasoning", "sideways")
        .json(&sdk_body("my-model"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["type"], "error");
    assert_eq!(err["error"]["type"], "invalid_request_error");
}

/// Only the thinking is lmgw's to replace (review R3 #6): the rest of the
/// client's `output_config` is part of the request being counted, and a
/// header's effort is merged into it.
#[tokio::test]
async fn messages_count_on_anthropic_keeps_the_rest_of_output_config() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 9})))
        .expect(2)
        .mount(&mock)
        .await;
    let (_state, gw) = gateway(
        &mock.uri(),
        Protocol::Anthropic,
        UpstreamKind::Generic,
        "tgt-model",
    )
    .await;
    let format = json!({"type": "json_schema", "schema": {"type": "object"}});
    let mut body = sdk_body("my-model");
    body["output_config"] = json!({"format": format, "effort": "high"});

    for (name, value) in [
        ("x-lmgw-reasoning-effort", "low"),
        ("x-lmgw-reasoning", "off"),
    ] {
        let resp = reqwest::Client::new()
            .post(format!("{gw}/v1/messages/count_tokens"))
            .header(name, value)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{name}");
    }
    let sent: Vec<Value> = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(sent[0]["thinking"], json!({"type": "adaptive"}));
    assert_eq!(
        sent[0]["output_config"],
        json!({"format": format, "effort": "low"})
    );
    assert_eq!(sent[1]["thinking"], json!({"type": "disabled"}));
    assert_eq!(sent[1]["output_config"], json!({"format": format}));
}

#[tokio::test]
async fn messages_count_on_gemini_counts_the_whole_request() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/tgt-model:countTokens"))
        .and(header("x-goog-api-key", "sk-up"))
        .and(body_partial_json(json!({"generateContentRequest": {
            "model": "models/tgt-model",
            "systemInstruction": {"parts": [{"text": "Be brief."}]},
            "contents": [{"role": "user", "parts": [{"text": "Weather in Oslo?"}]}],
            "tools": [{"functionDeclarations": [{"name": "get_weather"}]}],
        }})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"totalTokens": 58})))
        .expect(1)
        .mount(&mock)
        .await;
    let (_state, gw) = gateway(
        &mock.uri(),
        Protocol::Gemini,
        UpstreamKind::Generic,
        "tgt-model",
    )
    .await;

    let resp = post(&gw, "/v1/messages/count_tokens", sdk_body("my-model")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(head(&resp, APPROX), None);
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({"input_tokens": 58})
    );
}

/// The reasoning headers reach Gemini's whole-request count the way they
/// reach its send: rendered into `generationConfig.thinkingConfig` by the
/// same egress, so the count is of the request `/v1/messages` would make.
#[tokio::test]
async fn messages_count_on_gemini_renders_the_reasoning_header() {
    let mock = MockServer::start().await;
    for (budget, tokens) in [(512, 12), (0, 10)] {
        Mock::given(method("POST"))
            .and(path("/v1beta/models/tgt-model:countTokens"))
            .and(body_partial_json(json!({"generateContentRequest": {
                "generationConfig": {"thinkingConfig": {"thinkingBudget": budget}},
            }})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"totalTokens": tokens})))
            .expect(1)
            .mount(&mock)
            .await;
    }
    let (_s, gw) = gateway(
        &mock.uri(),
        Protocol::Gemini,
        UpstreamKind::Generic,
        "tgt-model",
    )
    .await;

    for ((name, value), tokens) in [
        (("x-lmgw-reasoning-budget", "512"), 12),
        (("x-lmgw-reasoning", "off"), 10),
    ] {
        let resp = reqwest::Client::new()
            .post(format!("{gw}/v1/messages/count_tokens"))
            .header(name, value)
            .json(&sdk_body("my-model"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{name}: {value}");
        assert_eq!(head(&resp, APPROX), None);
        assert_eq!(
            resp.json::<Value>().await.unwrap(),
            json!({"input_tokens": tokens}),
            "{name}: {value}"
        );
    }
}

#[tokio::test]
async fn messages_count_on_generic_openai_flattens_and_says_so() {
    let mock = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&mock)
        .await;

    // A model tiktoken knows: flattened, and nothing else.
    let (_s, gw) = gateway(
        &mock.uri(),
        Protocol::Openai,
        UpstreamKind::Generic,
        "gpt-4o",
    )
    .await;
    let resp = post(&gw, "/v1/messages/count_tokens", sdk_body("my-model")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(head(&resp, APPROX), Some("flattened"));
    let n = resp.json::<Value>().await.unwrap()["input_tokens"]
        .as_u64()
        .unwrap();
    assert!(n > 0);

    // One it does not: the tokenizer was a guess too. An image is not in the
    // number at all.
    let (_s, gw) = gateway(
        &mock.uri(),
        Protocol::Openai,
        UpstreamKind::Generic,
        "some-open-model",
    )
    .await;
    let mut body = sdk_body("my-model");
    body["messages"][0]["content"] = json!([
        {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                     "data": "iVBORw0KGgo="}},
        {"type": "text", "text": "Weather in Oslo?"}
    ]);
    let resp = post(&gw, "/v1/messages/count_tokens", body).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        head(&resp, APPROX),
        Some("flattened,tokenizer_guess,media_omitted")
    );
}

#[tokio::test]
async fn messages_count_on_remote_llama_flattens_via_tokenize() {
    let mock = MockServer::start().await;
    let schema = sdk_body("_")["tools"][0]["input_schema"].to_string();
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .and(header("authorization", "Bearer sk-up"))
        .and(body_json(json!({
            "model": "tgt-model",
            "content": format!(
                "Be brief.\n\nget_weather\nWeather by city\n{schema}\n\nuser: Weather in Oslo?"
            ),
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tokens": [1, 2, 3, 4]})))
        .expect(1)
        .mount(&mock)
        .await;
    let (_s, gw) = gateway(
        &mock.uri(),
        Protocol::Openai,
        UpstreamKind::LlamaServer,
        "tgt-model",
    )
    .await;

    let resp = post(&gw, "/v1/messages/count_tokens", sdk_body("my-model")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(head(&resp, APPROX), Some("flattened"));
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({"input_tokens": 4})
    );
}

#[tokio::test]
async fn messages_count_on_local_llama_renders_the_template() {
    let gpu = Gpu::new(8 * GIB, 2, 5).await;
    gpu.model("chat-model", GIB).await;
    gpu.world().prompt_tokens = 37;
    let gw = common::serve(gpu.state.clone()).await;

    let resp = post(&gw, "/v1/messages/count_tokens", sdk_body("chat-model")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(head(&resp, APPROX), None, "the template count is exact");
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({"input_tokens": 37})
    );
    {
        let w = gpu.world();
        assert_eq!(w.templated.len(), 1, "one /apply-template");
        let rendered = &w.templated[0];
        assert_eq!(rendered["model"], "chat-model");
        assert_eq!(
            rendered["messages"][0],
            json!({"role": "system", "content": "Be brief."})
        );
        assert_eq!(rendered["tools"][0]["function"]["name"], "get_weather");
        // The completion path's own flags (gate::count): BOS included.
        assert_eq!(
            w.tokenized,
            vec![json!({"content": "p", "add_special": true})]
        );
    }

    // An image on a row with no per-image bound (no projector): the text is
    // counted exactly, the image not at all — and the header says so. The
    // template is not handed the image either: a server without a projector
    // refuses one with a 500, which made this count a 502 (review R1 #1).
    let mut body = sdk_body("chat-model");
    body["messages"][0]["content"] = json!([
        {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                     "data": "iVBORw0KGgo="}},
        {"type": "text", "text": "Weather in Oslo?"}
    ]);
    let resp = post(&gw, "/v1/messages/count_tokens", body).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(head(&resp, APPROX), Some("media_omitted"));
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({"input_tokens": 37})
    );
    assert_eq!(
        gpu.world().templated[1]["messages"][1],
        json!({"role": "user", "content": "Weather in Oslo?"}),
        "the turn went to the template as its text alone"
    );
    assert_eq!(gpu.runs(), vec!["chat-model".to_string()], "one start");
}

#[tokio::test]
async fn messages_count_errors_are_anthropic_shaped() {
    let mock = MockServer::start().await;
    let (_s, gw) = gateway(
        &mock.uri(),
        Protocol::Openai,
        UpstreamKind::Generic,
        "gpt-4o",
    )
    .await;

    let shape = |status: u16, kind: &'static str| {
        move |(got, body): (u16, Value)| {
            assert_eq!(got, status, "{body}");
            assert_eq!(body["type"], "error", "{body}");
            assert_eq!(body["error"]["type"], kind, "{body}");
        }
    };
    let call = |body: Value| {
        let gw = gw.clone();
        async move {
            let resp = post(&gw, "/v1/messages/count_tokens", body).await;
            (resp.status().as_u16(), resp.json::<Value>().await.unwrap())
        }
    };

    shape(400, "invalid_request_error")(
        call(json!({"messages": [{"role": "user", "content": "hi"}]})).await,
    );
    shape(404, "not_found_error")(call(sdk_body("ghost")).await);
    // A server tool on a route that has to translate the request.
    let mut body = sdk_body("my-model");
    body["tools"] = json!([{"type": "web_search_20250305", "name": "web_search"}]);
    shape(400, "invalid_request_error")(call(body).await);
    // Not JSON at all: the body extractor's 400, in the same dialect.
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/messages/count_tokens"))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    shape(400, "invalid_request_error")((
        resp.status().as_u16(),
        resp.json::<Value>().await.unwrap(),
    ));
}

#[tokio::test]
async fn messages_count_relays_an_upstream_refusal_in_anthropic_shape() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": "messages: at least one"},
        })))
        .mount(&mock)
        .await;
    let (_s, gw) = gateway(
        &mock.uri(),
        Protocol::Anthropic,
        UpstreamKind::Generic,
        "tgt-model",
    )
    .await;
    let resp = post(
        &gw,
        "/v1/messages/count_tokens",
        json!({"model": "my-model", "messages": []}),
    )
    .await;
    // The provider's status and words, in the envelope every lmgw Anthropic
    // route gives an upstream error (`api_error`, as on `/v1/messages`).
    assert_eq!(resp.status(), 400);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["type"], "error");
    assert_eq!(err["error"]["type"], "api_error");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("messages: at least one"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Hold and fallback: all three counters, as on /v1/count_tokens
// ---------------------------------------------------------------------------

/// A local chat row under the GPU hold, with `cloud-chat` as the hold's
/// fallback when `fallback` is set.
async fn held(fallback: bool) -> (Gpu, Gw) {
    let gpu = Gpu::new(8 * GIB, 2, 5).await;
    gpu.model("chat-model", GIB).await;
    if fallback {
        gpu.cloud("cloud-chat", None).await;
        let mut s = gpu.state.snapshot().settings.clone();
        s.hold.fallback_alias = Some("cloud-chat".into());
        store::save_settings(&gpu.state.db, &s).await.unwrap();
        gpu.state.reload_snapshot().await.unwrap();
    }
    lmgw_core::ops::hold_set(&gpu.state, true).await.unwrap();
    let gw = common::serve(gpu.state.clone()).await;
    (gpu, gw)
}

#[tokio::test]
async fn messages_count_under_hold_names_the_fallback() {
    let (gpu, gw) = held(true).await;

    let resp = post(&gw, "/v1/messages/count_tokens", sdk_body("chat-model")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(head(&resp, "x-lmgw-fallback"), Some("cloud-chat"));
    assert_eq!(head(&resp, "x-lmgw-fallback-reason"), Some("hold"));
    // The fallback is a generic cloud route: its count is flattened.
    assert!(
        head(&resp, APPROX)
            .unwrap_or_default()
            .contains("flattened"),
        "{:?}",
        resp.headers()
    );

    // `/v1/count_tokens` the same way, with the flags of the route that
    // actually counted.
    let resp = post(
        &gw,
        "/v1/count_tokens",
        json!({"model": "chat-model", "input": "hello there"}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(head(&resp, "x-lmgw-fallback"), Some("cloud-chat"));
    assert_eq!(head(&resp, "x-lmgw-fallback-reason"), Some("hold"));

    // `/tokenize` resolves to the same fallback, which has no token ids: the
    // 501 still names who would have answered.
    let resp = post(
        &gw,
        "/tokenize",
        json!({"model": "chat-model", "content": "hi"}),
    )
    .await;
    assert_eq!(resp.status(), 501);
    assert_eq!(head(&resp, "x-lmgw-fallback"), Some("cloud-chat"));
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["type"], "not_supported_error");

    assert!(gpu.runs().is_empty(), "a held count starts nothing");
}

#[tokio::test]
async fn a_hold_without_fallback_is_refused_in_each_dialect() {
    let (gpu, gw) = held(false).await;

    let resp = post(&gw, "/v1/messages/count_tokens", sdk_body("chat-model")).await;
    assert_eq!(resp.status(), 503);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["type"], "error");
    assert_eq!(err["error"]["type"], "api_error", "never overloaded_error");

    let resp = post(
        &gw,
        "/tokenize",
        json!({"model": "chat-model", "content": "hi"}),
    )
    .await;
    assert_eq!(resp.status(), 503);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["code"], 503);
    assert_eq!(err["error"]["type"], "unavailable_error");
    assert_eq!(err["error"]["lmgw_code"], "gpu_hold", "{err}");

    assert!(gpu.runs().is_empty());
}

// ---------------------------------------------------------------------------
// What is logged (§5.4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn counts_are_not_logged_but_scope_refusals_are() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tokens": [1, 2, 3]})))
        .mount(&mock)
        .await;
    let (state, gw) = gateway(
        &mock.uri(),
        Protocol::Openai,
        UpstreamKind::LlamaServer,
        "tgt-model",
    )
    .await;
    // Auth on, and a key allowed `my-model` only.
    let mut settings = state.snapshot().settings.clone();
    settings.auth_enabled = true;
    store::save_settings(&state.db, &settings).await.unwrap();
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, scope_mode, scope_patterns)
         VALUES ('scoped', ?1, 1, 'allow', 'my-model')",
    )
    .bind(hash_api_key("lmgw-scoped"))
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let mut rx = state.telemetry.subscribe();

    let send = |route: &'static str, body: Value| {
        let gw = gw.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{gw}{route}"))
                .bearer_auth("lmgw-scoped")
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };
    let counters = |alias: &str| {
        [
            ("/v1/count_tokens", json!({"model": alias, "input": "hi"})),
            (
                "/v1/messages/count_tokens",
                json!({"model": alias, "messages": [{"role": "user", "content": "hi"}]}),
            ),
            ("/tokenize", json!({"model": alias, "content": "hi"})),
        ]
    };

    for (route, body) in counters("my-model") {
        let resp = send(route, body).await;
        assert_eq!(resp.status(), 200, "{route}");
    }
    assert!(
        store::query_logs(&state.db, &Default::default())
            .await
            .unwrap()
            .is_empty(),
        "a count is not traffic"
    );
    while let Ok(ev) = rx.try_recv() {
        assert!(
            !matches!(ev, Event::Request(_)),
            "no live-feed event either"
        );
    }

    let mut bodies = Vec::new();
    for (route, body) in counters("other-model") {
        let resp = send(route, body).await;
        assert_eq!(resp.status(), 403, "{route}");
        bodies.push(resp.json::<Value>().await.unwrap());
    }
    // Each in its own client's dialect.
    assert_eq!(bodies[0]["error"]["code"], "key_scope", "{}", bodies[0]);
    assert_eq!(bodies[1]["type"], "error");
    assert_eq!(bodies[1]["error"]["type"], "permission_error");
    assert_eq!(
        bodies[2]["error"],
        json!({"code": 403, "type": "permission_error", "lmgw_code": "key_scope",
               "message": bodies[2]["error"]["message"]})
    );

    let logs = store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    assert_eq!(logs.len(), 3, "one row per refusal");
    for log in &logs {
        assert_eq!(log.status, 403);
        assert_eq!(log.requested_alias, "other-model");
        assert_eq!(log.error_kind.as_deref(), Some("key_scope"));
        assert_eq!(log.client_key.as_deref(), Some("scoped"));
    }
    assert_eq!(
        state.telemetry.stats().active_requests,
        0,
        "the refusal rows leave the in-flight gauge balanced"
    );
}

/// A count costs nothing, so no budget refuses one (§12 entry 15): not the
/// key's own, not the gateway's. Both spent, every counter still counts —
/// while a chat send with the same key is refused, so the budgets really are
/// spent.
#[tokio::test]
async fn a_spent_budget_never_refuses_a_count() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tokens": [1, 2, 3]})))
        .mount(&mock)
        .await;
    let (state, gw) = gateway(
        &mock.uri(),
        Protocol::Openai,
        UpstreamKind::LlamaServer,
        "tgt-model",
    )
    .await;
    let mut settings = state.snapshot().settings.clone();
    settings.auth_enabled = true;
    settings.global_budget_micro = 1_000_000;
    store::save_settings(&state.db, &settings).await.unwrap();
    // `broke` is over its own monthly budget; `free` has none of its own, and
    // meets the gateway's.
    for (name, key, budget) in [("broke", "lmgw-broke", 1_000_000), ("free", "lmgw-free", 0)] {
        sqlx::query(
            "INSERT INTO api_keys (name, key_hash, enabled, budget_micro, budget_period)
             VALUES (?1, ?2, 1, ?3, 'month')",
        )
        .bind(name)
        .bind(hash_api_key(key))
        .bind(budget)
        .execute(&state.db)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO usage_hourly (bucket_utc, key_id, alias, upstream_id, class, outcome,
             requests, cost_micro)
         VALUES (strftime('%Y-%m-%dT%H','now'),
                 (SELECT id FROM api_keys WHERE name='broke'), 'my-model', 1, 'chat', 'ok', 1,
                 2000000)",
    )
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    for key in ["lmgw-broke", "lmgw-free"] {
        let send = |route: &'static str, body: Value| {
            let gw = gw.clone();
            async move {
                reqwest::Client::new()
                    .post(format!("{gw}{route}"))
                    .bearer_auth(key)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
            }
        };
        let resp = send(
            "/v1/chat/completions",
            json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(resp.status(), 403, "{key}: the budget is spent");
        let err: Value = resp.json().await.unwrap();
        assert_eq!(err["error"]["code"], "key_budget", "{key}: {err}");

        for (route, body) in [
            (
                "/v1/count_tokens",
                json!({"model": "my-model", "input": "hi"}),
            ),
            (
                "/v1/messages/count_tokens",
                json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}),
            ),
            ("/tokenize", json!({"model": "my-model", "content": "hi"})),
        ] {
            let resp = send(route, body).await;
            assert_eq!(resp.status(), 200, "{key} {route}");
        }
    }
    let logs = store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    assert!(
        logs.iter()
            .all(|l| l.requested_alias == "my-model" && l.status == 403),
        "only the two chat refusals are rows: {logs:?}"
    );
    assert_eq!(logs.len(), 2);
    assert_eq!(state.telemetry.stats().active_requests, 0);
}

// ---------------------------------------------------------------------------
// POST /tokenize (§5.3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tokenize_forwards_the_llama_shape_verbatim() {
    let mock = MockServer::start().await;
    // llama.cpp's own spacing and a byte-array piece: relayed byte for byte.
    let answer =
        br#"{"tokens": [{"id": 9906, "piece": "Hello"}, {"id": 158, "piece": [240, 159]}]}"#;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .and(header("authorization", "Bearer sk-up"))
        .and(body_json(json!({
            "model": "tgt-model",
            "content": ["Hello", 158],
            "add_special": true,
            "parse_special": false,
            "with_pieces": true,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(answer.to_vec(), "application/json"))
        .expect(1)
        .mount(&mock)
        .await;
    let (_s, gw) = gateway(
        &mock.uri(),
        Protocol::Openai,
        UpstreamKind::LlamaServer,
        "tgt-model",
    )
    .await;

    let resp = post(
        &gw,
        "/tokenize",
        json!({
            "model": "my-model",
            "content": ["Hello", 158],
            "add_special": true,
            "parse_special": false,
            "with_pieces": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(head(&resp, "content-type"), Some("application/json"));
    assert_eq!(head(&resp, APPROX), None, "token ids are never approximate");
    assert_eq!(resp.bytes().await.unwrap().as_ref(), answer.as_slice());
}

#[tokio::test]
async fn tokenize_without_model_is_a_llama_400() {
    let (_s, gw) = gateway(
        "http://127.0.0.1:1",
        Protocol::Openai,
        UpstreamKind::LlamaServer,
        "tgt-model",
    )
    .await;
    let resp = post(&gw, "/tokenize", json!({"content": "hi"})).await;
    assert_eq!(resp.status(), 400);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["code"], 400);
    assert_eq!(err["error"]["type"], "invalid_request_error");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("'model' is required"),
        "{err}"
    );

    // Not JSON, and an alias nobody has: the same shape.
    let resp = reqwest::Client::new()
        .post(format!("{gw}/tokenize"))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(resp.json::<Value>().await.unwrap()["error"]["code"], 400);
    let resp = post(&gw, "/tokenize", json!({"model": "ghost", "content": "hi"})).await;
    assert_eq!(resp.status(), 404);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["type"], "not_found_error");
    assert_eq!(err["error"]["lmgw_code"], "unknown_alias");
}

#[tokio::test]
async fn tokenize_on_a_cloud_model_is_501_not_supported() {
    for protocol in [Protocol::Openai, Protocol::Anthropic, Protocol::Gemini] {
        let mock = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock)
            .await;
        let (_s, gw) = gateway(&mock.uri(), protocol, UpstreamKind::Generic, "tgt-model").await;
        let resp = post(
            &gw,
            "/tokenize",
            json!({"model": "my-model", "content": "hi"}),
        )
        .await;
        assert_eq!(resp.status(), 501, "{protocol:?}");
        let err: Value = resp.json().await.unwrap();
        assert_eq!(err["error"]["code"], 501);
        assert_eq!(err["error"]["type"], "not_supported_error");
        let message = err["error"]["message"].as_str().unwrap();
        assert!(message.contains("test-up"), "names the upstream: {message}");
        assert!(
            message.contains("POST /v1/count_tokens"),
            "points at the universal counter: {message}"
        );
    }
}

/// The backend's own refusals come back as it gave them — here llama-server's
/// answer while its model is still loading. (A missing `content` is no
/// refusal there: llama-server tokenizes it as empty, `{"tokens": []}`.)
/// Except an upstream refusing lmgw's own key, which is the gateway's
/// configuration and a 502, as on every other route (review R1 #4).
#[tokio::test]
async fn tokenize_relays_upstream_errors() {
    let mock = MockServer::start().await;
    let loading = json!({"error": {"code": 503, "type": "unavailable_error",
                                   "message": "Loading model"}});
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .and(body_partial_json(json!({"content": "while loading"})))
        .respond_with(ResponseTemplate::new(503).set_body_json(&loading))
        .expect(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .and(body_partial_json(json!({"content": "with a revoked key"})))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": {
            "code": 401, "type": "authentication_error", "message": "Invalid API Key",
        }})))
        .expect(1)
        .mount(&mock)
        .await;
    let (_s, gw) = gateway(
        &mock.uri(),
        Protocol::Openai,
        UpstreamKind::LlamaServer,
        "tgt-model",
    )
    .await;

    let resp = post(
        &gw,
        "/tokenize",
        json!({"model": "my-model", "content": "while loading"}),
    )
    .await;
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.json::<Value>().await.unwrap(), loading);

    let resp = post(
        &gw,
        "/tokenize",
        json!({"model": "my-model", "content": "with a revoked key"}),
    )
    .await;
    assert_eq!(resp.status(), 502);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["code"], 502);
    assert_eq!(err["error"]["type"], "server_error");
    assert_eq!(err["error"]["lmgw_code"], "upstream");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Invalid API Key"),
        "{err}"
    );
}

/// llama-server reads the body as JSON whatever its `Content-Type` says, and
/// so does lmgw (review R1 #6): none at all, or `text/plain`.
#[tokio::test]
async fn tokenize_reads_json_whatever_the_content_type() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .and(body_json(json!({"model": "tgt-model", "content": "hi"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tokens": [7]})))
        .expect(2)
        .mount(&mock)
        .await;
    let (_s, gw) = gateway(
        &mock.uri(),
        Protocol::Openai,
        UpstreamKind::LlamaServer,
        "tgt-model",
    )
    .await;
    let raw = r#"{"model": "my-model", "content": "hi"}"#;
    for content_type in [None, Some("text/plain")] {
        let mut request = reqwest::Client::new()
            .post(format!("{gw}/tokenize"))
            .body(raw);
        if let Some(ct) = content_type {
            request = request.header("content-type", ct);
        }
        let resp = request.send().await.unwrap();
        assert_eq!(resp.status(), 200, "{content_type:?}");
        assert_eq!(resp.json::<Value>().await.unwrap(), json!({"tokens": [7]}));
    }
}

/// `max_body_mb` bounds `/tokenize` like every JSON `/v1` route. A declared
/// oversize length is refused by the shared layer, before the route runs, in
/// the `/v1` OpenAI dialect — the documented exception (review R1 #5); one
/// that only trips the limit while streaming is refused by the route, in
/// llama.cpp's shape.
#[tokio::test]
async fn tokenize_over_the_body_limit_is_413() {
    let (state, gw) = gateway(
        "http://127.0.0.1:1",
        Protocol::Openai,
        UpstreamKind::LlamaServer,
        "tgt-model",
    )
    .await;
    let mut settings = state.snapshot().settings.clone();
    settings.max_body_mb = 1;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let big = json!({"model": "my-model", "content": "x".repeat(2 * 1024 * 1024)});
    let resp = post(&gw, "/tokenize", big).await;
    assert_eq!(resp.status(), 413);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["code"], "body_limit", "OpenAI-shaped: {err}");

    let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
        (0..4).map(|_| Ok(vec![b' '; 512 * 1024])).collect();
    let resp = reqwest::Client::new()
        .post(format!("{gw}/tokenize"))
        .body(reqwest::Body::wrap_stream(futures::stream::iter(chunks)))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["code"], 413, "llama.cpp-shaped: {err}");
    assert_eq!(err["error"]["type"], "invalid_request_error");
    assert_eq!(err["error"]["lmgw_code"], "body_limit");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_body_mb"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// /v1/count_tokens: the flags it now states (§5.1, §10 choice 3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn count_tokens_flags_a_guessed_tokenizer() {
    let body = json!({"model": "my-model", "input": "hello world"});

    let (_s, gw) = gateway(
        "http://127.0.0.1:1",
        Protocol::Openai,
        UpstreamKind::Generic,
        "gpt-9-something",
    )
    .await;
    let resp = post(&gw, "/v1/count_tokens", body.clone()).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(head(&resp, APPROX), Some("tokenizer_guess"));
    assert!(
        resp.json::<Value>().await.unwrap()["tokens"]
            .as_u64()
            .unwrap()
            >= 2
    );

    // A model tiktoken knows is its own exact count.
    let (_s, gw) = gateway(
        "http://127.0.0.1:1",
        Protocol::Openai,
        UpstreamKind::Generic,
        "gpt-4o",
    )
    .await;
    let resp = post(&gw, "/v1/count_tokens", body).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(head(&resp, APPROX), None);
}

#[tokio::test]
async fn count_tokens_flags_message_framing() {
    let body = json!({"model": "my-model", "input": "count me"});

    let anthropic = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 11})))
        .mount(&anthropic)
        .await;
    let (_s, gw) = gateway(
        &anthropic.uri(),
        Protocol::Anthropic,
        UpstreamKind::Generic,
        "tgt-model",
    )
    .await;
    let resp = post(&gw, "/v1/count_tokens", body.clone()).await;
    assert_eq!(head(&resp, APPROX), Some("message_framing"));
    assert_eq!(resp.json::<Value>().await.unwrap()["tokens"], 11);

    let gemini = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/tgt-model:countTokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"totalTokens": 5})))
        .mount(&gemini)
        .await;
    let (_s, gw) = gateway(
        &gemini.uri(),
        Protocol::Gemini,
        UpstreamKind::Generic,
        "tgt-model",
    )
    .await;
    let resp = post(&gw, "/v1/count_tokens", body.clone()).await;
    assert_eq!(head(&resp, APPROX), Some("message_framing"));

    // llama.cpp counts the string itself: exact.
    let llama = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tokens": [1, 2]})))
        .mount(&llama)
        .await;
    let (_s, gw) = gateway(
        &llama.uri(),
        Protocol::Openai,
        UpstreamKind::LlamaServer,
        "tgt-model",
    )
    .await;
    let resp = post(&gw, "/v1/count_tokens", body).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(head(&resp, APPROX), None);
}
