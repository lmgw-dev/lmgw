//! A `/v1/messages` request's `context_management` (gateway design §6.1):
//! an Anthropic upstream gets it as written and its answer's
//! `context_management` comes back, whole and streamed; every other
//! upstream gets neither it nor the top-level `compaction`, and
//! `x-lmgw-params-ignored` says so (compaction itself:
//! `messages_compaction`). The body with
//! it passes the level-1 schema check as the client's own key, and so does
//! a `/v1/messages/count_tokens` body, whose answer's `context_management`
//! (`original_input_tokens`) comes back.

use lmgw_core::config::{Protocol, Upstream, UpstreamKind};
use lmgw_core::egress::for_protocol;
use lmgw_core::egress::watch::Built;
use lmgw_core::ingress::anthropic::parse_messages_request;
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Map, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::api_compat::egress;
use crate::common::{serve, Gw};

/// One alias, `m`, on an upstream of `protocol` at `base`.
pub(crate) async fn world(base: &str, protocol: Protocol) -> Gw {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "cm-up".into(),
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
            param_overrides: Default::default(),
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

const BETA: &str = "context-management-2025-06-27";

fn clearing() -> Value {
    json!({"edits": [{
        "type": "clear_tool_uses_20250919",
        "trigger": {"type": "input_tokens", "value": 30000},
        "keep": {"type": "tool_uses", "value": 3},
    }]})
}

fn applied() -> Value {
    json!({"applied_edits": [{
        "type": "clear_tool_uses_20250919",
        "cleared_tool_uses": 2,
        "cleared_input_tokens": 1200,
    }]})
}

fn body(cm: Value, stream: bool) -> Value {
    json!({
        "model": "m", "max_tokens": 16, "stream": stream,
        "messages": [{"role": "user", "content": "hi"}],
        "context_management": cm,
    })
}

pub(crate) async fn post(gw: &Gw, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gw}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", BETA)
        .json(body)
        .send()
        .await
        .unwrap()
}

/// The JSON bodies the mock received at `p`.
pub(crate) async fn bodies_at(mock: &MockServer, p: &str) -> Vec<Value> {
    mock.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == p)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn an_anthropic_upstream_gets_it_as_written_and_its_answer_comes_back() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "model": "tgt", "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 1},
            "context_management": applied(),
        })))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;

    let resp = post(&gw, &body(clearing(), false)).await;
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers().get("x-lmgw-params-ignored").is_none(),
        "nothing was left out: {:?}",
        resp.headers()
    );
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(answer["context_management"], applied(), "{answer}");

    let sent = bodies_at(&mock, "/v1/messages").await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["context_management"], clearing());
    let received = mock.received_requests().await.unwrap();
    assert_eq!(received[0].headers["anthropic-beta"], BETA);
}

#[tokio::test]
async fn a_streamed_answers_context_management_rides_its_closing_message_delta() {
    let sse = [
        json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "tgt",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 3, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": "ok"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta",
               "delta": {"stop_reason": "end_turn", "stop_sequence": null},
               "usage": {"output_tokens": 1},
               "context_management": applied()}),
        json!({"type": "message_stop"}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect::<String>();
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse, "text/event-stream"),
        )
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;

    let resp = post(&gw, &body(clearing(), true)).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let deltas: Vec<Value> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter(|v| v["type"] == "message_delta")
        .collect();
    assert_eq!(deltas.len(), 1, "{text}");
    assert_eq!(deltas[0]["context_management"], applied(), "{text}");
    assert_eq!(
        bodies_at(&mock, "/v1/messages").await[0]["context_management"],
        clearing()
    );
}

/// A count with context editing: Anthropic's answer says what the input was
/// before the edits, and the Messages count answer relays it. The body goes
/// through the level-1 schema check like every egress body (the watcher the
/// test process installs), with `context_management` as the client's own
/// key: the pinned non-beta count schema has no such field.
#[tokio::test]
async fn a_count_relays_the_original_input_tokens() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "input_tokens": 25000,
            "context_management": {"original_input_tokens": 70000},
        })))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;

    let mut b = body(clearing(), false);
    b.as_object_mut().unwrap().remove("max_tokens");
    b.as_object_mut().unwrap().remove("stream");
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/messages/count_tokens"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", BETA)
        .json(&b)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(
        answer,
        json!({"input_tokens": 25000,
               "context_management": {"original_input_tokens": 70000}})
    );
    let sent = bodies_at(&mock, "/v1/messages/count_tokens").await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["context_management"], clearing());

    // The same body as the watcher saw it passes the pinned schema only
    // with the key marked as the client's.
    let bytes = serde_json::to_vec(&sent[0]).unwrap();
    let check = |passthrough: &Map<String, Value>| {
        egress::check(&Built {
            protocol: Protocol::Anthropic,
            url: "https://anthropic.test/v1/messages/count_tokens",
            body: &bytes,
            passthrough,
        })
    };
    let mut verbatim = Map::new();
    verbatim.insert("context_management".into(), clearing());
    let out = check(&verbatim);
    assert!(out.entry.is_some());
    assert!(out.violations.is_empty(), "{:?}", out.violations);
    assert!(!check(&Map::new()).violations.is_empty());
}

/// A count of a request without context editing answers the number alone.
#[tokio::test]
async fn a_count_without_it_answers_the_number_alone() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 7})))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Anthropic).await;
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/messages/count_tokens"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({"input_tokens": 7})
    );
}

#[tokio::test]
async fn another_upstream_gets_none_and_the_answer_names_it() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "model": "tgt",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::OpenaiCompatible).await;

    // A compaction edit too: it is dropped with the rest; and so is
    // on-demand compaction, which has no equivalent here either.
    let cm = json!({"edits": [{"type": "compact_20260112"}]});
    let mut b = body(cm, false);
    b["compaction"] = json!({"type": "summarize"});
    let resp = post(&gw, &b).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["x-lmgw-params-ignored"],
        "context_management,compaction"
    );
    let sent = bodies_at(&mock, "/chat/completions").await;
    assert_eq!(sent.len(), 1);
    assert!(sent[0].get("context_management").is_none(), "{}", sent[0]);
    assert!(sent[0].get("compaction").is_none(), "{}", sent[0]);
}

/// Level 1 (api-compat design): the Anthropic body carrying it passes the
/// pinned schema as the client's own key. The pinned schema is the SDK's
/// non-beta request, which has no `context_management`, so the same body
/// without the key marked as the client's is refused: what the egress shows
/// the watcher is what saves it.
#[test]
fn the_anthropic_body_with_it_passes_the_schema_check_as_the_clients_key() {
    let up = Upstream {
        id: 9,
        name: "anthropic".into(),
        protocol: Protocol::Anthropic,
        kind: UpstreamKind::Generic,
        base_url: "https://anthropic.test".into(),
        api_key: Some("sk-test".into()),
        extra_headers: vec![],
        timeout_ms: 30_000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    };
    let ir = parse_messages_request(&body(clearing(), false)).unwrap();
    assert_eq!(ir.context_management, Some(clearing()));
    let req = for_protocol(Protocol::Anthropic)
        .build_chat(&reqwest::Client::new(), &up, "tgt", &ir, &ir.params, false)
        .unwrap()
        .build()
        .unwrap();
    let bytes = req.body().and_then(reqwest::Body::as_bytes).unwrap();
    let sent: Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(sent["context_management"], clearing());

    let mut verbatim = Map::new();
    verbatim.insert("context_management".into(), clearing());
    let check = |passthrough: &Map<String, Value>| {
        egress::check(&Built {
            protocol: Protocol::Anthropic,
            url: req.url().as_str(),
            body: bytes,
            passthrough,
        })
    };
    let out = check(&verbatim);
    assert!(out.entry.is_some());
    assert!(out.violations.is_empty(), "{:?}", out.violations);
    let out = check(&Map::new());
    assert!(
        out.violations
            .iter()
            .any(|l| l.to_string().contains("context_management")),
        "{:?}",
        out.violations
    );
}
