//! A fallback that cannot see, on `/v1` (the owner's ruling, 2026-10-06: a
//! configured fallback is always used, with no exception by content; only
//! capability keeps content from a route — `gate::fallback_images`).
//!
//! Under the GPU hold the fallback answers every ingress — `/v1/chat/
//! completions`, `/v1/messages`, `/v1/responses` synthesized and native —
//! and the request's images reach it as placeholders, a tool result's
//! included, with a WARN; `x-lmgw-fallback` says who answered and
//! `x-lmgw-images-omitted` how many images it did not get. A ladder climb's
//! fallback gets them the same way, re-fitted on the route it hands the
//! request to. A fallback that sees gets the images, and so does a route
//! the client named itself. `/v1/messages/count_tokens` counts what the send
//! carries, on an OpenAI and on an Anthropic route. The Chat's half is
//! `blind_fallback_chat`; the outside-VRAM swap's twin is
//! `outside_vram_fallback::images_go_to_a_fallback_that_cannot_see_them_as_placeholders`.
//!
//! Every fallback is a wiremock ([`cloud_chat`]): nothing here calls a cloud.

use super::outside_vram_fallback::{cloud_alias_seeing, last_cloud_chat};
use super::*;
use crate::common::captured_log::capture_log;

/// One PNG, as a client inlines it.
pub(super) const PNG_B64: &str = "iVBORw0KGgo=";

/// The reason every placeholder gives: no alias, the text goes to the
/// provider.
pub(super) const OMITTED: &str = "omitted: the answering model cannot see images]";

/// The request row's marker for one image `cloud-blind` got as a
/// placeholder (`request_logs.degraded`).
pub(super) const ONE_PLACEHOLDER: &str =
    "fallback 'cloud-blind' lacks vision: 1 image sent as a placeholder";

/// The newest request row for `alias`: what its content lost.
pub(super) async fn marker_of(f: &Fixture, alias: &str) -> Option<String> {
    newest_log(f, alias, 0).await.degraded
}

/// The fixture's cloud upstream with three fallbacks on it — `cloud-chat`
/// (vision unknown), `cloud-blind` (`vision: false`) and `cloud-sees`
/// (`vision: true`) — `fallback` the global one, and the GPU hold on.
pub(super) async fn held_with(f: &Fixture, fallback: &str) -> MockServer {
    let cloud = cloud_chat(f, "cloud-chat").await;
    cloud_alias_seeing(f, "cloud-blind", false).await;
    cloud_alias_seeing(f, "cloud-sees", true).await;
    set_global_fallback(f, fallback).await;
    engage_hold(f).await;
    cloud
}

pub(super) async fn post(f: &Fixture, route: &str, body: Value) -> reqwest::Response {
    f.gateway
        .client()
        .post(format!("{}{route}", f.gateway))
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn header_of(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn openai_with_image(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": [
        {"type": "text", "text": "what is this"},
        {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{PNG_B64}")}},
    ]}]})
}

/// An Anthropic conversation whose tool returned a screenshot: the tool
/// result's image block, after `prompt`.
fn tool_image_messages(model: &str, prompt: &str) -> Value {
    json!({"model": model, "max_tokens": 16,
    "tools": [{"name": "shot", "input_schema": {"type": "object"}}],
    "messages": [
        {"role": "user", "content": prompt},
        {"role": "assistant", "content": [
            {"type": "tool_use", "id": "t1", "name": "shot", "input": {}}]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "text", "text": "the screen:"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                    "data": PNG_B64}}]}]},
    ]})
}

/// `/v1/chat/completions`, `/v1/messages` (a user image, then a tool
/// result's) and `/v1/responses` (synthesized) under the hold, to a fallback
/// that cannot see: each is answered by it, `x-lmgw-fallback` names it,
/// `x-lmgw-images-omitted` counts the images, its request carries the
/// placeholder and no image, and the log WARNs.
#[tokio::test]
async fn under_the_hold_a_blind_fallback_answers_v1_with_placeholders() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let cloud = held_with(&f, "cloud-blind").await;
    let (log, capturing) = capture_log();
    let answered = |resp: &reqwest::Response| {
        assert_eq!(resp.status(), 200);
        assert_eq!(
            header_of(resp, "x-lmgw-fallback").as_deref(),
            Some("cloud-blind")
        );
        assert_eq!(fallback_reason(resp), Some("hold"));
        assert_eq!(
            header_of(resp, "x-lmgw-images-omitted").as_deref(),
            Some("1")
        );
    };

    let resp = post(&f, "/v1/chat/completions", openai_with_image("chat-model")).await;
    answered(&resp);
    let sent = last_cloud_chat(&cloud).await;
    assert!(sent.contains(OMITTED), "{sent}");
    assert!(sent.contains("what is this"), "{sent}");
    assert!(!sent.contains(PNG_B64), "{sent}");
    // The request row says what it lost (the owner's requirement).
    assert_eq!(
        marker_of(&f, "chat-model").await.as_deref(),
        Some(ONE_PLACEHOLDER)
    );

    let resp = post(
        &f,
        "/v1/messages",
        json!({"model": "chat-model", "max_tokens": 64, "messages": [{"role": "user",
        "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                "data": PNG_B64}},
            {"type": "text", "text": "and this?"},
        ]}]}),
    )
    .await;
    answered(&resp);
    let sent = last_cloud_chat(&cloud).await;
    assert!(sent.contains(OMITTED), "{sent}");
    assert!(sent.contains("and this?"), "{sent}");
    assert!(!sent.contains(PNG_B64), "{sent}");

    // A tool result's image: its own reason, not the text-only slot's.
    let resp = post(
        &f,
        "/v1/messages",
        tool_image_messages("chat-model", "shoot"),
    )
    .await;
    answered(&resp);
    let sent = last_cloud_chat(&cloud).await;
    assert!(sent.contains(OMITTED), "{sent}");
    assert!(sent.contains("the screen:"), "{sent}");
    assert!(!sent.contains("text-only"), "{sent}");
    assert_eq!(
        marker_of(&f, "chat-model").await.as_deref(),
        Some(ONE_PLACEHOLDER)
    );

    let resp = post(
        &f,
        "/v1/responses",
        json!({"model": "chat-model", "store": false, "input": [{"role": "user", "content": [
            {"type": "input_text", "text": "and that?"},
            {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG_B64}")},
        ]}]}),
    )
    .await;
    answered(&resp);
    let sent = last_cloud_chat(&cloud).await;
    assert!(sent.contains(OMITTED), "{sent}");
    assert!(sent.contains("and that?"), "{sent}");
    assert!(!sent.contains(PNG_B64), "{sent}");
    // The run's turn row, written from the runner's own placeholders.
    assert_eq!(
        marker_of(&f, "chat-model").await.as_deref(),
        Some(ONE_PLACEHOLDER)
    );

    drop(capturing);
    let warned = log.text();
    assert!(
        warned.contains(
            "images not sent to the fallback 'cloud-blind' answering for 'chat-model', which \
             cannot see images — replaced with placeholders: image/png image, 12 base64 bytes"
        ),
        "{warned}"
    );
    assert!(f.runs().is_empty(), "{:?}", f.runs());
}

/// The same request to a fallback that sees, and to one whose vision is
/// unknown: the image goes as it was sent, and no count is stamped.
#[tokio::test]
async fn under_the_hold_a_fallback_that_sees_gets_the_images() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let cloud = held_with(&f, "cloud-sees").await;
    for fallback in ["cloud-sees", "cloud-chat"] {
        set_global_fallback(&f, fallback).await;
        let resp = post(&f, "/v1/chat/completions", openai_with_image("chat-model")).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(
            header_of(&resp, "x-lmgw-fallback").as_deref(),
            Some(fallback)
        );
        assert_eq!(header_of(&resp, "x-lmgw-images-omitted"), None);
        let sent = last_cloud_chat(&cloud).await;
        assert!(
            sent.contains(&format!("data:image/png;base64,{PNG_B64}")),
            "{sent}"
        );
        assert!(!sent.contains("omitted:"), "{sent}");
        assert_eq!(marker_of(&f, "chat-model").await, None, "nothing was lost");
    }
}

/// A route the client named itself is never touched (review I9): a client
/// that asks for the blind model by name sends it its image, as it chose,
/// and the provider answers for itself.
#[tokio::test]
async fn a_client_that_names_a_blind_model_sends_it_its_image() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let cloud = held_with(&f, "cloud-blind").await;
    let resp = post(&f, "/v1/chat/completions", openai_with_image("cloud-blind")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header_of(&resp, "x-lmgw-fallback"), None);
    assert_eq!(header_of(&resp, "x-lmgw-images-omitted"), None);
    let sent = last_cloud_chat(&cloud).await;
    assert!(sent.contains(PNG_B64), "{sent}");
    assert!(!sent.contains("omitted:"), "{sent}");
}

/// A cloud upstream of the fixture's cloud mock that implements
/// `/v1/responses` natively, with the blind alias `native-blind` on it.
async fn native_blind(f: &Fixture, cloud: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_1", "object": "response", "status": "completed", "output": [],
        })))
        .mount(cloud)
        .await;
    let native = store::insert_upstream(
        &f.state.db,
        &store::NewUpstream {
            name: "cloud-native".into(),
            protocol: lmgw_core::config::Protocol::Openai,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: format!("{}/v1", cloud.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &f.state.db,
        &store::NewAlias {
            alias: "native-blind".into(),
            upstream_id: native,
            upstream_model_id: "gpt-native".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({"capabilities": {
                "task": "chat",
                "endpoints": ["/v1/responses"],
                "input_modalities": ["text"],
                "vision": false,
                "source": "owner",
            }})),
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// The last body the native upstream got.
async fn last_native(cloud: &MockServer) -> Value {
    let reqs = cloud.received_requests().await.unwrap();
    let sent = reqs
        .iter()
        .rev()
        .find(|r| r.url.path() == "/v1/responses")
        .expect("the native upstream was sent the body");
    serde_json::from_slice(&sent.body).unwrap()
}

/// `/v1/responses`' native passthrough forwards the client's body; under the
/// hold, to a fallback that cannot see, each `input_image` in it becomes an
/// `input_text` placeholder and nothing else changes — also one the IR
/// keeps as text, a tool output's image by URL (review I3), when it is the
/// body's only image.
#[tokio::test]
async fn native_responses_passthrough_to_a_blind_fallback_sends_placeholders() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let cloud = cloud_chat(&f, "cloud-chat").await;
    native_blind(&f, &cloud).await;
    set_global_fallback(&f, "native-blind").await;
    engage_hold(&f).await;

    let resp = post(
        &f,
        "/v1/responses",
        json!({"model": "chat-model", "input": [{"role": "user", "content": [
            {"type": "input_text", "text": "what is this"},
            {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG_B64}")},
        ]}]}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header_of(&resp, "x-lmgw-fallback").as_deref(),
        Some("native-blind")
    );
    assert_eq!(
        header_of(&resp, "x-lmgw-images-omitted").as_deref(),
        Some("1")
    );
    let body = last_native(&cloud).await;
    assert_eq!(body["model"], "gpt-native");
    assert_eq!(
        marker_of(&f, "chat-model").await.as_deref(),
        Some("fallback 'native-blind' lacks vision: 1 image sent as a placeholder")
    );
    let content = &body["input"][0]["content"];
    assert_eq!(
        content[0],
        json!({"type": "input_text", "text": "what is this"})
    );
    assert_eq!(
        content[1],
        json!({"type": "input_text", "text":
            "[image/png image, 12 base64 bytes — omitted: the answering model cannot see images]"})
    );

    let resp = post(
        &f,
        "/v1/responses",
        json!({"model": "chat-model", "input": [
            {"role": "user", "content": "look at the shot"},
            {"type": "function_call", "call_id": "c1", "name": "shot", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": [
                {"type": "input_image", "image_url": "https://example.com/s.png?sig=x"}]},
        ]}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header_of(&resp, "x-lmgw-images-omitted").as_deref(),
        Some("1")
    );
    let body = last_native(&cloud).await;
    assert_eq!(
        body["input"][2]["output"][0],
        json!({"type": "input_text", "text":
            "[image/png image by URL — omitted: the answering model cannot see images]"})
    );
    assert!(!body.to_string().contains("example.com"), "{body}");
}

/// A ladder climb that cannot happen hands the request to the fallback
/// (ladder §12 entry 8), which is fitted again on its own route: a tool
/// result's image reaches a fallback that cannot see as its placeholder
/// (review I9) — the IR the climb's re-fit sends is the fallback's, not the
/// one fitted for the rung.
#[tokio::test]
async fn a_climbs_blind_fallback_gets_the_images_as_placeholders() {
    let f = ladder_fixture(12 * GIB, 0).await;
    f.attribute(6 * GIB);
    let cloud = cloud_chat(&f, "cloud-chat").await;
    cloud_alias_seeing(&f, "cloud-blind", false).await;
    set_global_fallback(&f, "cloud-blind").await;
    // Rung 3 needs 8.5 GiB: 12 − 6 outside − 2 (the base) = 4 free.
    let resp = post(
        &f,
        "/v1/messages",
        tool_image_messages(LADDER, &words("w", 200)),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header_of(&resp, "x-lmgw-fallback").as_deref(),
        Some("cloud-blind")
    );
    assert_eq!(fallback_reason(&resp), Some("external_vram"));
    assert_eq!(
        header_of(&resp, "x-lmgw-images-omitted").as_deref(),
        Some("1")
    );
    let sent = last_cloud_chat(&cloud).await;
    assert!(sent.contains(OMITTED), "{sent}");
    assert!(!sent.contains(PNG_B64), "{sent}");
    assert_eq!(
        marker_of(&f, LADDER).await.as_deref(),
        Some(ONE_PLACEHOLDER)
    );
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf"]);
}

/// `/v1/messages/count_tokens` under the hold counts what the send to a
/// fallback that cannot see carries (review I4, I9): on an OpenAI route the
/// placeholder, flattened, with no `media_omitted` (no image is left out of
/// the count, it is text now); on an Anthropic route the client's body with
/// the same placeholder as a text block, counted by the provider.
#[tokio::test]
async fn count_tokens_counts_what_a_blind_fallback_is_sent() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let _cloud = held_with(&f, "cloud-blind").await;
    let body = json!({"model": "chat-model", "messages": [{"role": "user", "content": [
        {"type": "image", "source": {"type": "base64", "media_type": "image/png",
            "data": PNG_B64}},
        {"type": "text", "text": "what is this"},
    ]}]});

    let resp = post(&f, "/v1/messages/count_tokens", body.clone()).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header_of(&resp, "x-lmgw-fallback").as_deref(),
        Some("cloud-blind")
    );
    let approx = header_of(&resp, "x-lmgw-count-approximate").unwrap_or_default();
    assert!(approx.contains("flattened"), "{approx}");
    assert!(!approx.contains("media_omitted"), "{approx}");
    let n: Value = resp.json().await.unwrap();
    assert!(
        n["input_tokens"].as_u64().unwrap() > 10,
        "the placeholder counts: {n}"
    );

    // An Anthropic-protocol fallback that cannot see.
    let anthropic = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 42})))
        .mount(&anthropic)
        .await;
    let up = store::insert_upstream(
        &f.state.db,
        &store::NewUpstream {
            name: "anthropic".into(),
            protocol: lmgw_core::config::Protocol::Anthropic,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: anthropic.uri(),
            api_key: Some("sk-test".into()),
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &f.state.db,
        &store::NewAlias {
            alias: "anthropic-blind".into(),
            upstream_id: up,
            upstream_model_id: "claude-text".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({"capabilities": {
                "task": "chat", "endpoints": ["/v1/messages"],
                "input_modalities": ["text"], "vision": false, "source": "owner",
            }})),
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    set_global_fallback(&f, "anthropic-blind").await;

    let resp = post(&f, "/v1/messages/count_tokens", body).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header_of(&resp, "x-lmgw-fallback").as_deref(),
        Some("anthropic-blind")
    );
    let reqs = anthropic.received_requests().await.unwrap();
    let counted: Value = serde_json::from_slice(&reqs.last().unwrap().body).unwrap();
    assert_eq!(
        counted["messages"][0]["content"][0],
        json!({"type": "text", "text":
            "[image/png image, 12 base64 bytes — omitted: the answering model cannot see images]"}),
        "{counted}"
    );
    assert_eq!(counted["messages"][0]["content"][1]["text"], "what is this");
}
