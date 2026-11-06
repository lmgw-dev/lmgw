//! An off on an Anthropic model, decided by the catalog's own
//! `thinking.types` (model-capabilities design §5.6, rev 2026-11-05): a
//! model that can switch off gets `thinking: {type: "disabled"}`; one that
//! cannot but takes `between_tools` (Claude Sonnet 5.5) gets that type; one
//! that takes neither (Claude Opus 5.5) gets its lowest effort level with
//! adaptive thinking — each with no refused send first; one whose catalog
//! says nothing gets the off, as before.
//!
//! And the Messages client's own `thinking` (§5.2): `between_tools` goes to
//! an Anthropic model as written and becomes another provider's off, named in
//! `x-lmgw-reasoning-ignored`; an unknown type is refused; `display` goes to
//! an Anthropic model inside the type that takes it and is named in
//! `x-lmgw-params-ignored` everywhere else.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ir::{Params, ReasoningControl};
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{serve, Gw};

/// An Anthropic upstream whose catalog lists `tgt` with `thinking` as given,
/// and whose Messages endpoint answers every request.
async fn upstream(thinking: Value) -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "type": "model", "id": "tgt", "max_tokens": 64000,
                "capabilities": {
                    "thinking": thinking,
                    "effort": {"supported": true,
                               "low": {"supported": true}, "high": {"supported": true}},
                }
            }],
            "has_more": false, "first_id": null, "last_id": null
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "model": "tgt", "role": "assistant",
            "content": [{"type": "text", "text": "OK"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 1}
        })))
        .mount(&mock)
        .await;
    mock
}

async fn world(base: &str) -> Gw {
    world_with(base, Protocol::Anthropic, Params::default()).await
}

/// One alias `m` onto `tgt` on an upstream of `protocol`, with `defaults` as
/// the alias's parameter defaults.
async fn world_with(base: &str, protocol: Protocol, defaults: Params) -> Gw {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
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
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "m".into(),
            upstream_id: up_id,
            upstream_model_id: "tgt".into(),
            param_overrides: defaults,
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    serve(state).await
}

/// An off on `/v1/messages`: the answer's `x-lmgw-reasoning-ignored` and the
/// one body the upstream got.
async fn off(thinking: Value) -> (Option<String>, Vec<Value>) {
    let mock = upstream(thinking).await;
    let gw = world(&mock.uri()).await;
    let (status, headers, sent) =
        ask(&gw, &mock, "/v1/messages", json!({"type": "disabled"})).await;
    assert_eq!(status, 200);
    (header(&headers, "x-lmgw-reasoning-ignored"), sent)
}

/// A `/v1/messages` request with `thinking` as given (`null`: none): its
/// status, headers, and the bodies `mock` got at `at`.
async fn ask(
    gw: &Gw,
    mock: &MockServer,
    at: &str,
    thinking: Value,
) -> (u16, reqwest::header::HeaderMap, Vec<Value>) {
    ask_with(gw, mock, at, thinking, &[]).await
}

async fn ask_with(
    gw: &Gw,
    mock: &MockServer,
    at: &str,
    thinking: Value,
    headers: &[(&str, &str)],
) -> (u16, reqwest::header::HeaderMap, Vec<Value>) {
    let mut body = json!({
        "model": "m", "max_tokens": 16,
        "messages": [{"role": "user", "content": "Reply with OK."}],
    });
    if !thinking.is_null() {
        body["thinking"] = thinking;
    }
    let mut rb = reqwest::Client::new()
        .post(format!("{gw}/v1/messages"))
        .header("anthropic-version", "2023-06-01");
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    let resp = rb.json(&body).send().await.unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let _ = resp.text().await;
    let sent = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == at)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    (status, headers, sent)
}

fn header(h: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    h.get(name).map(|v| v.to_str().unwrap().to_string())
}

/// An OpenAI-compatible upstream answering every chat request.
async fn compatible() -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "model": "tgt",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "OK"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&mock)
        .await;
    mock
}

/// Claude Sonnet 5.5's `thinking.types`, as Anthropic's catalog publishes
/// them: `between_tools` yes, `disabled` no.
fn sonnet_5_5() -> Value {
    json!({"supported": true, "types": {
        "adaptive": {"supported": true}, "between_tools": {"supported": true},
        "disabled": {"supported": false}, "enabled": {"supported": false},
    }})
}

#[tokio::test]
async fn a_model_that_can_switch_off_gets_the_off() {
    let (ignored, sent) = off(json!({"supported": true, "types": {
        "adaptive": {"supported": true}, "disabled": {"supported": true},
    }}))
    .await;
    assert_eq!(ignored, None);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["thinking"], json!({"type": "disabled"}));
}

#[tokio::test]
async fn a_model_that_cannot_switch_off_gets_its_lowest_level_first_time() {
    let (ignored, sent) = off(json!({"supported": true, "types": {
        "adaptive": {"supported": true}, "disabled": {"supported": false},
    }}))
    .await;
    assert_eq!(ignored.as_deref(), Some("enabled"));
    assert_eq!(sent.len(), 1, "no refused send first: {sent:?}");
    assert_eq!(sent[0]["thinking"], json!({"type": "adaptive"}));
    assert_eq!(sent[0]["output_config"]["effort"], "low");
}

#[tokio::test]
async fn a_catalog_silent_on_disabled_still_gets_the_off() {
    let (ignored, sent) = off(json!({"supported": true, "types": {
        "adaptive": {"supported": true},
    }}))
    .await;
    assert_eq!(ignored, None);
    assert_eq!(sent[0]["thinking"], json!({"type": "disabled"}));
}

/// Claude Sonnet 5.5: no `disabled`, but `between_tools`, its documented
/// lowest setting — sent bare (the type takes no other field) and with no
/// effort beside it, so the model's own default applies; reported as
/// honoured, being the model's own off.
#[tokio::test]
async fn a_model_whose_lowest_setting_is_between_tools_gets_that_type() {
    let (ignored, sent) = off(sonnet_5_5()).await;
    assert_eq!(ignored, None);
    assert_eq!(sent.len(), 1, "no refused send first: {sent:?}");
    assert_eq!(sent[0]["thinking"], json!({"type": "between_tools"}));
    assert!(sent[0].get("output_config").is_none(), "{}", sent[0]);
}

/// Claude Opus 5.5 (and Fable, Mythos): neither `disabled` nor
/// `between_tools` — its lowest effort level, as before.
#[tokio::test]
async fn a_model_that_takes_neither_off_type_gets_its_lowest_level() {
    let (ignored, sent) = off(json!({"supported": true, "types": {
        "adaptive": {"supported": true}, "between_tools": {"supported": false},
        "disabled": {"supported": false}, "enabled": {"supported": false},
    }}))
    .await;
    assert_eq!(ignored.as_deref(), Some("enabled"));
    assert_eq!(sent[0]["thinking"], json!({"type": "adaptive"}));
    assert_eq!(sent[0]["output_config"]["effort"], "low");
}

/// Claude Haiku 5.5 takes `disabled` only at effort `high` or below. An off
/// carries no effort (§5.1), not even an alias default of `xhigh` under an
/// `x-lmgw-reasoning: off`, so the model's own default effort applies.
#[tokio::test]
async fn an_off_never_carries_an_effort_the_disabled_type_refuses() {
    let mock = upstream(json!({"supported": true, "types": {
        "adaptive": {"supported": true}, "between_tools": {"supported": false},
        "disabled": {"supported": true}, "enabled": {"supported": false},
    }}))
    .await;
    let defaults = Params {
        reasoning: Some(ReasoningControl {
            effort: Some("xhigh".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let gw = world_with(&mock.uri(), Protocol::Anthropic, defaults).await;
    let (status, headers, sent) = ask_with(
        &gw,
        &mock,
        "/v1/messages",
        Value::Null,
        &[("x-lmgw-reasoning", "off")],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(header(&headers, "x-lmgw-reasoning-ignored"), None);
    assert_eq!(sent[0]["thinking"], json!({"type": "disabled"}));
    assert!(sent[0].get("output_config").is_none(), "{}", sent[0]);
}

/// A client's own `between_tools` goes to an Anthropic model as written,
/// whatever its catalog says: the client chose the protocol's spelling.
#[tokio::test]
async fn a_clients_between_tools_goes_to_an_anthropic_model_as_written() {
    for thinking in [
        sonnet_5_5(),
        json!({"supported": true, "types": {"adaptive": {"supported": true}}}),
    ] {
        let mock = upstream(thinking).await;
        let gw = world(&mock.uri()).await;
        let (status, headers, sent) =
            ask(&gw, &mock, "/v1/messages", json!({"type": "between_tools"})).await;
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "x-lmgw-reasoning-ignored"), None);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["thinking"], json!({"type": "between_tools"}));
    }
}

/// Elsewhere it is that provider's off, the nearest meaning, and the answer
/// says the rest of it was not kept.
#[tokio::test]
async fn a_clients_between_tools_is_another_providers_off_and_named() {
    let mock = compatible().await;
    let gw = world_with(&mock.uri(), Protocol::OpenaiCompatible, Params::default()).await;
    let (status, headers, sent) = ask(
        &gw,
        &mock,
        "/chat/completions",
        json!({"type": "between_tools"}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "x-lmgw-reasoning-ignored").as_deref(),
        Some("between_tools")
    );
    assert_eq!(sent[0]["reasoning_effort"], "none");
}

/// A thinking type lmgw does not know is refused by name, never dropped.
#[tokio::test]
async fn an_unknown_thinking_type_is_refused_by_name() {
    let mock = upstream(sonnet_5_5()).await;
    let gw = world(&mock.uri()).await;
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "m", "max_tokens": 16,
            "thinking": {"type": "interleaved_lite"},
            "messages": [{"role": "user", "content": "Reply with OK."}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let text = resp.text().await.unwrap();
    assert!(text.contains("interleaved_lite"), "{text}");
    let sent = mock.received_requests().await.unwrap();
    assert!(
        sent.iter().all(|r| r.url.path() != "/v1/messages"),
        "nothing was sent"
    );
}

/// `display` rides inside the type that takes it to an Anthropic model;
/// beside an off there, and to any other provider, it is left out and
/// named.
#[tokio::test]
async fn thinking_display_goes_where_it_is_taken_and_is_named_elsewhere() {
    let mock = upstream(sonnet_5_5()).await;
    let gw = world(&mock.uri()).await;
    let shown = json!({"type": "adaptive", "display": "summarized"});
    let (status, headers, sent) = ask(&gw, &mock, "/v1/messages", shown.clone()).await;
    assert_eq!(status, 200);
    assert_eq!(header(&headers, "x-lmgw-params-ignored"), None);
    assert_eq!(sent[0]["thinking"], shown);

    // An off over it (the header): between_tools takes no other field.
    let (status, headers, sent) = ask_with(
        &gw,
        &mock,
        "/v1/messages",
        shown.clone(),
        &[("x-lmgw-reasoning", "off")],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "x-lmgw-params-ignored").as_deref(),
        Some("thinking.display")
    );
    assert_eq!(sent[1]["thinking"], json!({"type": "between_tools"}));

    let other = compatible().await;
    let gw = world_with(&other.uri(), Protocol::OpenaiCompatible, Params::default()).await;
    let (status, headers, sent) = ask(&gw, &other, "/chat/completions", shown).await;
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "x-lmgw-params-ignored").as_deref(),
        Some("thinking.display")
    );
    assert!(sent[0].get("thinking").is_none(), "{}", sent[0]);
}
