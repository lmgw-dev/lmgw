//! `POST /v1/systemone` (decision-models design §2): the relay is byte for
//! byte both ways with only `model` rewritten, on a local decision row and on
//! an OpenAI-protocol upstream; a 404 from an image without the route is a
//! named 501; a cloud route is refused only when its catalog states it is no
//! decision model, and anthropic and gemini upstreams always; lmgw's own
//! credential refused upstream is a 502; and what lmgw refuses itself is
//! llama.cpp-shaped. The kind gate both ways is `aux_router.rs`'s, the price
//! `usage_units.rs`'s, the schema drift `openapi_v1_wire.rs`'s.

use axum::http::header;
use lmgw_core::config::{AuxBackend, AuxKind, HoldFallbackMode, Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewAuxModel, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, body_string_contains, header as has_header};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{serve, Gw};
use crate::stream_usage::row_after;
use crate::support::decision_world::{
    decision_world, healthy, ticket, DECISION, LLAMA_ANSWER, OPENROUTER_ANSWER, TYPESAFE_ANSWERS,
};

/// A gateway with an upstream of `protocol` at `base` and `aliases` on it
/// (alias, upstream model).
async fn cloud(base: &str, protocol: Protocol, aliases: &[(&str, &str)]) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    add_upstream(&state, base, protocol, aliases).await;
    let gw = serve(state.clone()).await;
    (state, gw)
}

/// [`cloud`]'s upstream and aliases, on `state`.
async fn add_upstream(
    state: &SharedState,
    base: &str,
    protocol: Protocol,
    aliases: &[(&str, &str)],
) {
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "up".into(),
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
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    for (alias, model) in aliases {
        store::insert_alias(
            &state.db,
            &NewAlias {
                alias: alias.to_string(),
                upstream_id: up,
                upstream_model_id: model.to_string(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: None,
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
}

/// One question for `model`, as JSON.
fn ask(model: &str) -> Value {
    json!({"model": model, "state": "I was charged twice for my subscription.",
           "questions": {"refund": {"type": "noul",
                                    "instructions": "Is the customer asking for money back?"}}})
}

/// Post `body` (bytes, as sent) and return the status, the content type and
/// the body bytes.
async fn post_raw(gw: &Gw, body: Vec<u8>, content_type: &str) -> (u16, String, Vec<u8>) {
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/systemone"))
        .header(header::CONTENT_TYPE, content_type)
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    (status, ct, resp.bytes().await.unwrap().to_vec())
}

async fn post(gw: &Gw, body: Value) -> (u16, Value) {
    let (status, _, bytes) =
        post_raw(gw, serde_json::to_vec(&body).unwrap(), "application/json").await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The bodies `server` received on a path ending in `/systemone`.
async fn forwarded(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/systemone"))
        .map(|r| String::from_utf8(r.body.clone()).unwrap())
        .collect()
}

/// The local relay (§2.1): the ticket goes to the container with only
/// `model` rewritten to the row id — every other byte, the options' order
/// included, as the client sent it, posted as `text/plain` because the body
/// is JSON whatever its `Content-Type` says — and llama.cpp's answer comes
/// back byte for byte with its content type, `model` the server's own. The
/// row says `systemone`, `aux`, the answer's tokens and a free local cost.
/// The engine's own refusal is relayed the same way: the 500 for a prompt
/// over the ubatch (Appendix A.6, §7 Q6).
#[tokio::test]
async fn a_local_decision_row_relays_the_wire_byte_for_byte() {
    const UBATCH: &str = r#"{"error":{"code":500,"message":"input (774 tokens) is too large to process. increase the physical batch size (current batch size: 512)","type":"server_error"}}"#;
    let container = MockServer::start().await;
    healthy(&container).await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(body_string_contains("OVER-THE-UBATCH"))
        .respond_with(ResponseTemplate::new(500).set_body_raw(UBATCH, "application/json"))
        .with_priority(1)
        .mount(&container)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(LLAMA_ANSWER, "application/json; charset=utf-8"),
        )
        .mount(&container)
        .await;
    let w = decision_world(&container.uri()).await;
    let laya = w.name(DECISION);

    let (status, ct, bytes) = post_raw(&w.gw, ticket(&laya).into_bytes(), "text/plain").await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    assert_eq!(ct, "application/json; charset=utf-8");
    assert_eq!(String::from_utf8(bytes).unwrap(), LLAMA_ANSWER);
    assert_eq!(forwarded(&container).await, [ticket(DECISION)]);

    let row = row_after(&w.state, 0).await;
    assert_eq!(
        (
            row.status,
            row.ingress_proto.as_str(),
            row.class.as_deref(),
            row.requested_alias.as_str(),
            row.upstream_model.as_deref(),
        ),
        (200, "systemone", Some("aux"), laya.as_str(), Some(DECISION)),
        "{row:?}"
    );
    assert_eq!(
        (row.prompt_tokens, row.completion_tokens, row.cost_micro),
        (Some(299), Some(0), Some(0)),
        "input_tokens as prompt tokens, priced as free local: {row:?}"
    );

    let over = ticket(&laya).replace("Hi, my invoice", "OVER-THE-UBATCH Hi, my invoice");
    let (status, _, bytes) = post_raw(&w.gw, over.into_bytes(), "application/json").await;
    assert_eq!(
        (status, String::from_utf8(bytes).unwrap().as_str()),
        (500, UBATCH)
    );
    let row = row_after(&w.state, 1).await;
    assert_eq!(
        (row.status, row.error_kind.as_deref()),
        (500, Some("upstream")),
        "{row:?}"
    );
    assert!(
        row.error_msg
            .as_deref()
            .unwrap_or_default()
            .contains("physical batch size"),
        "{row:?}"
    );
}

/// An image from before the route answers llama-server's bare 404 on it.
/// lmgw answers 501 instead, naming the row, its image and the fix (§3.6).
#[tokio::test]
async fn a_404_from_an_image_without_the_route_is_a_named_501() {
    let container = MockServer::start().await;
    healthy(&container).await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(404).set_body_json(
            json!({"error": {"message": "File Not Found", "type": "not_found_error", "code": 404}}),
        ))
        .mount(&container)
        .await;
    let w = decision_world(&container.uri()).await;
    let image = w.state.snapshot().settings.aux_router.image.clone();

    let (status, body) = post(&w.gw, ask(&w.name(DECISION))).await;
    assert_eq!(status, 501, "{body}");
    assert_eq!(body["error"]["type"], "not_supported_error", "{body}");
    assert_eq!(body["error"]["lmgw_code"], "not_supported", "{body}");
    let msg = body["error"]["message"].as_str().unwrap();
    for part in [DECISION, image.as_str(), "/v1/systemone", "Backends"] {
        assert!(msg.contains(part), "{part}: {msg}");
    }
    let row = row_after(&w.state, 0).await;
    assert_eq!(
        (row.status, row.error_kind.as_deref()),
        (501, Some("not_supported")),
        "{row:?}"
    );
}

/// The 404 mapping is a local decision row's only (§2.7): a cloud
/// upstream's 404 — an unknown model on OpenRouter — is relayed as it came.
#[tokio::test]
async fn a_cloud_upstreams_404_is_relayed_as_sent() {
    const UNKNOWN: &str =
        r#"{"error":{"message":"No endpoints found for typesafe/jev-9.","code":404}}"#;
    let up = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .respond_with(ResponseTemplate::new(404).set_body_raw(UNKNOWN, "application/json"))
        .mount(&up)
        .await;
    let (state, gw) = cloud(&up.uri(), Protocol::Openai, &[("jev", "typesafe/jev-9")]).await;
    let (status, ct, bytes) = post_raw(
        &gw,
        serde_json::to_vec(&ask("jev")).unwrap(),
        "application/json",
    )
    .await;
    assert_eq!(
        (
            status,
            ct.as_str(),
            String::from_utf8(bytes).unwrap().as_str()
        ),
        (404, "application/json", UNKNOWN)
    );
    let row = row_after(&state, 0).await;
    assert_eq!(
        (row.status, row.error_kind.as_deref()),
        (404, Some("upstream")),
        "{row:?}"
    );
}

/// A rate limit on the upstream reaches the client with the upstream's own
/// `Retry-After`, beside its status, content type and body.
#[tokio::test]
async fn a_cloud_429_is_relayed_with_its_retry_after() {
    const LIMITED: &str = r#"{"error":{"message":"rate limited","code":429}}"#;
    let up = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "12")
                .insert_header("x-upstream-private", "1")
                .set_body_raw(LIMITED, "application/json"),
        )
        .mount(&up)
        .await;
    let (_state, gw) = cloud(&up.uri(), Protocol::Openai, &[("jev", "typesafe/jev-9")]).await;
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/systemone"))
        .json(&ask("jev"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 429);
    assert_eq!(
        resp.headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("12")
    );
    assert!(resp.headers().get("x-upstream-private").is_none());
    assert_eq!(resp.text().await.unwrap(), LIMITED);
}

/// Under the GPU hold a decision row with no fallback is refused, as any
/// held GPU row is: 503 `gpu_hold`, in llama.cpp's shape with lmgw's code,
/// and nothing starts (§2.3, §2.7).
#[tokio::test]
async fn under_the_hold_a_gpu_decision_row_without_a_fallback_is_a_503() {
    let w = decision_world("http://127.0.0.1:1").await;
    lmgw_core::ops::hold_set(&w.state, true).await.unwrap();
    let (status, body) = post(&w.gw, ask(&w.name(DECISION))).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(
        (
            &body["error"]["code"],
            &body["error"]["type"],
            &body["error"]["lmgw_code"]
        ),
        (&json!(503), &json!("unavailable_error"), &json!("gpu_hold")),
        "{body}"
    );
    assert_eq!(w.podman.runs(), 0);
}

/// A decision row with `backend: cpu` takes no VRAM, so the hold does not
/// pause it (§2.3): it starts and answers itself, no fallback in between.
#[tokio::test]
async fn under_the_hold_a_cpu_decision_row_answers_itself() {
    let container = MockServer::start().await;
    healthy(&container).await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(LLAMA_ANSWER, "application/json"))
        .mount(&container)
        .await;
    let w = decision_world(&container.uri()).await;
    store::insert_aux_model(
        &w.state.db,
        &NewAuxModel {
            model_id: "laya-cpu".into(),
            gguf_path: "laya.gguf".into(),
            kind: AuxKind::Decision,
            pooling: None,
            ctx_size: None,
            ubatch_size: None,
            batch_size: None,
            mmproj_path: None,
            backend: Some(AuxBackend::Cpu),
            threads: Some(4),
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        },
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    lmgw_core::ops::hold_set(&w.state, true).await.unwrap();

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/systemone", w.gw))
        .json(&ask(&w.name("laya-cpu")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-lmgw-fallback").is_none());
    assert_eq!(resp.text().await.unwrap(), LLAMA_ANSWER);
    assert_eq!(w.podman.runs(), 1, "its own container, on the CPU");
}

/// A cloud route (§2.2, §4.1): an openai upstream's model goes through
/// unless its catalog states output modalities without `decisions`. A model
/// the catalog lists as `["decisions"]`, one it states nothing for and one
/// it does not list are forwarded; one it lists as `["text"]` is a 501 and
/// never reaches the upstream.
#[tokio::test]
async fn a_cloud_route_is_refused_only_when_its_catalog_rules_the_model_out() {
    let up = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": [
                {"id": "chatty", "architecture": {"input_modalities": ["text"],
                                                  "output_modalities": ["text"]}},
                {"id": "jev", "architecture": {"input_modalities": ["text"],
                                               "output_modalities": ["decisions"]}},
                {"id": "plain"},
            ]})),
        )
        .mount(&up)
        .await;
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(TYPESAFE_ANSWERS[0], "application/json"),
        )
        .mount(&up)
        .await;
    let models = [
        ("chatty", "chatty"),
        ("jev", "jev"),
        ("plain", "plain"),
        ("unlisted", "unlisted"),
    ];
    let (_state, gw) = cloud(&up.uri(), Protocol::Openai, &models).await;

    let (status, body) = post(&gw, ask("chatty")).await;
    assert_eq!(status, 501, "{body}");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("chatty") && msg.contains("text"), "{msg}");
    for alias in ["jev", "plain", "unlisted"] {
        let (status, body) = post(&gw, ask(alias)).await;
        assert_eq!(status, 200, "{alias}: {body}");
    }
    let sent = forwarded(&up).await;
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(!sent.iter().any(|b| b.contains("chatty")), "{sent:?}");
}

/// A held decision row's fallback has to answer System One (§2.4). The gate
/// runs the route check on the route the hold swapped to, so a fallback whose
/// catalog says it outputs text is refused by name before anything is sent —
/// even one set before it changed, which is why these rows are written past
/// the op's own check — and one that outputs decisions answers, with
/// `x-lmgw-fallback`. No container starts for either.
#[tokio::test]
async fn a_held_decision_row_falls_back_only_to_a_model_that_answers_system_one() {
    let up = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": [
                {"id": "chatty", "architecture": {"output_modalities": ["text"]}},
                {"id": "jev", "architecture": {"output_modalities": ["decisions"]}},
            ]})),
        )
        .mount(&up)
        .await;
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(TYPESAFE_ANSWERS[0], "application/json"),
        )
        .mount(&up)
        .await;
    let w = decision_world("http://127.0.0.1:1").await;
    add_upstream(
        &w.state,
        &up.uri(),
        Protocol::Openai,
        &[("chatty", "chatty"), ("jev", "jev")],
    )
    .await;
    for (model_id, fallback) in [("laya-jev", "jev"), ("laya-chatty", "chatty")] {
        store::insert_aux_model(
            &w.state.db,
            &NewAuxModel {
                model_id: model_id.into(),
                gguf_path: "laya.gguf".into(),
                kind: AuxKind::Decision,
                pooling: None,
                ctx_size: None,
                ubatch_size: None,
                batch_size: None,
                mmproj_path: None,
                backend: None,
                threads: None,
                args: vec![],
                idle_seconds: 0,
                enabled: true,
                image: None,
                extra_run_args: None,
                warm_start: false,
                hold_fallback_mode: HoldFallbackMode::Alias,
                hold_fallback: Some(fallback.into()),
            },
        )
        .await
        .unwrap();
    }
    w.state.reload_snapshot().await.unwrap();
    lmgw_core::ops::hold_set(&w.state, true).await.unwrap();

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/systemone", w.gw))
        .json(&ask(&w.name("laya-jev")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let h = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    };
    assert_eq!(
        (h("x-lmgw-fallback"), h("x-lmgw-fallback-reason")),
        (Some("jev".into()), Some("hold".into()))
    );
    assert_eq!(resp.text().await.unwrap(), TYPESAFE_ANSWERS[0]);

    let (status, body) = post(&w.gw, ask(&w.name("laya-chatty"))).await;
    assert_eq!(status, 501, "{body}");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("'chatty'") && msg.contains("text"), "{msg}");

    let sent = forwarded(&up).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains(r#""model":"jev""#), "{sent:?}");
    assert_eq!(w.podman.runs(), 0);
}

/// Neither an Anthropic nor a Gemini upstream serves the wire (§2.2): a 501
/// that names the protocol, and nothing is sent.
#[tokio::test]
async fn anthropic_and_gemini_upstreams_are_refused() {
    for protocol in [Protocol::Anthropic, Protocol::Gemini] {
        let up = MockServer::start().await;
        let (_state, gw) = cloud(&up.uri(), protocol, &[("m", "m")]).await;
        let (status, body) = post(&gw, ask("m")).await;
        assert_eq!(status, 501, "{body}");
        assert_eq!(body["error"]["type"], "not_supported_error", "{body}");
        let msg = body["error"]["message"].as_str().unwrap();
        assert!(msg.contains(protocol.as_str()), "{msg}");
        assert!(forwarded(&up).await.is_empty());
    }
}

/// An upstream's errors are relayed as sent — TypeSafe's 422 for a malformed
/// question included — except a 401 or 403, which refuses lmgw's own
/// credential and is the gateway's 502 (§2.7).
#[tokio::test]
async fn an_upstream_401_or_403_is_a_502_and_its_other_errors_are_relayed() {
    const INVALID: &str = r#"{"detail": "questions.refund.instructions: field required"}"#;
    let up = MockServer::start().await;
    for (model, status) in [("m401", 401), ("m403", 403)] {
        Mock::given(method("POST"))
            .and(path("/systemone"))
            .and(body_partial_json(json!({"model": model})))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_json(json!({"error": {"message": "invalid api key"}})),
            )
            .mount(&up)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .and(body_partial_json(json!({"model": "m422"})))
        .respond_with(ResponseTemplate::new(422).set_body_raw(INVALID, "application/json"))
        .mount(&up)
        .await;
    let (_state, gw) = cloud(
        &up.uri(),
        Protocol::Openai,
        &[("a401", "m401"), ("a403", "m403"), ("a422", "m422")],
    )
    .await;

    for alias in ["a401", "a403"] {
        let (status, body) = post(&gw, ask(alias)).await;
        assert_eq!(status, 502, "{alias}: {body}");
        assert_eq!(body["error"]["code"], 502, "{body}");
        assert_eq!(body["error"]["type"], "server_error", "{body}");
    }
    let (status, _, bytes) = post_raw(
        &gw,
        serde_json::to_vec(&ask("a422")).unwrap(),
        "application/json",
    )
    .await;
    assert_eq!(
        (status, String::from_utf8(bytes).unwrap().as_str()),
        (422, INVALID)
    );
}

/// OpenRouter in slice 1 (§7 Q1): a plain openai-protocol row at its base,
/// the upstream's key as the bearer, the client's `model` rewritten to the
/// route's, and OpenRouter's documented answer back byte for byte —
/// `id`, `provider` and `usage.cost` included. With no price row the row
/// is unpriced, never 0.
#[tokio::test]
async fn an_openrouter_answer_is_relayed_byte_for_byte() {
    let up = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/systemone"))
        .and(has_header("authorization", "Bearer sk-up"))
        .and(body_partial_json(json!({"model": "typesafe/jev-1.13"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(OPENROUTER_ANSWER, "application/json"),
        )
        .mount(&up)
        .await;
    let (state, gw) = cloud(
        &format!("{}/api/v1", up.uri()),
        Protocol::Openai,
        &[("jev", "typesafe/jev-1.13")],
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/systemone"))
        .json(&ask("jev"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-lmgw-fallback").is_none());
    assert_eq!(resp.text().await.unwrap(), OPENROUTER_ANSWER);

    let row = row_after(&state, 0).await;
    assert_eq!(
        (row.prompt_tokens, row.completion_tokens, row.cost_micro),
        (Some(275), Some(20), None),
        "{row:?}"
    );
}

/// What lmgw refuses before routing is a 400 in llama.cpp's error shape with
/// lmgw's code (§2.7): bytes that are not JSON, JSON that is not an object,
/// and an object without a string `model`. An unknown name is a 404 in the
/// same shape. Each writes its row under the route's label.
#[tokio::test]
async fn what_lmgw_cannot_route_is_refused_in_llama_cpp_shape() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    for (sent, says) in [
        ("nope", "not JSON"),
        (r#"[{"model": "m"}]"#, "JSON object"),
        (r#"{"state": "hi"}"#, "'model' is required"),
        (r#"{"model": 7}"#, "'model' must be a string"),
    ] {
        let (status, _, bytes) = post_raw(&gw, sent.into(), "application/json").await;
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status, 400, "{sent}: {body}");
        assert_eq!(
            (
                &body["error"]["code"],
                &body["error"]["type"],
                &body["error"]["lmgw_code"]
            ),
            (
                &json!(400),
                &json!("invalid_request_error"),
                &json!("bad_request")
            ),
            "{body}"
        );
        assert!(
            body["error"]["message"].as_str().unwrap().contains(says),
            "{sent}: {body}"
        );
    }
    let (status, body) = post(&gw, ask("nobody")).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"]["type"], "not_found_error", "{body}");

    let rows = store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    assert_eq!(rows.len(), 5, "{rows:?}");
    assert!(
        rows.iter().all(|r| r.ingress_proto == "systemone"),
        "{rows:?}"
    );
}
