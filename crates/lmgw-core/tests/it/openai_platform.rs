//! The OpenAI platform's own egress through the routes (openai egress
//! design §3.3, WP3): what a platform row sends and what its client is told,
//! against a wiremock standing in for api.openai.com. The bytes of each case
//! are the egress corpus's (`egress_golden.rs`); these check the route's
//! half — the response headers and the error codes a client reads.

use lmgw_core::config::{Protocol, UpstreamKind};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::Gw;
use crate::e2e_proxy::setup_kind;

/// A platform row in front of `mock`, aliased `my-model`.
async fn platform(mock: &MockServer) -> Gw {
    let (_state, gw) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    gw
}

/// A chat mock that answers anything, on either wire (the platform's chat
/// sends take the Responses one, openai egress design §7.2), and records
/// what it was sent.
async fn chat_mock() -> MockServer {
    let mock = MockServer::start().await;
    crate::support::responses_wire::mount_reply(&mock, "ok").await;
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

/// The last body posted to `mock`.
async fn sent_body(mock: &MockServer) -> Value {
    let posted = mock.received_requests().await.unwrap();
    let last = posted.last().expect("nothing posted to the mock");
    serde_json::from_slice(&last.body).unwrap()
}

async fn chat(gw: &Gw, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// P2: the llama samplers a platform row cannot take are not sent and are
/// named in `x-lmgw-params-ignored`; a client's own unknown key goes as it
/// came, and a request without them carries no header.
#[tokio::test]
async fn the_llama_samplers_are_named_not_sent() {
    let mock = chat_mock().await;
    let gw = platform(&mock).await;

    let resp = chat(
        &gw,
        json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}],
               "top_k": 40, "repeat_penalty": 1.1, "temperature": 0.5,
               "my_own_key": 1}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["x-lmgw-params-ignored"],
        "top_k,repeat_penalty"
    );
    let sent = sent_body(&mock).await;
    for key in ["top_k", "min_p", "repeat_penalty"] {
        assert!(sent.get(key).is_none(), "{key} went out: {sent}");
    }
    assert_eq!(sent["temperature"], 0.5, "{sent}");
    assert_eq!(sent["my_own_key"], 1, "{sent}");

    let resp = chat(
        &gw,
        json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}],
               "temperature": 0.5}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-lmgw-params-ignored").is_none());
}

/// P2 on the compatible egress: the samplers go, nothing is named.
#[tokio::test]
async fn a_compatible_row_still_sends_the_samplers() {
    let mock = chat_mock().await;
    let (_state, gw) = setup_kind(
        &mock.uri(),
        Protocol::OpenaiCompatible,
        UpstreamKind::Generic,
    )
    .await;
    let resp = chat(
        &gw,
        json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}],
               "top_k": 40, "min_p": 0.05}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-lmgw-params-ignored").is_none());
    let sent = sent_body(&mock).await;
    assert_eq!(sent["top_k"], 40, "{sent}");
    assert_eq!(sent["min_p"], 0.05, "{sent}");
}

/// P3: OpenRouter's `reasoning` object is not sent: its effort becomes
/// `reasoning.effort` on the Responses wire (`reasoning_effort` on the Chat
/// wire), the summary beside it; its budget, which has no OpenAI spelling,
/// is named as ignored, and so is a bare "on".
#[tokio::test]
async fn the_openrouter_object_is_translated_not_forwarded() {
    let mock = chat_mock().await;
    let gw = platform(&mock).await;
    let hi = json!([{"role": "user", "content": "hi"}]);

    for (object, effort, ignored) in [
        (
            json!({"effort": "low", "max_tokens": 100, "exclude": true}),
            Some("low"),
            Some("budget"),
        ),
        (json!({"enabled": false}), Some("none"), None),
        (json!({"enabled": true}), None, Some("enabled")),
        (json!({"exclude": true}), None, None),
    ] {
        let resp = chat(
            &gw,
            json!({"model": "my-model", "messages": hi, "reasoning": object}),
        )
        .await;
        assert_eq!(resp.status(), 200, "{object}");
        assert_eq!(
            resp.headers()
                .get("x-lmgw-reasoning-ignored")
                .map(|v| v.to_str().unwrap()),
            ignored,
            "{object}"
        );
        let sent = sent_body(&mock).await;
        let mut want = json!({"summary": "auto"});
        if let Some(e) = effort {
            want["effort"] = json!(e);
        }
        assert_eq!(sent["reasoning"], want, "{object}: {sent}");
        assert!(sent.get("reasoning_effort").is_none(), "{object}: {sent}");
    }
}

/// A mock that refuses every chat with `status` and `body`, on either wire.
async fn refusing(status: u16, body: Value) -> MockServer {
    let mock = MockServer::start().await;
    for p in ["/chat/completions", "/responses"] {
        Mock::given(method("POST"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(status).set_body_json(body.clone()))
            .mount(&mock)
            .await;
    }
    mock
}

/// P4: OpenAI's codes reach the client and the request log as their own
/// kinds — a quota apart from a rate limit, the flex 429 named, and a too-long
/// prompt as `context_length_exceeded` naming the model.
#[tokio::test]
async fn openais_error_codes_reach_the_client_and_the_log() {
    let cases = [
        (
            429,
            json!({"error": {"message": "You exceeded your current quota.",
                             "type": "insufficient_quota", "param": null,
                             "code": "insufficient_quota"}}),
            "insufficient_quota",
            "retrying will not help",
        ),
        (
            429,
            json!({"error": {"message": "Rate limit reached for requests.",
                             "type": "requests", "param": null,
                             "code": "rate_limit_exceeded"}}),
            "rate_limit_exceeded",
            "Rate limit reached for requests.",
        ),
        (
            429,
            json!({"error": {"message": "Resource Unavailable",
                             "type": "invalid_request_error", "param": null,
                             "code": "resource_unavailable"}}),
            "resource_unavailable",
            "send service_tier auto",
        ),
        (
            400,
            json!({"error": {"message": "This model's maximum context length is 128000 tokens. \
                               However, your messages resulted in 130021 tokens.",
                             "type": "invalid_request_error", "param": "messages",
                             "code": "context_length_exceeded"}}),
            "context_length_exceeded",
            "'tgt-model': prompt 130021 tokens exceeds the per-request context of 128000 tokens",
        ),
    ];
    for (status, body, code, says) in cases {
        let mock = refusing(status, body).await;
        let (state, gw) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
        let resp = chat(
            &gw,
            json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(resp.status(), status, "{code}");
        let answer: Value = resp.json().await.unwrap();
        assert_eq!(answer["error"]["code"], code, "{answer}");
        let message = answer["error"]["message"].as_str().unwrap();
        assert!(message.contains(says), "{code}: {message}");

        let logs = lmgw_core::store::query_logs(&state.db, &Default::default())
            .await
            .unwrap();
        assert_eq!(logs[0].error_kind.as_deref(), Some(code), "{code}");
    }
}

/// An OpenAI 4xx lmgw has no word for reaches the client as OpenAI sent it:
/// its message, type, code and param, with its status. The bodies follow the
/// a probe's messages (an unknown parameter, a deprecated model).
#[tokio::test]
async fn an_unclassified_openai_4xx_is_relayed_as_sent() {
    let cases = [
        (
            400,
            json!({"error": {"message": "Unknown parameter: 'foo_bar'.",
                             "type": "invalid_request_error", "param": "foo_bar",
                             "code": "unknown_parameter"}}),
        ),
        (
            404,
            json!({"error": {"message": "The model `gpt-4` has been deprecated, learn more \
                             here: https://platform.openai.com/docs/deprecations",
                             "type": "invalid_request_error", "param": null,
                             "code": "model_not_found"}}),
        ),
    ];
    for (status, body) in cases {
        let mock = refusing(status, body.clone()).await;
        let (state, gw) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
        let http = reqwest::Client::new();
        let hi = json!([{"role": "user", "content": "hi"}]);

        let resp = chat(&gw, json!({"model": "my-model", "messages": hi})).await;
        assert_eq!(resp.status(), status);
        let answer: Value = resp.json().await.unwrap();
        assert_eq!(answer, body, "chat");

        let resp = http
            .post(format!("{gw}/v1/responses"))
            .json(&json!({"model": "my-model", "input": "hi", "stream": false}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), status);
        let answer: Value = resp.json().await.unwrap();
        assert_eq!(
            answer["error"]["message"], body["error"]["message"],
            "{answer}"
        );
        assert_eq!(answer["error"]["type"], body["error"]["type"], "{answer}");
        assert_eq!(answer["error"]["code"], body["error"]["code"], "{answer}");
        assert_eq!(answer["error"]["param"], body["error"]["param"], "{answer}");

        let resp = http
            .post(format!("{gw}/v1/messages"))
            .json(&json!({"model": "my-model", "max_tokens": 8, "messages": hi}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), status);
        let answer: Value = resp.json().await.unwrap();
        assert_eq!(
            answer["error"]["message"], body["error"]["message"],
            "{answer}"
        );
        let anthropic_type = if status == 404 {
            "not_found_error"
        } else {
            "invalid_request_error"
        };
        assert_eq!(answer["error"]["type"], anthropic_type, "{answer}");

        let logs = lmgw_core::store::query_logs(&state.db, &Default::default())
            .await
            .unwrap();
        assert_eq!(logs[0].error_kind.as_deref(), Some("upstream"));
    }
}

/// The headers a rate refusal carries that tell a client when to come back.
const HINTS: [(&str, &str); 4] = [
    ("retry-after", "2"),
    ("retry-after-ms", "1500"),
    ("x-ratelimit-remaining-tokens", "0"),
    ("x-ratelimit-reset-tokens", "300ms"),
];

/// A mock that refuses every chat and every Responses call with a rate
/// limit, `body`, and [`HINTS`].
async fn limiting(body: Value) -> MockServer {
    let mock = MockServer::start().await;
    let mut template = ResponseTemplate::new(429)
        .set_body_json(body)
        .insert_header("x-request-id", "req_1");
    for (k, v) in HINTS {
        template = template.insert_header(k, v);
    }
    for p in ["/chat/completions", "/responses"] {
        Mock::given(method("POST"))
            .and(path(p))
            .respond_with(template.clone())
            .mount(&mock)
            .await;
    }
    mock
}

fn assert_hints(resp: &reqwest::Response, route: &str) {
    for (k, v) in HINTS {
        assert_eq!(
            resp.headers().get(k).map(|v| v.to_str().unwrap()),
            Some(v),
            "{route}: {k}"
        );
    }
    assert!(
        resp.headers().get("x-request-id").is_none(),
        "{route}: only the rate headers are relayed"
    );
}

/// An upstream rate limit's `Retry-After`, `retry-after-ms` and
/// `x-ratelimit-*` reach the client on every route that answers it with a
/// status: chat unary and streamed, `/v1/messages` and `/v1/responses`; and
/// the OpenAI-shaped wires keep OpenAI's own code and type.
#[tokio::test]
async fn a_rate_limits_hints_reach_the_client() {
    let mock = limiting(
        json!({"error": {"message": "Rate limit reached for tokens.",
                                         "type": "tokens", "param": null,
                                         "code": "rate_limit_exceeded"}}),
    )
    .await;
    let gw = platform(&mock).await;
    let hi = json!([{"role": "user", "content": "hi"}]);
    let http = reqwest::Client::new();

    for stream in [false, true] {
        let resp = chat(
            &gw,
            json!({"model": "my-model", "messages": hi, "stream": stream}),
        )
        .await;
        assert_eq!(resp.status(), 429, "stream {stream}");
        assert_hints(&resp, "chat");
        let answer: Value = resp.json().await.unwrap();
        assert_eq!(answer["error"]["code"], "rate_limit_exceeded", "{answer}");
        assert_eq!(answer["error"]["type"], "tokens", "{answer}");
    }

    for stream in [false, true] {
        let resp = http
            .post(format!("{gw}/v1/messages"))
            .header("anthropic-version", "2023-06-01")
            .json(
                &json!({"model": "my-model", "max_tokens": 16, "messages": hi,
                          "stream": stream}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 429, "messages stream {stream}");
        assert_hints(&resp, "messages");
        let answer: Value = resp.json().await.unwrap();
        assert_eq!(answer["error"]["type"], "rate_limit_error", "{answer}");
    }

    let resp = http
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": "my-model", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    assert_hints(&resp, "responses");
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(answer["error"]["code"], "rate_limit_exceeded", "{answer}");
    assert_eq!(answer["error"]["type"], "tokens", "{answer}");
}

/// An exhausted quota: OpenAI's own code on the OpenAI and Responses wires
/// (a 429, as OpenAI answers it), Anthropic's `402 billing_error` on
/// `/v1/messages`, whose SDKs would retry a 429; the log keeps the family's
/// kind either way.
#[tokio::test]
async fn an_exhausted_quota_answers_in_each_wires_words() {
    let mock = limiting(json!({"error": {"message": "No credits left.",
                                         "type": "insufficient_quota", "param": null,
                                         "code": "credit_balance_exhausted"}}))
    .await;
    let (state, gw) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    let hi = json!([{"role": "user", "content": "hi"}]);
    let http = reqwest::Client::new();

    let resp = chat(&gw, json!({"model": "my-model", "messages": hi})).await;
    assert_eq!(resp.status(), 429);
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(
        answer["error"]["code"], "credit_balance_exhausted",
        "{answer}"
    );
    assert_eq!(answer["error"]["type"], "insufficient_quota", "{answer}");

    let resp = http
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": "my-model", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(
        answer["error"]["code"], "credit_balance_exhausted",
        "{answer}"
    );

    let resp = http
        .post(format!("{gw}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({"model": "my-model", "max_tokens": 16, "messages": hi}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 402);
    assert_hints(&resp, "messages");
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(answer["error"]["type"], "billing_error", "{answer}");

    let logs = lmgw_core::store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    assert!(
        logs.iter()
            .all(|l| l.error_kind.as_deref() == Some("insufficient_quota")),
        "{:?}",
        logs.iter().map(|l| &l.error_kind).collect::<Vec<_>>()
    );
    // The `/v1/messages` row has the status its client got.
    assert_eq!(logs[0].status, 402);
}

/// Answers the embedder's probe and refuses every other input with `status`
/// and `body`.
struct EmbedRefusing(u16, Value);

impl wiremock::Respond for EmbedRefusing {
    fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let probe = body["input"]
            .as_array()
            .is_some_and(|a| a.len() == 1 && a[0] == "quickdoc probe");
        if probe {
            return ResponseTemplate::new(200).set_body_json(json!({
                "object": "list", "model": "tgt-model",
                "data": [{"object": "embedding", "index": 0, "embedding": [0.5, 0.5]}],
                "usage": {"prompt_tokens": 1, "total_tokens": 1},
            }));
        }
        ResponseTemplate::new(self.0).set_body_json(self.1.clone())
    }
}

/// A knowledge ingest splits a chunk the platform refuses as too long —
/// `context_length_exceeded` in either wording, neither of which carries the
/// phrases a plain upstream error is read by — and does not split on a rate
/// limit (openai egress design §8).
#[tokio::test]
async fn the_platforms_context_refusal_is_an_oversize_input_for_the_ingest() {
    use lmgw_core::knowledge::ingest_split::refused_as_oversize;
    use quickdoc_core::embed::Embedder;
    for (status, body, oversize) in [
        (
            400,
            json!({"error": {"message": "Your input exceeds the context window of this model.",
                             "type": "invalid_request_error", "param": "input",
                             "code": "context_length_exceeded"}}),
            true,
        ),
        (
            400,
            json!({"error": {"message": "This model's maximum context length is 8192 tokens. \
                               However, your messages resulted in 9000 tokens.",
                             "type": "invalid_request_error", "param": "input",
                             "code": "context_length_exceeded"}}),
            true,
        ),
        (
            429,
            json!({"error": {"message": "Rate limit reached: maximum number of tokens per min.",
                             "type": "tokens", "param": null,
                             "code": "rate_limit_exceeded"}}),
            false,
        ),
    ] {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(EmbedRefusing(status, body.clone()))
            .mount(&mock)
            .await;
        let (state, _gw) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
        let embedder = lmgw_core::quickdoc::InProcessEmbedder::probe(state, "my-model")
            .await
            .unwrap();
        let e = embedder
            .embed(&["a chunk".to_string()])
            .await
            .expect_err("refused");
        assert_eq!(refused_as_oversize(&e), oversize, "{body}: {e}");
    }
}

/// `x-lmgw-params-ignored` on the streamed chat and on `/v1/messages`, as on
/// the unary chat ([`the_llama_samplers_are_named_not_sent`]).
#[tokio::test]
async fn the_samplers_are_named_on_the_stream_and_on_messages() {
    let mock = MockServer::start().await;
    crate::support::responses_wire::mount_reply(&mock, "ok").await;
    let gw = platform(&mock).await;
    let hi = json!([{"role": "user", "content": "hi"}]);

    let resp = chat(
        &gw,
        json!({"model": "my-model", "messages": hi, "stream": true,
               "top_k": 40, "min_p": 0.05}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lmgw-params-ignored"], "top_k,min_p");
    let _ = resp.text().await;
    let sent = sent_body(&mock).await;
    assert!(sent.get("top_k").is_none(), "{sent}");

    for stream in [false, true] {
        let resp = reqwest::Client::new()
            .post(format!("{gw}/v1/messages"))
            .header("anthropic-version", "2023-06-01")
            .json(
                &json!({"model": "my-model", "max_tokens": 16, "messages": hi,
                          "top_k": 40, "stream": stream}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(resp.headers()["x-lmgw-params-ignored"], "top_k", "{stream}");
        let _ = resp.text().await;
        let sent = sent_body(&mock).await;
        assert!(sent.get("top_k").is_none(), "{stream}: {sent}");
    }
}

/// The header names what the Anthropic and Gemini egresses leave out too —
/// on the chat route, and on `/v1/responses`' synthesized loop, where an
/// alias default the provider cannot take is named as a request's own is.
#[tokio::test]
async fn anthropic_and_gemini_drops_are_named() {
    let anthropic = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt-model",
            "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 2, "output_tokens": 1}
        })))
        .mount(&anthropic)
        .await;
    let (state, gw) =
        setup_kind(&anthropic.uri(), Protocol::Anthropic, UpstreamKind::Generic).await;
    let hi = json!([{"role": "user", "content": "hi"}]);

    let resp = chat(
        &gw,
        json!({"model": "my-model", "messages": hi, "max_tokens": 16,
               "top_k": 40, "seed": 7, "presence_penalty": 0.1}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["x-lmgw-params-ignored"],
        "presence_penalty,seed"
    );
    let sent = sent_body(&anthropic).await;
    assert_eq!(sent["top_k"], 40, "{sent}");
    assert!(sent.get("seed").is_none(), "{sent}");

    // An alias whose own defaults carry a parameter Anthropic cannot take.
    let up = state.snapshot().upstreams.values().next().unwrap().id;
    lmgw_core::store::insert_alias(
        &state.db,
        &lmgw_core::store::NewAlias {
            alias: "with-min-p".into(),
            upstream_id: up,
            upstream_model_id: "tgt-model".into(),
            param_overrides: lmgw_core::ir::Params {
                min_p: Some(0.05),
                ..Default::default()
            },
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": "with-min-p", "input": "hi", "top_k": 40}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-lmgw-params-ignored"], "min_p");
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(answer["status"], "completed", "{answer}");

    let gemini = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "ok"}]},
                            "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 2, "candidatesTokenCount": 1}
        })))
        .mount(&gemini)
        .await;
    let (_state, gw) = setup_kind(&gemini.uri(), Protocol::Gemini, UpstreamKind::Generic).await;
    let resp = chat(
        &gw,
        json!({"model": "my-model", "messages": hi, "seed": 7,
               "frequency_penalty": 0.2, "repeat_penalty": 1.1}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["x-lmgw-params-ignored"],
        "repeat_penalty,frequency_penalty"
    );
}
