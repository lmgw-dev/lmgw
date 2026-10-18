//! The `/v1` answers that are hand-written schemas in the document, held to
//! real answers: a plain chat completion, a Responses body an upstream that
//! speaks the API natively sends (relayed as sent, background and all), and
//! an image an upstream answers with a url. The streamed chat answer is in
//! `stream_usage`, the Chat turn's frames in `chat_turn_wire`.
//!
//! System One's schema is generated from `lmgw_api_types::systemone`
//! instead (decision-models design §2.8), and held the same way: llama.cpp's,
//! TypeSafe's and OpenRouter's answers read into the type without loss and
//! validate against the document.

use lmgw_api_types::systemone::{Question, SystemOneRequest, SystemOneResponse};
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{round_trips, serve, validates_against, validates_against_component_of};
use crate::support::decision_world::{ticket, LLAMA_ANSWER, OPENROUTER_ANSWER, TYPESAFE_ANSWERS};

/// A gateway with the alias `my-model` on `upstream`.
async fn gateway(upstream: &MockServer, native_responses: bool) -> crate::common::Gw {
    let state = AppState::init_for_tests().await.unwrap();
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: upstream.uri().trim_end_matches('/').to_string(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: native_responses,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "my-model".into(),
            upstream_id: up,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    serve(state).await
}

async fn post(gw: &crate::common::Gw, route: &str, body: Value) -> Value {
    let resp = gw
        .client()
        .post(format!("{gw}{route}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, 200, "{route}: {text}");
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("{route}: not JSON: {text}"))
}

#[tokio::test]
async fn a_chat_completion_validates() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c1", "object": "chat.completion", "created": 1, "model": "tgt-model",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "ok"}}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
        })))
        .mount(&upstream)
        .await;
    let gw = gateway(&upstream, false).await;
    let answer = post(
        &gw,
        "/v1/chat/completions",
        json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    validates_against_component_of("chat completion", "ChatCompletion", &answer);
}

/// A native upstream's body is relayed as it sends it: a response that is
/// still `queued` has no usage yet and no `store` key.
#[tokio::test]
async fn a_native_background_response_validates() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_1", "object": "response", "created_at": 1, "status": "queued",
            "model": "tgt-model", "output": [], "error": null, "incomplete_details": null,
            "usage": null, "background": true
        })))
        .mount(&upstream)
        .await;
    let gw = gateway(&upstream, true).await;
    let answer = post(
        &gw,
        "/v1/responses",
        json!({"model": "my-model", "input": "hi"}),
    )
    .await;
    assert_eq!(answer["status"], "queued", "{answer}");
    validates_against_component_of("native response", "Response", &answer);
}

/// `response_format: url` (an upstream's default) answers `url` and
/// `revised_prompt`, no `b64_json`.
#[tokio::test]
async fn an_image_url_answer_validates() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 1,
            "data": [{"url": "https://example.test/i.png", "revised_prompt": "a red bicycle"}]
        })))
        .mount(&upstream)
        .await;
    let gw = gateway(&upstream, false).await;
    let answer = post(
        &gw,
        "/v1/images/generations",
        json!({"model": "my-model", "prompt": "a red bicycle"}),
    )
    .await;
    validates_against_component_of("image generations", "ImageGenerationsResponse", &answer);
}

/// System One's answer, from each of its three references (§2.8): relayed
/// from an upstream that answers with it, read into `SystemOneResponse`
/// and written back without loss — OpenRouter's `id`, `provider`,
/// `usage.cost` and non-zero output tokens included — and valid against the
/// documented component.
#[tokio::test]
async fn system_one_answers_round_trip_and_validate() {
    let answers = [
        ("llama.cpp (Appendix A.4)", LLAMA_ANSWER),
        ("TypeSafe noul", TYPESAFE_ANSWERS[0]),
        ("TypeSafe choice", TYPESAFE_ANSWERS[1]),
        ("TypeSafe score", TYPESAFE_ANSWERS[2]),
        ("OpenRouter", OPENROUTER_ANSWER),
    ];
    for (what, sent) in answers {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sent, "application/json"))
            .mount(&upstream)
            .await;
        let gw = gateway(&upstream, false).await;
        let body: Value = serde_json::from_str(&ticket("my-model")).unwrap();
        let answer = post(&gw, "/v1/systemone", body).await;
        let typed = round_trips::<SystemOneResponse>(what, &answer);
        assert!(!typed.answers.is_empty(), "{what}");
        validates_against_component_of(what, "SystemOneResponse", &answer);
    }
}

/// The request type takes Appendix A.3's ticket whole, keeps its options in
/// the order sent, and the ticket is valid against the documented request
/// schema.
#[test]
fn the_system_one_request_type_takes_the_ticket_whole() {
    // Read from the text: a `Value` would sort the members first.
    let text = ticket("decide-laya");
    let sent: Value = serde_json::from_str(&text).unwrap();
    let typed: SystemOneRequest = serde_json::from_str(&text).unwrap();
    assert_eq!(serde_json::to_value(&typed).unwrap(), sent);
    let Some(Question::Choice(department)) = typed.questions.get("department") else {
        panic!("{typed:?}")
    };
    assert_eq!(
        department.criteria.keys().collect::<Vec<_>>(),
        ["billing", "technical", "sales", "other"]
    );
    assert_eq!(
        typed.questions.keys().collect::<Vec<_>>(),
        ["department", "urgency", "angry"]
    );
    let doc = lmgw_core::openapi::admin_doc();
    let schema = &doc["paths"]["/v1/systemone"]["post"]["requestBody"]["content"]
        ["application/json"]["schema"];
    assert!(schema.is_object(), "no request schema");
    validates_against("the ticket", doc, schema, &sent);
}
